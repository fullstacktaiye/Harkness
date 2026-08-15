use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use harkness_core::ProjectId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tempfile::TempDir;
use time::OffsetDateTime;

use crate::agent::{
    AgentAction, ApprovalOutcomeView, MockAgent, ObservationPattern, Scenario, ScenarioId,
    ScenarioStep, WorkspaceRef,
};
use crate::approval::{ApprovalDecision, ApprovalScope, ApprovalState, DecidedVia};
use crate::domain::{ExecutionState, RunId, Task};
use crate::policy::{PolicyEngine, UserPolicy};
use crate::store::{EventKind, PassThrough, Store};
use crate::tool::{
    ExecutionContext, RiskLevel, Tool, ToolError, ToolIdentity, ToolMetadata, ToolRegistry,
};
use crate::trust::{TrustState, WorkspaceTrust};

use super::{RunCoordinator, RunSnapshot};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ObserveInput {
    message: String,
}

#[derive(Debug, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct ObserveOutput {
    echoed: String,
}

struct CountingObserve {
    executions: Arc<AtomicUsize>,
}

struct CountingWrite {
    executions: Arc<AtomicUsize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyInput {}

struct BlockingObserve {
    started: Arc<AtomicUsize>,
}

impl Tool for BlockingObserve {
    type Input = EmptyInput;
    type Output = ObserveOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse("fixture.blocking", "1.0.0").unwrap(),
            "Blocking fixture",
            "Waits cooperatively for cancellation.",
            RiskLevel::Observe,
        )
    }

    fn execute(
        &self,
        _input: EmptyInput,
        context: &mut ExecutionContext,
    ) -> Result<ObserveOutput, ToolError> {
        self.started.fetch_add(1, Ordering::Release);
        loop {
            context.check_cancelled()?;
            thread::sleep(Duration::from_millis(5));
        }
    }
}

struct PanickingObserve;

impl Tool for PanickingObserve {
    type Input = EmptyInput;
    type Output = ObserveOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse("fixture.panic", "1.0.0").unwrap(),
            "Panic fixture",
            "Panics so the coordinator can prove containment.",
            RiskLevel::Observe,
        )
    }

    fn execute(
        &self,
        _input: EmptyInput,
        _context: &mut ExecutionContext,
    ) -> Result<ObserveOutput, ToolError> {
        panic!("fixture panic")
    }
}

#[derive(Default)]
struct GateWitness {
    started: AtomicUsize,
    active: AtomicUsize,
    maximum: AtomicUsize,
    released: std::sync::atomic::AtomicBool,
}

struct GatedWrite {
    witness: Arc<GateWitness>,
}

impl Tool for GatedWrite {
    type Input = ObserveInput;
    type Output = ObserveOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse("fixture.gated_write", "1.0.0").unwrap(),
            "Gated write fixture",
            "Blocks a workspace mutation until its test releases it.",
            RiskLevel::WorkspaceWrite,
        )
    }

    fn execute(
        &self,
        input: ObserveInput,
        context: &mut ExecutionContext,
    ) -> Result<ObserveOutput, ToolError> {
        self.witness.started.fetch_add(1, Ordering::AcqRel);
        let active = self.witness.active.fetch_add(1, Ordering::AcqRel) + 1;
        self.witness.maximum.fetch_max(active, Ordering::AcqRel);
        while !self.witness.released.load(Ordering::Acquire) {
            context.check_cancelled()?;
            thread::sleep(Duration::from_millis(5));
        }
        self.witness.active.fetch_sub(1, Ordering::AcqRel);
        Ok(ObserveOutput {
            echoed: input.message,
        })
    }
}

impl Tool for CountingWrite {
    type Input = ObserveInput;
    type Output = ObserveOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse("fixture.write", "1.0.0").unwrap(),
            "Write fixture",
            "Represents one workspace mutation for coordinator tests.",
            RiskLevel::WorkspaceWrite,
        )
    }

    fn execute(
        &self,
        input: ObserveInput,
        _context: &mut ExecutionContext,
    ) -> Result<ObserveOutput, ToolError> {
        self.executions.fetch_add(1, Ordering::Release);
        Ok(ObserveOutput {
            echoed: input.message,
        })
    }
}

