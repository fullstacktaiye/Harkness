//! Admission, dispatch, cancellation, and shutdown.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use harkness_git::Cancellation;

use crate::domain::{RunId, ToolCallId};
use crate::tool::{
    CompletedCall, ExecutionError, RiskLevel, ToolError, ToolExecutor, ToolId, ToolVersion,
    WorkspaceMetadata,
};

use super::ticket::{CallTicket, Report, outcome_channel};
use super::{ProcessSlots, ScheduleError, ScheduleSnapshot, WorkspaceKey, WorkspaceLoad};

/// How many calls one workspace may hold before a submitter is made to wait.
///
/// Deep enough that an agent planning a whole step's worth of work does not
/// serialize itself against the scheduler, shallow enough that a queue is
/// something a person can be shown. A producer that reaches it is slowed rather
/// than buffered: the alternative to backpressure is an unbounded queue, and a
/// runtime that buffers without limit reports the past while consuming memory
/// in the present.
pub const WORKSPACE_QUEUE_CAPACITY: usize = 64;

/// How many [`Observe`](RiskLevel::Observe) calls may run at once per workspace.
///
/// Reads of one worktree do not interfere, so the cap is not a safety property
/// — it exists so that a burst of reads cannot occupy every thread and every
/// process slot Harkness has. Four is the same order as the process limit
/// below, which keeps a workspace of process-backed reads from being throttled
/// by whichever bound happens to be lower.
pub const WORKSPACE_READ_CONCURRENCY: usize = 4;

/// The most child processes Harkness will have alive at once, across all runs.
///
/// The effective limit is this or [`available_parallelism`][a], whichever is
/// smaller, so a single-core machine is not asked to interleave four builds.
/// It is a *global* bound rather than a per-workspace one because the resource
/// being protected — process table entries, memory, the machine's
/// responsiveness — is global; a per-workspace limit multiplied by the number
/// of open worktrees is not a limit.
///
/// [a]: std::thread::available_parallelism
pub const MAX_PROCESS_CONCURRENCY: usize = 4;

/// How long [`Scheduler::drop`] gives in-flight work to stop.
///
/// Comfortably more than the executor's own grace period plus the time a
/// process group takes to die, so an ordinary drop reports a clean stop; short
/// enough that a scheduler dropped by a front end on its way out does not look
/// like a hang.
pub const DEFAULT_SHUTDOWN_DEADLINE: Duration = Duration::from_secs(5);

/// How often a submitter blocked on a full queue re-checks for shutdown.
///
/// The queue notifies on every removal, so this is not the wake-up path — it is
/// the guarantee that a producer parked at the moment shutdown began still
/// learns about it.
const BACKPRESSURE_POLL: Duration = Duration::from_millis(50);

/// How often [`Scheduler::shutdown`] re-checks whether the workers have ended.
const SHUTDOWN_POLL: Duration = Duration::from_millis(10);

/// A recorded tool call, ready to be scheduled against one workspace.
///
/// Deliberately thin. Everything the scheduler can derive, it derives: the run
/// and step come from the recorded call, and whether the tool spawns processes
/// comes from its registered descriptor. What a caller supplies is what only a
/// caller knows — which workspace the work belongs to, how this particular
/// invocation was classified, and which token stops it.
#[derive(Clone, Debug)]
pub struct ScheduledCall {
    call: ToolCallId,
    workspace: WorkspaceKey,
    risk: RiskLevel,
    cancellation: Cancellation,
    admission: Admission,
    workspace_metadata: Option<WorkspaceMetadata>,
}

/// How a scheduled call is to be started once it reaches the front.
#[derive(Clone, Debug)]
enum Admission {
    /// An authorized call nobody had to decide on.
    Pending,
    /// A call held for a decision, resumed by the decision itself.
    Approved { decided_by: String },
}

impl ScheduledCall {
    /// Schedules `call` against `workspace`, classified at `risk`.
    ///
    /// `risk` is the classification of *this invocation*
    /// ([`classify_request`](crate::trust::classify_request)), not the tool's
    /// declared level. The scheduler takes whichever of the two is higher, so a
    /// submission can say that a particular call is more consequential than its
    /// tool usually is — a write that turned out to leave the workspace — and
    /// can never say it is less.
    #[must_use]
    pub fn new(
        call: ToolCallId,
        workspace: WorkspaceKey,
        risk: RiskLevel,
        cancellation: Cancellation,
    ) -> Self {
        Self {
            call,
            workspace,
            risk,
            cancellation,
            admission: Admission::Pending,
            workspace_metadata: None,
        }
    }

