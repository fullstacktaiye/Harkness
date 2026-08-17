//! Interruption detection, recovery, and retry.
//!
//! The process-level cases here re-execute this test binary in a named role,
//! wait on a file the child writes when it has reached the state under test,
//! and then kill it. There is no sleep anywhere in the synchronization: a
//! `SIGKILL` landing a moment too early would leave the run in a state the
//! assertions do not describe, and a test that waited a fixed time for the
//! right one would be a flake with a timer attached.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};

use harkness_core::ProjectId;
use harkness_test_fixtures::{child_path, park, wait_for_child_signal};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tempfile::TempDir;
use time::OffsetDateTime;

use crate::agent::{
    Agent, AgentAction, MockAgent, ObservationPattern, Scenario, ScenarioId, ScenarioStep,
    WorkspaceRef,
};
use crate::approval::{ApprovalScope, ApprovalState};
use crate::coordinator::{RunCoordinator, RuntimeError};
use crate::domain::{ExecutionState, LeaseId, Run, RunId, Task, ToolCallState};
use crate::policy::{PolicyEngine, UserPolicy};
use crate::store::{EventKind, LeaseRecord, PassThrough, RunEvent, Store};
use crate::tool::{
    ExecutionContext, RiskLevel, Tool, ToolError, ToolIdentity, ToolMetadata, ToolRegistry,
};
use crate::trust::{TrustState, WorkspaceTrust};

const CHILD_DATA_DIR_ENV: &str = "HARKNESS_RECOVERY_TEST_DATA_DIR";
const CHILD_WORKSPACE_ENV: &str = "HARKNESS_RECOVERY_TEST_WORKSPACE";
const CHILD_PROJECT_ENV: &str = "HARKNESS_RECOVERY_TEST_PROJECT";
const CHILD_READY_ENV: &str = "HARKNESS_RECOVERY_TEST_READY";

const PARK_CHILD: &str = "coordinator::tests::recovery::park_a_run_awaiting_approval";
const APPEND_CHILD: &str = "coordinator::tests::recovery::append_event_batches_until_killed";

/// How many events one child batch appends, so a torn batch would be visible.
const BATCH_EVENTS: usize = 8;

const POLL: Duration = Duration::from_millis(5);
const DEADLINE: Duration = Duration::from_secs(20);

// -- fixtures ----------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ParkInput {
    message: String,
}

#[derive(Debug, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct ParkOutput {
    echoed: String,
}

/// A mutation nobody is meant to reach.
///
/// Its risk is what matters: `WorkspaceWrite` makes the built-in policy ask, so
/// a run reaches `waiting_for_approval` with a pending question and an
/// `awaiting_approval` call — the exact shape a killed process leaves behind.
struct ParkingWrite;

impl Tool for ParkingWrite {
    type Input = ParkInput;
    type Output = ParkOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse("fixture.recovery_write", "1.0.0").unwrap(),
            "Recovery write fixture",
            "Represents one workspace mutation an interrupted run never made.",
            RiskLevel::WorkspaceWrite,
        )
    }

    fn execute(
        &self,
        input: ParkInput,
        _context: &mut ExecutionContext,
    ) -> Result<ParkOutput, ToolError> {
        Ok(ParkOutput {
            echoed: input.message,
        })
    }
}

fn parking_agent() -> MockAgent {
    MockAgent::from_scenario(
        Scenario::new(
            ScenarioId::new("recovery_park").unwrap(),
            vec![
                ScenarioStep::new(
                    ObservationPattern::RunStarted { task_title: None },
                    AgentAction::CallTool {
                        tool_id: "fixture.recovery_write".parse().unwrap(),
                        tool_version: "1.0.0".parse().unwrap(),
                        input: json!({"message": "hello"}),
                    },
                ),
                ScenarioStep::new(
                    ObservationPattern::ToolResult {
                        artifact_media_type: None,
                        output_contains: None,
                    },
                    AgentAction::CompleteRun {
                        summary: "done".to_owned(),
                    },
                ),
            ],
        )
        .unwrap(),
    )
}

/// A mutation that reaches its body and then fails.
///
/// The distinction it exists for: this call *starts*, so a retry of the run
/// that made it has to warn that the workspace may already differ, however the
/// call ended.
struct FailingWrite;

impl Tool for FailingWrite {
    type Input = ParkInput;
    type Output = ParkOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse("fixture.recovery_failing_write", "1.0.0").unwrap(),
            "Failing write fixture",
            "Starts a workspace mutation and then reports that it failed.",
            RiskLevel::WorkspaceWrite,
        )
    }

    fn execute(
        &self,
        _input: ParkInput,
        _context: &mut ExecutionContext,
    ) -> Result<ParkOutput, ToolError> {
        Err(ToolError::execution_failed("the fixture write failed"))
    }
}