impl Tool for CountingObserve {
    type Input = ObserveInput;
    type Output = ObserveOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse("fixture.observe", "1.0.0").unwrap(),
            "Observe fixture",
            "Returns one value for coordinator tests.",
            RiskLevel::Observe,
        )
    }

    fn execute(
        &self,
        input: ObserveInput,
        _context: &mut ExecutionContext,
    ) -> Result<ObserveOutput, ToolError> {
        self.executions.fetch_add(1, Ordering::Release);
        Ok(ObserveOutput {
            echoed: input.message,
        })
    }
}

struct Fixture {
    _data_dir: TempDir,
    workspace: TempDir,
    store: Arc<Store>,
    coordinator: RunCoordinator,
    project: ProjectId,
    executions: Arc<AtomicUsize>,
    write_executions: Arc<AtomicUsize>,
    blocking_started: Arc<AtomicUsize>,
    gate: Arc<GateWitness>,
}

impl Fixture {
    fn new() -> Self {
        let data_dir = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let store = Arc::new(Store::open(data_dir.path()).unwrap());
        let project = ProjectId::new();
        store
            .put_workspace_trust(
                &WorkspaceTrust::decide(
                    project,
                    workspace.path(),
                    TrustState::Trusted,
                    OffsetDateTime::now_utc(),
                )
                .unwrap(),
            )
            .unwrap();
        let executions = Arc::new(AtomicUsize::new(0));
        let write_executions = Arc::new(AtomicUsize::new(0));
        let blocking_started = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(GateWitness::default());
        let mut registry = ToolRegistry::new();
        registry
            .register(CountingObserve {
                executions: Arc::clone(&executions),
            })
            .unwrap();
        registry
            .register(BlockingObserve {
                started: Arc::clone(&blocking_started),
            })
            .unwrap();
        registry.register(PanickingObserve).unwrap();
        registry
            .register(GatedWrite {
                witness: Arc::clone(&gate),
            })
            .unwrap();
        registry
            .register(CountingWrite {
                executions: Arc::clone(&write_executions),
            })
            .unwrap();
        let coordinator = RunCoordinator::new(
            Arc::clone(&store),
            Arc::new(registry),
            PolicyEngine::new(UserPolicy::default(), None),
        );
        Self {
            _data_dir: data_dir,
            workspace,
            store,
            coordinator,
            project,
            executions,
            write_executions,
            blocking_started,
            gate,
        }
    }

    fn start(&self, agent: MockAgent) -> RunId {
        self.start_at(agent, self.workspace.path(), self.project)
    }

    fn start_at(&self, agent: MockAgent, root: &std::path::Path, project: ProjectId) -> RunId {
        let task = Task::new(
            "coordinator fixture",
            root,
            Some(project),
            OffsetDateTime::now_utc(),
        );
        let workspace = WorkspaceRef::from_task(&task, &PassThrough);
        let task_id = self.coordinator.start_task(task).unwrap();
        self.coordinator
            .start_run(task_id, Box::new(agent), workspace)
            .unwrap()
    }

