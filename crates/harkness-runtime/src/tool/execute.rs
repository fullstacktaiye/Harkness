//! Driving one recorded tool call to a terminal state.
//!
//! The invocation pipeline in [`erased`](super::erased) answers "what does
//! running this tool mean". This module answers the question a coordinator
//! actually has: *this call, recorded in the store, has to end somewhere, and it
//! has to end there whatever the tool does.* A tool that panics, hangs, ignores
//! its cancellation token, emits gigabytes, or returns a shape contradicting its
//! own schema must each become one recorded, structured failure of one call —
//! never a crashed run, an unbounded buffer, or a record stuck in `running` with
//! nothing said about why.
//!
//! Every property below is a property of the *executor*, so #94's and #95's
//! tools and #97's coordinator inherit identical semantics without implementing
//! any of it.
//!
//! # The body runs on its own thread, and that is load-bearing
//!
//! A tool body is synchronous Rust. There is no way to interrupt one from
//! outside, so a timeout cannot be enforced on the calling thread at all: a body
//! that never returns would simply never return. The body therefore runs on a
//! thread of its own, and the executor waits on a channel rather than on a join.
//!
//! That buys the guarantee that matters — **the call always reaches a terminal
//! state** — and it is worth being explicit about what it does not buy. When the
//! grace period below expires, the executor *abandons* the worker rather than
//! killing it, because Rust cannot kill a thread. The abandoned thread owns its
//! whole [`ExecutionContext`], so nothing dangles and nothing is shared; it
//! finishes whenever it finishes and its result is discarded. A tool that
//! ignores cancellation entirely therefore leaks one thread per call, which is a
//! real cost and the reason cancellation is a contract a tool has to keep rather
//! than a courtesy.
//!
//! A *process-backed* tool has no such caveat, because a process can be killed:
//! [`ToolProcess`](super::ToolProcess) polls the same token and the same
//! deadline and kills its child's whole process group, so the work stops even
//! though the thread that was waiting on it merely returns.
//!
//! # The caller's token is read, never written
//!
//! A tool receives a token belonging to *this call*, not the one its caller
//! holds. [`Cancellation`] latches and has no reset, so enforcing a deadline by
//! cancelling the caller's token would leave it cancelled for good: one slow
//! step would silently cancel every later call of the same run, each recorded
//! `cancelled` with nobody having cancelled anything.
//!
//! The executor watches the caller's token and cancels the call's own. That
//! costs one [`POLL_INTERVAL`] of propagation — a cancel reaches the tool on the
//! next poll rather than instantly — which is why the measured latency is around
//! 20 ms rather than around zero, comfortably inside the 250 ms the contract
//! promises. A cancel that arrives *before* dispatch is seeded onto the call's
//! token directly, so the pipeline's own gate still refuses to start a body that
//! was cancelled before it began.
//!
//! # Stopping is requested, then waited for
//!
//! On cancellation or a passed deadline the executor cancels the token and keeps
//! polling for [`TERMINATION_GRACE`]. A tool that returns inside that window has
//! its *own* outcome recorded — a tool that finished its work as the cancel
//! arrived did the work, and recording `cancelled` over a completed side effect
//! would make the run history lie about what is on disk.
//!
//! The one exception is the outcome that is merely this decision coming back:
//! stopping a call *means* cancelling its token, so a tool killed by its
//! deadline reports `cancelled` because that is what it observed. Taking it at
//! face value would tell a user their work was cancelled when in fact it ran out
//! of time, so a `cancelled` or `timed_out` arriving after the executor decided
//! to stop yields to the executor's own verdict. Everything else — a success, a
//! failure of its own — is the tool's to report.
//!
//! A body that does not come back at all is recorded with the executor's
//! verdict and abandoned.
//!
//! # Persist before deliver
//!
//! The terminal state and its event are committed — in one transaction, as the
//! store's pairing guarantees — *before* [`ToolExecutor::execute`] returns. A
//! caller that receives an outcome can therefore read the record behind it, and
//! a crash between the two is not a state this store can be found in. The same
//! ordering applies at the other end: the call is `running` in the store, with
//! the resolved version pinned, before the body is dispatched.
//!
//! # What is not here
//!
//! Scheduling, queueing, and concurrency limits (#93); deciding *whether* a call
//! may run (#91, #92); and any concrete tool (#94, #95).
//!
//! # Two ways in, because a decision resumes the work it decided
//!
//! [`ToolExecutor::execute`] runs a `pending` call. Approval-gated work does not
//! pass through `pending`: the domain resumes a held record *by* its decision —
//! [`ToolCall::approve`](crate::domain::ToolCall::approve) transitions
//! `awaiting_approval` straight to `running` — so there is no moment at which an
//! approved call is waiting to be dispatched separately.
//! [`ToolExecutor::execute_approved`] is therefore the second entry point: it
//! records the decision, pins the version, starts the call, and supervises it
//! exactly as the first does.
//!
//! The executor still decides nothing. It is *told* who approved, in the same
//! way it is told a pending call is already authorized; which party decides and
//! on what grounds is #91/#92's. What this layer owns is that the decision and
//! the version it authorized are written in one transaction, so an audit cannot
//! read an approval beside a version the approver never saw.
//!
//! Neither entry point accepts a `running` call. That is what stops one being
//! executed twice and its side effects applied twice; telling a call abandoned
//! by a dead process from one still executing is a question about run ownership,
//! and answering it is not this module's job.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use harkness_git::Cancellation;
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::domain::{Failure, RunId, StepId, ToolCall, ToolCallId, ToolCallState};
use crate::store::{EventKind, RunEvent, Store, StoreArtifacts, StoreError};

