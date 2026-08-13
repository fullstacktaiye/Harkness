//! Behavioural coverage for admission, backpressure, cancellation, and
//! shutdown.
//!
//! Every test opens its own store under a temporary directory, creates its own
//! workspace directories, and registers its own fixture tools, so nothing here
//! reads or writes the real Harkness data directory and no test depends on
//! another's registry.
//!
//! Two conventions keep these hermetic rather than timing-dependent. Tools
//! block on a [`Gate`] — a condition variable a test releases — instead of
//! sleeping, so "this call is running and will not finish yet" is a fact rather
//! than a guess. And negative assertions are made against state the admission
//! rules *forbid* from changing: while a mutation is held inside its gate, a
//! call behind it provably cannot have started, so its recorded state is read
//! once rather than watched for a while.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use harkness_core::ProjectId;
use harkness_git::Cancellation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tempfile::TempDir;
use time::OffsetDateTime;

use crate::domain::{Run, Step, Task, ToolCall, ToolCallId, ToolCallState};
use crate::store::Store;
use crate::tool::{
    CallOutcome, ErasedTool, ExecutionContext, ExecutionError, RiskLevel, Tool, ToolError,
    ToolExecutor, ToolIdentity, ToolMetadata, ToolRegistry, erase,
};

use super::{
    CallTicket, MAX_PROCESS_CONCURRENCY, OUTCOME_CAPACITY, ScheduleError, Scheduled, ScheduledCall,
    Scheduler, WORKSPACE_QUEUE_CAPACITY, WORKSPACE_READ_CONCURRENCY, WorkspaceKey,
};

// ---------------------------------------------------------------------------
// Waiting without depending on timing
// ---------------------------------------------------------------------------

/// The longest any bounded wait here will tolerate before failing the test.
///
/// Generous on purpose. It is not a latency assertion — every wait is for a
/// condition that either happens promptly or never — so a loaded machine must
/// not turn a correctness test into a flake.
const PATIENCE: Duration = Duration::from_secs(20);

/// How often a bounded wait re-checks a condition nothing can notify it about.
const POLL: Duration = Duration::from_millis(2);

/// Waits for `condition`, failing with `what` rather than hanging.
fn until(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        thread::sleep(POLL);
    }
    assert!(condition(), "{what} did not happen within {PATIENCE:?}");
}

/// Settles a ticket, failing rather than hanging when nothing arrives.
fn settled(ticket: CallTicket) -> Scheduled {
    let call = ticket.call();
    ticket
        .wait_for(PATIENCE)
        .unwrap_or_else(|_| panic!("tool call {call} never reached a terminal state"))
}

/// The record a successfully scheduled call left behind.
fn succeeded(ticket: CallTicket) -> ToolCallState {
    let call = ticket.call();
    match settled(ticket) {
        Ok(completed) => completed.state(),
        Err(error) => panic!("tool call {call} was refused: {error}"),
    }
}

// ---------------------------------------------------------------------------
// A gate a tool blocks on and a test releases
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Gate(Arc<GateInner>);

#[derive(Default)]
struct GateInner {
    state: Mutex<GateState>,
    changed: Condvar,
}

#[derive(Default)]
struct GateState {
    /// Every call that reached the gate, in the order it arrived.
    entered: Vec<ToolCallId>,
    /// How many are inside right now.
    inside: usize,
    /// The most that were ever inside at once — the concurrency assertion.
    peak: usize,
    released: bool,
}

impl Gate {
    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Blocks the calling tool until the test releases the gate.
    ///
    /// Polls its own cancellation the way any well-behaved tool body must, so
    /// a gated call is stoppable and a gate left unreleased still cannot
    /// outlive its scheduler.
    fn hold(&self, context: &mut ExecutionContext) -> Result<(), ToolError> {
        {
            let mut state = self.lock();
            state.entered.push(context.call());
            state.inside += 1;
            state.peak = state.peak.max(state.inside);
            self.0.changed.notify_all();
        }
        let outcome = loop {
            if let Err(error) = context.check_still_permitted() {
                break Err(error);
            }
            let state = self.lock();
            if state.released {
                break Ok(());
            }
            drop(
                self.0
                    .changed
                    .wait_timeout(state, POLL)
                    .unwrap_or_else(|error| error.into_inner())
                    .0,
            );
        };
        let mut state = self.lock();
        state.inside -= 1;
        self.0.changed.notify_all();
        outcome
    }

    /// Waits until `count` calls have reached the gate.
    fn wait_for(&self, count: usize) {
        until(&format!("{count} call(s) reaching the gate"), || {
            self.lock().entered.len() >= count
        });
    }

    fn entered(&self) -> Vec<ToolCallId> {
        self.lock().entered.clone()
    }

    fn arrivals(&self) -> usize {
        self.lock().entered.len()
    }

    fn peak(&self) -> usize {
        self.lock().peak
    }

    fn release(&self) {
        let mut state = self.lock();
        state.released = true;
        self.0.changed.notify_all();
    }
}

// ---------------------------------------------------------------------------
// Fixture tools
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct Ran {
    call: String,
}

/// A tool that holds its gate, at whatever risk the test registered it under.
struct Gated {
    id: &'static str,
    risk: RiskLevel,
    gate: Gate,
}

impl Gated {
    fn new(id: &'static str, risk: RiskLevel) -> (Self, Gate) {
        let gate = Gate::default();
        (
            Self {
                id,
                risk,
                gate: gate.clone(),
            },
            gate,
        )
    }
}

impl Tool for Gated {
    type Input = Empty;
    type Output = Ran;

    fn metadata(&self) -> ToolMetadata {
        // A finite limit rather than `OnlyByCancellation`, so a test that
        // forgets to release its gate fails instead of hanging forever.
        ToolMetadata::new(
            ToolIdentity::parse(self.id, "1.0.0").unwrap(),
            "Gated fixture",
            "Blocks until a test releases it.",
            self.risk,
        )
        .within(PATIENCE * 3)
    }

    fn execute(&self, _input: Empty, context: &mut ExecutionContext) -> Result<Ran, ToolError> {
        self.gate.hold(context)?;
        Ok(Ran {
            call: context.call().to_string(),
        })
    }
}