    /// Attaches authoritative catalog fields for tools that inspect identity.
    ///
    /// The metadata must name both the same project and the same canonical root
    /// as the scheduler key, so it cannot widen or relabel the workspace being
    /// serialized.
    pub fn with_workspace_metadata(
        mut self,
        metadata: WorkspaceMetadata,
    ) -> Result<Self, ToolError> {
        if metadata.project_id() != self.workspace.project_id() {
            return Err(ToolError::ForbiddenPath {
                path: metadata.canonical_root().to_path_buf(),
                reason: "workspace metadata names a different catalog project".to_owned(),
            });
        }
        let canonical = std::fs::canonicalize(metadata.canonical_root()).map_err(|error| {
            ToolError::ForbiddenPath {
                path: metadata.canonical_root().to_path_buf(),
                reason: format!("the catalog workspace root is unavailable: {error}"),
            }
        })?;
        if canonical != self.workspace.canonical_root() {
            return Err(ToolError::ForbiddenPath {
                path: metadata.canonical_root().to_path_buf(),
                reason: "workspace metadata names a different canonical root".to_owned(),
            });
        }
        self.workspace_metadata = Some(metadata);
        Ok(self)
    }

    /// Schedules a call held at `awaiting_approval`, resumed by its decision.
    ///
    /// Approval-gated work never passes through `pending`, so it cannot be
    /// submitted as an ordinary call and started later; the decision *is* the
    /// dispatch. It still queues behind the same slots as everything else,
    /// which matters most for exactly this work: an approved force push is the
    /// last thing that should skip a workspace's mutation slot.
    #[must_use]
    pub fn approved_by(mut self, decided_by: impl Into<String>) -> Self {
        self.admission = Admission::Approved {
            decided_by: decided_by.into(),
        };
        self
    }

    /// The recorded call this submission is about.
    #[must_use]
    pub const fn call(&self) -> ToolCallId {
        self.call
    }

    /// The workspace its mutations are serialized against.
    #[must_use]
    pub const fn workspace(&self) -> &WorkspaceKey {
        &self.workspace
    }

    /// How this invocation was classified.
    #[must_use]
    pub const fn risk(&self) -> RiskLevel {
        self.risk
    }
}

/// One admitted call's place in its workspace's running set.
///
/// A sequence number rather than the call's own identity, because only one of
/// the two is unique by construction. [`Scheduler::submit`] now refuses a
/// second claim on one recorded call, so a duplicate key should be
/// unreachable — this keeps it *unrepresentable*. Keyed by `ToolCallId`, two
/// admissions of one call would share an entry and the first completion would
/// free both: a process slot released that was never taken, and a workspace
/// reported idle while a call was still running in it.
type Dispatch = u64;

/// What one call is waiting for, and what to do when it gets it.
struct Waiting {
    dispatch: Dispatch,
    call: ToolCallId,
    run: RunId,
    root: PathBuf,
    /// Higher of the classified and the declared level; decides the slot.
    risk: RiskLevel,
    /// Declared by the tool, and the only thing that takes a process slot.
    spawns_processes: bool,
    cancellation: Cancellation,
    admission: Admission,
    workspace_metadata: Option<WorkspaceMetadata>,
    report: Report,
}

/// What one running call is holding.
struct Running {
    run: RunId,
    cancellation: Cancellation,
    mutating: bool,
    holds_process: bool,
}

/// One workspace's queue and running set, behind one mutex.
struct Workspace {
    key: WorkspaceKey,
    state: Mutex<WorkspaceState>,
    /// Notified whenever the queue shrinks, waking a blocked submitter.
    room: Condvar,
}

#[derive(Default)]
struct WorkspaceState {
    queue: VecDeque<Waiting>,
    running: BTreeMap<Dispatch, Running>,
    /// Submitters parked on a full queue, waiting to join it.
    ///
    /// Counted because a parked producer holds *neither* lock —
    /// [`Condvar::wait_timeout`] releases the workspace mutex for the duration
    /// of the wait — so without this it is invisible, and a workspace it is
    /// about to enqueue into looks idle to [`Inner::forget_idle`]. Evicting one
    /// there would let the producer wake and push into an orphan: the next
    /// submission for the same key would build a *second* `Workspace` with an
    /// empty running set, and one worktree would have two mutation slots.
    waiting: usize,
}

impl WorkspaceState {
    /// Whether the workspace's single mutation slot is held.
    fn mutating(&self) -> bool {
        self.running.values().any(|running| running.mutating)
    }

    /// Read-only calls executing right now.
    fn reads(&self) -> usize {
        self.running
            .values()
            .filter(|running| !running.mutating)
            .count()
    }

    /// Whether nothing is queued, running, or waiting to be queued.
    fn is_idle(&self) -> bool {
        self.queue.is_empty() && self.running.is_empty() && self.waiting == 0
    }
}

/// A counting semaphore that is never waited on.
///
/// Acquisition is a *try*, and a call that cannot have a slot stays queued
/// rather than occupying a thread to wait for one. That is what keeps the
/// process limit from becoming a second way to deadlock: no thread ever blocks
/// holding a workspace slot while hoping for a process slot, so the only thing
/// a full semaphore delays is dispatch.
struct ProcessLimit {
    capacity: usize,
    in_use: Mutex<usize>,
}