use super::{
    DEFAULT_PROGRESS_CAPACITY, DEFAULT_STREAM_TAIL_BYTES, Deadline, ErasedTool, ExecutionContext,
    InvocationError, POLL_INTERVAL, ProgressReceiver, ToolError, ToolId, ToolIdentity, ToolOutcome,
    ToolRegistry, ToolTimeout, ToolVersion, invoke_resolved, progress_channel,
};

/// Kind recorded when a tool's result is larger than a result may be.
///
/// Borrowed from [`StoreError::KINDS`] rather than invented here, because it is
/// the store's bound that was broken and a consumer branching on the kind should
/// not have to learn two spellings for one refusal. Borrowing means a recorded
/// tool-call failure can carry a kind from the *store's* namespace as well as
/// the tool's — `the_oversized_result_kind_is_one_a_consumer_can_look_up` holds
/// this constant to that table, so it cannot drift into a spelling no published
/// namespace defines and no caller can match on.
const OVERSIZED_RESULT_KIND: &str = "payload_too_large";

/// How long a tool is given to unwind after it has been told to stop.
///
/// Long enough that a body checking its token between units of work returns on
/// its own and gets to report what it did; short enough that a body which
/// ignores the token does not hold up the record of a call that is already over.
pub const TERMINATION_GRACE: Duration = Duration::from_millis(500);

/// What bounds one call, and how much of its output is kept in memory.
///
/// Two of the three limits are the executor's own. The third — the timeout — is
/// declared by the *tool*, because the author is the only party who knows
/// whether thirty seconds is generous or absurd for the work; see
/// [`ToolTimeout`].
///
/// A caller may replace that limit with any *finite* one, shorter or longer:
/// only the author knows the usual case, and only the caller knows this one — a
/// clone of a huge repository legitimately needs longer than the default that
/// suits every other clone. What a caller may not do is remove the bound
/// entirely, because the invariant being protected is not "the tool's number
/// wins" but "the call has a way to end". See
/// [`bounded_only_by_cancellation`](Self::bounded_only_by_cancellation).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutionLimits {
    timeout: Option<ToolTimeout>,
    stream_tail_bytes: usize,
    progress_capacity: usize,
}

impl Default for ExecutionLimits {
    /// Takes the timeout from the tool and the rest from the module defaults.
    fn default() -> Self {
        Self {
            timeout: None,
            stream_tail_bytes: DEFAULT_STREAM_TAIL_BYTES,
            progress_capacity: DEFAULT_PROGRESS_CAPACITY,
        }
    }
}

impl ExecutionLimits {
    /// Replaces the tool's declared timeout with `limit`, longer or shorter.
    ///
    /// Deliberately not clamped to the declared limit. A caller extending one
    /// still leaves the call a way to end, which is the property that matters;
    /// clamping would force anyone with a legitimately slower case to publish a
    /// second version of the tool to say so.
    #[must_use]
    pub const fn within(mut self, limit: Duration) -> Self {
        self.timeout = Some(ToolTimeout::After(limit));
        self
    }

    /// Asks that only cancellation bound the call.
    ///
    /// Accepted only for a tool that declared
    /// [`ToolTimeout::OnlyByCancellation`] itself, and refused with
    /// [`ExecutionError::UnboundedNotDeclared`] otherwise.
    ///
    /// This is the one thing [`within`](Self::within) permits that this does
    /// not, and the asymmetry is the point. Any finite limit still leaves the
    /// call a way to end; removing the bound leaves it with none unless the body
    /// polls its token, and only the tool's author can say whether it does.
    /// Declaring `OnlyByCancellation` is that claim.
    #[must_use]
    pub const fn bounded_only_by_cancellation(mut self) -> Self {
        self.timeout = Some(ToolTimeout::OnlyByCancellation);
        self
    }

    /// Bounds how much of each captured output stream is retained in memory.
    #[must_use]
    pub const fn retaining_stream_tail(mut self, bytes: usize) -> Self {
        self.stream_tail_bytes = bytes;
        self
    }

    /// Bounds how many progress events queue before a tool is made to wait.
    #[must_use]
    pub const fn with_progress_capacity(mut self, events: usize) -> Self {
        self.progress_capacity = events;
        self
    }