fn registry() -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register(ParkingWrite).unwrap();
    registry.register(FailingWrite).unwrap();
    Arc::new(registry)
}

/// Calls the failing mutation and fails the run when it fails.
///
/// Leaves a terminal, non-successful run whose write really did execute — the
/// only shape that is both retryable and genuinely uncertain about the
/// workspace.
fn failing_write_agent() -> MockAgent {
    MockAgent::from_scenario(
        Scenario::new(
            ScenarioId::new("recovery_failing_write").unwrap(),
            vec![
                ScenarioStep::new(
                    ObservationPattern::RunStarted { task_title: None },
                    AgentAction::CallTool {
                        tool_id: "fixture.recovery_failing_write".parse().unwrap(),
                        tool_version: "1.0.0".parse().unwrap(),
                        input: json!({"message": "hello"}),
                    },
                ),
                ScenarioStep::new(
                    ObservationPattern::ToolFailed {
                        error_kind: Some("execution_failed".to_owned()),
                    },
                    AgentAction::FailRun {
                        reason: crate::agent::AgentFailure::AgentFailed {
                            reason: "the write failed".to_owned(),
                        },
                    },
                ),
            ],
        )
        .unwrap(),
    )
}

/// Calls the parked mutation and fails the run when it is refused.
///
/// The negative case for the same flag: a denied call never entered its body,
/// so nothing it might have written exists to warn about.
fn denied_write_agent() -> MockAgent {
    MockAgent::from_scenario(
        Scenario::new(
            ScenarioId::new("recovery_denied_write").unwrap(),
            vec![
                ScenarioStep::new(
                    ObservationPattern::RunStarted { task_title: None },
                    AgentAction::CallTool {
                        tool_id: "fixture.recovery_write".parse().unwrap(),
                        tool_version: "1.0.0".parse().unwrap(),
                        input: json!({"message": "hello"}),
                    },
                ),
                ScenarioStep::new(
                    ObservationPattern::ApprovalOutcome {
                        outcome: Some(crate::agent::ApprovalOutcomeView::Denied),
                    },
                    AgentAction::FailRun {
                        reason: crate::agent::AgentFailure::ApprovalDenied {
                            reason: "refused".to_owned(),
                        },
                    },
                ),
            ],
        )
        .unwrap(),
    )
}

/// One data directory and one trusted workspace, shared by a parent and its
/// re-executed children.
struct Harness {
    _root: TempDir,
    data_dir: PathBuf,
    workspace: PathBuf,
    project: ProjectId,
}

impl Harness {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        let data_dir = root.path().join("data");
        let workspace = root.path().join("workspace");
        fs::create_dir_all(&data_dir).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        let project = ProjectId::new();
        let store = Store::open(&data_dir).unwrap();
        store
            .put_workspace_trust(
                &WorkspaceTrust::decide(
                    project,
                    &workspace,
                    TrustState::Trusted,
                    OffsetDateTime::now_utc(),
                )
                .unwrap(),
            )
            .unwrap();
        Self {
            _root: root,
            data_dir,
            workspace,
            project,
        }
    }

    fn store(&self) -> Arc<Store> {
        Arc::new(Store::open(&self.data_dir).unwrap())
    }

    /// Opens a coordinator, which sweeps before it returns.
    fn coordinator(&self) -> RunCoordinator {
        self.open().0
    }

    fn open(&self) -> (RunCoordinator, crate::coordinator::RecoveryReport) {
        let store = self.store();
        let executor = crate::tool::ToolExecutor::new(Arc::clone(&store), registry());
        RunCoordinator::open(
            store,
            registry(),
            Arc::new(PolicyEngine::new(UserPolicy::default(), None)),
            Arc::new(crate::approval::ApprovalGate::new()),
            Arc::new(crate::schedule::Scheduler::new(executor)),
        )
        .unwrap()
    }

    fn task(&self, title: &str) -> Task {
        Task::new(
            title,
            &self.workspace,
            Some(self.project),
            OffsetDateTime::now_utc(),
        )
    }

    /// The workspace view a retry of `run` has to be handed.
    ///
    /// Read back through the run's own task rather than rebuilt from this
    /// fixture, so a retry is bound to the workspace the original was bound to.
    fn workspace_ref(&self, coordinator: &RunCoordinator, run: RunId) -> WorkspaceRef {
        let store = coordinator.store();
        let task = store
            .load_task(store.load_run(run).unwrap().task_id())
            .unwrap();
        WorkspaceRef::from_task(&task, &PassThrough)
    }

    /// Starts a run that parks on its approval, and returns it once it has.
    fn parked_run(&self, coordinator: &RunCoordinator) -> RunId {
        let task = self.task("recovery fixture");
        let workspace = WorkspaceRef::from_task(&task, &PassThrough);
        let task_id = coordinator.start_task(task).unwrap();
        let run = coordinator
            .start_run(task_id, Box::new(parking_agent()), workspace)
            .unwrap();
        await_parked(coordinator, run);
        run
    }

    fn spawn(&self, role: &str, ready: &Path) -> Child {
        Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(role)
            .arg("--ignored")
            .env(CHILD_DATA_DIR_ENV, &self.data_dir)
            .env(CHILD_WORKSPACE_ENV, &self.workspace)
            .env(CHILD_PROJECT_ENV, self.project.to_string())
            .env(CHILD_READY_ENV, ready)
            .spawn()
            .unwrap()
    }

    /// Spawns `role`, waits for its ready file, and reads the run it recorded.
    fn spawn_until_ready(&self, role: &str, name: &str) -> (Child, RunId) {
        let ready = self._root.path().join(name);
        let mut child = self.spawn(role, &ready);
        wait_for_child_signal(&mut child, &ready);
        let recorded = fs::read_to_string(&ready).unwrap();
        (child, recorded.trim().parse().unwrap())
    }
}