/// A tool that returns at once, for scenarios about queueing rather than work.
struct Immediate(&'static str, RiskLevel);

impl Tool for Immediate {
    type Input = Empty;
    type Output = Ran;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse(self.0, "1.0.0").unwrap(),
            "Immediate fixture",
            "Returns without waiting for anything.",
            self.1,
        )
    }

    fn execute(&self, _input: Empty, context: &mut ExecutionContext) -> Result<Ran, ToolError> {
        Ok(Ran {
            call: context.call().to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A store, a project identity, and somewhere to put workspaces.
struct Fixture {
    _data_dir: TempDir,
    root: TempDir,
    store: Arc<Store>,
    project: ProjectId,
}

impl Fixture {
    fn new() -> Self {
        let data_dir = TempDir::new().unwrap();
        let store = Arc::new(Store::open(data_dir.path()).unwrap());
        Self {
            _data_dir: data_dir,
            root: TempDir::new().unwrap(),
            store,
            project: ProjectId::new(),
        }
    }

    /// Creates a directory and the key naming it.
    fn workspace(&self, name: &str) -> WorkspaceKey {
        let path = self.root.path().join(name);
        std::fs::create_dir_all(&path).unwrap();
        WorkspaceKey::new(self.project, path).unwrap()
    }

    /// Records a task, a run, and one step to hang calls off.
    fn run(&self, workspace: &WorkspaceKey) -> Step {
        let task = Task::new(
            "Schedule some work",
            workspace.canonical_root(),
            None,
            at(0),
        );
        self.store.insert_task(&task).unwrap();
        let run = Run::new(task.id(), at(1));
        self.store.insert_run(&run).unwrap();
        let step = Step::new(run.id(), 0, "Run the tools", at(2));
        self.store.insert_step(&step).unwrap();
        step
    }

    /// Records one pending call of `tool_id` against `step`.
    fn call(&self, step: &Step, tool_id: &str) -> ToolCallId {
        let call = ToolCall::new(step, tool_id, "", json!({}), at(3));
        self.store.insert_tool_call(&call).unwrap();
        call.id()
    }

    fn scheduler(&self, tools: Vec<Arc<dyn ErasedTool>>) -> Arc<Scheduler> {
        self.scheduler_with_process_limit(tools, MAX_PROCESS_CONCURRENCY)
    }

    fn scheduler_with_process_limit(
        &self,
        tools: Vec<Arc<dyn ErasedTool>>,
        processes: usize,
    ) -> Arc<Scheduler> {
        let mut registry = ToolRegistry::new();
        for tool in tools {
            registry.register_erased(tool).unwrap();
        }
        Arc::new(Scheduler::with_process_limit(
            ToolExecutor::new(Arc::clone(&self.store), Arc::new(registry)),
            processes,
        ))
    }

    fn state(&self, call: ToolCallId) -> ToolCallState {
        self.store.load_tool_call(call).unwrap().state()
    }
}

/// A deterministic instant, `offset` seconds after a fixed epoch.
fn at(offset: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000 + offset).unwrap()
}

fn eraseit<T: Tool + 'static>(tool: T) -> Arc<dyn ErasedTool> {
    erase(tool).unwrap()
}

/// Submits `call` for `workspace` at `risk`, failing rather than returning.
fn submit(
    scheduler: &Scheduler,
    call: ToolCallId,
    workspace: &WorkspaceKey,
    risk: RiskLevel,
) -> CallTicket {
    scheduler
        .submit(ScheduledCall::new(
            call,
            workspace.clone(),
            risk,
            Cancellation::default(),
        ))
        .unwrap_or_else(|error| panic!("submitting {call} was refused: {error}"))
}

// ---------------------------------------------------------------------------
// Per-workspace mutation serialization
// ---------------------------------------------------------------------------

#[test]
fn two_mutations_of_one_workspace_run_strictly_in_sequence() {
    let fixture = Fixture::new();
    let (first, first_gate) = Gated::new("fixture.first", RiskLevel::WorkspaceWrite);
    let (second, second_gate) = Gated::new("fixture.second", RiskLevel::WorkspaceWrite);
    let scheduler = fixture.scheduler(vec![eraseit(first), eraseit(second)]);

    let workspace = fixture.workspace("one");
    let step = fixture.run(&workspace);
    let leader = fixture.call(&step, "fixture.first");
    let follower = fixture.call(&step, "fixture.second");

    let leading = submit(&scheduler, leader, &workspace, RiskLevel::WorkspaceWrite);
    let following = submit(&scheduler, follower, &workspace, RiskLevel::WorkspaceWrite);
    first_gate.wait_for(1);

    // Read once rather than watched for a while: the follower cannot start
    // while the leader is inside its gate, so `pending` here is a fact about
    // the admission rules and not a race the test happened to win.
    assert_eq!(fixture.state(follower), ToolCallState::Pending);
    assert_eq!(second_gate.arrivals(), 0);
    let snapshot = scheduler.snapshot();
    let load = &snapshot.workspaces()[0];
    assert_eq!(
        (load.running(), load.queued(), load.mutating()),
        (1, 1, true)
    );

    first_gate.release();
    assert_eq!(succeeded(leading), ToolCallState::Succeeded);

    // The acceptance criterion in full: the second call starts only after the
    // first's *terminal state is persisted*, which is a stronger claim than
    // "after the first returned" and is the one a crash-consistent history
    // depends on.
    second_gate.wait_for(1);
    assert_eq!(fixture.state(leader), ToolCallState::Succeeded);

    second_gate.release();
    assert_eq!(succeeded(following), ToolCallState::Succeeded);
    assert_eq!(first_gate.peak(), 1);
    assert_eq!(second_gate.peak(), 1);
}

#[test]
fn the_same_two_mutations_on_two_workspaces_overlap() {
    let fixture = Fixture::new();
    let (tool, gate) = Gated::new("fixture.writes", RiskLevel::WorkspaceWrite);
    let scheduler = fixture.scheduler(vec![eraseit(tool)]);

    let here = fixture.workspace("here");
    let there = fixture.workspace("there");
    let here_step = fixture.run(&here);
    let there_step = fixture.run(&there);
    let one = fixture.call(&here_step, "fixture.writes");
    let another = fixture.call(&there_step, "fixture.writes");

    let first = submit(&scheduler, one, &here, RiskLevel::WorkspaceWrite);
    let second = submit(&scheduler, another, &there, RiskLevel::WorkspaceWrite);

    // Both inside the gate at once is the whole assertion: serialization is a
    // property of a workspace, not of the scheduler.
    gate.wait_for(2);
    assert_eq!(gate.peak(), 2);
    assert_eq!(scheduler.snapshot().running(), 2);
    assert_eq!(scheduler.snapshot().queued(), 0);

    gate.release();
    assert_eq!(succeeded(first), ToolCallState::Succeeded);
    assert_eq!(succeeded(second), ToolCallState::Succeeded);
}

#[test]
fn a_submission_may_raise_a_declared_risk_and_may_never_lower_one() {
    let fixture = Fixture::new();
    let (writes, write_gate) = Gated::new("fixture.declares_write", RiskLevel::WorkspaceWrite);
    let scheduler = fixture.scheduler(vec![eraseit(writes)]);

    let workspace = fixture.workspace("guarded");
    let step = fixture.run(&workspace);
    let one = fixture.call(&step, "fixture.declares_write");
    let another = fixture.call(&step, "fixture.declares_write");

    // Submitted as though they only observed. The declaration wins, so they
    // still take the mutation slot one at a time.
    let first = submit(&scheduler, one, &workspace, RiskLevel::Observe);
    let second = submit(&scheduler, another, &workspace, RiskLevel::Observe);
    write_gate.wait_for(1);
    assert_eq!(fixture.state(another), ToolCallState::Pending);

    write_gate.release();
    assert_eq!(succeeded(first), ToolCallState::Succeeded);
    assert_eq!(succeeded(second), ToolCallState::Succeeded);
    assert_eq!(
        write_gate.peak(),
        1,
        "an understated submission escaped the mutation slot"
    );
}

#[test]
fn a_classification_may_escalate_a_tool_that_only_declares_observation() {
    let fixture = Fixture::new();
    let (observes, gate) = Gated::new("fixture.declares_observe", RiskLevel::Observe);
    let scheduler = fixture.scheduler(vec![eraseit(observes)]);

    let workspace = fixture.workspace("escalated");
    let step = fixture.run(&workspace);
    let one = fixture.call(&step, "fixture.declares_observe");
    let another = fixture.call(&step, "fixture.declares_observe");

    // The invocation was classified as writing outside what the tool usually
    // touches — a path that left the workspace, say. It serializes.
    let first = submit(&scheduler, one, &workspace, RiskLevel::Destructive);
    let second = submit(&scheduler, another, &workspace, RiskLevel::Destructive);
    gate.wait_for(1);
    assert_eq!(fixture.state(another), ToolCallState::Pending);

    gate.release();
    assert_eq!(succeeded(first), ToolCallState::Succeeded);
    assert_eq!(succeeded(second), ToolCallState::Succeeded);
    assert_eq!(gate.peak(), 1);
}

// ---------------------------------------------------------------------------
// Read concurrency and FIFO fairness
// ---------------------------------------------------------------------------

#[test]
fn reads_of_one_workspace_run_concurrently_up_to_the_cap() {
    let fixture = Fixture::new();
    let (reads, gate) = Gated::new("fixture.reads", RiskLevel::Observe);
    let scheduler = fixture.scheduler(vec![eraseit(reads)]);

    let workspace = fixture.workspace("read-heavy");
    let step = fixture.run(&workspace);
    let submitted = WORKSPACE_READ_CONCURRENCY + 2;
    let tickets = (0..submitted)
        .map(|_| {
            let call = fixture.call(&step, "fixture.reads");
            submit(&scheduler, call, &workspace, RiskLevel::Observe)
        })
        .collect::<Vec<_>>();

    gate.wait_for(WORKSPACE_READ_CONCURRENCY);
    let snapshot = scheduler.snapshot();
    let load = &snapshot.workspaces()[0];
    assert_eq!(load.running(), WORKSPACE_READ_CONCURRENCY);
    assert_eq!(load.queued(), submitted - WORKSPACE_READ_CONCURRENCY);
    assert!(!load.mutating());

    gate.release();
    for ticket in tickets {
        assert_eq!(succeeded(ticket), ToolCallState::Succeeded);
    }
    assert_eq!(
        gate.peak(),
        WORKSPACE_READ_CONCURRENCY,
        "more reads ran at once than the cap allows"
    );
}

#[test]
fn a_queued_mutation_is_not_starved_by_a_continuous_stream_of_reads() {
    let fixture = Fixture::new();
    let (reads, read_gate) = Gated::new("fixture.reading", RiskLevel::Observe);
    let (writes, write_gate) = Gated::new("fixture.writing", RiskLevel::WorkspaceWrite);
    let scheduler = fixture.scheduler(vec![eraseit(reads), eraseit(writes)]);

    let workspace = fixture.workspace("contended");
    let step = fixture.run(&workspace);

    // One read running, then a mutation, then a stream of reads behind it.
    let leading_read = fixture.call(&step, "fixture.reading");
    let leading = submit(&scheduler, leading_read, &workspace, RiskLevel::Observe);
    read_gate.wait_for(1);

    let mutation = fixture.call(&step, "fixture.writing");
    let mutating = submit(&scheduler, mutation, &workspace, RiskLevel::WorkspaceWrite);
    let trailing_reads = (0..WORKSPACE_READ_CONCURRENCY * 2)
        .map(|_| {
            let call = fixture.call(&step, "fixture.reading");
            (
                call,
                submit(&scheduler, call, &workspace, RiskLevel::Observe),
            )
        })
        .collect::<Vec<_>>();

    // Strict FIFO: the reads behind the mutation do not jump it, even though
    // there is capacity for them and they would be admissible on their own.
    assert_eq!(read_gate.arrivals(), 1);
    for (call, _) in &trailing_reads {
        assert_eq!(fixture.state(*call), ToolCallState::Pending);
    }

    read_gate.release();
    assert_eq!(succeeded(leading), ToolCallState::Succeeded);
    write_gate.wait_for(1);
    assert_eq!(
        scheduler.snapshot().workspaces()[0].running(),
        1,
        "the mutation is running alone"
    );

    write_gate.release();
    assert_eq!(succeeded(mutating), ToolCallState::Succeeded);
    for (_, ticket) in trailing_reads {
        assert_eq!(succeeded(ticket), ToolCallState::Succeeded);
    }
    assert_eq!(write_gate.peak(), 1);
}

#[test]
fn one_workspaces_queue_is_served_in_the_order_it_was_submitted() {
    let fixture = Fixture::new();
    let (writes, gate) = Gated::new("fixture.ordered", RiskLevel::WorkspaceWrite);
    let scheduler = fixture.scheduler(vec![eraseit(writes)]);

    let workspace = fixture.workspace("fifo");
    let step = fixture.run(&workspace);
    let mut submitted = Vec::new();
    let mut tickets = Vec::new();
    for _ in 0..8 {
        let call = fixture.call(&step, "fixture.ordered");
        submitted.push(call);
        tickets.push(submit(
            &scheduler,
            call,
            &workspace,
            RiskLevel::WorkspaceWrite,
        ));
    }

    // Released up front: the tools no longer block, so what is being observed
    // is purely the order the scheduler admitted them in.
    gate.release();
    for ticket in tickets {
        assert_eq!(succeeded(ticket), ToolCallState::Succeeded);
    }
    assert_eq!(gate.entered(), submitted);
    assert_eq!(gate.peak(), 1);
}

// ---------------------------------------------------------------------------
// Backpressure
// ---------------------------------------------------------------------------

#[test]
fn a_full_submission_queue_blocks_the_submitter_rather_than_growing() {
    let fixture = Fixture::new();
    let (writes, gate) = Gated::new("fixture.backpressure", RiskLevel::WorkspaceWrite);
    let scheduler = fixture.scheduler(vec![eraseit(writes)]);

    let workspace = fixture.workspace("full");
    let step = fixture.run(&workspace);

    // One running inside the gate, then exactly enough to fill the queue.
    let running = fixture.call(&step, "fixture.backpressure");
    let mut tickets = vec![submit(
        &scheduler,
        running,
        &workspace,
        RiskLevel::WorkspaceWrite,
    )];
    gate.wait_for(1);
    for _ in 0..WORKSPACE_QUEUE_CAPACITY {
        let call = fixture.call(&step, "fixture.backpressure");
        tickets.push(submit(
            &scheduler,
            call,
            &workspace,
            RiskLevel::WorkspaceWrite,
        ));
    }
    assert_eq!(
        scheduler.snapshot().workspaces()[0].queued(),
        WORKSPACE_QUEUE_CAPACITY
    );

    let overflowing = fixture.call(&step, "fixture.backpressure");
    let returned = Arc::new(AtomicBool::new(false));
    let producer = {
        let scheduler = Arc::clone(&scheduler);
        let workspace = workspace.clone();
        let returned = Arc::clone(&returned);
        thread::spawn(move || {
            let ticket = submit(
                &scheduler,
                overflowing,
                &workspace,
                RiskLevel::WorkspaceWrite,
            );
            returned.store(true, Ordering::Release);
            ticket
        })
    };

    // The producer is parked, not buffered. Nothing can drain the queue while
    // the gate is held, so the depth cannot move and the submission cannot
    // have returned — both are facts, not a race the test won.
    until("the producer parking on a full queue", || {
        scheduler.snapshot().workspaces()[0].queued() == WORKSPACE_QUEUE_CAPACITY
    });
    assert!(!returned.load(Ordering::Acquire));
    assert_eq!(
        scheduler.snapshot().workspaces()[0].queued(),
        WORKSPACE_QUEUE_CAPACITY,
        "the queue grew past its named capacity instead of applying backpressure"
    );
    assert_eq!(fixture.state(overflowing), ToolCallState::Pending);

    gate.release();
    tickets.push(producer.join().unwrap());
    for ticket in tickets {
        assert_eq!(succeeded(ticket), ToolCallState::Succeeded);
    }
    assert_eq!(gate.peak(), 1);
}

/// A parked producer is registered on its workspace and keeps it from being
/// collected out from under it.
///
/// The failure this guards against is narrow and, deliberately, not what the
/// assertions below reproduce. Every path that shrinks a queue notifies the
/// condition variable, so a parked producer is woken promptly and refills the
/// queue before the workspace can be *seen* empty; what remains is the window
/// between that wake-up and the producer re-acquiring the mutex, during which a
/// completing worker can run `forget_idle`. Nothing outside the module can
/// schedule two threads into that window on purpose, so this test pins the
/// invariant — the producer is visible, the workspace survives, and one key
/// never yields two mutation slots — rather than staging the race.
#[test]
fn a_parked_producer_keeps_its_workspace_from_being_forgotten() {
    let fixture = Fixture::new();
    let (filling, fill_gate) = Gated::new("fixture.filling", RiskLevel::WorkspaceWrite);
    let (overflowing, overflow_gate) = Gated::new("fixture.overflowing", RiskLevel::WorkspaceWrite);
    let scheduler = fixture.scheduler(vec![eraseit(filling), eraseit(overflowing)]);

    // Two runs of one workspace: the first fills the queue and is then swept
    // away wholesale, and the second is what the parked producer carries.
    let workspace = fixture.workspace("evicted");
    let filler = fixture.run(&workspace);
    let overflow = fixture.run(&workspace);

    let running = fixture.call(&filler, "fixture.filling");
    submit(&scheduler, running, &workspace, RiskLevel::WorkspaceWrite);
    fill_gate.wait_for(1);
    for _ in 0..WORKSPACE_QUEUE_CAPACITY {
        let call = fixture.call(&filler, "fixture.filling");
        submit(&scheduler, call, &workspace, RiskLevel::WorkspaceWrite);
    }

    let carried = fixture.call(&overflow, "fixture.overflowing");
    let producer = {
        let scheduler = Arc::clone(&scheduler);
        let workspace = workspace.clone();
        thread::spawn(move || submit(&scheduler, carried, &workspace, RiskLevel::WorkspaceWrite))
    };
    until("the producer parking on the full queue", || {
        scheduler.snapshot().workspaces()[0].waiting() == 1
    });

    // Cancelling empties the workspace in one step while the producer is still
    // parked — and a parked producer holds neither the map lock nor the
    // workspace's own, because the condition variable released it for the
    // duration of the wait. This is precisely the moment the workspace looks
    // collectable and is not.
    scheduler.cancel_run(filler.run_id());
    until("the cancelled run's last call ending", || {
        fixture.state(running).is_terminal()
    });

    // An observation window rather than a wait: the claim is that the workspace
    // is never *absent* across the interval in which it holds nothing but a
    // parked producer, so a shorter window weakens this but cannot make it
    // wrong.
    let watching = Instant::now() + Duration::from_millis(200);
    while Instant::now() < watching {
        assert_eq!(
            scheduler.snapshot().workspaces().len(),
            1,
            "the workspace was forgotten while a producer was about to fill it"
        );
        thread::sleep(POLL);
    }

    // The end-to-end consequence of getting this wrong: the woken producer
    // pushes into an orphan, the next submission builds a second `Workspace`
    // for the same key with an empty running set, and one worktree ends up
    // with two mutation slots.
    let carrying = producer.join().unwrap();
    overflow_gate.wait_for(1);
    let following = fixture.call(&overflow, "fixture.overflowing");
    let followed = submit(&scheduler, following, &workspace, RiskLevel::WorkspaceWrite);

    assert_eq!(fixture.state(following), ToolCallState::Pending);
    assert_eq!(scheduler.snapshot().workspaces().len(), 1);
    assert_eq!(overflow_gate.arrivals(), 1);

    overflow_gate.release();
    assert_eq!(succeeded(carrying), ToolCallState::Succeeded);
    assert_eq!(succeeded(followed), ToolCallState::Succeeded);
    assert_eq!(
        overflow_gate.peak(),
        1,
        "one workspace admitted two mutations at once"
    );
    fill_gate.release();
}

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

#[test]
fn cancelling_a_run_resolves_queued_calls_without_ever_dispatching_them() {
    let fixture = Fixture::new();
    let (writes, write_gate) = Gated::new("fixture.cancelled_write", RiskLevel::WorkspaceWrite);
    let (queued_tool, queued_gate) = Gated::new("fixture.cancelled_queued", RiskLevel::Observe);
    let scheduler = fixture.scheduler(vec![eraseit(writes), eraseit(queued_tool)]);

    let workspace = fixture.workspace("cancelled");
    let step = fixture.run(&workspace);
    let run = step.run_id();

    let running = fixture.call(&step, "fixture.cancelled_write");
    let cancellation = Cancellation::default();
    let in_flight = scheduler
        .submit(ScheduledCall::new(
            running,
            workspace.clone(),
            RiskLevel::WorkspaceWrite,
            cancellation.clone(),
        ))
        .unwrap();
    write_gate.wait_for(1);

    let waiting = (0..3)
        .map(|_| {
            let call = fixture.call(&step, "fixture.cancelled_queued");
            (
                call,
                submit(&scheduler, call, &workspace, RiskLevel::Observe),
            )
        })
        .collect::<Vec<_>>();

    let report = scheduler.cancel_run(run);
    assert_eq!((report.queued(), report.running()), (3, 1));

    for (call, ticket) in waiting {
        let completed = settled(ticket).unwrap();
        assert_eq!(completed.outcome(), &CallOutcome::Cancelled);
        assert_eq!(fixture.state(call), ToolCallState::Cancelled);
    }
    assert_eq!(
        queued_gate.arrivals(),
        0,
        "a queued call was dispatched in order to cancel it"
    );

    // The running call is stopped through its token, which is what a user
    // cancelling actually does; the gated body observes it and returns.
    assert!(cancellation.is_cancelled());
    let completed = settled(in_flight).unwrap();
    assert_eq!(completed.outcome(), &CallOutcome::Cancelled);
    assert_eq!(fixture.state(running), ToolCallState::Cancelled);
}

#[test]
fn cancelling_one_run_leaves_another_runs_queued_work_in_place() {
    let fixture = Fixture::new();
    let (writes, gate) = Gated::new("fixture.shared", RiskLevel::WorkspaceWrite);
    let scheduler = fixture.scheduler(vec![eraseit(writes)]);

    // One workspace, two runs: the sweep is by run, and the queue it sweeps is
    // by workspace, so the two must not be confused.
    let workspace = fixture.workspace("shared");
    let doomed_step = fixture.run(&workspace);
    let surviving_step = fixture.run(&workspace);

    let holder = fixture.call(&doomed_step, "fixture.shared");
    let holding = submit(&scheduler, holder, &workspace, RiskLevel::WorkspaceWrite);
    gate.wait_for(1);

    let doomed = fixture.call(&doomed_step, "fixture.shared");
    let doomed_ticket = submit(&scheduler, doomed, &workspace, RiskLevel::WorkspaceWrite);
    let surviving = fixture.call(&surviving_step, "fixture.shared");
    let surviving_ticket = submit(&scheduler, surviving, &workspace, RiskLevel::WorkspaceWrite);

    let report = scheduler.cancel_run(doomed_step.run_id());
    assert_eq!((report.queued(), report.running()), (1, 1));
    assert_eq!(fixture.state(doomed), ToolCallState::Cancelled);
    assert_eq!(
        settled(doomed_ticket).unwrap().outcome(),
        &CallOutcome::Cancelled
    );
    assert_eq!(settled(holding).unwrap().outcome(), &CallOutcome::Cancelled);

    // Freeing the mutation slot is part of cancelling, so the other run's work
    // proceeds rather than waiting behind a call that no longer exists.
    gate.release();
    assert_eq!(succeeded(surviving_ticket), ToolCallState::Succeeded);
}

// ---------------------------------------------------------------------------
// Independence, snapshots, and the shape of the public surface
// ---------------------------------------------------------------------------

#[test]
fn a_slow_tool_in_one_workspace_never_stalls_dispatch_in_another() {
    let fixture = Fixture::new();
    let (slow, gate) = Gated::new("fixture.slow", RiskLevel::WorkspaceWrite);
    let scheduler = fixture.scheduler(vec![
        eraseit(slow),
        eraseit(Immediate("fixture.quick", RiskLevel::WorkspaceWrite)),
    ]);

    let stalled = fixture.workspace("stalled");
    let lively = fixture.workspace("lively");
    let stalled_step = fixture.run(&stalled);
    let lively_step = fixture.run(&lively);

    let blocking = fixture.call(&stalled_step, "fixture.slow");
    let blocked = submit(&scheduler, blocking, &stalled, RiskLevel::WorkspaceWrite);
    gate.wait_for(1);

    // Every one of these submits, admits, dispatches, executes, and records
    // while the other workspace's mutex-holding candidate sits inside a tool
    // body. A dispatch decision taken while holding anything shared would make
    // this hang rather than fail.
    for _ in 0..8 {
        let call = fixture.call(&lively_step, "fixture.quick");
        let ticket = submit(&scheduler, call, &lively, RiskLevel::WorkspaceWrite);
        assert_eq!(succeeded(ticket), ToolCallState::Succeeded);
    }
    // Reading the scheduler is equally unaffected.
    assert_eq!(scheduler.snapshot().running(), 1);

    gate.release();
    assert_eq!(succeeded(blocked), ToolCallState::Succeeded);
}

#[test]
fn a_snapshot_reports_exactly_the_scenario_that_was_constructed() {
    let fixture = Fixture::new();
    let (writes, write_gate) = Gated::new("fixture.snapshot_write", RiskLevel::WorkspaceWrite);
    let (reads, read_gate) = Gated::new("fixture.snapshot_read", RiskLevel::Observe);
    let scheduler = fixture.scheduler(vec![eraseit(writes), eraseit(reads)]);

    let writing = fixture.workspace("writing");
    let reading = fixture.workspace("reading");
    let writing_step = fixture.run(&writing);
    let reading_step = fixture.run(&reading);

    let mut tickets = Vec::new();
    let held = fixture.call(&writing_step, "fixture.snapshot_write");
    tickets.push(submit(
        &scheduler,
        held,
        &writing,
        RiskLevel::WorkspaceWrite,
    ));
    write_gate.wait_for(1);
    for _ in 0..2 {
        let call = fixture.call(&writing_step, "fixture.snapshot_write");
        tickets.push(submit(
            &scheduler,
            call,
            &writing,
            RiskLevel::WorkspaceWrite,
        ));
    }
    for _ in 0..2 {
        let call = fixture.call(&reading_step, "fixture.snapshot_read");
        tickets.push(submit(&scheduler, call, &reading, RiskLevel::Observe));
    }
    read_gate.wait_for(2);

    let snapshot = scheduler.snapshot();
    let by_workspace = snapshot
        .workspaces()
        .iter()
        .map(|load| (load.workspace().clone(), load))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(by_workspace.len(), 2);

    let writing_load = by_workspace[&writing];
    assert_eq!(
        (
            writing_load.running(),
            writing_load.queued(),
            writing_load.mutating()
        ),
        (1, 2, true)
    );
    let reading_load = by_workspace[&reading];
    assert_eq!(
        (
            reading_load.running(),
            reading_load.queued(),
            reading_load.mutating()
        ),
        (2, 0, false)
    );
    assert_eq!((snapshot.running(), snapshot.queued()), (3, 2));
    // No fixture tool here declared that it spawns children, so the global
    // process limit is untouched by three running calls.
    assert_eq!(snapshot.processes().in_use(), 0);
    assert_eq!(snapshot.processes().available(), MAX_PROCESS_CONCURRENCY);
    assert!(!snapshot.shutting_down());

    write_gate.release();
    read_gate.release();
    for ticket in tickets {
        assert_eq!(succeeded(ticket), ToolCallState::Succeeded);
    }

    // An idle workspace is forgotten rather than reported as empty, so the map
    // does not grow with every workspace the process has ever touched.
    until("the scheduler forgetting its idle workspaces", || {
        scheduler.snapshot().workspaces().is_empty()
    });
}

#[test]
fn an_approved_call_queues_behind_the_same_slots_as_everything_else() {
    let fixture = Fixture::new();
    let (writes, gate) = Gated::new("fixture.approved", RiskLevel::RemoteWrite);
    let scheduler = fixture.scheduler(vec![eraseit(writes)]);

    let workspace = fixture.workspace("approved");
    let step = fixture.run(&workspace);
    let leader = fixture.call(&step, "fixture.approved");
    let held = fixture.call(&step, "fixture.approved");
    fixture
        .store
        .transition_tool_call(held, ToolCallState::AwaitingApproval, at(4))
        .unwrap();

    let leading = submit(&scheduler, leader, &workspace, RiskLevel::RemoteWrite);
    let following = scheduler
        .submit(
            ScheduledCall::new(
                held,
                workspace.clone(),
                RiskLevel::RemoteWrite,
                Cancellation::default(),
            )
            .approved_by("tester"),
        )
        .unwrap();
    gate.wait_for(1);

    // An approved force push is the last call that should skip a mutation
    // slot, so it waits exactly as an unapproved one does.
    assert_eq!(fixture.state(held), ToolCallState::AwaitingApproval);

    gate.release();
    assert_eq!(succeeded(leading), ToolCallState::Succeeded);
    assert_eq!(succeeded(following), ToolCallState::Succeeded);
    let record = fixture.store.load_tool_call(held).unwrap();
    assert!(
        record
            .approvals()
            .iter()
            .any(|entry| entry.decided_by() == "tester"),
        "the decision that resumed the call was not recorded: {record:?}"
    );
}

#[test]
fn a_call_naming_an_unregistered_tool_is_recorded_rather_than_refused() {
    let fixture = Fixture::new();
    let scheduler = fixture.scheduler(vec![eraseit(Immediate(
        "fixture.known",
        RiskLevel::Observe,
    ))]);

    let workspace = fixture.workspace("unknown");
    let step = fixture.run(&workspace);
    let call = fixture.call(&step, "fixture.absent");

    // Refusing the submission would lose the fact that a run asked for a tool
    // that does not exist, which belongs in its history like any other failure.
    let ticket = submit(&scheduler, call, &workspace, RiskLevel::Observe);
    let completed = settled(ticket).unwrap();
    assert_eq!(completed.state(), ToolCallState::Failed);
    assert_eq!(completed.outcome().failure_kind(), Some("unknown_tool"));
}

#[test]
fn submitting_one_recorded_call_twice_leaves_the_accounting_intact() {
    let fixture = Fixture::new();
    let scheduler = fixture.scheduler_with_process_limit(
        vec![eraseit(Immediate("fixture.twice", RiskLevel::Observe))],
        1,
    );

    let workspace = fixture.workspace("twice");
    let step = fixture.run(&workspace);
    let call = fixture.call(&step, "fixture.twice");

    // Nothing stops a caller doing this, and the executor is what refuses it —
    // after dispatch. The scheduler's running set is keyed by dispatch rather
    // than by call, so the first completion frees the first admission and not
    // both.
    let first = submit(&scheduler, call, &workspace, RiskLevel::Observe);
    let second = submit(&scheduler, call, &workspace, RiskLevel::Observe);

    // One of the two is refused, and *which* refusal depends on which side of
    // the dispatch transaction the race lands on — `not_dispatchable` when the
    // executor's own pre-check sees the call already running, and the store's
    // refusal of the second transition when both got past it. Both are the
    // executor's business; what this test is about is that the scheduler's
    // accounting survives either.
    let outcomes = [settled(first), settled(second)];
    let refused = outcomes.iter().filter(|outcome| outcome.is_err()).count();
    assert_eq!(
        refused, 1,
        "exactly one of the two should have been refused"
    );
    assert_eq!(fixture.state(call), ToolCallState::Succeeded);

    until("the scheduler settling", || {
        scheduler.snapshot().workspaces().is_empty()
    });
    let processes = scheduler.snapshot().processes();
    assert_eq!(
        (processes.in_use(), processes.available()),
        (0, 1),
        "a slot was released that was never taken"
    );
}

#[test]
fn dropping_a_ticket_abandons_the_outcome_and_never_the_call() {
    let fixture = Fixture::new();
    let scheduler = fixture.scheduler(vec![eraseit(Immediate(
        "fixture.unwatched",
        RiskLevel::Observe,
    ))]);

    let workspace = fixture.workspace("unwatched");
    let step = fixture.run(&workspace);
    let call = fixture.call(&step, "fixture.unwatched");

    drop(submit(&scheduler, call, &workspace, RiskLevel::Observe));

    until("the abandoned call reaching a terminal state", || {
        fixture.state(call).is_terminal()
    });
    assert_eq!(fixture.state(call), ToolCallState::Succeeded);
}

// ---------------------------------------------------------------------------
// Shutdown
// ---------------------------------------------------------------------------

#[test]
fn shutdown_cancels_in_flight_work_joins_its_workers_and_refuses_more() {
    let fixture = Fixture::new();
    let (writes, gate) = Gated::new("fixture.shutdown", RiskLevel::WorkspaceWrite);
    let scheduler = fixture.scheduler(vec![eraseit(writes)]);

    let workspace = fixture.workspace("stopping");
    let step = fixture.run(&workspace);
    let running = fixture.call(&step, "fixture.shutdown");
    let in_flight = submit(&scheduler, running, &workspace, RiskLevel::WorkspaceWrite);
    gate.wait_for(1);
    let queued = fixture.call(&step, "fixture.shutdown");
    let waiting = submit(&scheduler, queued, &workspace, RiskLevel::WorkspaceWrite);

    let report = scheduler.shutdown(PATIENCE);
    assert_eq!(
        (report.cancelled().running(), report.cancelled().queued()),
        (1, 1)
    );
    assert!(
        report.is_clean(),
        "a worker outlived the deadline: {report:?}"
    );
    assert_eq!(report.joined(), 1);

    assert_eq!(fixture.state(running), ToolCallState::Cancelled);
    assert_eq!(fixture.state(queued), ToolCallState::Cancelled);
    assert_eq!(
        settled(in_flight).unwrap().outcome(),
        &CallOutcome::Cancelled
    );
    assert_eq!(settled(waiting).unwrap().outcome(), &CallOutcome::Cancelled);

    let refused = fixture.call(&step, "fixture.shutdown");
    let error = scheduler
        .submit(ScheduledCall::new(
            refused,
            workspace,
            RiskLevel::WorkspaceWrite,
            Cancellation::default(),
        ))
        .unwrap_err();
    assert_eq!(error.kind(), "scheduler_shutting_down");
    assert_eq!(error.call(), refused);
    assert!(scheduler.snapshot().shutting_down());

    // Idempotent: a second shutdown reports nothing left rather than waiting
    // out the deadline again.
    let again = scheduler.shutdown(PATIENCE);
    assert_eq!(again.cancelled(), super::CancelReport::default());
    assert!(again.is_clean());
}

#[test]
fn shutdown_wakes_a_producer_parked_on_a_full_queue() {
    let fixture = Fixture::new();
    let (writes, gate) = Gated::new("fixture.parked", RiskLevel::WorkspaceWrite);
    let scheduler = fixture.scheduler(vec![eraseit(writes)]);

    let workspace = fixture.workspace("parked");
    let step = fixture.run(&workspace);
    let running = fixture.call(&step, "fixture.parked");
    submit(&scheduler, running, &workspace, RiskLevel::WorkspaceWrite);
    gate.wait_for(1);
    for _ in 0..WORKSPACE_QUEUE_CAPACITY {
        let call = fixture.call(&step, "fixture.parked");
        submit(&scheduler, call, &workspace, RiskLevel::WorkspaceWrite);
    }

    let overflowing = fixture.call(&step, "fixture.parked");
    let producer = {
        let scheduler = Arc::clone(&scheduler);
        let workspace = workspace.clone();
        thread::spawn(move || {
            scheduler.submit(ScheduledCall::new(
                overflowing,
                workspace,
                RiskLevel::WorkspaceWrite,
                Cancellation::default(),
            ))
        })
    };
    until("the producer parking on the full queue", || {
        scheduler.snapshot().workspaces()[0].queued() == WORKSPACE_QUEUE_CAPACITY
    });

    // A producer waiting for room that will never be made has to be told,
    // rather than left holding a wait with no end.
    scheduler.shutdown(PATIENCE);
    let error = producer.join().unwrap().unwrap_err();
    assert_eq!(error.kind(), "scheduler_shutting_down");
    gate.release();
}

// ---------------------------------------------------------------------------
// Concurrency stress
// ---------------------------------------------------------------------------

/// Occupancy of one workspace, as the tools running in it observe it.
#[derive(Default)]
struct Occupancy {
    readers: usize,
    writers: usize,
    peak_readers: usize,
    violations: Vec<String>,
}

/// What the running tools saw, keyed by the workspace root they ran against.
///
/// The invariants are asserted from *inside* the tool bodies rather than from
/// the scheduler's own snapshot, because a snapshot is assembled one workspace
/// at a time and would be checking the accounting against itself.
#[derive(Clone, Default)]
struct Witness(Arc<Mutex<BTreeMap<std::path::PathBuf, Occupancy>>>);

impl Witness {
    fn enter(&self, root: &std::path::Path, mutating: bool) {
        let mut seen = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let occupancy = seen.entry(root.to_path_buf()).or_default();
        if mutating {
            if occupancy.writers > 0 || occupancy.readers > 0 {
                occupancy.violations.push(format!(
                    "a mutation started beside {} reader(s) and {} writer(s)",
                    occupancy.readers, occupancy.writers
                ));
            }
            occupancy.writers += 1;
        } else {
            if occupancy.writers > 0 {
                occupancy
                    .violations
                    .push("a read started while a mutation was running".to_owned());
            }
            occupancy.readers += 1;
            occupancy.peak_readers = occupancy.peak_readers.max(occupancy.readers);
            if occupancy.readers > WORKSPACE_READ_CONCURRENCY {
                occupancy
                    .violations
                    .push(format!("{} reads ran at once", occupancy.readers));
            }
        }
    }

    fn leave(&self, root: &std::path::Path, mutating: bool) {
        let mut seen = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let occupancy = seen.entry(root.to_path_buf()).or_default();
        if mutating {
            occupancy.writers -= 1;
        } else {
            occupancy.readers -= 1;
        }
    }

    fn violations(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .flat_map(|occupancy| occupancy.violations.clone())
            .collect()
    }

    fn peak_readers(&self) -> usize {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .map(|occupancy| occupancy.peak_readers)
            .max()
            .unwrap_or(0)
    }
}

/// A tool that records what it found running beside it.
struct Witnessing {
    id: &'static str,
    risk: RiskLevel,
    witness: Witness,
}

impl Tool for Witnessing {
    type Input = Empty;
    type Output = Ran;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse(self.id, "1.0.0").unwrap(),
            "Witnessing fixture",
            "Records what was running beside it in its workspace.",
            self.risk,
        )
        .within(PATIENCE * 3)
    }

    fn execute(&self, _input: Empty, context: &mut ExecutionContext) -> Result<Ran, ToolError> {
        let root = context.workspace_root().to_path_buf();
        let mutating = self.risk.mutates_state();
        self.witness.enter(&root, mutating);
        // Long enough that overlapping calls actually overlap, short enough
        // that the whole stress test stays quick. Nothing is asserted about
        // the duration itself.
        thread::sleep(Duration::from_millis(1));
        self.witness.leave(&root, mutating);
        Ok(Ran {
            call: context.call().to_string(),
        })
    }
}