impl ProcessLimit {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            in_use: Mutex::new(0),
        }
    }

    fn try_acquire(&self) -> bool {
        let mut in_use = self
            .in_use
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *in_use >= self.capacity {
            return false;
        }
        *in_use += 1;
        true
    }

    fn release(&self) {
        let mut in_use = self
            .in_use
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *in_use = in_use.saturating_sub(1);
    }

    fn slots(&self) -> ProcessSlots {
        let in_use = self
            .in_use
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        ProcessSlots::new(*in_use, self.capacity)
    }
}

/// How many worker threads are alive, so shutdown can wait for zero.
///
/// Both counters are leaves in the lock order: a workspace lock is held across
/// [`enter`](Self::enter) — deliberately, so a call `stop` can see is one
/// shutdown must wait for — but nothing here ever takes a workspace lock, so
/// the order **workspace → workers** cannot be inverted.
#[derive(Default)]
struct Workers {
    live: Mutex<usize>,
    idle: Condvar,
    handles: Mutex<Vec<JoinHandle<()>>>,
}

impl Workers {
    fn enter(&self) {
        *self.live.lock().unwrap_or_else(|error| error.into_inner()) += 1;
    }

    fn leave(&self) {
        let mut live = self.live.lock().unwrap_or_else(|error| error.into_inner());
        *live = live.saturating_sub(1);
        if *live == 0 {
            self.idle.notify_all();
        }
    }

    /// Records a handle, discarding those whose threads have already ended.
    ///
    /// Sweeping here rather than from the workers themselves avoids the race a
    /// self-removing worker has with the thread that is still holding its
    /// handle, and bounds the vector by the number of *live* workers plus
    /// whatever finished since the last dispatch.
    fn track(&self, handle: JoinHandle<()>) {
        let mut handles = self
            .handles
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        handles.retain(|tracked| !tracked.is_finished());
        handles.push(handle);
    }

    /// Waits for every worker to report completion, or for `deadline` to pass.
    fn wait_until_idle(&self, deadline: Duration) -> bool {
        let until = Instant::now() + deadline;
        let mut live = self.live.lock().unwrap_or_else(|error| error.into_inner());
        while *live > 0 {
            let Some(remaining) = until.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (guard, timeout) = self
                .idle
                .wait_timeout(live, remaining.min(SHUTDOWN_POLL))
                .unwrap_or_else(|error| error.into_inner());
            live = guard;
            if timeout.timed_out() && Instant::now() >= until {
                return *live == 0;
            }
        }
        true
    }

    fn live(&self) -> usize {
        *self.live.lock().unwrap_or_else(|error| error.into_inner())
    }
}

/// Everything the scheduler owns, shared with every worker it spawns.
struct Inner {
    executor: ToolExecutor,
    workspaces: Mutex<BTreeMap<WorkspaceKey, Arc<Workspace>>>,
    processes: ProcessLimit,
    workers: Workers,
    /// Source of the sequence numbers a workspace's running set is keyed by.
    dispatched: AtomicU64,
    /// Rotates which workspace a freed process slot is offered to first.
    sweep: AtomicUsize,
    /// Every recorded call this scheduler currently has a claim on.
    ///
    /// A leaf in the lock order — nothing is taken while it is held — so it
    /// composes with the rest without adding an edge to reason about.
    claimed: Mutex<BTreeSet<ToolCallId>>,
    shutting_down: AtomicBool,
}

/// How many calls one run had stopped, and how they were stopped.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CancelReport {
    queued: usize,
    running: usize,
}

impl CancelReport {
    /// Calls removed from a queue and recorded `cancelled` without dispatch.
    #[must_use]
    pub const fn queued(&self) -> usize {
        self.queued
    }

    /// Running calls whose tokens were tripped.
    #[must_use]
    pub const fn running(&self) -> usize {
        self.running
    }
}

/// What one shutdown stopped, and whether it finished in time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShutdownReport {
    cancelled: CancelReport,
    joined: usize,
    outstanding: usize,
}

impl ShutdownReport {
    /// What was cancelled on the way down.
    #[must_use]
    pub const fn cancelled(&self) -> CancelReport {
        self.cancelled
    }

    /// Worker threads joined before the deadline.
    #[must_use]
    pub const fn joined(&self) -> usize {
        self.joined
    }

    /// Workers still running when the deadline passed.
    ///
    /// Non-zero means a tool body outlived both its cancellation token and the
    /// executor's grace period. Its *children* are gone regardless — a process
    /// group is killed, not asked — but the thread was abandoned rather than
    /// joined, which a caller about to exit the process may want to report.
    #[must_use]
    pub const fn outstanding(&self) -> usize {
        self.outstanding
    }

    /// Whether every worker ended within the deadline.
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.outstanding == 0
    }
}

