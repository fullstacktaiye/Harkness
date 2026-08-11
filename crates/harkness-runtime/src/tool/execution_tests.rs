//! Behavioural coverage for the tool executor and its supervised processes.
//!
//! Every test opens its own store under a temporary directory and registers its
//! own fixture tools, so nothing here reads or writes the real Harkness data
//! directory and no test depends on another's registry.
//!
//! The panic-containment test prints `thread ... panicked at ...`. That output
//! is expected: the panic is deliberate and the test asserts on the recorded
//! failure it became. Silencing it would need a process-global panic hook, which
//! tests running in parallel share — see the note at the top of `tests.rs`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use harkness_git::Cancellation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tempfile::TempDir;
use time::OffsetDateTime;

use crate::domain::{Run, RunId, Step, Task, ToolCall, ToolCallId, ToolCallState};
use crate::store::{EventKind, Store, StoredEvent};

use super::{
    CallOutcome, CompletedCall, DEFAULT_STREAM_TAIL_BYTES, ExecutionContext, ExecutionLimits,
    ProgressEvent, ProgressUnit, REJECTED_OUTPUT_ARTIFACT, RiskLevel, TERMINATION_GRACE, Tool,
    ToolError, ToolExecutor, ToolIdentity, ToolMetadata, ToolRegistry, progress_channel,
};

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A store, a run, and a step, ready for calls to be recorded against.
struct Fixture {
    _data_dir: TempDir,
    store: Arc<Store>,
    run: Run,
    step: Step,
    workspace: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let data_dir = TempDir::new().unwrap();
        let store = Arc::new(Store::open(data_dir.path()).unwrap());
        let workspace = TempDir::new().unwrap();

        let task = Task::new("Execute a tool", workspace.path(), None, at(0));
        store.insert_task(&task).unwrap();
        let run = Run::new(task.id(), at(1));
        store.insert_run(&run).unwrap();
        let step = Step::new(run.id(), 0, "Run the tool", at(2));
        store.insert_step(&step).unwrap();

        Self {
            _data_dir: data_dir,
            store,
            run,
            step,
            workspace,
        }
    }

    /// Records a pending call of `tool_id` at whichever version is latest.
    fn pending(&self, tool_id: &str, input: Value) -> ToolCallId {
        self.pending_version(tool_id, "", input)
    }

    /// Records a pending call naming an explicit version, or none at all.
    fn pending_version(&self, tool_id: &str, version: &str, input: Value) -> ToolCallId {
        let call = ToolCall::new(&self.step, tool_id, version, input, at(3));
        self.store.insert_tool_call(&call).unwrap();
        call.id()
    }

    fn executor(&self, registry: ToolRegistry) -> ToolExecutor {
        ToolExecutor::new(Arc::clone(&self.store), Arc::new(registry))
    }

    /// The whole log, paged through rather than truncated at one page.
    ///
    /// A page limit here would silently cap what a test can assert, which is
    /// exactly the mistake that hides a dropped progress event behind an
    /// assertion that counts to the limit and stops.
    fn events(&self) -> Vec<StoredEvent> {
        let mut all: Vec<StoredEvent> = Vec::new();
        loop {
            let after = all.last().map(|stored| stored.seq);
            let page = self.store.events(self.run.id(), after, 500).unwrap();
            if page.is_empty() {
                return all;
            }
            all.extend(page);
        }
    }

    /// Every event of one kind, in log order.
    fn events_of(&self, kind: &EventKind) -> Vec<StoredEvent> {
        self.events()
            .into_iter()
            .filter(|stored| stored.event.kind() == kind)
            .collect()
    }

    fn run_id(&self) -> RunId {
        self.run.id()
    }
}

/// A deterministic instant, `offset` seconds after a fixed epoch.
fn at(offset: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000 + offset).unwrap()
}

fn registry_of(tools: Vec<Arc<dyn super::ErasedTool>>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry.register_erased(tool).unwrap();
    }
    registry
}

fn erase<T: Tool + 'static>(tool: T) -> Arc<dyn super::ErasedTool> {
    super::erase(tool).unwrap()
}

fn metadata(id: &str, version: &str, risk: RiskLevel) -> ToolMetadata {
    ToolMetadata::new(
        ToolIdentity::parse(id, version).unwrap(),
        "Fixture tool",
        "A tool that exists to be executed by a test.",
        risk,
    )
}

// ---------------------------------------------------------------------------
// Fixture tools
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct Echoed {
    echoed: String,
}

/// Returns its input, and reports one progress event on the way.
struct Echo(&'static str);

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EchoInput {
    message: String,
}

impl Tool for Echo {
    type Input = EchoInput;
    type Output = Echoed;

    fn metadata(&self) -> ToolMetadata {
        metadata("fixture.echo", self.0, RiskLevel::Observe)
    }

    fn execute(
        &self,
        input: EchoInput,
        context: &mut ExecutionContext,
    ) -> Result<Echoed, ToolError> {
        context.report(ProgressEvent::stage("echoing"));
        context.report(ProgressEvent::counted(1, 1, ProgressUnit::Items));
        Ok(Echoed {
            echoed: input.message,
        })
    }
}

/// Panics, to prove the run survives one.
struct Panicking;

impl Tool for Panicking {
    type Input = Empty;
    type Output = Echoed;

    fn metadata(&self) -> ToolMetadata {
        metadata("fixture.panics", "1.0.0", RiskLevel::Observe)
    }

    fn execute(&self, _input: Empty, _context: &mut ExecutionContext) -> Result<Echoed, ToolError> {
        panic!("a deliberate panic from a fixture tool");
    }
}

/// Returns a value its own declared output schema refuses.
struct ContradictsItsSchema;

#[derive(Debug, JsonSchema)]
struct Contradiction;

impl Serialize for Contradiction {
    /// Serializes as an object the generated schema — a unit struct, and so
    /// `null` — cannot accept.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map([("smuggled", "value")])
    }
}

impl Tool for ContradictsItsSchema {
    type Input = Empty;
    type Output = Contradiction;

    fn metadata(&self) -> ToolMetadata {
        metadata("fixture.contradicts", "1.0.0", RiskLevel::Observe)
    }

    fn execute(
        &self,
        _input: Empty,
        _context: &mut ExecutionContext,
    ) -> Result<Contradiction, ToolError> {
        Ok(Contradiction)
    }
}

/// Polls its token between units of work, as a well-behaved tool must.
struct Cooperative {
    started: Arc<AtomicBool>,
    stopped_at: Arc<AtomicUsize>,
}

impl Tool for Cooperative {
    type Input = Empty;
    type Output = Echoed;

    fn metadata(&self) -> ToolMetadata {
        metadata("fixture.cooperative", "1.0.0", RiskLevel::Observe)
    }

    fn execute(&self, _input: Empty, context: &mut ExecutionContext) -> Result<Echoed, ToolError> {
        self.started.store(true, Ordering::Release);
        for unit in 0..100_000 {
            context.check_still_permitted().inspect_err(|_| {
                self.stopped_at.store(unit, Ordering::Release);
            })?;
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(Echoed {
            echoed: "never".to_owned(),
        })
    }
}

/// Never checks its token and never returns in time, as a bad tool does.
struct Unstoppable {
    started: Arc<AtomicBool>,
}

impl Tool for Unstoppable {
    type Input = Empty;
    type Output = Echoed;