    /// Resolves the timeout this call actually runs under.
    fn timeout_for(
        self,
        tool: &ToolIdentity,
        declared: ToolTimeout,
    ) -> Result<ToolTimeout, ExecutionError> {
        match self.timeout {
            None => Ok(declared),
            Some(ToolTimeout::After(limit)) => Ok(ToolTimeout::After(limit)),
            Some(ToolTimeout::OnlyByCancellation) => match declared {
                ToolTimeout::OnlyByCancellation => Ok(ToolTimeout::OnlyByCancellation),
                ToolTimeout::After(limit) => Err(ExecutionError::UnboundedNotDeclared {
                    tool: tool.clone(),
                    declared: limit,
                }),
            },
        }
    }
}

/// How one tool call ended.
///
/// Cancellation and a timeout are named apart from an ordinary failure because
/// callers act on them differently: a failure is worth reporting to whoever
/// asked for the work, a cancellation is what they already asked for, and a
/// timeout is the one outcome that says nothing about whether the work was
/// wrong — only that it was slow.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum CallOutcome {
    /// The tool returned a result that satisfied its published output schema.
    Succeeded {
        /// The validated result, exactly as it was persisted.
        output: Value,
    },

    /// The call ended in a recorded failure.
    Failed {
        /// The durable detail written against the call.
        failure: Failure,
    },

    /// The call was stopped through its cancellation token.
    Cancelled,

    /// The worker carrying the call died without reporting anything.
    ///
    /// Not something a tool can cause: the invocation pipeline contains the body
    /// in a panic boundary, so a panicking tool reports a failure like any
    /// other. This is the layer *underneath* that going away — an abort, a
    /// failed allocation — and it is recorded as
    /// [`ToolCallState::Interrupted`](crate::domain::ToolCallState::Interrupted)
    /// because that state exists for exactly this and nothing else would ever
    /// reach it.
    Interrupted,

    /// The call exceeded the time it was allowed and was stopped.
    TimedOut {
        /// The limit the call was given.
        limit: Duration,
    },
}

impl CallOutcome {
    /// Whether the call produced a result.
    #[must_use]
    pub const fn succeeded(&self) -> bool {
        matches!(self, Self::Succeeded { .. })
    }

    /// The stable failure kind, when the call ended in a recorded failure.
    #[must_use]
    pub fn failure_kind(&self) -> Option<&str> {
        match self {
            Self::Failed { failure } => Some(failure.kind()),
            _ => None,
        }
    }
}

/// One executed call: what ran, how it ended, and the record it left behind.
#[derive(Clone, Debug)]
pub struct CompletedCall {
    tool: Option<ToolIdentity>,
    record: ToolCall,
    outcome: CallOutcome,
}

impl CompletedCall {
    /// The `(id, version)` that ran, when one was resolved.
    ///
    /// `None` only when resolution itself failed, which is also the only case in
    /// which the call's recorded version is the one that was *asked for* rather
    /// than the one that executed.
    #[must_use]
    pub const fn tool(&self) -> Option<&ToolIdentity> {
        self.tool.as_ref()
    }

    /// The stored call in its terminal state.
    ///
    /// This is the row as the store committed it, not a projection assembled
    /// afterwards, so it cannot disagree with what a later read returns.
    #[must_use]
    pub const fn record(&self) -> &ToolCall {
        &self.record
    }

    /// How the call ended.
    #[must_use]
    pub const fn outcome(&self) -> &CallOutcome {
        &self.outcome
    }

    /// The terminal lifecycle state the call was recorded in.
    #[must_use]
    pub fn state(&self) -> ToolCallState {
        self.record.state()
    }
}

/// Failures of the executor itself, as distinct from failures of a call.
///
/// The separation is the same one [`InvocationError`] draws and matters for the
/// same reason: a call that failed is an outcome worth recording and reporting
/// to whoever asked for the work, while a store that cannot be written or a call
/// that was never dispatchable is a fault in Harkness or in its caller. Only the
/// first kind ever becomes a [`CompletedCall`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ExecutionError {
    /// The run store refused a read or a write.
    #[error(transparent)]
    Store(#[from] StoreError),

    /// The call is not in a state the chosen entry point can begin from.
    ///
    /// Each entry point admits exactly one state:
    /// [`execute`](ToolExecutor::execute) a `pending` call, and
    /// [`execute_approved`](ToolExecutor::execute_approved) one held at
    /// `awaiting_approval`. A `running` call is refused by both, which is what
    /// stops a call being executed twice and its side effects applied twice —
    /// telling one abandoned by a dead process from one still executing is a
    /// question about run ownership, not about this call, and deliberately not
    /// answered here.
    #[error("tool call {call} is {state}, and this dispatch requires {expected}")]
    NotDispatchable {
        /// Call that was handed to the executor.
        call: ToolCallId,
        /// State it was found in.
        state: ToolCallState,
        /// State the entry point that was used requires.
        expected: ToolCallState,
    },

    /// The execution context could not be built for this call.
    #[error("the execution context for tool call {call} could not be built: {source}")]
    Context {
        /// Call the context belonged to.
        call: ToolCallId,
        /// Why it was refused.
        #[source]
        source: ToolError,
    },

    /// A caller asked to lift a timeout the tool did not declare liftable.
    #[error("{tool} declared a {declared:?} limit, which a caller may tighten but not remove")]
    UnboundedNotDeclared {
        /// Tool whose declaration was contradicted.
        tool: ToolIdentity,
        /// The limit it declared.
        declared: Duration,
    },
}