/// Serializes mutations per workspace, bounds reads and child processes, and
/// dispatches what it admits to the [`ToolExecutor`].
///
/// # The three admission rules
///
/// Every scheduled call joins the FIFO queue of exactly one
/// [`WorkspaceKey`], and only the call at the *front* of that queue is ever
/// considered. Nothing is scanned past, which is the whole of the fairness
/// story: a mutation at the front stops later reads from being admitted, so a
/// continuous stream of reads cannot starve it.
///
/// The front call is dispatched when all of these hold:
///
/// 1. **The mutation slot.** A call above [`RiskLevel::Observe`] needs the
///    workspace completely idle; a call at `Observe` needs no mutation running.
///    At most one mutating call per workspace runs at a time, which is a safety
///    property and not a throughput choice — two concurrent mutations of one
///    worktree interleave index writes.
/// 2. **The read cap.** At most [`WORKSPACE_READ_CONCURRENCY`] `Observe` calls
///    run per workspace.
/// 3. **A process slot,** if the tool declared that it
///    [spawns processes](crate::tool::ToolMetadata::spawning_processes). At
///    most [`MAX_PROCESS_CONCURRENCY`] — or `available_parallelism`, whichever
///    is lower — children are alive across every run. A call that spawns
///    nothing takes no slot.
///
/// # Lock ordering
///
/// Three locks live here and are taken in one order, never the reverse:
/// **workspace map → one workspace → process limit.** No two workspaces are
/// ever locked at once, and no lock is held across an executor call, a store
/// write, or a child wait — dispatch decides under the lock and *spawns*
/// outside it. That is what lets one slow tool leave every other workspace
/// dispatching normally.
///
/// This runtime-level serialization nests outside the ones below it. The full
/// order across the workspace remains **scheduler workspace slot → repository
/// lock → catalog lock**, and the scheduler never calls into catalog or Git
/// code at all, so it cannot violate the two beneath it.
///
/// # Backpressure, never dropping
///
/// Every queue here has a named capacity: [`WORKSPACE_QUEUE_CAPACITY`] for
/// submissions and [`OUTCOME_CAPACITY`](super::OUTCOME_CAPACITY) for a
/// ticket's one result, above the executor's own bounded progress channel. A
/// full submission queue *blocks* its producer. Nothing is discarded to make
/// room, because a dropped call is a run whose history omits work that was
/// asked for.
///
/// # Cancellation and shutdown
///
/// [`cancel_run`](Self::cancel_run) sweeps a run's queued calls out of every
/// queue and records them `cancelled` without dispatching them, and trips the
/// token of each of its running calls. From there the chain is the executor's:
/// the token reaches the call's own, a cooperative body returns, and a
/// process-backed one has its child's whole process group killed.
/// [`shutdown`](Self::shutdown) does the same for every run and then waits for
/// the workers, so no child process group outlives the application.
pub struct Scheduler {
    inner: Arc<Inner>,
}

impl Scheduler {
    /// Schedules calls onto `executor`, with the default process limit.
    ///
    /// The limit is [`MAX_PROCESS_CONCURRENCY`] or the machine's available
    /// parallelism, whichever is smaller.
    #[must_use]
    pub fn new(executor: ToolExecutor) -> Self {
        Self::with_process_limit(executor, default_process_limit())
    }