    fn metadata(&self) -> ToolMetadata {
        // Declared limits are the tool's own; this one is short so the test does
        // not have to wait out a real timeout.
        metadata("fixture.unstoppable", "1.0.0", RiskLevel::Observe)
            .within(Duration::from_millis(80))
    }

    fn execute(&self, _input: Empty, _context: &mut ExecutionContext) -> Result<Echoed, ToolError> {
        self.started.store(true, Ordering::Release);
        std::thread::sleep(Duration::from_secs(30));
        Ok(Echoed {
            echoed: "never".to_owned(),
        })
    }
}

/// Emits more progress than any consumer could want, to observe backpressure.
struct Chatty {
    emitted: Arc<AtomicUsize>,
}

impl Tool for Chatty {
    type Input = Empty;
    type Output = Echoed;

    fn metadata(&self) -> ToolMetadata {
        metadata("fixture.chatty", "1.0.0", RiskLevel::Observe)
    }

    fn execute(&self, _input: Empty, context: &mut ExecutionContext) -> Result<Echoed, ToolError> {
        for index in 0..64 {
            context.report(ProgressEvent::message(format!("line {index}")));
            self.emitted.fetch_add(1, Ordering::AcqRel);
        }
        Ok(Echoed {
            echoed: "chatty".to_owned(),
        })
    }
}

// ---------------------------------------------------------------------------
// Successful execution and its record
// ---------------------------------------------------------------------------

#[test]
fn a_successful_call_is_recorded_with_its_output_and_its_events() {
    let fixture = Fixture::new();
    let executor = fixture.executor(registry_of(vec![erase(Echo("1.0.0"))]));
    let call = fixture.pending("fixture.echo", json!({"message": "hello"}));

    let completed = executor
        .execute(call, fixture.workspace.path(), &Cancellation::default())
        .unwrap();

    assert!(completed.outcome().succeeded());
    assert_eq!(completed.state(), ToolCallState::Succeeded);
    assert_eq!(
        completed.record().output(),
        Some(&json!({"echoed": "hello"}))
    );

    // The record handed back is the row the store committed, so a caller acting
    // on it and a caller re-reading it cannot see different things.
    let reloaded = fixture.store.load_tool_call(call).unwrap();
    assert_eq!(&reloaded, completed.record());

    // The log says how it got there: dispatched, progress, terminal.
    let states = fixture.events_of(&EventKind::ToolCallStateChanged);
    assert_eq!(states.len(), 2);
    assert_eq!(states[0].event.payload()["state"], json!("running"));
    assert_eq!(states[1].event.payload()["state"], json!("succeeded"));
    assert!(
        states[0].seq < states[1].seq,
        "the terminal event must follow the dispatch"
    );

    let progress = fixture.events_of(&EventKind::ToolProgress);
    assert_eq!(progress.len(), 2);
    assert_eq!(progress[0].event.payload()["event"], json!("stage"));
    assert_eq!(progress[1].event.payload()["completed"], json!(1));

    // Every event this call writes names both the call and the step. A consumer
    // rendering one step's timeline filters by `step_id`, so an event that omits
    // it would show the call starting and never finishing.
    for stored in states.iter().chain(&progress) {
        assert_eq!(stored.event.tool_call_id(), Some(call));
        assert_eq!(
            stored.event.step_id(),
            Some(fixture.step.id()),
            "an event of this call is missing from its step's timeline"
        );
    }
}

#[test]
fn the_terminal_state_and_its_events_are_readable_before_the_caller_sees_the_outcome() {
    // The ordering the whole module rests on. A separate reader — its own
    // connection, opened after the fact — sees the terminal row and its event
    // the instant `execute` has returned, because the commit precedes the
    // return rather than following it.
    let fixture = Fixture::new();
    let executor = fixture.executor(registry_of(vec![erase(Echo("1.0.0"))]));
    let call = fixture.pending("fixture.echo", json!({"message": "ordered"}));

    let completed = executor
        .execute(call, fixture.workspace.path(), &Cancellation::default())
        .unwrap();

    let observer = Store::open(fixture._data_dir.path()).unwrap();
    let observed = observer.load_tool_call(call).unwrap();
    assert_eq!(observed.state(), ToolCallState::Succeeded);
    assert_eq!(observed.output(), completed.record().output());

    let tail = observer.events(fixture.run_id(), None, 200).unwrap();
    let terminal = tail
        .iter()
        .rev()
        .find(|stored| stored.event.kind() == &EventKind::ToolCallStateChanged)
        .expect("a terminal state event");
    assert_eq!(terminal.event.payload()["state"], json!("succeeded"));
}

#[test]
fn an_unpinned_call_records_the_version_that_actually_ran() {
    let fixture = Fixture::new();
    let executor = fixture.executor(registry_of(vec![
        erase(Echo("1.0.0")),
        erase(Echo("1.10.0")),
        erase(Echo("2.0.0-rc.1")),
    ]));
    let call = fixture.pending("fixture.echo", json!({"message": "unpinned"}));

    let completed = executor
        .execute(call, fixture.workspace.path(), &Cancellation::default())
        .unwrap();

    // The highest *stable* version, and it is written to the row rather than
    // left for a second resolution that could disagree.
    assert_eq!(
        completed.tool().map(ToString::to_string),
        Some("fixture.echo@1.10.0".to_owned())
    );
    assert_eq!(completed.record().tool_version(), "1.10.0");
    assert_eq!(
        fixture.store.load_tool_call(call).unwrap().tool_version(),
        "1.10.0"
    );

    // And the dispatch event names it, so the log is legible without joining.
    let dispatched = &fixture.events_of(&EventKind::ToolCallStateChanged)[0];
    assert_eq!(dispatched.event.payload()["tool_version"], json!("1.10.0"));
}

#[test]
fn a_pinned_call_runs_the_version_it_named() {
    let fixture = Fixture::new();
    let executor = fixture.executor(registry_of(vec![
        erase(Echo("1.0.0")),
        erase(Echo("2.0.0")),
    ]));
    let call = fixture.pending_version("fixture.echo", "1.0.0", json!({"message": "pinned"}));

    let completed = executor
        .execute(call, fixture.workspace.path(), &Cancellation::default())
        .unwrap();

    assert_eq!(completed.record().tool_version(), "1.0.0");
}

// ---------------------------------------------------------------------------
// Failure, panic, and refused output
// ---------------------------------------------------------------------------

#[test]
fn a_panicking_tool_becomes_a_structured_failure_and_the_run_survives() {
    let fixture = Fixture::new();
    let executor = fixture.executor(registry_of(vec![erase(Panicking), erase(Echo("1.0.0"))]));
    let panics = fixture.pending("fixture.panics", json!({}));

    let completed = executor
        .execute(panics, fixture.workspace.path(), &Cancellation::default())
        .unwrap();

    assert_eq!(completed.state(), ToolCallState::Failed);
    assert_eq!(completed.outcome().failure_kind(), Some("tool_panicked"));
    let failure = completed.record().failure().expect("a recorded failure");
    assert_eq!(failure.kind(), "tool_panicked");
    assert!(
        failure.message().contains("a deliberate panic"),
        "{}",
        failure.message()
    );

    // The point of containment: the run is untouched and the *next* call on the
    // same executor, store and step still works.
    let echo = fixture.pending("fixture.echo", json!({"message": "after the panic"}));
    let survived = executor
        .execute(echo, fixture.workspace.path(), &Cancellation::default())
        .unwrap();
    assert!(survived.outcome().succeeded());
}

#[test]
fn tool_output_that_violates_its_schema_is_stored_as_evidence_and_never_as_a_result() {
    let fixture = Fixture::new();
    let executor = fixture.executor(registry_of(vec![erase(ContradictsItsSchema)]));
    let call = fixture.pending("fixture.contradicts", json!({}));

    let completed = executor
        .execute(call, fixture.workspace.path(), &Cancellation::default())
        .unwrap();

    assert_eq!(completed.state(), ToolCallState::Failed);
    assert_eq!(completed.outcome().failure_kind(), Some("invalid_output"));
    assert_eq!(
        completed.record().output(),
        None,
        "a refused result must never be delivered as one"
    );

    // The value itself is not discarded: it is the only thing that says what the
    // tool actually produced, so it goes where untrusted non-result bytes go.
    let rejected = fixture
        .store
        .run_artifacts(fixture.run_id())
        .unwrap()
        .into_iter()
        .find(|artifact| artifact.name() == REJECTED_OUTPUT_ARTIFACT)
        .expect("the refused output should be preserved as an artifact");
    assert_eq!(rejected.tool_call_id(), Some(call));
    assert_eq!(
        fixture.store.read_artifact(rejected.id()).unwrap(),
        br#"{"smuggled":"value"}"#
    );
}

#[test]
fn an_approved_call_runs_and_records_the_decision_beside_the_version_it_authorized() {
    // Approval-gated work never passes through `pending` on its way to running:
    // the domain resumes a held call *by* its decision. So the decision, the
    // version it authorized, and the start are one step — an audit that could
    // read an approval beside a version the approver never saw would not be an
    // audit of anything.
    let fixture = Fixture::new();
    let executor = fixture.executor(registry_of(vec![
        erase(Echo("1.0.0")),
        erase(Echo("1.10.0")),
    ]));
    // Recorded without a version, so the approval is what pins it.
    let call = fixture.pending("fixture.echo", json!({"message": "approved"}));
    fixture
        .store
        .transition_tool_call(call, ToolCallState::AwaitingApproval, at(4))
        .unwrap();

    let completed = executor
        .execute_approved(
            call,
            "taiye@example.com",
            fixture.workspace.path(),
            &Cancellation::default(),
        )
        .unwrap();

    assert!(completed.outcome().succeeded(), "{completed:?}");
    assert_eq!(completed.record().tool_version(), "1.10.0");

    let approvals = completed.record().approvals();
    assert_eq!(approvals.len(), 1);
    assert_eq!(approvals[0].decided_by(), "taiye@example.com");
    assert_eq!(
        approvals[0].decision(),
        crate::domain::ApprovalDecision::Approved
    );

    // Both halves survive the reload, so the audit trail is durable and not a
    // projection assembled in memory.
    let stored = fixture.store.load_tool_call(call).unwrap();
    assert_eq!(stored.tool_version(), "1.10.0");
    assert_eq!(stored.approvals().len(), 1);

    // And the timeline says why a call that was waiting is suddenly running.
    let dispatched = &fixture.events_of(&EventKind::ToolCallStateChanged)[0];
    assert_eq!(
        dispatched.event.payload()["approved_by"],
        json!("taiye@example.com")
    );
    assert_eq!(dispatched.event.payload()["tool_version"], json!("1.10.0"));
}

#[test]
fn an_unavailable_workspace_terminalizes_a_pending_call() {
    let fixture = Fixture::new();
    let executor = fixture.executor(registry_of(vec![erase(Echo("1.0.0"))]));
    let call = fixture.pending("fixture.echo", json!({"message": "never runs"}));
    let missing = fixture.workspace.path().join("missing");

    let completed = executor
        .execute(call, &missing, &Cancellation::default())
        .unwrap();

    assert_eq!(completed.state(), ToolCallState::Failed);
    assert_eq!(completed.outcome().failure_kind(), Some("root_unavailable"));
    assert_eq!(
        fixture.store.load_tool_call(call).unwrap().state(),
        ToolCallState::Failed
    );
}

#[test]
fn an_unavailable_workspace_still_records_an_approval_and_terminal_failure() {
    let fixture = Fixture::new();
    let executor = fixture.executor(registry_of(vec![erase(Echo("1.0.0"))]));
    let call = fixture.pending("fixture.echo", json!({"message": "never runs"}));
    fixture
        .store
        .transition_tool_call(call, ToolCallState::AwaitingApproval, at(4))
        .unwrap();
    let missing = fixture.workspace.path().join("missing");

    let completed = executor
        .execute_approved(
            call,
            "taiye@example.com",
            &missing,
            &Cancellation::default(),
        )
        .unwrap();

    assert_eq!(completed.state(), ToolCallState::Failed);
    assert_eq!(completed.outcome().failure_kind(), Some("root_unavailable"));
    assert_eq!(completed.record().approvals().len(), 1);
    assert_eq!(completed.record().tool_version(), "1.0.0");
}

#[test]
fn each_entry_point_admits_exactly_one_state() {
    let fixture = Fixture::new();
    let executor = fixture.executor(registry_of(vec![erase(Echo("1.0.0"))]));

    // A call waiting on a decision is not one the ungated path may start: doing
    // so would run work a human was asked about and never answered.
    let waiting = fixture.pending("fixture.echo", json!({"message": "waiting"}));
    fixture
        .store
        .transition_tool_call(waiting, ToolCallState::AwaitingApproval, at(4))
        .unwrap();
    let error = executor
        .execute(waiting, fixture.workspace.path(), &Cancellation::default())
        .unwrap_err();
    assert_eq!(error.kind(), "not_dispatchable");
    assert!(error.to_string().contains("requires pending"), "{error}");

    // And a decision cannot be recorded against a call nobody asked about.
    let ungated = fixture.pending("fixture.echo", json!({"message": "ungated"}));
    let error = executor
        .execute_approved(
            ungated,
            "taiye@example.com",
            fixture.workspace.path(),
            &Cancellation::default(),
        )
        .unwrap_err();
    assert_eq!(error.kind(), "not_dispatchable");
    assert!(
        error.to_string().contains("requires awaiting_approval"),
        "{error}"
    );

    // Neither leaves a mark: both calls are exactly where they were.
    assert_eq!(
        fixture.store.load_tool_call(waiting).unwrap().state(),
        ToolCallState::AwaitingApproval
    );
    let untouched = fixture.store.load_tool_call(ungated).unwrap();
    assert_eq!(untouched.state(), ToolCallState::Pending);
    assert!(untouched.approvals().is_empty());
}

#[test]
fn a_blank_approver_is_refused_without_starting_the_work() {
    // An approval history whose decider cannot be identified is not an audit
    // trail, and refusing after the body ran would be worse than useless.
    let fixture = Fixture::new();
    let executor = fixture.executor(registry_of(vec![erase(Echo("1.0.0"))]));
    let call = fixture.pending("fixture.echo", json!({"message": "anonymous"}));
    fixture
        .store
        .transition_tool_call(call, ToolCallState::AwaitingApproval, at(4))
        .unwrap();

    let error = executor
        .execute_approved(
            call,
            "  ",
            fixture.workspace.path(),
            &Cancellation::default(),
        )
        .unwrap_err();

    assert_eq!(error.kind(), "store_failed");
    let untouched = fixture.store.load_tool_call(call).unwrap();
    assert_eq!(untouched.state(), ToolCallState::AwaitingApproval);
    assert!(untouched.approvals().is_empty());
    assert!(fixture.events().is_empty(), "a refused start wrote history");
}

#[test]
fn a_worker_that_dies_without_reporting_is_recorded_interrupted() {
    // `ToolCallState::Interrupted` exists for "the owning process stopped before
    // the invocation completed", and this is the only path that reaches it —
    // a panicking *tool* is contained by the pipeline and reports a failure like
    // any other. Recording it as a failure with an `interrupted` kind would mean
    // a consumer filtering on the state never saw one.
    let fixture = Fixture::new();
    let executor = fixture.executor(registry_of(vec![erase(Echo("1.0.0"))]));
    let call = fixture.pending("fixture.echo", json!({"message": "orphaned"}));

    let running = fixture
        .store
        .dispatch_tool_call_with_event(
            call,
            "1.0.0",
            at(4),
            crate::store::RunEvent::new(EventKind::ToolCallStateChanged, at(4)).for_tool_call(call),
        )
        .unwrap()
        .0;
    assert_eq!(running.state(), ToolCallState::Running);

    // Driven through the executor's own recording path rather than by
    // arranging a dead worker thread, which nothing can do deterministically.
    let completed = executor
        .finish(call, fixture.step.id(), None, CallOutcome::Interrupted)
        .unwrap();

    assert_eq!(completed.state(), ToolCallState::Interrupted);
    assert_eq!(completed.outcome(), &CallOutcome::Interrupted);
    assert_eq!(
        fixture.store.load_tool_call(call).unwrap().state(),
        ToolCallState::Interrupted
    );
}

#[test]
fn a_result_too_large_to_store_fails_the_call_rather_than_stranding_it() {
    // The failure mode the store's inline bound creates and this executor has to
    // absorb: refusing the write and returning an error would leave the call in
    // `running` for ever, with nothing recorded about why it never finished.
    struct Enormous;

    #[derive(Debug, Deserialize, JsonSchema, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Bulk {
        text: String,
    }