impl ExecutionError {
    /// Every stable discriminant this error namespace can emit.
    ///
    /// Deliberately disjoint from [`InvocationError::kinds`]: the two describe
    /// different things, and `harkness contract` publishes both.
    pub const KINDS: &'static [&'static str] = &[
        "store_failed",
        "not_dispatchable",
        "context_unavailable",
        "unbounded_not_declared",
    ];

    /// Stable machine-readable discriminant for caller-facing error handling.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Store(_) => "store_failed",
            Self::NotDispatchable { .. } => "not_dispatchable",
            Self::Context { .. } => "context_unavailable",
            Self::UnboundedNotDeclared { .. } => "unbounded_not_declared",
        }
    }
}

/// How a call comes to be running.
///
/// The two entry points differ in exactly this and nothing else, which is why it
/// is a value rather than a duplicated body: a difference in *how a call starts*
/// must not become a difference in how it is supervised, recorded, bounded, or
/// stopped.
enum Start<'a> {
    /// An authorized call nobody had to decide on.
    Pending,
    /// A call held for a decision, resumed by the decision itself.
    Approved {
        /// Stable identity of whoever decided, recorded in the audit history.
        decided_by: &'a str,
    },
}

impl Start<'_> {
    /// The state a call must be in for this start to be legitimate.
    const fn required_state(&self) -> ToolCallState {
        match self {
            Self::Pending => ToolCallState::Pending,
            Self::Approved { .. } => ToolCallState::AwaitingApproval,
        }
    }
}

/// What one call being waited on consists of.
///
/// Grouped rather than passed as eight arguments, and grouped *here* rather than
/// merged into the executor, because every field belongs to one call while a
/// [`ToolExecutor`] serves any number of them concurrently.
struct Supervised<'a> {
    run_id: RunId,
    call: ToolCallId,
    step: StepId,
    awaiting: &'a Receiver<Result<ToolOutcome, ToolError>>,
    reports: &'a ProgressReceiver,
    /// The token a user cancels. Read, never written.
    caller: &'a Cancellation,
    /// The token this call's body and children hold, cancelled to stop them.
    call_token: &'a Cancellation,
    deadline: Option<Deadline>,
}

/// Runs recorded tool calls against a registry, writing through a store.
///
/// One executor serves any number of concurrent calls: it holds nothing
/// per-call, and every call gets its own thread, context, and progress channel.
#[derive(Clone, Debug)]
pub struct ToolExecutor {
    store: Arc<Store>,
    registry: Arc<ToolRegistry>,
    limits: ExecutionLimits,
}

impl ToolExecutor {
    /// Executes calls from `registry`, recording them in `store`.
    #[must_use]
    pub fn new(store: Arc<Store>, registry: Arc<ToolRegistry>) -> Self {
        Self {
            store,
            registry,
            limits: ExecutionLimits::default(),
        }
    }