/// Publishes a value to the parent only once it is complete.
///
/// `exists` becomes true when a file is created, which is before anything has
/// been written to it, so the parent would otherwise be free to read an empty
/// ready file and parse nothing.
fn signal(ready: &Path, value: &str) {
    let staged = ready.with_extension("staged");
    fs::write(&staged, value).unwrap();
    fs::rename(&staged, ready).unwrap();
}

fn await_parked(coordinator: &RunCoordinator, run: RunId) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let snapshot = coordinator.run_snapshot(run).unwrap();
        if snapshot.run.state() == ExecutionState::WaitingForApproval
            && snapshot
                .approvals
                .iter()
                .any(|request| request.state() == ApprovalState::Pending)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "run {run} never reached a pending approval: {:?}",
            snapshot.run.state()
        );
        std::thread::sleep(POLL);
    }
}

// -- child roles -------------------------------------------------------------

/// Drives a run to a pending approval, says so, and waits to be killed.
#[test]
#[ignore = "only run as a child process by the interrupted-run recovery tests"]
fn park_a_run_awaiting_approval() {
    let data_dir = child_path(CHILD_DATA_DIR_ENV);
    let workspace = child_path(CHILD_WORKSPACE_ENV);
    let ready = child_path(CHILD_READY_ENV);
    let project = std::env::var(CHILD_PROJECT_ENV)
        .unwrap()
        .parse::<ProjectId>()
        .unwrap();

    let store = Arc::new(Store::open(&data_dir).unwrap());
    let coordinator = RunCoordinator::new(
        store,
        registry(),
        PolicyEngine::new(UserPolicy::default(), None),
    )
    .unwrap();
    let task = Task::new(
        "recovery child",
        &workspace,
        Some(project),
        OffsetDateTime::now_utc(),
    );
    let workspace_ref = WorkspaceRef::from_task(&task, &PassThrough);
    let task_id = coordinator.start_task(task).unwrap();
    let run = coordinator
        .start_run(task_id, Box::new(parking_agent()), workspace_ref)
        .unwrap();

    await_parked(&coordinator, run);
    signal(&ready, &run.to_string());
    // Deliberately never returns: this process is expected to die holding its
    // lease, which is the whole thing under test. Returning would release it
    // cleanly and prove nothing about a crash.
    park()
}

/// Appends whole event batches until the parent kills this process.
#[test]
#[ignore = "only run as a child process by the interrupted-run recovery tests"]
fn append_event_batches_until_killed() {
    let data_dir = child_path(CHILD_DATA_DIR_ENV);
    let workspace = child_path(CHILD_WORKSPACE_ENV);
    let ready = child_path(CHILD_READY_ENV);
    let project = std::env::var(CHILD_PROJECT_ENV)
        .unwrap()
        .parse::<ProjectId>()
        .unwrap();

    let store = Store::open(&data_dir).unwrap();
    let task = Task::new(
        "batch child",
        &workspace,
        Some(project),
        OffsetDateTime::now_utc(),
    );
    store.insert_task(&task).unwrap();
    let run = Run::new(task.id(), OffsetDateTime::now_utc());
    store.insert_run(&run).unwrap();

    let batch = |index: usize| {
        (0..BATCH_EVENTS)
            .map(|offset| {
                RunEvent::new(EventKind::Diagnostic, OffsetDateTime::now_utc())
                    .with_payload(json!({"batch": index, "offset": offset}))
            })
            .collect::<Vec<_>>()
    };
    store.append_events(run.id(), batch(0)).unwrap();
    signal(&ready, &run.id().to_string());
    for index in 1.. {
        store.append_events(run.id(), batch(index)).unwrap();
    }
}