    /// Schedules calls onto `executor` with an explicit process limit.
    ///
    /// A limit of zero would admit no process-backed call at all, so it is
    /// raised to one: the bound exists to stop a machine being swamped, not to
    /// make a class of tool unrunnable, and a scheduler that silently never
    /// dispatches is worse than one that runs children one at a time.
    #[must_use]
    pub fn with_process_limit(executor: ToolExecutor, processes: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                executor,
                workspaces: Mutex::new(BTreeMap::new()),
                processes: ProcessLimit::new(processes),
                workers: Workers::default(),
                dispatched: AtomicU64::new(0),
                sweep: AtomicUsize::new(0),
                claimed: Mutex::new(BTreeSet::new()),
                shutting_down: AtomicBool::new(false),
            }),
        }
    }

    /// Accepts a call, blocking while its workspace's queue is full.
    ///
    /// Returns as soon as the call is *queued*, not when it runs: the
    /// [`CallTicket`] is how a caller waits, and holding one lets it be
    /// cancelled, rendered, or abandoned in the meantime.
    ///
    /// The recorded call is read here so that the run it belongs to and the
    /// tool it names are derived rather than asserted — a submission cannot
    /// misattribute work to another run's cancellation, and cannot understate a
    /// tool's declared risk to skip a mutation slot.
    ///
    /// One recorded call has at most one claim on it at a time. A second
    /// submission of a call still queued or running here is refused rather than
    /// admitted, because two claims would mean two workspace slots and two
    /// tickets for one row — and the executor's refusal of the duplicate
    /// arrives only *after* dispatch, by which point the loser has already
    /// occupied a mutation slot. The claim is released as soon as the call
    /// reaches a terminal state, so a genuine retry is never blocked.
    ///
    /// # Errors
    ///
    /// Returns [`ScheduleError::Shutdown`] when the scheduler is stopping,
    /// [`ScheduleError::AlreadyScheduled`] for a call this scheduler is already
    /// carrying, and [`ScheduleError::Execution`] wrapping a store fault when
    /// the recorded call cannot be read. A call naming a tool that is not
    /// registered is *not* an error here: it is queued, dispatched, and
    /// recorded as a failed call, because that is a fact about the run and
    /// belongs in its history.
    pub fn submit(&self, call: ScheduledCall) -> Result<CallTicket, ScheduleError> {
        let inner = &self.inner;
        if inner.is_shutting_down() {
            return Err(ScheduleError::Shutdown { call: call.call });
        }

        // Taken before anything else is done on this call's behalf, so a second
        // claim is refused rather than admitted and then unwound.
        inner.claim(call.call)?;
        let record = match inner.executor.store().load_tool_call(call.call) {
            Ok(record) => record,
            Err(source) => {
                inner.release(call.call);
                return Err(ScheduleError::Execution {
                    call: call.call,
                    source: Box::new(ExecutionError::from(source)),
                });
            }
        };
        let declared = inner.declared(record.tool_id(), record.tool_version());
        let (report, ticket) = outcome_channel(call.call);
        let waiting = Waiting {
            dispatch: inner.dispatched.fetch_add(1, Ordering::Relaxed),
            call: call.call,
            run: record.run_id(),
            root: call.workspace.canonical_root().to_path_buf(),
            // The higher of the two, so a classification may escalate a call
            // and a submission may never quietly de-escalate one.
            risk: call.risk.max(declared.risk),
            spawns_processes: declared.spawns_processes,
            cancellation: call.cancellation,
            admission: call.admission,
            workspace_metadata: call.workspace_metadata,
            report,
        };

        // Get-or-insert and the workspace lock are taken *without releasing the
        // map lock in between*, and this is the only place anything is ever
        // added to a workspace. Together with `WorkspaceState::waiting`, which
        // covers the one moment below where this thread holds neither lock,
        // that is what makes `forget_idle` safe: an entry the sweep finds idle
        // and can `try_lock` has nobody about to fill it.
        let mut workspaces = inner
            .workspaces
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let workspace = Arc::clone(workspaces.entry(call.workspace.clone()).or_insert_with(|| {
            Arc::new(Workspace {
                key: call.workspace.clone(),
                state: Mutex::new(WorkspaceState::default()),
                room: Condvar::new(),
            })
        }));
        let mut state = workspace
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        drop(workspaces);

        // Registered before the first wait and cleared however this leaves the
        // loop, so the workspace is never forgotten out from under a producer
        // that is about to fill it.
        state.waiting += 1;
        while state.queue.len() >= WORKSPACE_QUEUE_CAPACITY && !inner.is_shutting_down() {
            // Timed rather than plain, so a producer parked at the instant
            // shutdown begins still learns about it on its own.
            state = workspace
                .room
                .wait_timeout(state, BACKPRESSURE_POLL)
                .unwrap_or_else(|error| error.into_inner())
                .0;
        }
        state.waiting -= 1;
        if inner.is_shutting_down() {
            drop(state);
            inner.release(call.call);
            return Err(ScheduleError::Shutdown { call: call.call });
        }
        state.queue.push_back(waiting);
        let ready = inner.admit(&mut state);
        drop(state);

        inner.dispatch(&workspace, ready);
        // Closes the one window a flag cannot: a shutdown that swept the queues
        // just before this call joined one. The sweep takes the workspace lock
        // after setting the flag, so a submission that saw the flag unset was
        // necessarily already in the queue the sweep read — and a submission
        // that sees it set here resolves its own call rather than leaving a
        // ticket nobody will ever settle.
        if inner.is_shutting_down() {
            inner.stop(None);
        }
        Ok(ticket)
    }

    /// Stops every call of one run, queued or running.
    ///
    /// A queued call is removed and recorded `cancelled` without ever being
    /// dispatched — it never becomes `running`, never takes a process slot, and
    /// leaves no started body behind. A running call has its token tripped;
    /// what happens next is the executor's contract, ending in the child's
    /// process group if it has one.
    ///
    /// Removing queued work also frees the workspace it was blocking, so calls
    /// of *other* runs behind it are dispatched before this returns.
    pub fn cancel_run(&self, run: RunId) -> CancelReport {
        self.inner.stop(Some(run))
    }

    /// Reads what is queued, what is running, and how the process slots stand.
    ///
    /// Takes each workspace's lock in turn and never blocks dispatch. See
    /// [`ScheduleSnapshot`] for what that means about reading two workspaces'
    /// numbers as though they described one instant.
    #[must_use]
    pub fn snapshot(&self) -> ScheduleSnapshot {
        let workspaces = self
            .inner
            .live_workspaces()
            .into_iter()
            .filter_map(|workspace| {
                let state = workspace
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                (!state.is_idle()).then(|| {
                    WorkspaceLoad::new(
                        workspace.key.clone(),
                        state.queue.len(),
                        state.running.len(),
                        state.waiting,
                        state.mutating(),
                    )
                })
            })
            .collect();
        ScheduleSnapshot::new(
            workspaces,
            self.inner.processes.slots(),
            self.inner.is_shutting_down(),
        )
    }

    /// Stops dispatch, cancels everything in flight, and waits for the workers.
    ///
    /// Idempotent: a second call reports nothing left to stop rather than
    /// waiting again. Submission is refused from the moment this begins,
    /// including by producers already parked on a full queue.
    ///
    /// Cancelling rather than abandoning is the point. A worker that is
    /// abandoned may still be supervising a child, and a child whose parent
    /// exits is not a child that stops — so shutdown trips every token, lets
    /// the executor kill every process group, and only then stops waiting.
    /// [`ShutdownReport::outstanding`] reports what did not end in time.
    pub fn shutdown(&self, deadline: Duration) -> ShutdownReport {
        if self.inner.shutting_down.swap(true, Ordering::AcqRel) {
            return ShutdownReport {
                cancelled: CancelReport::default(),
                joined: 0,
                outstanding: self.inner.workers.live(),
            };
        }
        // Wake every producer parked on a full queue before draining, so none
        // is left waiting for room that will never be made.
        for workspace in self.inner.live_workspaces() {
            workspace.room.notify_all();
        }

        let cancelled = self.inner.stop(None);
        let idle = self.inner.workers.wait_until_idle(deadline);
        let tracked = std::mem::take(
            &mut *self
                .inner
                .workers
                .handles
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        let outstanding = self.inner.workers.live();
        let mut joined = 0;
        if idle {
            // Every worker has reported completion, so each join returns at
            // once. Joining before that would be an unbounded wait wearing a
            // deadline's clothes.
            for handle in tracked {
                let _ = handle.join();
                joined += 1;
            }
        }

        ShutdownReport {
            cancelled,
            joined,
            outstanding,
        }
    }
}

