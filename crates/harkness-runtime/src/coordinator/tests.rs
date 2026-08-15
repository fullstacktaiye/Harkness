use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use harkness_core::{Project, ProjectId, ProjectSource};
use harkness_git::Cancellation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tempfile::TempDir;
use time::OffsetDateTime;

use crate::agent::{
    Agent, AgentAction, AgentSessionId, AgentSessionState, ApprovalOutcomeView, MockAgent,
    Observation, ObservationPattern, PlannedStep, Scenario, ScenarioId, ScenarioStep, WorkspaceRef,
};
use crate::approval::{ApprovalDecision, ApprovalScope, ApprovalState, DecidedVia};
use crate::domain::{ExecutionState, Run, RunId, Step, Task, ToolCall};
use crate::policy::{PolicyEngine, PolicyVerdict, RepositoryPolicy, UserPolicy};
use crate::schedule::WorkspaceKey;
use crate::store::{EventKind, PassThrough, Store};
use crate::tool::{
    ExecutionContext, RiskLevel, Tool, ToolError, ToolIdentity, ToolMetadata, ToolRegistry,
    WorkspaceMetadata,
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
    started_at: Arc<Mutex<Option<Instant>>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyInput {}

struct BlockingObserve {
    started: Arc<AtomicUsize>,
}

struct PanickingPrepare;

impl Tool for PanickingPrepare {
    type Input = EmptyInput;
    type Output = ObserveOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse("fixture.prepare_panic", "1.0.0").unwrap(),
            "Prepare panic fixture",
            "Panics while deriving request effects.",
            RiskLevel::Observe,
        )
    }

    fn request_effects(
        &self,
        _input: &Self::Input,
        _boundary: &crate::trust::PathBoundary,
    ) -> Result<crate::tool::RequestEffects, ToolError> {
        panic!("prepare fixture panic")
    }

    fn execute(
        &self,
        _input: EmptyInput,
        _context: &mut ExecutionContext,
    ) -> Result<ObserveOutput, ToolError> {
        panic!("prepare panic tool body must not execute")
    }
}

struct BlockingPrepare {
    started: Arc<AtomicBool>,
    released: Arc<AtomicBool>,
    executions: Arc<AtomicUsize>,
}

impl Tool for BlockingPrepare {
    type Input = EmptyInput;
    type Output = ObserveOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse("fixture.blocking_prepare", "1.0.0").unwrap(),
            "Blocking prepare fixture",
            "Waits while deriving policy facts.",
            RiskLevel::Observe,
        )
    }

    fn request_effects(
        &self,
        _input: &Self::Input,
        _boundary: &crate::trust::PathBoundary,
    ) -> Result<crate::tool::RequestEffects, ToolError> {
        self.started.store(true, Ordering::Release);
        while !self.released.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(2));
        }
        Ok(crate::tool::RequestEffects::default())
    }

    fn execute(
        &self,
        _input: EmptyInput,
        _context: &mut ExecutionContext,
    ) -> Result<ObserveOutput, ToolError> {
        self.executions.fetch_add(1, Ordering::Release);
        Ok(ObserveOutput {
            echoed: "ran".to_owned(),
        })
    }
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

struct GatedObserve {
    witness: Arc<GateWitness>,
}