// -- the sweep, in this process ----------------------------------------------

/// Records a run claimed by a lease whose lock file was never taken.
///
/// That is exactly what a dead process leaves: a row naming a claim, and no
/// lock anywhere backing it. Building it directly keeps the sweep's own logic
/// testable without a second process — the process-level cases below prove the
/// same thing against a real `SIGKILL`.
fn abandoned_run(store: &Store, task: &Task, lease: LeaseId) -> Run {
    let record = LeaseRecord::acquired(lease, 4_242, OffsetDateTime::now_utc());
    let run = Run::new(task.id(), OffsetDateTime::now_utc());
    store
        .insert_run_with_event(
            &run,
            Some(&record),
            RunEvent::new(EventKind::RunStateChanged, OffsetDateTime::now_utc())
                .with_payload(json!({"state": "queued"})),
        )
        .unwrap();
    run
}

#[test]
fn a_run_whose_claim_has_no_lock_is_interrupted_at_the_next_start() {
    let harness = Harness::new();
    let store = harness.store();
    let task = harness.task("abandoned");
    store.insert_task(&task).unwrap();
    let run = abandoned_run(&store, &task, LeaseId::new());
    drop(store);

    let (coordinator, report) = harness.open();

    assert_eq!(report.interrupted_runs(), [run.id()]);
    assert!(report.failures().is_empty());
    assert!(!report.was_contended());
    let snapshot = coordinator.run_snapshot(run.id()).unwrap();
    assert_eq!(snapshot.run.state(), ExecutionState::Interrupted);
    assert!(
        snapshot
            .events
            .iter()
            .any(|stored| stored.event.kind() == &EventKind::RunInterrupted),
        "the sweep must say why, not only that"
    );
}

#[test]
fn a_second_sweep_over_the_same_store_marks_nothing_again() {
    let harness = Harness::new();
    let store = harness.store();
    let task = harness.task("swept twice");
    store.insert_task(&task).unwrap();
    let run = abandoned_run(&store, &task, LeaseId::new());
    drop(store);

    let (first, first_report) = harness.open();
    let events = first.run_snapshot(run.id()).unwrap().events.len();
    let (second, second_report) = harness.open();

    assert_eq!(first_report.interrupted_runs(), [run.id()]);
    assert!(second_report.interrupted_runs().is_empty());
    assert_eq!(
        second.run_snapshot(run.id()).unwrap().events.len(),
        events,
        "a terminal run must not collect a second set of markings"
    );
}

#[test]
fn two_coordinators_starting_at_once_produce_one_set_of_markings() {
    let harness = Harness::new();
    let store = harness.store();
    let task = harness.task("contended");
    store.insert_task(&task).unwrap();
    let run = abandoned_run(&store, &task, LeaseId::new());
    drop(store);

    let reports = std::thread::scope(|scope| {
        let handles = (0..2)
            .map(|_| scope.spawn(|| harness.open()))
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });

    let marked = reports
        .iter()
        .filter(|(_, report)| report.interrupted_runs() == [run.id()])
        .count();
    assert_eq!(marked, 1, "both coordinators marked the same run");
    let interruptions = reports[0]
        .0
        .run_snapshot(run.id())
        .unwrap()
        .events
        .iter()
        .filter(|stored| stored.event.kind() == &EventKind::RunInterrupted)
        .count();
    assert_eq!(interruptions, 1);
}

#[test]
fn a_run_this_coordinator_is_driving_is_never_swept_by_the_next_one() {
    let harness = Harness::new();
    let live = harness.coordinator();
    let run = harness.parked_run(&live);

    let (observer, report) = harness.open();

    assert!(
        report.interrupted_runs().is_empty(),
        "a live sibling's run was claimed"
    );
    assert_eq!(
        observer.run_snapshot(run).unwrap().run.state(),
        ExecutionState::WaitingForApproval
    );
}