    /// Replaces the limits every call this executor runs is subject to.
    #[must_use]
    pub const fn with_limits(mut self, limits: ExecutionLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The limits this executor applies.
    #[must_use]
    pub const fn limits(&self) -> ExecutionLimits {
        self.limits
    }

    /// Runs one recorded call to a terminal state.
    ///
    /// `workspace_root` is the absolute directory the call may touch, and
    /// `cancellation` is the token that stops it — shared with everything the
    /// tool starts, so one request reaches the whole tree.
    ///
    /// The call is left in a terminal state on *every* return path that produces
    /// a [`CompletedCall`], including a tool that panicked or one that never
    /// returned, and the state and its event are readable before this function
    /// does.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutionError`] only for faults that are not outcomes of the
    /// call: a store that refused a read or write, a call that was not
    /// dispatchable, a workspace root that cannot be used, or limits the tool's
    /// declaration forbids. Everything the *tool* can do — failing, panicking,
    /// timing out, being cancelled, or contradicting its own schema — is a
    /// `CompletedCall`, not an error.
    pub fn execute(
        &self,
        call: ToolCallId,
        workspace_root: impl Into<PathBuf>,
        cancellation: &Cancellation,
    ) -> Result<CompletedCall, ExecutionError> {
        self.run(call, &Start::Pending, workspace_root, cancellation)
    }

    /// Runs one call that a human or a policy has decided may proceed.
    ///
    /// The approval-gated entry point. A call held at `awaiting_approval` is
    /// resumed *by* its decision — [`ToolCall::approve`] is defined that way, and
    /// so are runs and steps — so recording the decision and starting the work
    /// are one step rather than two, and this is where they meet.
    ///
    /// `decided_by` is the stable identity of whoever decided, and it is
    /// recorded in the call's approval history. The executor does not evaluate
    /// policy or collect decisions: it is *told* one, exactly as it is told a
    /// pending call is already authorized. Which party decides, and on what
    /// grounds, belongs to #91/#92.
    ///
    /// The version that ran is pinned by the same transaction that records the
    /// approval, so an audit can never read a decision beside a version the
    /// approver did not see.
    ///
    /// # Errors
    ///
    /// As [`execute`](Self::execute), except that the call must be
    /// `awaiting_approval` rather than `pending`, and the identity must not be
    /// blank.
    pub fn execute_approved(
        &self,
        call: ToolCallId,
        decided_by: &str,
        workspace_root: impl Into<PathBuf>,
        cancellation: &Cancellation,
    ) -> Result<CompletedCall, ExecutionError> {
        self.run(
            call,
            &Start::Approved { decided_by },
            workspace_root,
            cancellation,
        )
    }

    /// The body both entry points share, differing only in how the call starts.
    fn run(
        &self,
        call: ToolCallId,
        start: &Start<'_>,
        workspace_root: impl Into<PathBuf>,
        cancellation: &Cancellation,
    ) -> Result<CompletedCall, ExecutionError> {
        let record = self.store.load_tool_call(call)?;
        if record.state() != start.required_state() {
            return Err(ExecutionError::NotDispatchable {
                call,
                state: record.state(),
                expected: start.required_state(),
            });
        }

        // Resolution happens before anything is written, so a call naming a tool
        // that does not exist fails without ever having been `running`.
        let step = record.step_id();
        let tool = match self.resolve(&record) {
            Ok(tool) => tool,
            Err(error) => {
                let failure = error.as_failure();
                return self.finish(call, step, None, CallOutcome::Failed { failure });
            }
        };
        let identity = tool.descriptor().identity().clone();
        let timeout = self
            .limits
            .timeout_for(&identity, tool.descriptor().timeout())?;

        // Build the context before dispatch, but defer handling a refusal until
        // after dispatch. Every recorded call must reach a terminal state; an
        // approved call must also persist the approval and pinned version even
        // when an unusable workspace means its body cannot start.
        //
        // The token the tool receives is this call's own, not the caller's.
        // `Cancellation` latches and has no reset, so cancelling the caller's to
        // enforce a deadline would leave it cancelled for every later call
        // sharing it: one slow step would silently cancel the rest of its run.
        // The executor watches the caller's token and cancels this one.
        let (progress, reports) = progress_channel(self.limits.progress_capacity);
        let call_token = Cancellation::default();
        if cancellation.is_cancelled() {
            // Seeded rather than left for the first poll, so the pipeline's own
            // gate still refuses a body dispatched after a cancel: a tool that
            // does its work in one non-polling call must not start at all.
            call_token.cancel();
        }
        let context = ExecutionContext::new(
            record.run_id(),
            record.step_id(),
            call,
            workspace_root,
            call_token.clone(),
            Box::new(progress),
            Box::new(StoreArtifacts::new(
                Arc::clone(&self.store),
                record.run_id(),
                record.step_id(),
                call,
            )),
        )
        .map(|context| context.with_stream_tail_bytes(self.limits.stream_tail_bytes));

        // Everything that could refuse this call has now refused it, so the
        // record moves to `running` — pinning the version that was resolved —
        // before the body is allowed to start.
        let at = OffsetDateTime::now_utc();
        let version = identity.version.to_string();
        let mut payload = json!({
            "state": ToolCallState::Running.as_str(),
            "tool_id": identity.id.as_str(),
            "tool_version": version,
        });
        if let Start::Approved { decided_by } = start {
            // The decision belongs on the timeline as well as in the approval
            // history: a reader of the log should see *why* a call that was
            // waiting is suddenly running.
            payload["approved_by"] = json!(decided_by);
        }
        let event = RunEvent::new(EventKind::ToolCallStateChanged, at)
            .for_step(record.step_id())
            .for_tool_call(call)
            .with_payload(payload);
        let (dispatched, _) = match start {
            Start::Pending => self
                .store
                .dispatch_tool_call_with_event(call, &version, at, event)?,
            Start::Approved { decided_by } => self
                .store
                .dispatch_approved_tool_call_with_event(call, decided_by, &version, at, event)?,
        };
        let run_id = dispatched.run_id();

        let mut context = match context {
            Ok(context) => context,
            Err(source) => {
                return self.finish(
                    call,
                    step,
                    Some(identity),
                    CallOutcome::Failed {
                        failure: source.as_failure(),
                    },
                );
            }
        };

        let deadline = timeout.limit().and_then(Deadline::starting_now);
        if let Some(deadline) = deadline {
            context = context.with_deadline(deadline);
        }

        let input = raw_input(dispatched.input());
        let (finished, awaiting) = mpsc::sync_channel(1);
        thread::spawn(move || {
            // The result is discarded when nobody is waiting for it, which is
            // exactly the abandoned-worker case: the send fails and the thread
            // ends rather than blocking on a channel with no receiver.
            let _ = finished.send(invoke_resolved(&tool, &input, &mut context));
        });

        let outcome = self.supervise(Supervised {
            run_id,
            call,
            step,
            awaiting: &awaiting,
            reports: &reports,
            caller: cancellation,
            call_token: &call_token,
            deadline,
        });
        self.finish(call, step, Some(identity), outcome)
    }

    /// Resolves the tool a recorded call names.
    ///
    /// An empty recorded version means "whichever is latest", which is the state
    /// a caller leaves behind when it records the request without wanting to
    /// pin one; the version that wins is written back by the dispatch.
    fn resolve(&self, record: &ToolCall) -> Result<Arc<dyn ErasedTool>, InvocationError> {
        let id = record
            .tool_id()
            .parse::<ToolId>()
            .map_err(InvocationError::from)?;
        let version = match record.tool_version() {
            "" => None,
            requested => Some(ToolVersion::new(requested).map_err(InvocationError::from)?),
        };
        self.registry
            .resolve(&id, version.as_ref())
            .cloned()
            .map_err(InvocationError::from)
    }

    /// Waits for the body, forwarding progress and enforcing the two limits.
    fn supervise(&self, watched: Supervised<'_>) -> CallOutcome {
        let Supervised {
            run_id,
            call,
            step,
            awaiting,
            reports,
            caller,
            call_token,
            deadline,
        } = watched;

        // `Some` once the executor has asked the call to stop: it holds the
        // verdict to record if the body does not come back, and the instant the
        // grace period is measured from.
        let mut stopping: Option<(CallOutcome, std::time::Instant)> = None;

        loop {
            self.record_progress(run_id, call, step, reports);

            // Waiting on the channel rather than sleeping beside it: a fast
            // tool is not made to pay a poll interval it had no reason to, and a
            // slow one still wakes this loop on the cadence progress draining
            // and the two limits are checked at.
            match awaiting.recv_timeout(POLL_INTERVAL) {
                Ok(result) => {
                    self.record_progress(run_id, call, step, reports);
                    let reported = outcome_of(result);
                    // A body that finished on its own terms outranks anything
                    // the executor could infer about work it could not see —
                    // including work completed just as a stop was requested,
                    // whose side effects have happened and must be recorded.
                    //
                    // What it does *not* outrank is the executor's own reason
                    // for stopping. Stopping means cancelling the call's own
                    // token, so a tool killed by its deadline reports
                    // `cancelled`: that is the echo of this decision, not
                    // independent evidence, and recording it would tell a user
                    // their work was cancelled when in fact it ran out of time.
                    return match (&stopping, &reported) {
                        (
                            Some((verdict, _)),
                            CallOutcome::Cancelled | CallOutcome::TimedOut { .. },
                        ) => verdict.clone(),
                        _ => reported,
                    };
                }
                Err(RecvTimeoutError::Disconnected) => {
                    // The worker died without sending, which `catch_unwind`
                    // inside the pipeline makes impossible for a panicking tool
                    // — so this is a panic in the pipeline itself or a failed
                    // allocation, and the call still has to end somewhere.
                    self.record_progress(run_id, call, step, reports);
                    return stopping.map_or(CallOutcome::Interrupted, |(verdict, _)| verdict);
                }
                Err(RecvTimeoutError::Timeout) => {}
            }

            if let Some((verdict, since)) = &stopping {
                if since.elapsed() >= TERMINATION_GRACE {
                    return verdict.clone();
                }
            } else if let Some(verdict) = Self::reason_to_stop(caller, call_token, deadline) {
                // Cancelling *this call's* token is what actually stops the
                // work: a process-backed tool kills its child's group off the
                // back of it, and a cooperative body returns at its next check.
                // The caller's token is only ever read — it latches, and
                // cancelling it here would cancel every later call sharing it.
                call_token.cancel();
                stopping = Some((verdict, std::time::Instant::now()));
            }
        }
    }

    /// The verdict to record, when there is a reason to stop the call.
    ///
    /// Either token counts as a cancellation: the caller's is how a user asks,
    /// and the call's own is how work that already stopped itself reports the
    /// same thing.
    fn reason_to_stop(
        caller: &Cancellation,
        call_token: &Cancellation,
        deadline: Option<Deadline>,
    ) -> Option<CallOutcome> {
        if caller.is_cancelled() || call_token.is_cancelled() {
            return Some(CallOutcome::Cancelled);
        }
        // Cancellation is tested first, so a call that was cancelled and also
        // ran out of time is recorded as what its caller asked for.
        deadline
            .filter(|deadline| deadline.has_passed())
            .map(|deadline| CallOutcome::TimedOut {
                limit: deadline.limit(),
            })
    }

    /// Appends every queued progress event to the run's log.
    ///
    /// A log that cannot be written is not worth failing a call over — the work
    /// itself is unaffected — so a refused append is dropped rather than
    /// propagated. The terminal state is a different matter and is not treated
    /// this way.
    fn record_progress(
        &self,
        run_id: RunId,
        call: ToolCallId,
        step: StepId,
        reports: &ProgressReceiver,
    ) {
        let reported = reports.drain();
        if reported.is_empty() {
            return;
        }
        // One transaction for the whole burst. A tool supervising a chatty child
        // can report faster than a commit takes, and a transaction per event
        // would make the executor's throughput — not the work — the thing that
        // decides whether the call finishes inside its timeout.
        let at = OffsetDateTime::now_utc();
        let _ = self.store.append_events(
            run_id,
            reported.into_iter().map(|event| {
                // Associated with the step as well as the call, like every other
                // event this module writes: a consumer rendering one step's
                // timeline filters by `step_id`, and an event that omits it
                // simply does not appear there.
                RunEvent::new(EventKind::ToolProgress, at)
                    .for_step(step)
                    .for_tool_call(call)
                    .with_payload(serde_json::to_value(&event).unwrap_or(Value::Null))
            }),
        );
    }

    /// Records the terminal state and its event, then reports the outcome.
    ///
    /// The commit happens here and nowhere else, so "persisted before delivered"
    /// is a property of one function rather than of every path that reaches one.
    /// `pub(super)` only so a test can drive one terminal recording directly.
    /// [`CallOutcome::Interrupted`] is reached by a worker thread dying, which
    /// nothing can arrange deterministically, and the recording is the part
    /// worth asserting on.
    pub(super) fn finish(
        &self,
        call: ToolCallId,
        step: StepId,
        tool: Option<ToolIdentity>,
        outcome: CallOutcome,
    ) -> Result<CompletedCall, ExecutionError> {
        let at = OffsetDateTime::now_utc();
        let record = match self.persist(call, step, &outcome, at) {
            Ok(record) => record,
            // A result the store cannot hold becomes a *recorded failure* rather
            // than an error handed back. Returning one would leave the call in
            // `running` with nothing written about why it never finished, which
            // is the state every bound in this crate exists to prevent — and
            // what went wrong is the size of the result, not the run.
            Err(StoreError::PayloadTooLarge { bytes, .. }) if outcome.succeeded() => {
                let outcome = CallOutcome::Failed {
                    failure: Failure::new(
                        OVERSIZED_RESULT_KIND,
                        format!(
                            "the tool returned {bytes} bytes, which is more than a result may \
                             hold; content that large belongs in an artifact with only a \
                             reference to it in the result"
                        ),
                    ),
                };
                let record = self.persist(call, step, &outcome, at)?;
                return Ok(CompletedCall {
                    tool,
                    record,
                    outcome,
                });
            }
            Err(error) => return Err(error.into()),
        };

        Ok(CompletedCall {
            tool,
            record,
            outcome,
        })
    }

    /// Writes one terminal state and the event describing it, in one
    /// transaction.
    fn persist(
        &self,
        call: ToolCallId,
        step: StepId,
        outcome: &CallOutcome,
        at: OffsetDateTime,
    ) -> Result<ToolCall, StoreError> {
        let event = |state: ToolCallState, detail: Value| {
            RunEvent::new(EventKind::ToolCallStateChanged, at)
                .for_step(step)
                .for_tool_call(call)
                .with_payload(json!({ "state": state.as_str(), "detail": detail }))
        };
        let failed = |failure: &Failure, detail: Value| {
            self.store.fail_tool_call_with_event(
                call,
                failure.clone(),
                at,
                event(ToolCallState::Failed, detail),
            )
        };

        let (record, _) = match outcome {
            CallOutcome::Succeeded { output } => self.store.succeed_tool_call_with_event(
                call,
                output.clone(),
                at,
                event(ToolCallState::Succeeded, Value::Null),
            )?,
            CallOutcome::Failed { failure } => failed(
                failure,
                json!({"kind": failure.kind(), "message": failure.message()}),
            )?,
            CallOutcome::Cancelled => self.store.transition_tool_call_with_event(
                call,
                ToolCallState::Cancelled,
                at,
                event(ToolCallState::Cancelled, Value::Null),
            )?,
            CallOutcome::Interrupted => self.store.transition_tool_call_with_event(
                call,
                ToolCallState::Interrupted,
                at,
                event(ToolCallState::Interrupted, Value::Null),
            )?,
            CallOutcome::TimedOut { limit } => {
                // A timeout is persisted as a failure carrying the `timed_out`
                // kind rather than as a lifecycle state of its own: the domain
                // has none, and adding one would mean a migration for something
                // the failure kind already says exactly.
                let failure = ToolError::TimedOut { limit: *limit }.as_failure();
                failed(
                    &failure,
                    json!({
                        "kind": failure.kind(),
                        "timeout_ms": u64::try_from(limit.as_millis()).unwrap_or(u64::MAX),
                    }),
                )?
            }
        };
        Ok(record)
    }
}

/// Projects what the invocation pipeline returned into a recorded outcome.
fn outcome_of(result: Result<ToolOutcome, ToolError>) -> CallOutcome {
    match result {
        Ok(produced) => CallOutcome::Succeeded {
            // Back through `Value` because that is what the store column holds,
            // and the pipeline already canonicalized the key order on the way
            // out, so nothing is reordered by the round trip.
            output: serde_json::from_str(produced.output().get()).unwrap_or(Value::Null),
        },
        Err(ToolError::Cancelled) => CallOutcome::Cancelled,
        Err(ToolError::TimedOut { limit }) => CallOutcome::TimedOut { limit },
        Err(error) => CallOutcome::Failed {
            failure: error.as_failure(),
        },
    }
}

/// Re-encodes a recorded input for the pipeline that validates it.
///
/// A malformed value cannot arrive here — it came out of a column the store
/// parsed as JSON — so a failure to re-encode falls back to `null`, which the
/// input schema then refuses in the ordinary way rather than through a second
/// error path nothing else uses.
fn raw_input(input: &Value) -> Box<serde_json::value::RawValue> {
    serde_json::value::to_raw_value(input).unwrap_or_else(|_| {
        serde_json::value::RawValue::from_string("null".to_owned()).expect("null is valid JSON")
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{ExecutionError, ExecutionLimits, TERMINATION_GRACE};
    use crate::tool::POLL_INTERVAL;
    use crate::tool::{InvocationError, ToolIdentity, ToolTimeout};

    fn identity() -> ToolIdentity {
        ToolIdentity::parse("fixture.tool", "1.0.0").unwrap()
    }

    #[test]
    fn execution_error_kinds_round_trip_and_do_not_collide_with_the_others() {
        let cases = [
            (
                ExecutionError::Store(crate::store::StoreError::DataDirectoryUnavailable),
                "store_failed",
            ),
            (
                ExecutionError::NotDispatchable {
                    call: crate::domain::ToolCallId::new(),
                    state: crate::domain::ToolCallState::Running,
                    expected: crate::domain::ToolCallState::Pending,
                },
                "not_dispatchable",
            ),
            (
                ExecutionError::Context {
                    call: crate::domain::ToolCallId::new(),
                    source: crate::tool::ToolError::Cancelled,
                },
                "context_unavailable",
            ),
            (
                ExecutionError::UnboundedNotDeclared {
                    tool: identity(),
                    declared: Duration::from_secs(30),
                },
                "unbounded_not_declared",
            ),
        ];

        let kinds = cases.iter().map(|(_, kind)| *kind).collect::<Vec<_>>();
        assert_eq!(kinds, ExecutionError::KINDS);
        for (error, expected) in cases {
            assert_eq!(error.kind(), expected, "unexpected kind for {error:?}");
        }

        // `harkness contract` publishes both namespaces, so one spelling must
        // never mean two things.
        for kind in ExecutionError::KINDS {
            assert!(
                !InvocationError::kinds().contains(kind),
                "{kind} is claimed by two namespaces"
            );
        }
    }

    #[test]
    fn the_oversized_result_kind_is_one_a_consumer_can_look_up() {
        // The executor records this against a tool call, so it has to be a kind
        // some published namespace actually defines. It is not the tool's — the
        // tool did nothing wrong — so it is the store's, and this is what stops
        // it drifting into a spelling nothing publishes and no caller matches.
        assert!(
            crate::store::StoreError::KINDS.contains(&super::OVERSIZED_RESULT_KIND),
            "{} is recorded but published by no namespace",
            super::OVERSIZED_RESULT_KIND
        );
    }

    #[test]
    fn a_caller_may_replace_a_declared_limit_but_never_remove_the_bound() {
        let declared = ToolTimeout::After(Duration::from_secs(30));

        assert_eq!(
            ExecutionLimits::default()
                .timeout_for(&identity(), declared)
                .unwrap(),
            declared,
            "no override means the tool's own declaration"
        );
        assert_eq!(
            ExecutionLimits::default()
                .within(Duration::from_millis(50))
                .timeout_for(&identity(), declared)
                .unwrap(),
            ToolTimeout::After(Duration::from_millis(50))
        );
        // Longer is permitted too, and deliberately not clamped: the call still
        // has a way to end, and clamping would make anyone with a legitimately
        // slower case publish a second version of the tool to say so.
        assert_eq!(
            ExecutionLimits::default()
                .within(Duration::from_secs(3_600))
                .timeout_for(&identity(), declared)
                .unwrap(),
            ToolTimeout::After(Duration::from_secs(3_600))
        );

        // Removing the bound is the one thing refused, because only the author
        // can claim the body polls its token.
        let error = ExecutionLimits::default()
            .bounded_only_by_cancellation()
            .timeout_for(&identity(), declared)
            .unwrap_err();
        assert_eq!(error.kind(), "unbounded_not_declared");

        // A tool that did declare it may of course be run that way, and a caller
        // may still tighten even that.
        assert_eq!(
            ExecutionLimits::default()
                .bounded_only_by_cancellation()
                .timeout_for(&identity(), ToolTimeout::OnlyByCancellation)
                .unwrap(),
            ToolTimeout::OnlyByCancellation
        );
        assert_eq!(
            ExecutionLimits::default()
                .within(Duration::from_secs(1))
                .timeout_for(&identity(), ToolTimeout::OnlyByCancellation)
                .unwrap(),
            ToolTimeout::After(Duration::from_secs(1))
        );
    }

    #[test]
    fn the_grace_period_is_long_enough_for_a_cooperative_body_to_notice() {
        // A body checking its token between units of work sees the cancel at
        // most one poll after it is set. A grace shorter than a few polls would
        // abandon threads that were about to return on their own.
        assert!(
            TERMINATION_GRACE >= POLL_INTERVAL * 4,
            "a {TERMINATION_GRACE:?} grace gives a cooperative body too little time"
        );
    }
}