#[test]
fn mixed_traffic_from_many_producers_never_breaks_an_admission_rule() {
    const PRODUCERS: usize = 6;
    const WORKSPACES: usize = 3;
    const PER_PRODUCER: usize = 24;

    let fixture = Fixture::new();
    let witness = Witness::default();
    let scheduler = fixture.scheduler(vec![
        eraseit(Witnessing {
            id: "fixture.stress_read",
            risk: RiskLevel::Observe,
            witness: witness.clone(),
        }),
        eraseit(Witnessing {
            id: "fixture.stress_write",
            risk: RiskLevel::WorkspaceWrite,
            witness: witness.clone(),
        }),
    ]);

    let workspaces = (0..WORKSPACES)
        .map(|index| {
            let workspace = fixture.workspace(&format!("stress-{index}"));
            let step = fixture.run(&workspace);
            (workspace, step)
        })
        .collect::<Vec<_>>();

    // Calls are recorded up front so the producers do nothing but submit, and
    // the store is not the thing being measured.
    let mut planned: Vec<Vec<(ToolCallId, WorkspaceKey, RiskLevel)>> = vec![Vec::new(); PRODUCERS];
    for producer in 0..PRODUCERS {
        for index in 0..PER_PRODUCER {
            let (workspace, step) = &workspaces[(producer + index) % WORKSPACES];
            // Roughly one mutation in three, interleaved rather than grouped.
            let mutating = index % 3 == 0;
            let (tool, risk) = if mutating {
                ("fixture.stress_write", RiskLevel::WorkspaceWrite)
            } else {
                ("fixture.stress_read", RiskLevel::Observe)
            };
            planned[producer].push((fixture.call(step, tool), workspace.clone(), risk));
        }
    }

    let producers = planned
        .into_iter()
        .map(|batch| {
            let scheduler = Arc::clone(&scheduler);
            thread::spawn(move || {
                batch
                    .into_iter()
                    .map(|(call, workspace, risk)| submit(&scheduler, call, &workspace, risk))
                    .collect::<Vec<_>>()
            })
        })
        .collect::<Vec<_>>();

    for producer in producers {
        for ticket in producer.join().unwrap() {
            assert_eq!(succeeded(ticket), ToolCallState::Succeeded);
        }
    }

    assert_eq!(
        witness.violations(),
        Vec::<String>::new(),
        "the admission rules were broken under load"
    );
    assert!(
        witness.peak_readers() > 1,
        "no two reads ever overlapped, so this run proved nothing about concurrency"
    );

    // Everything is done, so every workspace is idle and forgotten.
    until("the scheduler settling", || {
        scheduler.snapshot().workspaces().is_empty()
    });
    assert_eq!(scheduler.snapshot().processes().in_use(), 0);
}