#[test]
fn an_interrupted_runs_timeline_and_artifacts_survive_intact() {
    let harness = Harness::new();
    let store = harness.store();
    let task = harness.task("inspectable");
    store.insert_task(&task).unwrap();
    let run = abandoned_run(&store, &task, LeaseId::new());
    for index in 0..12 {
        store
            .append_event(
                run.id(),
                RunEvent::new(EventKind::Diagnostic, OffsetDateTime::now_utc())
                    .with_payload(json!({"index": index})),
            )
            .unwrap();
    }
    let mut sink = store
        .create_artifact(
            run.id(),
            "build.log",
            "text/plain",
            OffsetDateTime::now_utc(),
        )
        .unwrap();
    std::io::Write::write_all(&mut sink, b"partial output\n").unwrap();
    let artifact = sink.finish().unwrap();
    let before = store.events(run.id(), None, 1_000).unwrap();
    drop(store);

    let coordinator = harness.coordinator();
    let snapshot = coordinator.run_snapshot(run.id()).unwrap();

    // Every event committed before the sweep is present, in the order it was
    // written, and the sweep only ever appended after them.
    assert_eq!(
        snapshot.events[..before.len()]
            .iter()
            .map(|stored| stored.seq)
            .collect::<Vec<_>>(),
        before.iter().map(|stored| stored.seq).collect::<Vec<_>>()
    );
    assert!(snapshot.events.len() > before.len());
    assert!(
        snapshot
            .events
            .windows(2)
            .all(|pair| pair[0].seq < pair[1].seq),
        "sequence numbers must stay monotonic across a recovery"
    );
    assert_eq!(
        snapshot
            .artifacts
            .iter()
            .map(|stored| stored.id())
            .collect::<Vec<_>>(),
        [artifact.id()]
    );
}