    impl Tool for Enormous {
        type Input = Empty;
        type Output = Bulk;

        fn metadata(&self) -> ToolMetadata {
            metadata("fixture.enormous", "1.0.0", RiskLevel::Observe)
        }

        fn execute(
            &self,
            _input: Empty,
            _context: &mut ExecutionContext,
        ) -> Result<Bulk, ToolError> {
            Ok(Bulk {
                text: "x".repeat(crate::store::MAX_INLINE_PAYLOAD_BYTES + 1),
            })
        }
    }

    let fixture = Fixture::new();
    let executor = fixture.executor(registry_of(vec![erase(Enormous)]));
    let call = fixture.pending("fixture.enormous", json!({}));

    let completed = executor
        .execute(call, fixture.workspace.path(), &Cancellation::default())
        .unwrap();

    assert_eq!(completed.state(), ToolCallState::Failed);
    assert_eq!(
        completed.outcome().failure_kind(),
        Some("payload_too_large")
    );
    assert!(
        completed
            .record()
            .failure()
            .unwrap()
            .message()
            .contains("belongs in an artifact"),
        "the failure should say what to do instead"
    );
    // And it is a terminal record, readable like any other.
    assert_eq!(
        fixture.store.load_tool_call(call).unwrap().state(),
        ToolCallState::Failed
    );
}