impl Drop for Scheduler {
    /// Stops in-flight work rather than letting it outlive its scheduler.
    ///
    /// A dropped scheduler with running calls would leave workers holding an
    /// `Arc` to everything here and children with nobody supervising them. The
    /// deadline is [`DEFAULT_SHUTDOWN_DEADLINE`]; a caller that needs another
    /// one calls [`shutdown`](Self::shutdown) explicitly first, which makes
    /// this a no-op.
    fn drop(&mut self) {
        if !self.inner.is_shutting_down() {
            self.shutdown(DEFAULT_SHUTDOWN_DEADLINE);
        }
    }
}

impl std::fmt::Debug for Scheduler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Scheduler")
            .field("processes", &self.inner.processes.slots())
            .field("workers", &self.inner.workers.live())
            .field("shutting_down", &self.inner.is_shutting_down())
            .finish()
    }
}

/// Releases one worker's slots however its thread ends.
///
/// A guard rather than two statements at the end of the closure, because the
/// executor's panic boundary covers the *tool body* and nothing else: a panic
/// in the pipeline around it — a poisoned store mutex, an allocation failure —
/// unwinds the scheduler's own worker. Without this the workspace's mutation
/// slot would be held forever, a process slot would be leaked from a global
/// pool that never grows back, and [`Workers::live`] would never reach zero, so
/// every later shutdown would burn its whole deadline and report an outstanding
/// worker. The ticket still resolves: the sender drops with the thread, and its
/// holder sees [`ScheduleError::WorkerLost`], which is what that variant is for.
struct Completion {
    inner: Arc<Inner>,
    workspace: Arc<Workspace>,
    dispatch: Dispatch,
    call: ToolCallId,
}

impl Drop for Completion {
    fn drop(&mut self) {
        self.inner.complete(&self.workspace, self.dispatch);
        // After the slots, because a call is only schedulable again once it is
        // no longer occupying anything — and before `leave`, so a shutdown that
        // observes no live workers also observes no claims.
        self.inner.release(self.call);
        self.inner.workers.leave();
    }
}

/// What a tool's registration says about calls of it.
struct Declared {
    risk: RiskLevel,
    spawns_processes: bool,
}