#[test]
fn a_hundred_abandoned_runs_are_swept_without_reading_their_timelines() {
    let harness = Harness::new();
    let store = harness.store();
    let task = harness.task("bulk");
    store.insert_task(&task).unwrap();
    let lease = LeaseId::new();
    let runs = (0..100)
        .map(|_| {
            let run = abandoned_run(&store, &task, lease);
            store
                .append_events(
                    run.id(),
                    (0..20).map(|index| {
                        RunEvent::new(EventKind::Diagnostic, OffsetDateTime::now_utc())
                            .with_payload(json!({"index": index}))
                    }),
                )
                .unwrap();
            run.id()
        })
        .collect::<Vec<_>>();
    drop(store);

    let (coordinator, report) = harness.open();

    let mut interrupted = report.interrupted_runs().to_vec();
    interrupted.sort_unstable();
    let mut expected = runs.clone();
    expected.sort_unstable();
    assert_eq!(interrupted, expected);
    assert!(report.failures().is_empty());

    // One process died once, so every run it left says so the same way. The
    // claim is written off while the first of its runs is marked, and a sweep
    // that re-read it per run would report `lease_released` for the other
    // ninety-nine — one death described a hundred ways.
    let reasons = runs
        .iter()
        .map(|run| {
            coordinator
                .run_snapshot(*run)
                .unwrap()
                .events
                .iter()
                .filter(|stored| stored.event.kind() == &EventKind::RunInterrupted)
                .map(|stored| stored.event.payload()["reason"].clone())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    assert!(
        reasons
            .iter()
            .all(|reason| reason.as_slice() == [json!("lease_lock_released")]),
        "one death was described in more than one way"
    );
}

#[test]
fn a_clean_shutdown_leaves_no_live_claim_and_no_unfinished_run() {
    let harness = Harness::new();
    let coordinator = harness.coordinator();
    let run = harness.parked_run(&coordinator);
    assert_eq!(harness.store().live_leases().unwrap().len(), 1);

    coordinator.shutdown();

    let store = harness.store();
    assert!(
        store.live_leases().unwrap().is_empty(),
        "an exiting coordinator kept its claim"
    );
    // The run it was driving is cancelled by the cooperative stop, which is
    // better evidence than an inferred ending. Whatever it reaches, it is
    // terminal, so the next start finds nothing to recover.
    let state = store.load_run(run).unwrap().state();
    assert!(state.is_terminal(), "shutdown left run {run} {state}");
    assert!(store.unfinished_runs().unwrap().is_empty());

    let (_next, report) = harness.open();
    assert!(report.interrupted_runs().is_empty());
}

#[test]
fn a_coordinator_that_has_shut_down_refuses_to_start_anything_else() {
    let harness = Harness::new();
    let coordinator = harness.coordinator();
    coordinator.shutdown();

    let task = harness.task("after shutdown");
    let workspace = WorkspaceRef::from_task(&task, &PassThrough);
    let task_id = coordinator.start_task(task).unwrap();
    let error = coordinator
        .start_run(task_id, Box::new(parking_agent()), workspace)
        .unwrap_err();

    assert_eq!(error.kind(), "lease_unavailable");
    assert!(
        harness.store().unfinished_runs().unwrap().is_empty(),
        "a run was recorded under a claim nothing holds"
    );
}

#[test]
fn dropping_a_coordinator_releases_its_claim_so_the_next_start_can_recover() {
    let harness = Harness::new();
    let coordinator = harness.coordinator();
    let lease = coordinator.lease_id();
    harness.parked_run(&coordinator);
    drop(coordinator);

    let store = harness.store();
    assert!(
        store
            .lease(lease)
            .unwrap()
            .is_none_or(|record| record.is_released()),
        "a dropped coordinator left a live claim behind"
    );
}

// -- retry --------------------------------------------------------------------

#[test]
fn retrying_an_interrupted_run_creates_a_fresh_attempt_with_provenance() {
    let harness = Harness::new();
    let store = harness.store();
    let task = harness.task("retried");
    store.insert_task(&task).unwrap();
    let original = abandoned_run(&store, &task, LeaseId::new()).id();
    drop(store);
    let coordinator = harness.coordinator();
    let workspace = harness.workspace_ref(&coordinator, original);

    let retry = coordinator
        .retry_run(original, Box::new(parking_agent()), workspace)
        .unwrap();
    await_parked(&coordinator, retry);

    let record = coordinator.run_snapshot(retry).unwrap().run;
    assert_ne!(retry, original, "a retry is a new run, never a rewind");
    assert_eq!(record.retry_of(), Some(original));
    assert_eq!(record.task_id(), task.id());
    assert!(
        !record.workspace_may_be_modified(),
        "the abandoned attempt never started a call, so nothing may have been written"
    );
    assert_eq!(coordinator.store().retries_of(original).unwrap(), [retry]);

    // The original keeps its own terminal state and its own history, and gains
    // exactly one line saying it was re-attempted.
    let originals = coordinator.run_snapshot(original).unwrap();
    assert_eq!(originals.run.state(), ExecutionState::Interrupted);
    let retried = originals
        .events
        .iter()
        .filter(|stored| stored.event.kind() == &EventKind::RunRetried)
        .collect::<Vec<_>>();
    assert_eq!(retried.len(), 1);
    assert_eq!(
        retried[0].event.payload()["retry_run_id"],
        json!(retry.to_string())
    );
}

#[test]
fn a_retry_inherits_no_approval_and_asks_its_own_questions() {
    let harness = Harness::new();
    let coordinator = harness.coordinator();
    // Granted for the remainder of *that* run, which is the broadest a
    // workspace write can be given.
    let original = run_to_terminal(&harness, &coordinator, failing_write_agent(), Answer::Grant);
    let granted = sole_approval(&coordinator, original);
    assert_eq!(granted.state(), ApprovalState::Granted);
    assert_eq!(coordinator.store().run_grants(original).unwrap().len(), 1);

    let workspace = harness.workspace_ref(&coordinator, original);
    let retry = coordinator
        .retry_run(original, Box::new(parking_agent()), workspace)
        .unwrap();
    await_parked(&coordinator, retry);

    let fresh = coordinator.run_snapshot(retry).unwrap();
    assert!(
        fresh
            .approvals
            .iter()
            .any(|request| request.state() == ApprovalState::Pending),
        "the retry ran under an inherited grant instead of asking again"
    );
    assert!(
        !fresh
            .approvals
            .iter()
            .any(|request| request.id() == granted.id()),
        "an approval answered for one run reached another"
    );
    assert!(
        coordinator.store().run_grants(retry).unwrap().is_empty(),
        "a grant given to the earlier attempt applied to the retry"
    );
}

#[test]
fn a_retry_warns_only_when_the_earlier_attempt_started_something_that_could_write() {
    let harness = Harness::new();
    let coordinator = harness.coordinator();

    // A write that reached its body and failed. Whatever it managed before it
    // returned is on disk, and nothing in v0.3 undoes it.
    let executed = run_to_terminal(&harness, &coordinator, failing_write_agent(), Answer::Grant);
    let started = coordinator.run_snapshot(executed).unwrap();
    assert_eq!(started.run.state(), ExecutionState::Failed);
    assert_eq!(started.tool_calls[0].state(), ToolCallState::Failed);
    assert!(started.tool_calls[0].started_at().is_some());

    // A write policy refused before execution. It never entered its body, so a
    // retry of it has nothing to warn about.
    let refused = run_to_terminal(&harness, &coordinator, denied_write_agent(), Answer::Deny);
    let denied = coordinator.run_snapshot(refused).unwrap();
    assert_eq!(denied.run.state(), ExecutionState::Failed);
    assert_eq!(denied.tool_calls[0].state(), ToolCallState::Denied);
    assert!(denied.tool_calls[0].started_at().is_none());

    let workspace = harness.workspace_ref(&coordinator, executed);
    let warned = coordinator
        .retry_run(executed, Box::new(parking_agent()), workspace.clone())
        .unwrap();
    await_parked(&coordinator, warned);
    let quiet = coordinator
        .retry_run(refused, Box::new(parking_agent()), workspace)
        .unwrap();
    await_parked(&coordinator, quiet);

    assert!(
        coordinator
            .run_snapshot(warned)
            .unwrap()
            .run
            .workspace_may_be_modified(),
        "a write that executed must be surfaced to whoever retries it"
    );
    assert!(
        !coordinator
            .run_snapshot(quiet)
            .unwrap()
            .run
            .workspace_may_be_modified(),
        "a call refused before execution cannot have written anything"
    );
}

#[test]
fn a_retry_is_refused_for_an_active_run_and_for_one_that_succeeded() {
    let harness = Harness::new();
    let coordinator = harness.coordinator();
    let active = harness.parked_run(&coordinator);
    let workspace = harness.workspace_ref(&coordinator, active);

    let error = coordinator
        .retry_run(active, Box::new(parking_agent()), workspace.clone())
        .unwrap_err();
    assert_eq!(error.kind(), "run_still_active");

    answer(&coordinator, active, Answer::Grant);
    await_terminal(&coordinator, active);
    assert_eq!(
        coordinator.run_snapshot(active).unwrap().run.state(),
        ExecutionState::Succeeded
    );

    let error = coordinator
        .retry_run(active, Box::new(parking_agent()), workspace)
        .unwrap_err();
    assert_eq!(error.kind(), "run_not_retryable");
    assert!(matches!(
        error,
        RuntimeError::RunNotRetryable { state, .. } if state == ExecutionState::Succeeded
    ));
}

/// Which way a test answers the one approval a fixture run asks for.
#[derive(Clone, Copy)]
enum Answer {
    Grant,
    Deny,
}

fn sole_approval(coordinator: &RunCoordinator, run: RunId) -> crate::approval::ApprovalRequest {
    let approvals = coordinator.run_snapshot(run).unwrap().approvals;
    assert_eq!(approvals.len(), 1, "run {run} asked more than one question");
    approvals.into_iter().next().expect("one approval")
}

fn answer(coordinator: &RunCoordinator, run: RunId, answer: Answer) {
    let request = sole_approval(coordinator, run);
    let decision = match answer {
        Answer::Grant => crate::approval::ApprovalDecision::grant(
            request.id(),
            ApprovalScope::ToolForRun,
            crate::approval::DecidedVia::Cli,
            OffsetDateTime::now_utc(),
        ),
        Answer::Deny => crate::approval::ApprovalDecision::deny(
            request.id(),
            crate::approval::DecidedVia::Cli,
            OffsetDateTime::now_utc(),
        ),
    };
    coordinator.decide_approval(decision).unwrap();
}

/// Runs `agent` to a terminal state, answering its one approval on the way.
fn run_to_terminal(
    harness: &Harness,
    coordinator: &RunCoordinator,
    agent: MockAgent,
    decision: Answer,
) -> RunId {
    let task = harness.task("recovery fixture");
    let workspace = WorkspaceRef::from_task(&task, &PassThrough);
    let task_id = coordinator.start_task(task).unwrap();
    let run = coordinator
        .start_run(task_id, Box::new(agent), workspace)
        .unwrap();
    await_parked(coordinator, run);
    answer(coordinator, run, decision);
    await_terminal(coordinator, run);
    run
}

fn await_terminal(coordinator: &RunCoordinator, run: RunId) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let state = coordinator.run_snapshot(run).unwrap().run.state();
        if state.is_terminal() {
            return;
        }
        assert!(Instant::now() < deadline, "run {run} stayed {state}");
        std::thread::sleep(POLL);
    }
}