#[test]
fn a_call_naming_a_tool_that_does_not_exist_fails_without_ever_running() {
    let fixture = Fixture::new();
    let executor = fixture.executor(registry_of(vec![erase(Echo("1.0.0"))]));
    let call = fixture.pending("fixture.absent", json!({}));

    let completed = executor
        .execute(call, fixture.workspace.path(), &Cancellation::default())
        .unwrap();

    assert_eq!(completed.state(), ToolCallState::Failed);
    assert_eq!(completed.outcome().failure_kind(), Some("unknown_tool"));
    assert_eq!(completed.tool(), None, "nothing was resolved to attempt");

    // It never reached `running`, so the log holds exactly one state event.
    let states = fixture.events_of(&EventKind::ToolCallStateChanged);
    assert_eq!(states.len(), 1);
    assert_eq!(states[0].event.payload()["state"], json!("failed"));
}

#[test]
fn only_a_pending_call_can_be_dispatched() {
    let fixture = Fixture::new();
    let executor = fixture.executor(registry_of(vec![erase(Echo("1.0.0"))]));
    let call = fixture.pending("fixture.echo", json!({"message": "once"}));

    executor
        .execute(call, fixture.workspace.path(), &Cancellation::default())
        .unwrap();

    // Re-executing a finished call would either duplicate its side effects or
    // overwrite the record of what happened; both are worse than a refusal.
    let error = executor
        .execute(call, fixture.workspace.path(), &Cancellation::default())
        .unwrap_err();
    assert_eq!(error.kind(), "not_dispatchable");
    assert!(error.to_string().contains("succeeded"), "{error}");
}

#[test]
fn an_input_its_schema_refuses_fails_the_call_without_running_the_body() {
    let fixture = Fixture::new();
    let executor = fixture.executor(registry_of(vec![erase(Echo("1.0.0"))]));
    let call = fixture.pending("fixture.echo", json!({"message": 7}));

    let completed = executor
        .execute(call, fixture.workspace.path(), &Cancellation::default())
        .unwrap();

    assert_eq!(completed.outcome().failure_kind(), Some("invalid_input"));
    // The call did reach `running` — resolution succeeded, so the version that
    // was going to run is recorded — and then failed at the gate.
    assert_eq!(completed.record().tool_version(), "1.0.0");
}

// ---------------------------------------------------------------------------
// Cancellation and timeouts
// ---------------------------------------------------------------------------

#[test]
fn cancelling_stops_a_cooperative_tool_and_records_cancelled() {
    let fixture = Fixture::new();
    let started = Arc::new(AtomicBool::new(false));
    let stopped_at = Arc::new(AtomicUsize::new(usize::MAX));
    let executor = fixture.executor(registry_of(vec![erase(Cooperative {
        started: Arc::clone(&started),
        stopped_at: Arc::clone(&stopped_at),
    })]));
    let call = fixture.pending("fixture.cooperative", json!({}));

    let cancellation = Cancellation::default();
    let watcher = cancellation.clone();
    let started_watch = Arc::clone(&started);
    std::thread::spawn(move || {
        while !started_watch.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
        watcher.cancel();
    });

    let began = Instant::now();
    let completed = executor
        .execute(call, fixture.workspace.path(), &cancellation)
        .unwrap();
    let elapsed = began.elapsed();

    assert_eq!(completed.outcome(), &CallOutcome::Cancelled);
    assert_eq!(completed.state(), ToolCallState::Cancelled);
    assert!(
        stopped_at.load(Ordering::Acquire) < 100_000,
        "the body should have stopped at one of its own checks"
    );
    // Well inside the grace period: a cooperative body returns on its own, so
    // the executor never has to abandon it.
    assert!(
        elapsed < TERMINATION_GRACE + Duration::from_secs(5),
        "cancelling took {elapsed:?}"
    );

    let states = fixture.events_of(&EventKind::ToolCallStateChanged);
    assert_eq!(states.last().unwrap().event.payload()["state"], "cancelled");
}