impl Tool for GatedObserve {
    type Input = ObserveInput;
    type Output = ObserveOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse("fixture.gated_observe", "1.0.0").unwrap(),
            "Gated read fixture",
            "Blocks a workspace read until its test releases it.",
            RiskLevel::Observe,
        )
    }

    fn execute(
        &self,
        input: ObserveInput,
        context: &mut ExecutionContext,
    ) -> Result<ObserveOutput, ToolError> {
        GatedWrite {
            witness: Arc::clone(&self.witness),
        }
        .execute(input, context)
    }
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
        *self
            .started_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
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
    write_started_at: Arc<Mutex<Option<Instant>>>,
    blocking_started: Arc<AtomicUsize>,
    gate: Arc<GateWitness>,
    prepare_started: Arc<AtomicBool>,
    prepare_released: Arc<AtomicBool>,
    prepare_executions: Arc<AtomicUsize>,
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
        let write_started_at = Arc::new(Mutex::new(None));
        let blocking_started = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(GateWitness::default());
        let prepare_started = Arc::new(AtomicBool::new(false));
        let prepare_released = Arc::new(AtomicBool::new(false));
        let prepare_executions = Arc::new(AtomicUsize::new(0));
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
        registry.register(PanickingPrepare).unwrap();
        registry
            .register(BlockingPrepare {
                started: Arc::clone(&prepare_started),
                released: Arc::clone(&prepare_released),
                executions: Arc::clone(&prepare_executions),
            })
            .unwrap();
        registry
            .register(GatedWrite {
                witness: Arc::clone(&gate),
            })
            .unwrap();
        registry
            .register(GatedObserve {
                witness: Arc::clone(&gate),
            })
            .unwrap();
        registry
            .register(CountingWrite {
                executions: Arc::clone(&write_executions),
                started_at: Arc::clone(&write_started_at),
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
            write_started_at,
            blocking_started,
            gate,
            prepare_started,
            prepare_released,
            prepare_executions,
        }
    }

    fn start(&self, agent: MockAgent) -> RunId {
        self.start_at(agent, self.workspace.path(), self.project)
    }

    fn start_agent(&self, agent: Box<dyn Agent>) -> RunId {
        let task = Task::new(
            "coordinator fixture",
            self.workspace.path(),
            Some(self.project),
            OffsetDateTime::now_utc(),
        );
        let workspace = WorkspaceRef::from_task(&task, &PassThrough);
        let task_id = self.coordinator.start_task(task).unwrap();
        self.coordinator
            .start_run(task_id, agent, workspace)
            .unwrap()
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

struct PanickingAgent {
    panic_in_next: bool,
}

struct BlockingAgent {
    id: AgentSessionId,
    started: Arc<AtomicBool>,
    released: Arc<AtomicBool>,
}

impl Agent for BlockingAgent {
    fn session_id(&self) -> AgentSessionId {
        self.id
    }

    fn next_action(&mut self, _observation: Observation) -> AgentAction {
        self.started.store(true, Ordering::Release);
        while !self.released.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(2));
        }
        AgentAction::CompleteRun {
            summary: "cancel must win".to_owned(),
        }
    }

    fn state(&self) -> AgentSessionState {
        panic!("cancellation must be checked before checkpointing")
    }
}

impl Agent for PanickingAgent {
    fn session_id(&self) -> AgentSessionId {
        AgentSessionId::new()
    }

    fn next_action(&mut self, _observation: Observation) -> AgentAction {
        if self.panic_in_next {
            panic!("agent next_action panic")
        }
        AgentAction::CompleteRun {
            summary: "must not complete".to_owned(),
        }
    }