// -- across a real process death ---------------------------------------------

#[test]
fn killing_a_process_mid_run_makes_the_next_start_mark_everything_it_left() {
    let harness = Harness::new();
    let (mut child, run) = harness.spawn_until_ready(PARK_CHILD, "killed-ready");
    child.kill().unwrap();
    child.wait().unwrap();

    let (coordinator, report) = harness.open();

    assert_eq!(report.interrupted_runs(), [run]);
    let snapshot = coordinator.run_snapshot(run).unwrap();
    assert_eq!(snapshot.run.state(), ExecutionState::Interrupted);
    assert_eq!(snapshot.tool_calls.len(), 1);
    assert_eq!(
        snapshot.tool_calls[0].state(),
        ToolCallState::Interrupted,
        "the call the dead process was holding stayed in flight"
    );
    assert_eq!(snapshot.approvals.len(), 1);
    assert_eq!(snapshot.approvals[0].state(), ApprovalState::Superseded);
    assert!(
        snapshot.approvals[0].decision().is_none(),
        "nobody answered it, so nothing may be recorded as an answer"
    );
    assert_eq!(report.expired_approvals(), [snapshot.approvals[0].id()]);

    // Each marking carries its own event, and the sweep says why.
    let kinds = snapshot
        .events
        .iter()
        .map(|stored| stored.event.kind().as_str().to_owned())
        .collect::<Vec<_>>();
    assert!(kinds.contains(&"run_interrupted".to_owned()));
    assert_eq!(
        snapshot
            .events
            .iter()
            .filter(|stored| stored.event.kind() == &EventKind::RunInterrupted)
            .map(|stored| stored.event.payload()["reason"].clone())
            .collect::<Vec<_>>(),
        [json!("lease_lock_released")]
    );

    // And it is retryable, which is the point of ending it at all.
    let workspace = harness.workspace_ref(&coordinator, run);
    let retry = coordinator
        .retry_run(run, Box::new(parking_agent()), workspace)
        .unwrap();
    await_parked(&coordinator, retry);
    let record = coordinator.run_snapshot(retry).unwrap().run;
    assert_eq!(record.retry_of(), Some(run));
    assert!(
        !record.workspace_may_be_modified(),
        "the killed attempt was still waiting to be allowed to write"
    );
}