#[test]
fn a_tool_dispatched_after_cancellation_never_starts() {
    let fixture = Fixture::new();
    let started = Arc::new(AtomicBool::new(false));
    let stopped_at = Arc::new(AtomicUsize::new(usize::MAX));
    let executor = fixture.executor(registry_of(vec![erase(Cooperative {
        started: Arc::clone(&started),
        stopped_at,
    })]));
    let call = fixture.pending("fixture.cooperative", json!({}));

    let cancellation = Cancellation::default();
    cancellation.cancel();

    let completed = executor
        .execute(call, fixture.workspace.path(), &cancellation)
        .unwrap();

    assert_eq!(completed.outcome(), &CallOutcome::Cancelled);
    assert!(
        !started.load(Ordering::Acquire),
        "the body ran despite the call having been cancelled before dispatch"
    );
}

#[test]
fn a_tool_that_ignores_its_token_is_abandoned_and_the_call_still_ends() {
    // The guarantee the worker thread exists for. Rust cannot kill a thread, so
    // a body that neither returns nor polls is abandoned — but the *call* still
    // reaches a terminal state, which is what a run and its history depend on.
    let fixture = Fixture::new();
    let started = Arc::new(AtomicBool::new(false));
    let executor = fixture.executor(registry_of(vec![erase(Unstoppable {
        started: Arc::clone(&started),
    })]));
    let call = fixture.pending("fixture.unstoppable", json!({}));

    let began = Instant::now();
    let completed = executor
        .execute(call, fixture.workspace.path(), &Cancellation::default())
        .unwrap();
    let elapsed = began.elapsed();

    assert!(started.load(Ordering::Acquire), "the body never ran");
    assert_eq!(
        completed.outcome(),
        &CallOutcome::TimedOut {
            limit: Duration::from_millis(80)
        }
    );
    // A timeout has no lifecycle state of its own; it is a failure whose kind
    // says what happened, so a consumer branches on the kind rather than on a
    // state the domain would have had to grow.
    assert_eq!(completed.state(), ToolCallState::Failed);
    assert_eq!(
        completed.record().failure().map(|failure| failure.kind()),
        Some("timed_out")
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "the executor waited for the abandoned body: {elapsed:?}"
    );

    let states = fixture.events_of(&EventKind::ToolCallStateChanged);
    let terminal = states.last().unwrap();
    assert_eq!(
        terminal.event.payload()["detail"]["kind"],
        json!("timed_out")
    );
    assert_eq!(terminal.event.payload()["detail"]["timeout_ms"], json!(80));
}

#[test]
fn a_caller_may_tighten_a_declared_limit() {
    let fixture = Fixture::new();
    let started = Arc::new(AtomicBool::new(false));
    let executor = fixture
        .executor(registry_of(vec![erase(Cooperative {
            started: Arc::clone(&started),
            stopped_at: Arc::new(AtomicUsize::new(usize::MAX)),
        })]))
        .with_limits(ExecutionLimits::default().within(Duration::from_millis(60)));
    let call = fixture.pending("fixture.cooperative", json!({}));

    let completed = executor
        .execute(call, fixture.workspace.path(), &Cancellation::default())
        .unwrap();

    assert_eq!(
        completed.outcome(),
        &CallOutcome::TimedOut {
            limit: Duration::from_millis(60)
        }
    );
}

#[test]
fn a_timeout_does_not_cancel_the_callers_token_for_every_later_call() {
    // `Cancellation` latches and has no reset, so an executor that enforced its
    // deadline by cancelling the *caller's* token would leave it cancelled for
    // good: one slow step would silently cancel the rest of a run that shares
    // it, and every later call would be recorded `cancelled` with nobody having
    // cancelled anything. This is the shape #97's coordinator has.
    let fixture = Fixture::new();
    let executor = fixture.executor(registry_of(vec![
        erase(Unstoppable {
            started: Arc::new(AtomicBool::new(false)),
        }),
        erase(Echo("1.0.0")),
    ]));
    let run_token = Cancellation::default();

    let slow = fixture.pending("fixture.unstoppable", json!({}));
    let timed_out = executor
        .execute(slow, fixture.workspace.path(), &run_token)
        .unwrap();
    assert!(matches!(timed_out.outcome(), CallOutcome::TimedOut { .. }));

    assert!(
        !run_token.is_cancelled(),
        "the executor cancelled the token its caller owns"
    );

    let next = fixture.pending("fixture.echo", json!({"message": "still running"}));
    let after = executor
        .execute(next, fixture.workspace.path(), &run_token)
        .unwrap();
    assert!(
        after.outcome().succeeded(),
        "a later call sharing the token was stopped by an earlier timeout: {after:?}"
    );
}

#[test]
fn a_caller_may_not_remove_a_limit_the_tool_declared() {
    let fixture = Fixture::new();
    let executor = fixture
        .executor(registry_of(vec![erase(Echo("1.0.0"))]))
        .with_limits(ExecutionLimits::default().bounded_only_by_cancellation());
    let call = fixture.pending("fixture.echo", json!({"message": "unbounded"}));

    let error = executor
        .execute(call, fixture.workspace.path(), &Cancellation::default())
        .unwrap_err();

    assert_eq!(error.kind(), "unbounded_not_declared");
    // Refused before anything was written: the call is still dispatchable once
    // the caller asks for limits the tool accepts.
    assert_eq!(
        fixture.store.load_tool_call(call).unwrap().state(),
        ToolCallState::Pending
    );
}

// ---------------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------------

#[test]
fn progress_backpressure_blocks_the_producer_instead_of_buffering() {
    // The channel is the tool's rate limiter, not its buffer. With nobody
    // draining, a producer must stop at the capacity rather than queue past it.
    let (sender, receiver) = progress_channel(2);
    let emitted = Arc::new(AtomicUsize::new(0));

    let counter = Arc::clone(&emitted);
    let producer = std::thread::spawn(move || {
        let mut sink = sender;
        for index in 0..100 {
            super::ProgressSink::emit(&mut sink, ProgressEvent::message(format!("line {index}")));
            counter.fetch_add(1, Ordering::AcqRel);
        }
    });

    // Two in the channel plus at most one parked in `send`, whatever the
    // producer does with the rest of its budget.
    std::thread::sleep(Duration::from_millis(100));
    let queued = emitted.load(Ordering::Acquire);
    assert!(
        queued <= 3,
        "an unread channel accepted {queued} events, so it is buffering rather than blocking"
    );

    // Draining releases the producer — which starts refilling the channel as it
    // is read, so what a drain returns is "everything queued as it went", not a
    // number this test may pin. Dropping the receiver releases the producer for
    // good: a consumer that gave up must not strand a running tool.
    assert!(
        !receiver.drain().is_empty(),
        "the events the producer was blocked behind should be readable"
    );
    drop(receiver);
    producer.join().unwrap();
    assert_eq!(emitted.load(Ordering::Acquire), 100);
}