impl Inner {
    fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }

    /// Takes this scheduler's one claim on `call`, or reports that it is taken.
    fn claim(&self, call: ToolCallId) -> Result<(), ScheduleError> {
        let mut claimed = self
            .claimed
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if claimed.insert(call) {
            Ok(())
        } else {
            Err(ScheduleError::AlreadyScheduled { call })
        }
    }

    /// Releases the claim, so a call may be scheduled again once it has ended.
    fn release(&self, call: ToolCallId) {
        self.claimed
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&call);
    }

    /// Reads the declared risk and process demand of the tool a call names.
    ///
    /// An unresolvable tool falls back to the mildest declaration rather than
    /// refusing the submission. The executor records such a call as a failure
    /// without running anything, so reserving a mutation or a process slot for
    /// it would hold real resources for work that provably cannot start.
    fn declared(&self, id: &str, version: &str) -> Declared {
        let resolved = id.parse::<ToolId>().ok().and_then(|id| {
            let version = match version {
                "" => None,
                requested => Some(ToolVersion::new(requested).ok()?),
            };
            self.executor.registry().get(&id, version.as_ref()).cloned()
        });
        resolved.map_or(
            Declared {
                risk: RiskLevel::lowest(),
                spawns_processes: false,
            },
            |tool| Declared {
                risk: tool.descriptor().risk(),
                spawns_processes: tool.descriptor().spawns_processes(),
            },
        )
    }

    /// Every workspace currently known, with the map lock released.
    ///
    /// Cloning out rather than iterating under the lock is what keeps the
    /// ordering honest: a caller then takes one workspace lock at a time and
    /// holds no map lock while doing it.
    fn live_workspaces(&self) -> Vec<Arc<Workspace>> {
        self.workspaces
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .map(Arc::clone)
            .collect()
    }

    /// Forgets workspaces with nothing queued and nothing running.
    ///
    /// Bounded memory is a release-gate requirement, and a map that kept an
    /// entry for every workspace the process had ever touched would grow with
    /// the session rather than with the work.
    ///
    /// Removing one is safe because [`submit`](Scheduler::submit) — the only
    /// thing that ever adds to a workspace — takes this map lock and that
    /// workspace's lock without releasing the first in between. A candidate
    /// that is both idle and `try_lock`-able therefore has nobody queued to
    /// fill it. Everything else that holds a workspace only ever drains one, so
    /// an entry that races past this sweep stays empty and is collected on the
    /// next.
    fn forget_idle(&self) {
        let mut workspaces = self
            .workspaces
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        workspaces.retain(|_, workspace| match workspace.state.try_lock() {
            Ok(state) => !state.is_idle(),
            Err(_) => true,
        });
    }

    /// Takes every call the front of the queue lets through, in order.
    ///
    /// Marks each as running before releasing the lock, so two threads pumping
    /// one workspace cannot both admit the same call, and returns them for the
    /// caller to spawn *outside* the lock.
    fn admit(&self, state: &mut MutexGuard<'_, WorkspaceState>) -> Vec<Waiting> {
        let mut ready = Vec::new();
        while let Some(head) = state.queue.front() {
            if self.is_shutting_down() {
                break;
            }
            let mutating = head.risk.mutates_state();
            if mutating {
                // A mutation waits for the workspace to be completely still:
                // a read overlapping a write of one worktree reads a state
                // that never existed on disk.
                if !state.running.is_empty() {
                    break;
                }
            } else if state.mutating() || state.reads() >= WORKSPACE_READ_CONCURRENCY {
                break;
            }
            // Last, because it is the only one that has a side effect: a slot
            // taken for a call that then failed a workspace check would be a
            // leak of the scarcest resource here.
            let holds_process = head.spawns_processes;
            if holds_process && !self.processes.try_acquire() {
                break;
            }

            let waiting = state
                .queue
                .pop_front()
                .expect("the queue was not empty a statement ago");
            // Counted here, under the workspace lock, rather than beside the
            // `thread::spawn` that follows. `stop` reads the running set under
            // this same lock, so a call it can see and cancel is one whose
            // worker `wait_until_idle` is already obliged to wait for.
            // Counting it later leaves a window in which `shutdown` trips a
            // token, observes no live workers, and reports a clean stop while
            // a worker is still about to start.
            self.workers.enter();
            state.running.insert(
                waiting.dispatch,
                Running {
                    run: waiting.run,
                    cancellation: waiting.cancellation.clone(),
                    mutating,
                    holds_process,
                },
            );
            ready.push(waiting);
        }
        ready
    }

    /// Spawns one worker per admitted call, with no lock held.
    fn dispatch(self: &Arc<Self>, workspace: &Arc<Workspace>, ready: Vec<Waiting>) {
        if ready.is_empty() {
            return;
        }
        // The queue shrank, so anybody blocked on backpressure may proceed.
        workspace.room.notify_all();
        // Each of these was counted into `Workers` by `admit`, under the
        // workspace lock, and is counted out again by its `Completion` guard.
        for waiting in ready {
            let inner = Arc::clone(self);
            let workspace = Arc::clone(workspace);
            let handle = thread::spawn(move || {
                let completion = Completion {
                    inner,
                    workspace,
                    dispatch: waiting.dispatch,
                    call: waiting.call,
                };
                let settled = completion.inner.run(&waiting);
                // Freeing the slots and starting the next call before the
                // result is handed over: the record is already committed, so
                // the workspace has nothing left to wait for. Dropping the
                // guard explicitly rather than at the end of the closure keeps
                // that ordering visible.
                drop(completion);
                let _ = waiting.report.send(settled);
            });
            self.workers.track(handle);
        }
    }

    /// Runs one admitted call through the executor.
    fn run(&self, waiting: &Waiting) -> Result<CompletedCall, ExecutionError> {
        match &waiting.admission {
            Admission::Pending => match &waiting.workspace_metadata {
                Some(metadata) => self.executor.execute_with_workspace_metadata(
                    waiting.call,
                    &waiting.root,
                    metadata.clone(),
                    &waiting.cancellation,
                ),
                None => self
                    .executor
                    .execute(waiting.call, &waiting.root, &waiting.cancellation),
            },
            Admission::Approved { decided_by } => match &waiting.workspace_metadata {
                Some(metadata) => self.executor.execute_approved_with_workspace_metadata(
                    waiting.call,
                    decided_by,
                    &waiting.root,
                    metadata.clone(),
                    &waiting.cancellation,
                ),
                None => self.executor.execute_approved(
                    waiting.call,
                    decided_by,
                    &waiting.root,
                    &waiting.cancellation,
                ),
            },
        }
    }

    /// Releases what a finished call held, then starts whatever it was blocking.
    fn complete(self: &Arc<Self>, workspace: &Arc<Workspace>, dispatch: Dispatch) {
        let released_process = {
            let mut state = workspace
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let released_process = state
                .running
                .remove(&dispatch)
                .is_some_and(|running| running.holds_process);
            if released_process {
                // Inside the workspace lock, which is the documented order, and
                // before anything is admitted anywhere, so whichever workspace
                // is offered the slot next actually finds it free.
                self.processes.release();
            }
            released_process
        };

        if released_process {
            // A freed process slot is global, so it can unblock a workspace
            // this call has nothing to do with — including, in the starving
            // case, only such a workspace.
            self.pump_in_turn(workspace);
        } else {
            let ready = {
                let mut state = workspace
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                self.admit(&mut state)
            };
            self.dispatch(workspace, ready);
        }
        self.forget_idle();
    }

    /// Offers a freed process slot to every workspace, starting somewhere new.
    ///
    /// FIFO within a workspace makes starvation unrepresentable *there*, but
    /// the global process limit is contention between workspaces and needs its
    /// own answer. Always sweeping in map order, or letting the workspace that
    /// released a slot re-admit before the others are asked, gives one key a
    /// permanent advantage: two workspaces with a steady supply of
    /// process-backed calls would see the lower-ordered one reclaim every slot
    /// its own completions release, and the other would never start.
    ///
    /// The rotating start makes each workspace first in turn, so a queued call
    /// waits for a bounded number of releases rather than for its neighbours to
    /// run out of work. The releasing workspace takes part on the same terms as
    /// everyone else, which is why it is not pumped separately first.
    fn pump_in_turn(self: &Arc<Self>, released: &Arc<Workspace>) {
        let mut workspaces = self.live_workspaces();
        // A workspace whose last call just ended can have been forgotten by a
        // concurrent sweep. It has no queue if so, but including it costs
        // nothing and keeps this the only dispatch path on the released side.
        if !workspaces
            .iter()
            .any(|workspace| Arc::ptr_eq(workspace, released))
        {
            workspaces.push(Arc::clone(released));
        }
        if workspaces.is_empty() {
            return;
        }

        let start = self.sweep.fetch_add(1, Ordering::Relaxed) % workspaces.len();
        workspaces.rotate_left(start);
        for workspace in workspaces {
            let ready = {
                let mut state = workspace
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                self.admit(&mut state)
            };
            self.dispatch(&workspace, ready);
        }
    }

    /// Cancels one run's calls, or every call when `run` is `None`.
    fn stop(self: &Arc<Self>, run: Option<RunId>) -> CancelReport {
        let matches = |candidate: RunId| run.is_none_or(|run| run == candidate);
        let mut report = CancelReport::default();
        let mut swept: Vec<(Arc<Workspace>, Vec<Waiting>)> = Vec::new();

        for workspace in self.live_workspaces() {
            let removed = {
                let mut state = workspace
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let mut removed = Vec::new();
                let mut kept = VecDeque::new();
                for waiting in std::mem::take(&mut state.queue) {
                    // Order among the survivors is preserved, so sweeping one
                    // run out of a shared workspace does not reshuffle another
                    // run's place in the queue.
                    if matches(waiting.run) {
                        removed.push(waiting);
                    } else {
                        kept.push_back(waiting);
                    }
                }
                state.queue = kept;
                for running in state.running.values() {
                    if matches(running.run) {
                        // The token a user cancels. Tripping it is what a user
                        // asking to stop *is*; the executor reads it and
                        // cancels the call's own.
                        running.cancellation.cancel();
                        report.running += 1;
                    }
                }
                removed
            };
            if !removed.is_empty() {
                workspace.room.notify_all();
            }
            swept.push((workspace, removed));
        }

        // Every store write happens here, with no scheduler lock held.
        for (workspace, removed) in swept {
            for waiting in removed {
                report.queued += 1;
                let _ = waiting
                    .report
                    .send(self.executor.cancel_undispatched(waiting.call));
                // This call never reached a worker, so nothing else will give
                // its claim back.
                self.release(waiting.call);
            }
            let ready = {
                let mut state = workspace
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                self.admit(&mut state)
            };
            self.dispatch(&workspace, ready);
        }
        self.forget_idle();
        report
    }
}

/// The process limit this machine gets by default.
fn default_process_limit() -> usize {
    thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(MAX_PROCESS_CONCURRENCY)
}