// ---------------------------------------------------------------------------
// The published namespaces
// ---------------------------------------------------------------------------

#[test]
fn schedule_error_kinds_round_trip_and_do_not_collide_with_the_others() {
    let call = ToolCallId::new();
    let cases = [
        (ScheduleError::Shutdown { call }, "scheduler_shutting_down"),
        (ScheduleError::WorkerLost { call }, "worker_lost"),
    ];

    let kinds = cases.iter().map(|(_, kind)| *kind).collect::<Vec<_>>();
    assert_eq!(kinds, ScheduleError::KINDS);
    for (error, expected) in cases {
        assert_eq!(error.kind(), expected, "unexpected kind for {error:?}");
        assert_eq!(error.call(), call);
    }

    // A wrapped executor fault answers to the executor's spelling, not to a
    // second name invented here — and the union is what a consumer matches on.
    let wrapped = ScheduleError::Execution {
        call,
        source: Box::new(ExecutionError::NotDispatchable {
            call,
            state: ToolCallState::Running,
            expected: ToolCallState::Pending,
        }),
    };
    assert_eq!(wrapped.kind(), "not_dispatchable");
    assert!(ScheduleError::kinds().contains(&wrapped.kind()));

    for kind in ScheduleError::KINDS {
        assert!(
            !ExecutionError::KINDS.contains(kind),
            "{kind} is claimed by two namespaces"
        );
        assert!(
            !crate::tool::InvocationError::kinds().contains(kind),
            "{kind} is claimed by two namespaces"
        );
    }
}