#[test]
fn every_progress_event_a_tool_reports_reaches_the_run_log() {
    let fixture = Fixture::new();
    let emitted = Arc::new(AtomicUsize::new(0));
    let executor = fixture
        .executor(registry_of(vec![erase(Chatty {
            emitted: Arc::clone(&emitted),
        })]))
        // Deliberately far smaller than the burst, so the call only completes if
        // the executor keeps draining while the tool is blocked on the channel.
        .with_limits(ExecutionLimits::default().with_progress_capacity(2));
    let call = fixture.pending("fixture.chatty", json!({}));

    let completed = executor
        .execute(call, fixture.workspace.path(), &Cancellation::default())
        .unwrap();

    assert!(completed.outcome().succeeded());
    assert_eq!(emitted.load(Ordering::Acquire), 64);
    assert_eq!(fixture.events_of(&EventKind::ToolProgress).len(), 64);
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

#[test]
fn the_executors_stream_bound_is_what_a_tool_sees() {
    // The bound is configured on the executor and enforced by the tool, so the
    // two have to be connected: a limit a body never receives is not a limit.
    let fixture = Fixture::new();
    let observed = Arc::new(AtomicUsize::new(0));

    struct Reports(Arc<AtomicUsize>);

    impl Tool for Reports {
        type Input = Empty;
        type Output = Echoed;

        fn metadata(&self) -> ToolMetadata {
            metadata("fixture.reports", "1.0.0", RiskLevel::Observe)
        }

        fn execute(
            &self,
            _input: Empty,
            context: &mut ExecutionContext,
        ) -> Result<Echoed, ToolError> {
            self.0.store(context.stream_tail_bytes(), Ordering::Release);
            Ok(Echoed {
                echoed: context.deadline().map_or_else(
                    || "unbounded".to_owned(),
                    |deadline| format!("{:?}", deadline.limit()),
                ),
            })
        }
    }

    let executor = fixture
        .executor(registry_of(vec![erase(Reports(Arc::clone(&observed)))]))
        .with_limits(ExecutionLimits::default().retaining_stream_tail(4_096));
    let call = fixture.pending("fixture.reports", json!({}));

    let completed = executor
        .execute(call, fixture.workspace.path(), &Cancellation::default())
        .unwrap();

    assert_eq!(observed.load(Ordering::Acquire), 4_096);
    assert_ne!(
        DEFAULT_STREAM_TAIL_BYTES, 4_096,
        "the test must not pass by default"
    );
    // The deadline reached the body too, so a tool supervising a child can stop
    // it at the call's limit rather than inventing one.
    assert_eq!(completed.record().output(), Some(&json!({"echoed": "30s"})));
}

// ---------------------------------------------------------------------------
// Supervised child processes
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod processes {
    use std::io::Write;
    use std::path::PathBuf;

    use harkness_test_fixtures::Fixture as ShimFixture;

    use super::*;
    use crate::tool::EnvironmentName;
    use crate::tool::{Capture, ProcessOutput, ToolProcess};
    use crate::trust::{AllowlistedEnv, CommandSpec};

    /// A tool that runs one shim and reports what it produced.
    ///
    /// The shim path and the capture arrive through the input rather than
    /// through the tool, so one fixture tool covers every process case.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct RunInput {
        program: String,
        #[serde(default)]
        capture_stdout: bool,
    }

    #[derive(Debug, Deserialize, JsonSchema, Serialize)]
    #[serde(deny_unknown_fields)]
    struct RunOutput {
        code: Option<i32>,
        stdout_bytes: u64,
        stderr_tail: String,
        stdout_artifact: Option<super::super::ArtifactRef>,
    }

    struct RunsAShim(Duration);

    impl Tool for RunsAShim {
        type Input = RunInput;
        type Output = RunOutput;

        fn metadata(&self) -> ToolMetadata {
            metadata("fixture.runs", "1.0.0", RiskLevel::Execute).within(self.0)
        }

        fn execute(
            &self,
            input: RunInput,
            context: &mut ExecutionContext,
        ) -> Result<RunOutput, ToolError> {
            let cwd = context.resolve(".")?;
            let env = AllowlistedEnv::build(std::iter::empty::<&EnvironmentName>());
            let spec = CommandSpec::new(&input.program, Vec::new(), cwd, env)
                .map_err(ToolError::execution_failed)?;
            let mut process = ToolProcess::new(spec).capture_stderr(Capture::Tail);
            if input.capture_stdout {
                process = process.capture_stdout(Capture::artifact("stdout.log"));
            }
            let output: ProcessOutput = process.run(context)?.require_success()?;
            let tail = output.stderr().tail();
            Ok(RunOutput {
                code: output.code(),
                stdout_bytes: output.stdout().byte_len(),
                // A *result* is held to the store's inline bound, and the
                // retained tail is up to the executor's stream limit — which can
                // be larger. A real tool summarizes; this one takes the last few
                // hundred bytes, which is what the fixtures assert on anyway.
                stderr_tail: tail[tail.len().saturating_sub(512)..].to_owned(),
                stdout_artifact: output.stdout().artifact().cloned(),
            })
        }
    }

    fn shim(fixture: &ShimFixture, name: &str, script: &str) -> String {
        fixture.shim(name, script).display().to_string()
    }

    #[test]
    fn streamed_stdout_lands_in_an_artifact_while_memory_stays_bounded() {
        const EMITTED: u64 = 1024 * 1024;

        let fixture = Fixture::new();
        let shims = ShimFixture::new();
        let flooding = shim(
            &shims,
            "flooding",
            &format!("#!/bin/sh\nyes harkness-stdout | head -c {EMITTED}\n"),
        );

        let executor = fixture
            .executor(registry_of(vec![erase(RunsAShim(Duration::from_secs(
                120,
            )))]))
            .with_limits(ExecutionLimits::default().retaining_stream_tail(1_024));
        let call = fixture.pending(
            "fixture.runs",
            json!({"program": flooding, "capture_stdout": true}),
        );

        let completed = executor
            .execute(call, fixture.workspace.path(), &Cancellation::default())
            .unwrap();

        assert!(completed.outcome().succeeded(), "{completed:?}");
        let output = completed.record().output().unwrap();
        assert_eq!(output["stdout_bytes"], json!(EMITTED));

        // The artifact holds every byte, and its recorded size agrees with what
        // the stream reported — the two are computed independently.
        let reference = &output["stdout_artifact"];
        assert_eq!(reference["byte_len"], json!(EMITTED));
        let stored = fixture
            .store
            .run_artifacts(fixture.run_id())
            .unwrap()
            .into_iter()
            .find(|artifact| artifact.name() == "stdout.log")
            .expect("the streamed artifact");
        assert_eq!(stored.byte_size(), EMITTED);
        assert_eq!(stored.tool_call_id(), Some(call));
        assert_eq!(
            std::fs::metadata(
                PathBuf::from(fixture._data_dir.path())
                    .join("artifacts")
                    .join(fixture.run_id().to_string())
                    .join(stored.id().to_string())
            )
            .unwrap()
            .len(),
            EMITTED,
            "the file on disk must be the size the row claims"
        );
    }

    #[test]
    fn a_nonzero_exit_becomes_process_failed_with_a_bounded_stderr_tail() {
        let fixture = Fixture::new();
        let shims = ShimFixture::new();
        // Far more diagnostic than the tail holds, with the part that matters
        // last — which is where a program puts its diagnosis.
        let refusing = shim(
            &shims,
            "refusing",
            "#!/bin/sh\n\
             i=0\n\
             while [ $i -lt 500 ]; do echo \"progress line $i\" >&2; i=$((i+1)); done\n\
             echo 'fatal: the shim refused' >&2\n\
             exit 3\n",
        );

        let executor = fixture
            .executor(registry_of(vec![erase(RunsAShim(Duration::from_secs(
                120,
            )))]))
            .with_limits(ExecutionLimits::default().retaining_stream_tail(256));
        let call = fixture.pending("fixture.runs", json!({"program": refusing}));

        let completed = executor
            .execute(call, fixture.workspace.path(), &Cancellation::default())
            .unwrap();

        assert_eq!(completed.outcome().failure_kind(), Some("process_failed"));
        let failure = completed.record().failure().unwrap();
        assert!(
            failure.message().contains("status 3"),
            "{}",
            failure.message()
        );
        assert!(
            failure.message().contains("fatal: the shim refused"),
            "the diagnosis a program prints last must survive the tail: {}",
            failure.message()
        );
        assert!(
            failure.message().len() < 2_048,
            "the tail is unbounded: {} bytes",
            failure.message().len()
        );
    }

    #[test]
    fn a_stopped_child_keeps_the_output_it_had_already_produced() {
        // A build log is at its most useful exactly when the build was killed.
        // An artifact stream that is dropped rather than finished deletes the
        // bytes it staged, so a timed-out capture would destroy the one thing a
        // user needs to find out why it was slow.
        let fixture = Fixture::new();
        let shims = ShimFixture::new();
        let talkative = shim(
            &shims,
            "talks-then-hangs",
            "#!/bin/sh\n\
             echo 'compiling everything'\n\
             echo 'still going'\n\
             sleep 30\n",
        );

        let executor = fixture.executor(registry_of(vec![erase(RunsAShim(
            Duration::from_millis(300),
        ))]));
        let call = fixture.pending(
            "fixture.runs",
            json!({"program": talkative, "capture_stdout": true}),
        );

        let completed = executor
            .execute(call, fixture.workspace.path(), &Cancellation::default())
            .unwrap();

        assert!(matches!(completed.outcome(), CallOutcome::TimedOut { .. }));

        let log = fixture
            .store
            .run_artifacts(fixture.run_id())
            .unwrap()
            .into_iter()
            .find(|artifact| artifact.name() == "stdout.log")
            .expect("the partial capture should survive the kill");
        assert_eq!(
            fixture.store.read_artifact(log.id()).unwrap(),
            b"compiling everything\nstill going\n"
        );
    }

    #[test]
    fn a_hanging_child_is_killed_at_its_timeout_with_its_whole_process_group() {
        let fixture = Fixture::new();
        let shims = ShimFixture::new();
        let activity = shims.root.path().join("helper-activity");
        // The helper is the point: killing the leader alone leaves it running,
        // and the file is the only evidence available once the group is gone.
        let hanging = shim(
            &shims,
            "hanging",
            &format!(
                "#!/bin/sh\n\
                 (while true; do printf x >> '{}'; sleep 0.01; done) 2>/dev/null &\n\
                 echo ready >&2\n\
                 wait\n",
                activity.display()
            ),
        );

        let executor = fixture.executor(registry_of(vec![erase(RunsAShim(
            Duration::from_millis(300),
        ))]));
        let call = fixture.pending("fixture.runs", json!({"program": hanging}));

        let began = Instant::now();
        let completed = executor
            .execute(call, fixture.workspace.path(), &Cancellation::default())
            .unwrap();

        assert_eq!(
            completed.outcome(),
            &CallOutcome::TimedOut {
                limit: Duration::from_millis(300)
            }
        );
        assert!(began.elapsed() < Duration::from_secs(10));

        let at_timeout = std::fs::read(&activity).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(
            std::fs::read(&activity).unwrap(),
            at_timeout,
            "a helper survived the timeout, so only the group leader was killed"
        );
    }

    #[test]
    fn cancellation_kills_the_child_process_group_and_records_cancelled() {
        let fixture = Fixture::new();
        let shims = ShimFixture::new();
        let activity = shims.root.path().join("cancel-activity");
        let hanging = shim(
            &shims,
            "cancelled",
            &format!(
                "#!/bin/sh\n\
                 (while true; do printf x >> '{}'; sleep 0.01; done) 2>/dev/null &\n\
                 echo ready >&2\n\
                 wait\n",
                activity.display()
            ),
        );

        let executor = fixture.executor(registry_of(vec![erase(RunsAShim(Duration::from_secs(
            120,
        )))]));
        let call = fixture.pending("fixture.runs", json!({"program": hanging}));

        let cancellation = Cancellation::default();
        let watcher = cancellation.clone();
        let watched = activity.clone();
        std::thread::spawn(move || {
            // Cancel once the child has provably started, so the test measures
            // stopping a running process rather than refusing to start one.
            while !watched.exists() {
                std::thread::sleep(Duration::from_millis(2));
            }
            watcher.cancel();
        });

        let began = Instant::now();
        let completed = executor
            .execute(call, fixture.workspace.path(), &cancellation)
            .unwrap();
        let elapsed = began.elapsed();

        assert_eq!(completed.outcome(), &CallOutcome::Cancelled);
        assert_eq!(completed.state(), ToolCallState::Cancelled);
        assert!(
            elapsed < Duration::from_secs(10),
            "cancelling took {elapsed:?}"
        );

        let at_cancel = std::fs::read(&activity).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(
            std::fs::read(&activity).unwrap(),
            at_cancel,
            "a helper survived cancellation"
        );
    }

    #[test]
    fn a_child_reports_its_stderr_as_progress_on_the_run_log() {
        let fixture = Fixture::new();
        let shims = ShimFixture::new();
        let chatty = shim(
            &shims,
            "chatty",
            "#!/bin/sh\n\
             printf 'phase one\\n' >&2\n\
             printf 'phase two: 50%%\\rphase two: 100%%\\n' >&2\n\
             exit 0\n",
        );

        let executor = fixture.executor(registry_of(vec![erase(RunsAShim(Duration::from_secs(
            120,
        )))]));
        let call = fixture.pending("fixture.runs", json!({"program": chatty}));

        let completed = executor
            .execute(call, fixture.workspace.path(), &Cancellation::default())
            .unwrap();

        assert!(completed.outcome().succeeded(), "{completed:?}");
        let messages = fixture
            .events_of(&EventKind::ToolProgress)
            .into_iter()
            .map(|stored| stored.event.payload()["text"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();

        // A carriage return ends a segment as well as a newline: a program that
        // overwrites its progress line would otherwise report nothing until the
        // phase ended.
        assert_eq!(messages, ["phase one", "phase two: 50%", "phase two: 100%"]);
    }

    #[test]
    fn a_child_flooding_both_streams_completes_rather_than_deadlocking() {
        // Both pipes are drained concurrently. Were only one reader running, the
        // child would block forever once it filled the other stream's buffer, so
        // a hang here is the expected failure mode. Both floods therefore have
        // to exceed the 64 KiB pipe buffer.
        //
        // The stderr flood carries no newlines on purpose. Every stderr *segment*
        // becomes one persisted run event, so a quarter-megabyte of short lines
        // would measure the store's write throughput rather than this module's
        // pipe handling — and would say nothing about the deadlock the test is
        // named for.
        const FLOOD: usize = 256 * 1024;

        let fixture = Fixture::new();
        let shims = ShimFixture::new();
        let flooding = shim(
            &shims,
            "both-streams",
            &format!(
                "#!/bin/sh\n\
                 yes harkness-stdout | head -c {FLOOD}\n\
                 yes harkness-stderr | tr -d '\\n' | head -c {FLOOD} >&2\n"
            ),
        );

        let executor = fixture.executor(registry_of(vec![erase(RunsAShim(Duration::from_secs(
            120,
        )))]));
        let call = fixture.pending("fixture.runs", json!({"program": flooding}));

        let completed = executor
            .execute(call, fixture.workspace.path(), &Cancellation::default())
            .unwrap();

        assert!(completed.outcome().succeeded(), "{completed:?}");
        assert_eq!(
            completed.record().output().unwrap()["stdout_bytes"],
            json!(FLOOD)
        );
    }

    #[test]
    fn a_child_whose_last_output_outruns_the_segment_queue_still_completes() {
        // The queue between the reader thread and the wait loop is bounded, and
        // a child can exit with a full pipe still unread. Everything left is
        // then turned into segments by a thread that blocks once the queue
        // fills — so a wait loop that stops draining in order to join its
        // readers waits for a thread that is waiting for it. The symptom is not
        // a slow call but a call that never ends, and the executor abandoning
        // it leaks both reader threads, the pipe, and an open artifact sink.
        //
        // The burst has to arrive faster than the wait loop drains, which a
        // shell loop does not: `yes` fills the pipe as fast as the kernel
        // allows. What has to be true at exit is that more than the queue's 256
        // slots are still unread, and the pipe holds 64 KiB — so the line length
        // is what decides it. At 128 bytes a full pipe is ~512 lines, comfortably
        // past the queue, while the total stays small enough that the test is
        // not measuring the store's write throughput. A five-second limit turns
        // a regression into a timed-out call rather than a suite that hangs.
        const LINE: &str = "a progress line long enough that a full pipe still holds far more of them than the segment queue between the reader and the wait loop can";
        const EMITTED: usize = 128 * 1024;

        let fixture = Fixture::new();
        let shims = ShimFixture::new();
        let chatty = shim(
            &shims,
            "chatty-then-exits",
            &format!("#!/bin/sh\nyes '{LINE}' | head -c {EMITTED} >&2\nexit 0\n"),
        );

        let executor =
            fixture.executor(registry_of(vec![erase(RunsAShim(Duration::from_secs(5)))]));
        let call = fixture.pending("fixture.runs", json!({"program": chatty}));

        let completed = executor
            .execute(call, fixture.workspace.path(), &Cancellation::default())
            .unwrap();

        assert!(completed.outcome().succeeded(), "{completed:?}");
        // Every whole line reaches the log, which is the other half of the
        // property: draining to avoid the deadlock must not mean discarding.
        // `head -c` cuts mid-line, so the final partial line is the remainder.
        let whole_lines = EMITTED / (LINE.len() + 1);
        assert!(
            whole_lines > 256,
            "the burst must outrun the queue to test anything: {whole_lines} lines"
        );
        assert_eq!(
            fixture.events_of(&EventKind::ToolProgress).len(),
            whole_lines + 1
        );
    }

    #[test]
    fn a_helper_holding_the_pipes_open_does_not_outlive_the_child_that_started_it() {
        // A pipe reaches end of file only when *every* write end is closed, and
        // a child that starts a background helper leaves one open behind it. The
        // direct child exits immediately, so waiting for the readers to reach
        // EOF means waiting for the helper — which is the whole length of
        // whatever it is doing, long past the call it belongs to.
        //
        // The group is the unit of execution here, so it is the unit that ends:
        // the helper is killed with it, and the call returns at once.
        let fixture = Fixture::new();
        let shims = ShimFixture::new();
        let activity = shims.root.path().join("orphan-activity");
        let orphaning = shim(
            &shims,
            "orphaning",
            &format!(
                "#!/bin/sh\n\
                 (while true; do printf x >> '{}'; sleep 0.01; done) &\n\
                 echo started >&2\n\
                 exit 0\n",
                activity.display()
            ),
        );

        let executor =
            fixture.executor(registry_of(vec![erase(RunsAShim(Duration::from_secs(5)))]));
        let call = fixture.pending("fixture.runs", json!({"program": orphaning}));

        let began = Instant::now();
        let completed = executor
            .execute(call, fixture.workspace.path(), &Cancellation::default())
            .unwrap();
        let elapsed = began.elapsed();

        assert!(completed.outcome().succeeded(), "{completed:?}");
        assert!(
            elapsed < Duration::from_secs(4),
            "the call waited on a helper it did not own: {elapsed:?}"
        );

        let at_return = std::fs::read(&activity).unwrap_or_default();
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(
            std::fs::read(&activity).unwrap_or_default(),
            at_return,
            "a helper outlived the call that started it"
        );
    }

    #[test]
    fn a_child_that_cannot_be_started_is_a_structured_failure() {
        let fixture = Fixture::new();
        let executor = fixture.executor(registry_of(vec![erase(RunsAShim(Duration::from_secs(
            120,
        )))]));
        let call = fixture.pending(
            "fixture.runs",
            json!({"program": "/nonexistent/harkness-shim"}),
        );

        let completed = executor
            .execute(call, fixture.workspace.path(), &Cancellation::default())
            .unwrap();

        assert_eq!(completed.outcome().failure_kind(), Some("execution_failed"));
        assert!(
            completed
                .record()
                .failure()
                .unwrap()
                .message()
                .contains("could not be started")
        );
    }

    /// Silences the unused-import warning for a helper only some tests need.
    #[allow(dead_code)]
    fn _writes(mut sink: impl Write) {
        let _ = sink.flush();
    }
}

// ---------------------------------------------------------------------------
// Performance
// ---------------------------------------------------------------------------

/// Latency targets are meaningful only in a release build, so debug and CI runs
/// skip them; run with `--release ... -- --ignored` to record numbers.
#[test]
#[ignore = "latency target; meaningful only in a release build"]
fn executor_overhead_per_call_meets_the_latency_target() {
    let fixture = Fixture::new();
    let executor = fixture.executor(registry_of(vec![erase(Echo("1.0.0"))]));

    // One warm call, so the measurement is not paying for the reader pool and
    // the prepared statements the first call establishes.
    let warm = fixture.pending("fixture.echo", json!({"message": "warm"}));
    executor
        .execute(warm, fixture.workspace.path(), &Cancellation::default())
        .unwrap();

    let call = fixture.pending("fixture.echo", json!({"message": "measured"}));
    let began = Instant::now();
    let completed: CompletedCall = executor
        .execute(call, fixture.workspace.path(), &Cancellation::default())
        .unwrap();
    let elapsed = began.elapsed();

    assert!(completed.outcome().succeeded());
    println!("one call of a trivial tool took {elapsed:?}");
    assert!(
        elapsed < Duration::from_millis(10),
        "per-call executor overhead was {elapsed:?}"
    );
}

#[test]
#[ignore = "latency target; meaningful only in a release build"]
fn cancellation_latency_meets_the_target() {
    let fixture = Fixture::new();
    let started = Arc::new(AtomicBool::new(false));
    let executor = fixture.executor(registry_of(vec![erase(Cooperative {
        started: Arc::clone(&started),
        stopped_at: Arc::new(AtomicUsize::new(usize::MAX)),
    })]));
    let call = fixture.pending("fixture.cooperative", json!({}));

    let cancellation = Cancellation::default();
    let watcher = cancellation.clone();
    let watched = Arc::clone(&started);
    let requested = Arc::new(std::sync::Mutex::new(None));
    let recorded = Arc::clone(&requested);
    std::thread::spawn(move || {
        while !watched.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
        *recorded.lock().unwrap() = Some(Instant::now());
        watcher.cancel();
    });

    let completed = executor
        .execute(call, fixture.workspace.path(), &cancellation)
        .unwrap();
    let stopped = Instant::now();

    assert_eq!(completed.outcome(), &CallOutcome::Cancelled);
    let elapsed = stopped
        - requested
            .lock()
            .unwrap()
            .expect("cancellation was requested");
    println!("a cooperative tool stopped {elapsed:?} after the request");
    assert!(
        elapsed < Duration::from_millis(250),
        "cancellation took {elapsed:?} to take effect"
    );
}