    fn terminal(&self, run: RunId) -> RunSnapshot {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = self.coordinator.run_snapshot(run).unwrap();
            if snapshot.run.state().is_terminal() {
                return snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "run {run} did not become terminal"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn pending_approval(&self, run: RunId) -> crate::approval::ApprovalRequest {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = self.coordinator.run_snapshot(run).unwrap();
            if let Some(request) = snapshot
                .approvals
                .into_iter()
                .find(|request| request.state() == ApprovalState::Pending)
            {
                return request;
            }
            assert!(
                Instant::now() < deadline,
                "run {run} did not request approval"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_until(&self, description: &str, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {description}"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}

fn scenario(id: &str, steps: Vec<ScenarioStep>) -> MockAgent {
    MockAgent::from_scenario(Scenario::new(ScenarioId::new(id).unwrap(), steps).unwrap())
}

fn run_started(action: AgentAction) -> ScenarioStep {
    ScenarioStep::new(ObservationPattern::RunStarted { task_title: None }, action)
}

fn call(input: serde_json::Value) -> AgentAction {
    AgentAction::CallTool {
        tool_id: "fixture.observe".parse().unwrap(),
        tool_version: "1.0.0".parse().unwrap(),
        input,
    }
}

fn call_write(input: serde_json::Value) -> AgentAction {
    AgentAction::CallTool {
        tool_id: "fixture.write".parse().unwrap(),
        tool_version: "1.0.0".parse().unwrap(),
        input,
    }
}

fn call_named(id: &str, input: serde_json::Value) -> AgentAction {
    AgentAction::CallTool {
        tool_id: id.parse().unwrap(),
        tool_version: "1.0.0".parse().unwrap(),
        input,
    }
}

fn complete_after_result() -> ScenarioStep {
    ScenarioStep::new(
        ObservationPattern::ToolResult {
            artifact_media_type: None,
            output_contains: Some(json!({"echoed": "hello"})),
        },
        AgentAction::CompleteRun {
            summary: "done".to_owned(),
        },
    )
}

fn gated_scenario(id: &str) -> MockAgent {
    scenario(
        id,
        vec![
            run_started(call_named(
                "fixture.gated_write",
                json!({"message": "hello"}),
            )),
            complete_after_result(),
        ],
    )
}

fn grant_pending(fixture: &Fixture, run: RunId) {
    let request = fixture.pending_approval(run);
    fixture
        .coordinator
        .decide_approval(ApprovalDecision::grant(
            request.id(),
            ApprovalScope::ExactCall,
            DecidedVia::Cli,
            OffsetDateTime::now_utc(),
        ))
        .unwrap();
}

#[test]
fn invalid_input_is_failed_before_policy_and_body() {
    let fixture = Fixture::new();
    let agent = scenario(
        "invalid_before_policy",
        vec![
            run_started(call(json!({"message": 7}))),
            ScenarioStep::new(
                ObservationPattern::ToolFailed {
                    error_kind: Some("invalid_input".to_owned()),
                },
                AgentAction::CompleteRun {
                    summary: "invalid input was observed".to_owned(),
                },
            ),
        ],
    );

    let snapshot = fixture.terminal(fixture.start(agent));

    assert_eq!(snapshot.run.state(), ExecutionState::Succeeded);
    assert_eq!(fixture.executions.load(Ordering::Acquire), 0);
    assert_eq!(snapshot.tool_calls.len(), 1);
    assert_eq!(
        snapshot.tool_calls[0].state(),
        crate::domain::ToolCallState::Failed
    );
    assert!(snapshot.tool_calls[0].policy_decision().is_none());
    assert!(
        snapshot
            .events
            .iter()
            .all(|stored| stored.event.kind() != &EventKind::PolicyDecision)
    );
}

#[test]
fn preparing_valid_input_derives_policy_facts_without_running_the_body() {
    let fixture = Fixture::new();
    let tool = fixture
        .coordinator
        .inner
        .registry
        .get(
            &"fixture.observe".parse().unwrap(),
            Some(&"1.0.0".parse().unwrap()),
        )
        .unwrap();
    let raw = serde_json::value::to_raw_value(&json!({"message": "hello"})).unwrap();
    let boundary = crate::trust::PathBoundary::new(
        fixture.workspace.path(),
        std::iter::empty::<&std::path::Path>(),
    )
    .unwrap();

    let prepared = tool.prepare_json(&raw, &boundary).unwrap();

    assert_eq!(prepared.classification().risk(), RiskLevel::Observe);
    assert!(prepared.paths().is_empty());
    assert_eq!(fixture.executions.load(Ordering::Acquire), 0);
}

#[test]
fn schedulable_runs_require_a_stable_project_identity() {
    let fixture = Fixture::new();
    let task = Task::new(
        "missing identity",
        fixture.workspace.path(),
        None,
        OffsetDateTime::now_utc(),
    );
    let workspace = WorkspaceRef::from_task(&task, &PassThrough);
    let task_id = fixture.coordinator.start_task(task).unwrap();
    let agent = scenario(
        "missing_project_identity",
        vec![run_started(AgentAction::CompleteRun {
            summary: "unreachable".to_owned(),
        })],
    );

    let error = fixture
        .coordinator
        .start_run(task_id, Box::new(agent), workspace)
        .unwrap_err();

    assert_eq!(error.kind(), "workspace_identity_required");
}

#[test]
fn allowed_call_records_policy_before_execution() {
    let fixture = Fixture::new();
    let agent = scenario(
        "allow_ordering",
        vec![
            run_started(call(json!({"message": "hello"}))),
            complete_after_result(),
        ],
    );

    let snapshot = fixture.terminal(fixture.start(agent));

    assert_eq!(snapshot.run.state(), ExecutionState::Succeeded);
    assert_eq!(fixture.executions.load(Ordering::Acquire), 1);
    let policy_seq = snapshot
        .events
        .iter()
        .find(|stored| stored.event.kind() == &EventKind::PolicyDecision)
        .unwrap()
        .seq;
    let running_seq = snapshot
        .events
        .iter()
        .find(|stored| {
            stored.event.kind() == &EventKind::ToolCallStateChanged
                && stored.event.payload().get("state") == Some(&json!("running"))
        })
        .unwrap()
        .seq;
    assert!(policy_seq < running_seq);
    let serialized = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(serialized["run"]["state"], "succeeded");
    assert_eq!(serialized["tool_calls"].as_array().unwrap().len(), 1);
    assert_eq!(
        serialized["events"].as_array().unwrap().len(),
        snapshot.events.len()
    );
}

#[test]
fn ask_grant_is_bound_to_the_exact_call_before_execution() {
    let fixture = Fixture::new();
    let agent = scenario(
        "ask_grant_exact",
        vec![
            run_started(call_write(json!({"message": "hello"}))),
            complete_after_result(),
        ],
    );
    let run = fixture.start(agent);
    let request = fixture.pending_approval(run);
    let call = fixture
        .store
        .load_run_tool_calls(run)
        .unwrap()
        .pop()
        .unwrap();

    assert_eq!(request.tool_call_id(), call.id());
    assert_eq!(request.tool().id.as_str(), "fixture.write");
    assert_eq!(
        request.input_hash(),
        crate::approval::canonical_input_hash(call.input()).unwrap()
    );
    assert_eq!(request.workspace().project_id(), Some(fixture.project));
    fixture
        .coordinator
        .decide_approval(ApprovalDecision::grant(
            request.id(),
            ApprovalScope::ExactCall,
            DecidedVia::Cli,
            OffsetDateTime::now_utc(),
        ))
        .unwrap();

    let snapshot = fixture.terminal(run);
    assert_eq!(snapshot.run.state(), ExecutionState::Succeeded);
    assert_eq!(fixture.write_executions.load(Ordering::Acquire), 1);
    assert_eq!(snapshot.approvals[0].state(), ApprovalState::Granted);
    assert_eq!(
        snapshot.approvals[0].effective_scope(),
        ApprovalScope::ExactCall
    );
}

#[test]
fn ask_denial_is_bound_to_the_call_and_never_executes_it() {
    let fixture = Fixture::new();
    let agent = scenario(
        "ask_deny_exact",
        vec![
            run_started(call_write(json!({"message": "blocked"}))),
            ScenarioStep::new(
                ObservationPattern::ApprovalOutcome {
                    outcome: Some(ApprovalOutcomeView::Denied),
                },
                AgentAction::CompleteRun {
                    summary: "denial handled".to_owned(),
                },
            ),
        ],
    );
    let run = fixture.start(agent);
    let request = fixture.pending_approval(run);

    fixture
        .coordinator
        .decide_approval(
            ApprovalDecision::deny(request.id(), DecidedVia::Cli, OffsetDateTime::now_utc())
                .because("not this change"),
        )
        .unwrap();

    let snapshot = fixture.terminal(run);
    assert_eq!(snapshot.run.state(), ExecutionState::Succeeded);
    assert_eq!(fixture.write_executions.load(Ordering::Acquire), 0);
    assert_eq!(
        snapshot.approvals[0].tool_call_id(),
        snapshot.tool_calls[0].id()
    );
    assert_eq!(snapshot.approvals[0].state(), ApprovalState::Denied);
    assert_eq!(
        snapshot.tool_calls[0].state(),
        crate::domain::ToolCallState::Denied
    );
}

#[test]
fn cancellation_reaches_an_executing_tool_and_terminalizes_the_run() {
    let fixture = Fixture::new();
    let agent = scenario(
        "cancel_execution",
        vec![
            run_started(call_named("fixture.blocking", json!({}))),
            ScenarioStep::new(
                ObservationPattern::ToolFailed { error_kind: None },
                AgentAction::CompleteRun {
                    summary: "unreachable".to_owned(),
                },
            ),
        ],
    );
    let run = fixture.start(agent);
    fixture.wait_until("blocking tool start", || {
        fixture.blocking_started.load(Ordering::Acquire) == 1
    });

    fixture.coordinator.cancel_run(run).unwrap();

    let snapshot = fixture.terminal(run);
    assert_eq!(snapshot.run.state(), ExecutionState::Cancelled);
    assert_eq!(
        snapshot.tool_calls[0].state(),
        crate::domain::ToolCallState::Cancelled
    );
    assert_eq!(snapshot.steps[0].state(), ExecutionState::Cancelled);
}

#[test]
fn cancellation_resolves_a_parked_approval_without_executing() {
    let fixture = Fixture::new();
    let agent = scenario(
        "cancel_approval",
        vec![
            run_started(call_write(json!({"message": "blocked"}))),
            ScenarioStep::new(
                ObservationPattern::ApprovalOutcome { outcome: None },
                AgentAction::CompleteRun {
                    summary: "unreachable".to_owned(),
                },
            ),
        ],
    );
    let run = fixture.start(agent);
    let request = fixture.pending_approval(run);

    fixture.coordinator.cancel_run(run).unwrap();

    let snapshot = fixture.terminal(run);
    assert_eq!(snapshot.run.state(), ExecutionState::Cancelled);
    assert_eq!(snapshot.approvals[0].id(), request.id());
    assert_eq!(snapshot.approvals[0].state(), ApprovalState::Cancelled);
    assert_eq!(fixture.write_executions.load(Ordering::Acquire), 0);
}

#[test]
fn a_panicking_tool_fails_its_call_and_the_coordinator_continues() {
    let fixture = Fixture::new();
    let agent = scenario(
        "panic_contained",
        vec![
            run_started(call_named("fixture.panic", json!({}))),
            ScenarioStep::new(
                ObservationPattern::ToolFailed {
                    error_kind: Some("tool_panicked".to_owned()),
                },
                call(json!({"message": "hello"})),
            ),
            complete_after_result(),
        ],
    );

    let snapshot = fixture.terminal(fixture.start(agent));

    assert_eq!(snapshot.run.state(), ExecutionState::Succeeded);
    assert_eq!(snapshot.tool_calls.len(), 2);
    assert_eq!(
        snapshot.tool_calls[0].state(),
        crate::domain::ToolCallState::Failed
    );
    assert_eq!(
        snapshot.tool_calls[1].state(),
        crate::domain::ToolCallState::Succeeded
    );
    assert_eq!(fixture.executions.load(Ordering::Acquire), 1);
}

#[test]
fn subscriptions_replay_in_order_and_disconnect_slow_consumers() {
    let fixture = Fixture::new();
    let agent = scenario(
        "subscriber_overflow",
        vec![
            run_started(call_write(json!({"message": "blocked"}))),
            ScenarioStep::new(
                ObservationPattern::ApprovalOutcome { outcome: None },
                AgentAction::CompleteRun {
                    summary: "unreachable".to_owned(),
                },
            ),
        ],
    );
    let run = fixture.start(agent);
    fixture.pending_approval(run);
    let receiver = fixture.coordinator.subscribe(run).unwrap();
    while matches!(receiver.try_recv(), Ok(super::EventDelivery::Event(_))) {}

    for index in 0..=super::SUBSCRIBER_CAPACITY {
        fixture
            .store
            .append_event(
                run,
                crate::store::RunEvent::new(EventKind::Diagnostic, OffsetDateTime::now_utc())
                    .with_payload(json!({"index": index})),
            )
            .unwrap();
    }
    fixture.coordinator.publish(run).unwrap();

    let delivery = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(
        matches!(delivery, super::EventDelivery::Lagged { run: observed, .. } if observed == run)
    );
    assert_eq!(
        receiver.try_recv(),
        Err(super::TryReceiveError::Disconnected)
    );
    fixture.coordinator.cancel_run(run).unwrap();
    fixture.terminal(run);

    let replay = fixture.coordinator.subscribe(run).unwrap();
    let mut previous = None;
    loop {
        match replay.try_recv() {
            Ok(super::EventDelivery::Event(stored)) => {
                assert!(previous.is_none_or(|seq| seq < stored.seq));
                previous = Some(stored.seq);
            }
            Ok(super::EventDelivery::Lagged { .. }) => panic!("durable replay must not lag"),
            Err(super::TryReceiveError::Disconnected) => break,
            Err(super::TryReceiveError::Empty) => thread::yield_now(),
        }
    }
}

#[test]
fn mutating_calls_serialize_per_workspace_but_different_workspaces_overlap() {
    let fixture = Fixture::new();
    let first = fixture.start(gated_scenario("same_workspace_one"));
    let second = fixture.start(gated_scenario("same_workspace_two"));
    let first_request = fixture.pending_approval(first);
    let second_request = fixture.pending_approval(second);
    for request in [first_request, second_request] {
        fixture
            .coordinator
            .decide_approval(ApprovalDecision::grant(
                request.id(),
                ApprovalScope::ExactCall,
                DecidedVia::Cli,
                OffsetDateTime::now_utc(),
            ))
            .unwrap();
    }
    fixture.wait_until("first same-workspace mutation", || {
        fixture.gate.started.load(Ordering::Acquire) == 1
    });
    thread::sleep(Duration::from_millis(75));
    assert_eq!(fixture.gate.started.load(Ordering::Acquire), 1);
    assert_eq!(fixture.gate.maximum.load(Ordering::Acquire), 1);
    fixture.gate.released.store(true, Ordering::Release);
    fixture.terminal(first);
    fixture.terminal(second);
    assert_eq!(fixture.gate.maximum.load(Ordering::Acquire), 1);

    let fixture = Fixture::new();
    let other_workspace = TempDir::new().unwrap();
    let other_project = ProjectId::new();
    fixture
        .store
        .put_workspace_trust(
            &WorkspaceTrust::decide(
                other_project,
                other_workspace.path(),
                TrustState::Trusted,
                OffsetDateTime::now_utc(),
            )
            .unwrap(),
        )
        .unwrap();
    let first = fixture.start(gated_scenario("different_workspace_one"));
    let second = fixture.start_at(
        gated_scenario("different_workspace_two"),
        other_workspace.path(),
        other_project,
    );
    grant_pending(&fixture, first);
    grant_pending(&fixture, second);
    fixture.wait_until("both different-workspace mutations", || {
        fixture.gate.started.load(Ordering::Acquire) == 2
    });
    assert_eq!(fixture.gate.maximum.load(Ordering::Acquire), 2);
    fixture.gate.released.store(true, Ordering::Release);
    fixture.terminal(first);
    fixture.terminal(second);
}