    fn state(&self) -> AgentSessionState {
        panic!("agent state panic")
    }
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
fn coordinator_refuses_workspace_metadata_for_another_project_or_root() {
    let fixture = Fixture::new();
    let task = Task::new(
        "metadata identity",
        fixture.workspace.path(),
        Some(fixture.project),
        OffsetDateTime::now_utc(),
    );
    let workspace = WorkspaceRef::from_task(&task, &PassThrough);
    let task_id = fixture.coordinator.start_task(task).unwrap();
    let project = |id, root: &std::path::Path| Project {
        id,
        display_name: "Catalog project".to_owned(),
        root: root.to_owned(),
        source: ProjectSource::Local,
        last_opened: OffsetDateTime::now_utc(),
        available: true,
        git: None,
    };

    let error = fixture
        .coordinator
        .start_run_with_workspace_metadata(
            task_id,
            Box::new(scenario(
                "wrong_metadata_project",
                vec![run_started(AgentAction::CompleteRun {
                    summary: "unreachable".to_owned(),
                })],
            )),
            workspace.clone(),
            WorkspaceMetadata::from_project(&project(ProjectId::new(), fixture.workspace.path())),
        )
        .unwrap_err();
    assert_eq!(error.kind(), "workspace_mismatch");

    let other_root = TempDir::new().unwrap();
    let error = fixture
        .coordinator
        .start_run_with_workspace_metadata(
            task_id,
            Box::new(scenario(
                "wrong_metadata_root",
                vec![run_started(AgentAction::CompleteRun {
                    summary: "unreachable".to_owned(),
                })],
            )),
            workspace,
            WorkspaceMetadata::from_project(&project(fixture.project, other_root.path())),
        )
        .unwrap_err();
    assert_eq!(error.kind(), "workspace_mismatch");
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
fn panics_in_prepare_and_agent_methods_become_terminal_diagnostics() {
    let fixture = Fixture::new();
    let prepare = scenario(
        "prepare_panic_contained",
        vec![
            run_started(call_named("fixture.prepare_panic", json!({}))),
            ScenarioStep::new(
                ObservationPattern::ToolFailed {
                    error_kind: Some("tool_panicked".to_owned()),
                },
                AgentAction::CompleteRun {
                    summary: "contained".to_owned(),
                },
            ),
        ],
    );
    let snapshot = fixture.terminal(fixture.start(prepare));
    assert_eq!(snapshot.run.state(), ExecutionState::Succeeded);
    assert_eq!(
        snapshot.tool_calls[0].state(),
        crate::domain::ToolCallState::Failed
    );
    assert!(snapshot.tool_calls[0].policy_decision().is_none());

    for panic_in_next in [true, false] {
        let run = fixture.start_agent(Box::new(PanickingAgent { panic_in_next }));
        let snapshot = fixture.terminal(run);
        assert_eq!(snapshot.run.state(), ExecutionState::Failed);
        assert!(snapshot.events.iter().any(|stored| {
            stored.event.kind() == &EventKind::Diagnostic
                && stored.event.payload()["error_kind"] == "agent_panicked"
        }));
    }
}

#[test]
fn cancellation_after_prepare_wins_before_policy_or_execution() {
    let fixture = Fixture::new();
    let agent = scenario(
        "cancel_before_policy",
        vec![
            run_started(call_named("fixture.blocking_prepare", json!({}))),
            ScenarioStep::new(
                ObservationPattern::ToolFailed { error_kind: None },
                AgentAction::CompleteRun {
                    summary: "unreachable".to_owned(),
                },
            ),
        ],
    );
    let run = fixture.start(agent);
    fixture.wait_until("request effect preparation", || {
        fixture.prepare_started.load(Ordering::Acquire)
    });
    fixture.coordinator.cancel_run(run).unwrap();
    fixture.prepare_released.store(true, Ordering::Release);

    let snapshot = fixture.terminal(run);
    assert_eq!(snapshot.run.state(), ExecutionState::Cancelled);
    assert_eq!(fixture.prepare_executions.load(Ordering::Acquire), 0);
    assert!(snapshot.tool_calls[0].policy_decision().is_none());
    assert!(
        snapshot
            .events
            .iter()
            .all(|event| event.event.kind() != &EventKind::PolicyDecision)
    );
}

#[test]
fn cancellation_after_agent_return_wins_before_action_side_effects() {
    let fixture = Fixture::new();
    let started = Arc::new(AtomicBool::new(false));
    let released = Arc::new(AtomicBool::new(false));
    let run = fixture.start_agent(Box::new(BlockingAgent {
        id: AgentSessionId::new(),
        started: Arc::clone(&started),
        released: Arc::clone(&released),
    }));
    fixture.wait_until("agent entering next_action", || {
        started.load(Ordering::Acquire)
    });
    fixture.coordinator.cancel_run(run).unwrap();
    released.store(true, Ordering::Release);

    let snapshot = fixture.terminal(run);
    assert_eq!(snapshot.run.state(), ExecutionState::Cancelled);
    assert!(snapshot.events.iter().all(|event| {
        event.event.kind() != &EventKind::AgentAction
            || event.event.payload()["action"]["kind"] != "complete_run"
    }));
}

#[test]
fn queued_plan_steps_are_terminalized_before_agent_completion() {
    let fixture = Fixture::new();
    let agent = scenario(
        "plan_then_complete",
        vec![
            run_started(AgentAction::Plan {
                steps: vec![PlannedStep::new("planned but not executed")],
            }),
            run_started(AgentAction::CompleteRun {
                summary: "nothing to execute".to_owned(),
            }),
        ],
    );
    let snapshot = fixture.terminal(fixture.start(agent));
    assert_eq!(snapshot.run.state(), ExecutionState::Succeeded);
    assert_eq!(snapshot.steps.len(), 1);
    assert_eq!(snapshot.steps[0].state(), ExecutionState::Cancelled);
}

#[test]
fn repository_policy_is_loaded_for_each_run_workspace() {
    let fixture = Fixture::new();
    let other_workspace = TempDir::new().unwrap();
    std::fs::create_dir(other_workspace.path().join(".harkness")).unwrap();
    RepositoryPolicy::default()
        .with_tool(&"fixture.observe".parse().unwrap(), PolicyVerdict::Deny)
        .persist(other_workspace.path().join(".harkness/policy.json"))
        .unwrap();
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

    let allowed = fixture.start(scenario(
        "workspace_a_policy",
        vec![
            run_started(call(json!({"message": "hello"}))),
            complete_after_result(),
        ],
    ));
    let denied = fixture.start_at(
        scenario(
            "workspace_b_policy",
            vec![
                run_started(call(json!({"message": "hello"}))),
                ScenarioStep::new(
                    ObservationPattern::PolicyDenied {
                        reason_contains: None,
                    },
                    AgentAction::CompleteRun {
                        summary: "denied".to_owned(),
                    },
                ),
            ],
        ),
        other_workspace.path(),
        other_project,
    );
    assert_eq!(
        fixture.terminal(allowed).run.state(),
        ExecutionState::Succeeded
    );
    let denied = fixture.terminal(denied);
    assert_eq!(denied.run.state(), ExecutionState::Succeeded);
    assert_eq!(
        denied.tool_calls[0].state(),
        crate::domain::ToolCallState::Denied
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
fn subscription_handoff_filters_replayed_live_duplicates_and_pages_large_history() {
    let fixture = Fixture::new();
    let run = fixture.start(scenario(
        "subscription_handoff",
        vec![
            run_started(call_write(json!({"message": "blocked"}))),
            ScenarioStep::new(
                ObservationPattern::ApprovalOutcome { outcome: None },
                AgentAction::CompleteRun {
                    summary: "unreachable".to_owned(),
                },
            ),
        ],
    ));
    fixture.pending_approval(run);

    let history = crate::store::DEFAULT_EVENT_PAGE_LIMIT + 37;
    for index in 0..history {
        fixture
            .store
            .append_event(
                run,
                crate::store::RunEvent::new(EventKind::Diagnostic, OffsetDateTime::now_utc())
                    .with_payload(json!({"history": index})),
            )
            .unwrap();
    }
    // Delivery deliberately lags durable storage here. The subscriber's replay
    // includes these rows, then publish offers them live: each must appear once.
    let receiver = fixture.coordinator.subscribe(run).unwrap();
    fixture.coordinator.publish(run).unwrap();
    let tip = fixture.store.latest_event_seq(run).unwrap().unwrap();
    let mut sequences = Vec::new();
    while sequences.last().copied() != Some(tip) {
        match receiver.recv_timeout(Duration::from_secs(1)).unwrap() {
            super::EventDelivery::Event(event) => sequences.push(event.seq),
            super::EventDelivery::Lagged { .. } => panic!("bounded durable replay must not lag"),
        }
    }
    assert!(sequences.len() > crate::store::DEFAULT_EVENT_PAGE_LIMIT);
    assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
    let unique = sequences
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(unique.len(), sequences.len());

    fixture.coordinator.cancel_run(run).unwrap();
    fixture.terminal(run);
}

#[test]
fn a_stalled_subscriber_does_not_delay_run_completion() {
    let make_agent = |id: &str| {
        scenario(
            id,
            vec![
                run_started(AgentAction::Plan {
                    steps: (0..(super::SUBSCRIBER_CAPACITY + 20))
                        .map(|index| PlannedStep::new(format!("planned {index}")))
                        .collect(),
                }),
                run_started(AgentAction::CompleteRun {
                    summary: "planned".to_owned(),
                }),
            ],
        )
    };
    let fixture = Fixture::new();
    let started = Instant::now();
    let baseline = fixture.start(make_agent("subscriber_baseline"));
    fixture.terminal(baseline);
    let baseline_duration = started.elapsed();

    let started = Instant::now();
    let stalled = fixture.start(make_agent("subscriber_stalled"));
    let receiver = fixture.coordinator.subscribe(stalled).unwrap();
    fixture.terminal(stalled);
    let stalled_duration = started.elapsed();
    assert!(
        stalled_duration <= baseline_duration + Duration::from_millis(500),
        "stalled subscriber delayed completion: baseline={baseline_duration:?}, stalled={stalled_duration:?}"
    );
    let mut lagged = false;
    while let Ok(delivery) = receiver.try_recv() {
        lagged |= matches!(delivery, super::EventDelivery::Lagged { .. });
    }
    assert!(
        lagged,
        "the unconsumed subscriber did not exercise overflow"
    );
}

#[test]
fn approved_dispatch_rejects_input_tampered_while_parked() {
    let fixture = Fixture::new();
    let run = fixture.start(scenario(
        "parked_input_tamper",
        vec![
            run_started(call_write(json!({"message": "approved"}))),
            ScenarioStep::new(
                ObservationPattern::ToolFailed { error_kind: None },
                AgentAction::CompleteRun {
                    summary: "unreachable".to_owned(),
                },
            ),
        ],
    ));
    let request = fixture.pending_approval(run);
    let call = fixture
        .store
        .load_run_tool_calls(run)
        .unwrap()
        .pop()
        .unwrap();
    let connection = rusqlite::Connection::open(fixture.store.path()).unwrap();
    connection
        .execute(
            "UPDATE tool_calls SET input_json = ?1 WHERE id = ?2",
            rusqlite::params![r#"{"message":"tampered"}"#, call.id().to_string()],
        )
        .unwrap();
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
    assert_eq!(snapshot.run.state(), ExecutionState::Failed);
    assert_eq!(fixture.write_executions.load(Ordering::Acquire), 0);
    assert!(snapshot.steps.iter().all(|step| step.state().is_terminal()));
    assert!(
        snapshot
            .tool_calls
            .iter()
            .all(|call| call.state().is_terminal())
    );
    assert!(snapshot.events.iter().any(|event| {
        event.event.kind() == &EventKind::Diagnostic
            && event.event.payload()["error_kind"] == "approval_binding_mismatch"
    }));
}

#[test]
fn approved_dispatch_rechecks_identity_after_scheduler_queueing() {
    let fixture = Fixture::new();
    let leader = fixture.start(gated_scenario("binding_queue_leader"));
    grant_pending(&fixture, leader);
    fixture.wait_until("leader entering workspace slot", || {
        fixture.gate.started.load(Ordering::Acquire) == 1
    });

    let queued = fixture.start(gated_scenario("binding_queue_tamper"));
    let request = fixture.pending_approval(queued);
    fixture
        .coordinator
        .decide_approval(ApprovalDecision::grant(
            request.id(),
            ApprovalScope::ExactCall,
            DecidedVia::Cli,
            OffsetDateTime::now_utc(),
        ))
        .unwrap();
    thread::sleep(Duration::from_millis(50));
    assert_eq!(fixture.gate.started.load(Ordering::Acquire), 1);
    let call = fixture
        .store
        .load_run_tool_calls(queued)
        .unwrap()
        .pop()
        .unwrap();
    let connection = rusqlite::Connection::open(fixture.store.path()).unwrap();
    connection
        .execute(
            "UPDATE tool_calls SET tool_id = 'fixture.observe' WHERE id = ?1",
            rusqlite::params![call.id().to_string()],
        )
        .unwrap();

    fixture.gate.released.store(true, Ordering::Release);
    assert_eq!(
        fixture.terminal(leader).run.state(),
        ExecutionState::Succeeded
    );
    let snapshot = fixture.terminal(queued);
    assert_eq!(snapshot.run.state(), ExecutionState::Failed);
    assert_eq!(fixture.gate.started.load(Ordering::Acquire), 1);
    assert!(snapshot.events.iter().any(|event| {
        event.event.kind() == &EventKind::Diagnostic
            && event.event.payload()["error_kind"] == "approval_binding_mismatch"
    }));
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

#[test]
fn read_only_calls_in_one_workspace_overlap_while_parked() {
    let fixture = Fixture::new();
    let read_scenario = |id| {
        scenario(
            id,
            vec![
                run_started(call_named(
                    "fixture.gated_observe",
                    json!({"message": "hello"}),
                )),
                complete_after_result(),
            ],
        )
    };
    let started = Instant::now();
    let first = fixture.start(read_scenario("overlap_read_one"));
    let second = fixture.start(read_scenario("overlap_read_two"));
    fixture.wait_until("both same-workspace reads entering", || {
        fixture.gate.started.load(Ordering::Acquire) == 2
    });
    let overlap_delay = started.elapsed();
    assert_eq!(fixture.gate.maximum.load(Ordering::Acquire), 2);
    assert!(
        overlap_delay < Duration::from_secs(1),
        "reads did not overlap promptly: {overlap_delay:?}"
    );
    fixture.gate.released.store(true, Ordering::Release);
    fixture.terminal(first);
    fixture.terminal(second);
}

#[test]
fn a_pending_approval_does_not_block_reads_in_the_same_workspace() {
    let fixture = Fixture::new();
    let parked = fixture.start(scenario(
        "parked_write_for_read",
        vec![
            run_started(call_write(json!({"message": "parked"}))),
            ScenarioStep::new(
                ObservationPattern::ApprovalOutcome { outcome: None },
                AgentAction::CompleteRun {
                    summary: "unreachable".to_owned(),
                },
            ),
        ],
    ));
    fixture.pending_approval(parked);

    let started = Instant::now();
    let read = fixture.start(scenario(
        "read_during_parked_write",
        vec![
            run_started(call(json!({"message": "hello"}))),
            complete_after_result(),
        ],
    ));
    assert_eq!(
        fixture.terminal(read).run.state(),
        ExecutionState::Succeeded
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "parked approval delayed read by {elapsed:?}"
    );
    let parked_snapshot = fixture.coordinator.run_snapshot(parked).unwrap();
    assert_eq!(
        parked_snapshot.run.state(),
        ExecutionState::WaitingForApproval
    );
    assert_eq!(parked_snapshot.approvals[0].state(), ApprovalState::Pending);
    fixture.coordinator.cancel_run(parked).unwrap();
    fixture.terminal(parked);
}

#[test]
fn forced_invalid_transition_is_persisted_as_a_diagnostic_instead_of_panicking() {
    let fixture = Fixture::new();
    let run = fixture.start(gated_scenario("forced_invalid_transition"));
    let request = fixture.pending_approval(run);
    fixture
        .store
        .transition_run(run, ExecutionState::Cancelled, OffsetDateTime::now_utc())
        .unwrap();

    fixture
        .coordinator
        .decide_approval(ApprovalDecision::grant(
            request.id(),
            ApprovalScope::ExactCall,
            DecidedVia::Cli,
            OffsetDateTime::now_utc(),
        ))
        .unwrap();

    fixture.wait_until("coordinator diagnostic", || {
        fixture
            .coordinator
            .run_snapshot(run)
            .unwrap()
            .events
            .iter()
            .any(|stored| {
                stored.event.kind() == &EventKind::Diagnostic
                    && stored.event.payload()["kind"] == "coordinator_error"
                    && stored.event.payload()["error_kind"] == "invalid_transition"
            })
    });
    let snapshot = fixture.coordinator.run_snapshot(run).unwrap();
    assert_eq!(snapshot.run.state(), ExecutionState::Cancelled);
    assert_eq!(fixture.write_executions.load(Ordering::Acquire), 0);
    assert!(
        snapshot
            .approvals
            .iter()
            .all(|approval| approval.state() != ApprovalState::Pending)
    );
    assert!(
        snapshot
            .tool_calls
            .iter()
            .all(|call| call.state().is_terminal())
    );
    assert!(snapshot.steps.iter().all(|step| step.state().is_terminal()));
}

#[test]
fn oversized_panic_fault_is_bounded_before_every_cleanup_transition() {
    let fixture = Fixture::new();
    let now = OffsetDateTime::now_utc();
    let task = Task::new(
        "oversized panic cleanup",
        fixture.workspace.path(),
        Some(fixture.project),
        now,
    );
    fixture.store.insert_task(&task).unwrap();
    let run = Run::new(task.id(), now);
    fixture.store.insert_run(&run).unwrap();
    fixture
        .store
        .transition_run_with_event(
            run.id(),
            ExecutionState::Running,
            now,
            super::run_state_event(ExecutionState::Running, now),
        )
        .unwrap();
    let step = Step::new(run.id(), 0, "active step", now);
    fixture.store.insert_step(&step).unwrap();
    fixture
        .store
        .transition_step(step.id(), ExecutionState::Running, now)
        .unwrap();
    let call = ToolCall::new(
        &step,
        "fixture.observe",
        "1.0.0",
        json!({"message": "active"}),
        now,
    );
    fixture.store.insert_tool_call(&call).unwrap();
    let workspace = WorkspaceRef::from_task(&task, &PassThrough);
    let workspace_key = WorkspaceKey::new(fixture.project, fixture.workspace.path()).unwrap();
    let policy = fixture
        .coordinator
        .inner
        .policy
        .for_workspace(workspace_key.canonical_root());
    let worker = super::RunWorker::new(
        Arc::clone(&fixture.coordinator.inner),
        run.id(),
        task,
        workspace,
        workspace_key,
        None,
        policy,
        Cancellation::default(),
        Box::new(PanickingAgent {
            panic_in_next: true,
        }),
    );
    let panic = std::panic::catch_unwind(|| {
        std::panic::panic_any("x".repeat(65_537));
    })
    .unwrap_err();
    worker.record_fault(super::WorkerFault::new(
        "agent_panicked",
        super::panic_payload(&*panic),
    ));

    let snapshot = fixture.coordinator.run_snapshot(run.id()).unwrap();
    assert_eq!(snapshot.run.state(), ExecutionState::Failed);
    assert!(snapshot.steps.iter().all(|step| step.state().is_terminal()));
    assert!(
        snapshot
            .tool_calls
            .iter()
            .all(|call| call.state().is_terminal())
    );
    assert!(snapshot.events.iter().any(|event| {
        event.event.kind() == &EventKind::Diagnostic
            && event.event.payload()["error_kind"] == "agent_panicked"
    }));
    let bounded_failure_bytes = crate::tool::MAX_FAILURE_MESSAGE_BYTES + "… (truncated)".len();
    assert!(snapshot.run.failure().unwrap().message().len() <= bounded_failure_bytes);
    assert!(snapshot.steps[0].failure().unwrap().message().len() <= bounded_failure_bytes);
    assert!(snapshot.tool_calls[0].failure().unwrap().message().len() <= bounded_failure_bytes);
}

#[test]
#[ignore = "latency target; meaningful only in a release build"]
fn approval_decision_to_tool_dispatch_stays_below_ten_milliseconds() {
    assert!(
        !std::hint::black_box(cfg!(debug_assertions)),
        "the dispatch latency benchmark must run with --release"
    );
    let mut samples = Vec::new();
    for index in 0..9 {
        let fixture = Fixture::new();
        let agent = scenario(
            &format!("coordinator_dispatch_latency_{index}"),
            vec![
                run_started(call_write(json!({"message": "hello"}))),
                complete_after_result(),
            ],
        );
        let run = fixture.start(agent);
        let request = fixture.pending_approval(run);
        let started = Instant::now();
        fixture
            .coordinator
            .decide_approval(ApprovalDecision::grant(
                request.id(),
                ApprovalScope::ExactCall,
                DecidedVia::Cli,
                OffsetDateTime::now_utc(),
            ))
            .unwrap();
        fixture.wait_until("approved tool dispatch", || {
            fixture
                .write_started_at
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_some()
        });
        let dispatched = fixture
            .write_started_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .expect("the wait observed a dispatch timestamp");
        samples.push(dispatched.duration_since(started));
        fixture.terminal(run);
    }
    samples.sort_unstable();
    let median = samples[samples.len() / 2];
    let rustc = std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "rustc version unavailable".to_owned());
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("model name\t: "))
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "CPU unavailable".to_owned());
    let machine = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|name| name.trim().to_owned())
        .unwrap_or_else(|| "machine unavailable".to_owned());
    println!(
        "decision-to-dispatch samples={samples:?}; median={median:?}; {rustc}; {}-{}; parallelism={}; cpu={cpu}; machine={machine}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        thread::available_parallelism().map_or(0, std::num::NonZero::get),
    );
    assert!(
        median < Duration::from_millis(10),
        "median dispatch took {median:?}; samples={samples:?}"
    );
}

#[test]
fn production_tools_complete_the_flagship_edit_test_diff_run() {
    use std::fs;

    use harkness_test_fixtures::{git, initialize_repository};

    let data_dir = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    initialize_repository(workspace.path());
    fs::create_dir(workspace.path().join("src")).unwrap();
    fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"harkness-runtime\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(
        workspace.path().join("src/lib.rs"),
        b"pub const VALUE: &str = \"old\";\n",
    )
    .unwrap();
    fs::create_dir(workspace.path().join("tests")).unwrap();
    fs::write(
        workspace.path().join("tests/value.rs"),
        b"#[test]\nfn value_is_updated() {\n    assert_eq!(harkness_runtime::VALUE, \"new\");\n}\n",
    )
    .unwrap();
    git(
        workspace.path(),
        ["add", "Cargo.toml", "src/lib.rs", "tests/value.rs"],
    );
    git(workspace.path(), ["commit", "-m", "add flagship crate"]);

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
    let mut registry = ToolRegistry::new();
    crate::tools::register_read_only_tools(&mut registry).unwrap();
    crate::tools::register_mutating_tools(&mut registry).unwrap();
    let coordinator = RunCoordinator::new(
        Arc::clone(&store),
        Arc::new(registry),
        PolicyEngine::new(UserPolicy::default(), None),
    );
    let task = Task::new(
        "flagship edit test diff",
        workspace.path(),
        Some(project),
        OffsetDateTime::now_utc(),
    );
    let workspace_ref = WorkspaceRef::from_task(&task, &PassThrough);
    let task_id = coordinator.start_task(task).unwrap();
    let project = Project {
        id: project,
        display_name: "Flagship project".to_owned(),
        root: workspace.path().canonicalize().unwrap(),
        source: ProjectSource::Local,
        last_opened: OffsetDateTime::now_utc(),
        available: true,
        git: None,
    };
    let run = coordinator
        .start_run_with_workspace_metadata(
            task_id,
            Box::new(MockAgent::scenario("edit_test_diff_success").unwrap()),
            workspace_ref,
            WorkspaceMetadata::from_project(&project),
        )
        .unwrap();

    for _ in 0..2 {
        let deadline = Instant::now() + Duration::from_secs(20);
        let request = loop {
            let snapshot = coordinator.run_snapshot(run).unwrap();
            if let Some(request) = snapshot
                .approvals
                .into_iter()
                .find(|request| request.state() == ApprovalState::Pending)
            {
                break request;
            }
            assert!(
                Instant::now() < deadline,
                "flagship approval was not requested: {}",
                serde_json::to_string_pretty(&coordinator.run_snapshot(run).unwrap()).unwrap()
            );
            thread::sleep(Duration::from_millis(10));
        };
        coordinator
            .decide_approval(ApprovalDecision::grant(
                request.id(),
                ApprovalScope::ExactCall,
                DecidedVia::Cli,
                OffsetDateTime::now_utc(),
            ))
            .unwrap();
    }

    let deadline = Instant::now() + Duration::from_secs(30);
    let snapshot = loop {
        let snapshot = coordinator.run_snapshot(run).unwrap();
        if snapshot.run.state().is_terminal() {
            break snapshot;
        }
        assert!(Instant::now() < deadline, "flagship run did not finish");
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(snapshot.run.state(), ExecutionState::Succeeded);
    assert_eq!(snapshot.steps.len(), 5);
    assert!(snapshot.steps.iter().all(|step| step.state().is_terminal()));
    assert_eq!(snapshot.tool_calls.len(), 5);
    assert!(
        snapshot
            .tool_calls
            .iter()
            .all(|call| call.state().is_terminal())
    );
    assert_eq!(snapshot.approvals.len(), 2);
    assert!(
        snapshot
            .approvals
            .iter()
            .all(|request| request.state() == ApprovalState::Granted)
    );
    assert!(!snapshot.artifacts.is_empty());
    assert!(
        snapshot
            .events
            .iter()
            .enumerate()
            .all(|(index, event)| { event.seq.get() == u64::try_from(index + 1).unwrap() })
    );
    let test_call = snapshot
        .tool_calls
        .iter()
        .find(|call| call.tool_id() == "test.run")
        .unwrap();
    assert_eq!(
        test_call.output().and_then(|output| output.get("passed")),
        Some(&json!(true))
    );
    let inspect_call = snapshot
        .tool_calls
        .iter()
        .find(|call| call.tool_id() == "workspace.inspect")
        .unwrap();
    assert_eq!(
        inspect_call
            .output()
            .and_then(|output| output.get("project"))
            .and_then(|project| project.get("id")),
        Some(&json!(project.id.to_string()))
    );
    assert_eq!(
        inspect_call.output().unwrap()["project"]["display_name"],
        "Flagship project"
    );
    assert_eq!(inspect_call.output().unwrap()["project"]["source"], "local");
    assert_eq!(
        fs::read_to_string(workspace.path().join("src/lib.rs")).unwrap(),
        "pub const VALUE: &str = \"new\";\n"
    );
    for call in &snapshot.tool_calls {
        let policy = snapshot
            .events
            .iter()
            .find(|stored| {
                stored.event.kind() == &EventKind::PolicyDecision
                    && stored.event.tool_call_id() == Some(call.id())
            })
            .unwrap();
        let running = snapshot
            .events
            .iter()
            .find(|stored| {
                stored.event.kind() == &EventKind::ToolCallStateChanged
                    && stored.event.tool_call_id() == Some(call.id())
                    && stored.event.payload()["state"] == "running"
            })
            .unwrap();
        assert!(policy.seq < running.seq);
    }
}