#[test]
fn every_bound_in_the_module_is_a_named_nonzero_constant() {
    // The release gate is that no path buffers without limit. Each of these is
    // the capacity of exactly one queue or slot pool, and a zero would either
    // deadlock the scheduler or make a bound meaningless.
    for (name, capacity) in [
        ("WORKSPACE_QUEUE_CAPACITY", WORKSPACE_QUEUE_CAPACITY),
        ("WORKSPACE_READ_CONCURRENCY", WORKSPACE_READ_CONCURRENCY),
        ("MAX_PROCESS_CONCURRENCY", MAX_PROCESS_CONCURRENCY),
        ("OUTCOME_CAPACITY", OUTCOME_CAPACITY),
    ] {
        assert!(capacity > 0, "{name} is zero");
    }
    assert_eq!(
        OUTCOME_CAPACITY, 1,
        "a ticket carries one call, which reaches one terminal state"
    );
}

#[test]
fn a_process_limit_of_zero_is_raised_rather_than_making_a_tool_unrunnable() {
    let fixture = Fixture::new();
    let scheduler = fixture.scheduler_with_process_limit(
        vec![eraseit(Immediate("fixture.none", RiskLevel::Observe))],
        0,
    );
    assert_eq!(scheduler.snapshot().processes().capacity(), 1);
}