/// The frozen `restart_recovery` script and what recovery actually records
/// have to agree, and this is where they meet.
///
/// Scenario 8 is written against a `tool_failed` observation whose kind is
/// `interrupted`. That kind is not invented by the agent seam — it is the
/// spelling the sweep writes onto the call the dead process was holding, so if
/// the two ever drift, a restarted run would hand the script an observation it
/// diverges on. Driving the real kill, the real sweep, and then the frozen
/// script over the record it produced is what pins them together.
#[test]
fn the_restart_recovery_script_answers_what_a_recovered_call_records() {
    let harness = Harness::new();
    let (mut child, run) = harness.spawn_until_ready(PARK_CHILD, "scenario-ready");
    child.kill().unwrap();
    child.wait().unwrap();
    let coordinator = harness.coordinator();
    let snapshot = coordinator.run_snapshot(run).unwrap();
    let recovered = &snapshot.tool_calls[0];
    assert_eq!(recovered.state(), ToolCallState::Interrupted);

    let store = coordinator.store();
    let task = store.load_task(snapshot.run.task_id()).unwrap();
    let mut agent = MockAgent::scenario("restart_recovery").unwrap();
    let requested = agent.next_action(crate::agent::Observation::RunStarted {
        task: crate::agent::TaskRef::from_task(&task, &PassThrough),
        workspace: WorkspaceRef::from_task(&task, &PassThrough),
    });
    assert!(matches!(requested, AgentAction::CallTool { .. }));

    let terminal = agent.next_action(crate::agent::Observation::ToolFailed {
        call: recovered.id(),
        error: crate::agent::ToolErrorView::new(
            recovered.state().as_str(),
            "the owning process stopped before this call completed",
            &PassThrough,
        ),
    });

    assert!(
        matches!(
            terminal,
            AgentAction::FailRun {
                reason: crate::agent::AgentFailure::Interrupted { .. }
            }
        ),
        "the frozen scenario diverged on the kind recovery records: {terminal:?}"
    );
}

#[test]
fn a_run_owned_by_a_live_second_process_is_left_alone() {
    let harness = Harness::new();
    let (mut child, run) = harness.spawn_until_ready(PARK_CHILD, "live-ready");

    let (coordinator, report) = harness.open();
    let observed = coordinator.run_snapshot(run).unwrap();

    child.kill().unwrap();
    child.wait().unwrap();

    assert!(
        report.interrupted_runs().is_empty(),
        "a live sibling process had its run claimed"
    );
    assert_eq!(observed.run.state(), ExecutionState::WaitingForApproval);
    assert_eq!(observed.approvals[0].state(), ApprovalState::Pending);
}

#[test]
fn a_process_killed_between_event_batches_leaves_whole_batches_behind() {
    let harness = Harness::new();
    let (mut child, run) = harness.spawn_until_ready(APPEND_CHILD, "batch-ready");
    child.kill().unwrap();
    child.wait().unwrap();

    let store = harness.store();
    let mut events = Vec::new();
    let mut after = None;
    loop {
        let page = store.events(run, after, 1_000).unwrap();
        if page.is_empty() {
            break;
        }
        after = page.last().map(|stored| stored.seq);
        events.extend(page);
    }

    assert!(
        !events.is_empty(),
        "the child signalled before its first batch"
    );
    assert_eq!(
        events.len() % BATCH_EVENTS,
        0,
        "a partially applied batch is observable after reopening"
    );
    assert_eq!(
        events
            .iter()
            .map(|stored| stored.seq.get())
            .collect::<Vec<_>>(),
        (1..=events.len() as u64).collect::<Vec<_>>(),
        "the surviving log must be the prefix the child committed"
    );
}