// ---------------------------------------------------------------------------
// Process-backed scheduling
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod processes {
    use std::path::{Path, PathBuf};

    use harkness_test_fixtures::Fixture as ShimFixture;

    use super::*;
    use crate::tool::{Capture, EnvironmentName, ToolProcess};
    use crate::trust::{AllowlistedEnv, CommandSpec};

    /// A tool that declares it spawns children and then does.
    struct RunsAShim;

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct ShimInput {
        program: String,
    }

    #[derive(Debug, Deserialize, JsonSchema, Serialize)]
    #[serde(deny_unknown_fields)]
    struct ShimOutput {
        code: Option<i32>,
    }

    impl Tool for RunsAShim {
        type Input = ShimInput;
        type Output = ShimOutput;

        fn metadata(&self) -> ToolMetadata {
            ToolMetadata::new(
                ToolIdentity::parse("fixture.spawns", "1.0.0").unwrap(),
                "Process fixture",
                "Runs one shim so the process limit has something to bound.",
                RiskLevel::Execute,
            )
            .spawning_processes()
            .within(PATIENCE * 3)
        }

        fn execute(
            &self,
            input: ShimInput,
            context: &mut ExecutionContext,
        ) -> Result<ShimOutput, ToolError> {
            let cwd = context.resolve(".")?;
            let env = AllowlistedEnv::build(std::iter::empty::<&EnvironmentName>());
            let spec = CommandSpec::new(&input.program, Vec::new(), cwd, env)
                .map_err(ToolError::execution_failed)?;
            let output = ToolProcess::new(spec)
                .capture_stderr(Capture::Tail)
                .run(context)?;
            Ok(ShimOutput {
                code: output.code(),
            })
        }
    }

    /// A shim that announces itself and waits for the test to let it exit.
    fn waiting_shim(shims: &ShimFixture, index: usize, ready: &Path, go: &Path) -> String {
        shims
            .shim(
                &format!("waiting-{index}"),
                &format!(
                    "#!/bin/sh\n\
                     : > '{}'\n\
                     while [ ! -e '{}' ]; do sleep 0.01; done\n",
                    ready.join(index.to_string()).display(),
                    go.display()
                ),
            )
            .display()
            .to_string()
    }

    fn ready_count(ready: &Path) -> usize {
        std::fs::read_dir(ready).map(Iterator::count).unwrap_or(0)
    }

    #[test]
    fn the_global_process_limit_bounds_live_children_across_every_run() {
        const LIMIT: usize = 2;
        const SUBMITTED: usize = 5;

        let fixture = Fixture::new();
        let shims = ShimFixture::new();
        let ready = shims.directory("ready");
        let go = shims.root.path().join("go");
        let scheduler = fixture.scheduler_with_process_limit(vec![eraseit(RunsAShim)], LIMIT);

        // One workspace and one run each, so nothing but the process limit is
        // capable of holding any of them back.
        let mut tickets = Vec::new();
        for index in 0..SUBMITTED {
            let workspace = fixture.workspace(&format!("spawner-{index}"));
            let step = fixture.run(&workspace);
            let program = waiting_shim(&shims, index, &ready, &go);
            let call = ToolCall::new(
                &step,
                "fixture.spawns",
                "",
                json!({"program": program}),
                at(3),
            );
            fixture.store.insert_tool_call(&call).unwrap();
            tickets.push(submit(
                &scheduler,
                call.id(),
                &workspace,
                RiskLevel::Execute,
            ));
        }

        until("the process limit filling", || ready_count(&ready) == LIMIT);
        // Deterministic rather than observational: no child can exit before
        // `go` exists, so no further slot can free and no third child can
        // start. The scheduler's own accounting has to agree.
        let snapshot = scheduler.snapshot();
        assert_eq!(snapshot.processes().in_use(), LIMIT);
        assert_eq!(snapshot.processes().available(), 0);
        assert_eq!(snapshot.running(), LIMIT);
        assert_eq!(snapshot.queued(), SUBMITTED - LIMIT);
        assert_eq!(
            ready_count(&ready),
            LIMIT,
            "more children were alive at once than the limit allows"
        );

        std::fs::write(&go, b"go").unwrap();
        for ticket in tickets {
            assert_eq!(succeeded(ticket), ToolCallState::Succeeded);
        }
        assert_eq!(ready_count(&ready), SUBMITTED);
        assert_eq!(scheduler.snapshot().processes().in_use(), 0);
    }

    /// A shim that announces itself and exits immediately.
    fn quick_shim(shims: &ShimFixture, index: usize, ran: &Path) -> String {
        shims
            .shim(
                &format!("quick-{index}"),
                &format!(
                    "#!/bin/sh\n: >> '{}'\n",
                    ran.join(index.to_string()).display()
                ),
            )
            .display()
            .to_string()
    }

    #[test]
    fn one_process_slot_is_shared_between_workspaces_rather_than_captured() {
        const WORKSPACES: usize = 3;
        const EACH: usize = 6;

        let fixture = Fixture::new();
        let shims = ShimFixture::new();
        let ran = shims.directory("ran");
        // A single slot, so every one of these calls contends for the same
        // global resource and nothing else can hide a scheduling bias.
        let scheduler = fixture.scheduler_with_process_limit(vec![eraseit(RunsAShim)], 1);

        let mut tickets = Vec::new();
        for index in 0..WORKSPACES {
            let workspace = fixture.workspace(&format!("sharing-{index}"));
            let step = fixture.run(&workspace);
            for attempt in 0..EACH {
                let program = quick_shim(&shims, index * EACH + attempt, &ran);
                let call = ToolCall::new(
                    &step,
                    "fixture.spawns",
                    "",
                    json!({"program": program}),
                    at(3),
                );
                fixture.store.insert_tool_call(&call).unwrap();
                tickets.push(submit(
                    &scheduler,
                    call.id(),
                    &workspace,
                    RiskLevel::Execute,
                ));
            }
        }

        // Every call completes. Without a rotating hand-off the workspace that
        // released a slot would reclaim it before its neighbours were offered
        // it, and a workspace with a steady supply of process-backed calls
        // would keep the others from ever starting — so this finishing at all
        // is the assertion, and `PATIENCE` is what makes starvation a failure
        // rather than a hang.
        for ticket in tickets {
            assert_eq!(succeeded(ticket), ToolCallState::Succeeded);
        }
        assert_eq!(ready_count(&ran), WORKSPACES * EACH);
        assert_eq!(scheduler.snapshot().processes().in_use(), 0);
    }

    #[test]
    fn a_call_that_spawns_nothing_takes_no_process_slot() {
        let fixture = Fixture::new();
        // Read-only, so the workspace admits both at once and a process slot is
        // the only thing that could hold either back. The tool declares no
        // spawning, which is what the slot is taken from — nothing infers it
        // from the risk level.
        let (reads, gate) = Gated::new("fixture.in_process", RiskLevel::Observe);
        // One slot in total: were an in-process call to take it, the second
        // would never be dispatched and this test would time out.
        let scheduler = fixture.scheduler_with_process_limit(vec![eraseit(reads)], 1);

        let workspace = fixture.workspace("in-process");
        let step = fixture.run(&workspace);
        let tickets = (0..2)
            .map(|_| {
                let call = fixture.call(&step, "fixture.in_process");
                submit(&scheduler, call, &workspace, RiskLevel::Observe)
            })
            .collect::<Vec<_>>();

        gate.wait_for(2);
        assert_eq!(scheduler.snapshot().processes().in_use(), 0);

        gate.release();
        for ticket in tickets {
            assert_eq!(succeeded(ticket), ToolCallState::Succeeded);
        }
    }

    /// A shim whose background helper keeps writing until its group is killed.
    fn orphaning_shim(shims: &ShimFixture, activity: &Path) -> String {
        shims
            .shim(
                "orphaning",
                &format!(
                    "#!/bin/sh\n\
                     (while true; do printf x >> '{}'; sleep 0.01; done) 2>/dev/null &\n\
                     wait\n",
                    activity.display()
                ),
            )
            .display()
            .to_string()
    }

    fn activity_len(activity: &Path) -> u64 {
        std::fs::metadata(activity)
            .map(|meta| meta.len())
            .unwrap_or(0)
    }

    #[test]
    fn shutdown_leaves_no_child_process_group_behind() {
        let fixture = Fixture::new();
        let shims = ShimFixture::new();
        let activity = PathBuf::from(shims.root.path()).join("shutdown-activity");
        let scheduler = fixture.scheduler(vec![eraseit(RunsAShim)]);

        let workspace = fixture.workspace("orphans");
        let step = fixture.run(&workspace);
        let program = orphaning_shim(&shims, &activity);
        let call = ToolCall::new(
            &step,
            "fixture.spawns",
            "",
            json!({"program": program}),
            at(3),
        );
        fixture.store.insert_tool_call(&call).unwrap();
        let ticket = submit(&scheduler, call.id(), &workspace, RiskLevel::Execute);
        until("the child's helper starting", || {
            activity_len(&activity) > 0
        });

        let report = scheduler.shutdown(PATIENCE);
        assert!(
            report.is_clean(),
            "a worker outlived the deadline: {report:?}"
        );
        assert_eq!(report.cancelled().running(), 1);
        assert_eq!(settled(ticket).unwrap().outcome(), &CallOutcome::Cancelled);
        assert_eq!(scheduler.snapshot().processes().in_use(), 0);

        // The helper is not the direct child, so only killing the whole group
        // stops it. Its own writes are the evidence.
        let at_shutdown = activity_len(&activity);
        thread::sleep(Duration::from_millis(150));
        assert_eq!(
            activity_len(&activity),
            at_shutdown,
            "a helper survived the scheduler it was started under"
        );
    }

    #[test]
    #[ignore = "latency target; meaningful only in a release build"]
    fn cancelling_a_run_stops_a_cooperative_child_within_the_promised_latency() {
        let fixture = Fixture::new();
        let shims = ShimFixture::new();
        let activity = PathBuf::from(shims.root.path()).join("latency-activity");
        let scheduler = fixture.scheduler(vec![eraseit(RunsAShim)]);

        let workspace = fixture.workspace("latency");
        let step = fixture.run(&workspace);
        let program = orphaning_shim(&shims, &activity);
        let call = ToolCall::new(
            &step,
            "fixture.spawns",
            "",
            json!({"program": program}),
            at(3),
        );
        fixture.store.insert_tool_call(&call).unwrap();
        let ticket = submit(&scheduler, call.id(), &workspace, RiskLevel::Execute);
        until("the child's helper starting", || {
            activity_len(&activity) > 0
        });

        let began = Instant::now();
        scheduler.cancel_run(step.run_id());
        assert_eq!(settled(ticket).unwrap().outcome(), &CallOutcome::Cancelled);
        let elapsed = began.elapsed();

        assert!(
            elapsed < Duration::from_millis(250),
            "the cancellation chain took {elapsed:?}"
        );
    }
}
