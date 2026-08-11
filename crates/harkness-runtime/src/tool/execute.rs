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
//! Scheduling, queueing, and concurrency limits (#93); policy evaluation and
//! approvals (#91, #92) — the executor assumes the call it is handed is already
//! authorized; and any concrete tool (#94, #95).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use harkness_git::Cancellation;
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::domain::{Failure, RunId, ToolCall, ToolCallId, ToolCallState};
use crate::store::{EventKind, RunEvent, Store, StoreArtifacts, StoreError};

use super::{
    DEFAULT_PROGRESS_CAPACITY, DEFAULT_STREAM_TAIL_BYTES, Deadline, ErasedTool, ExecutionContext,
    InvocationError, POLL_INTERVAL, ProgressReceiver, ToolError, ToolId, ToolIdentity, ToolOutcome,
    ToolRegistry, ToolTimeout, ToolVersion, invoke_resolved, progress_channel,
};

/// Kind recorded when a tool's result is larger than a result may be.
///
/// Borrowed from the store's own namespace rather than invented here, because it
/// is the store's bound that was broken and a consumer branching on the kind
/// should not have to learn two spellings for one refusal.
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
/// [`ToolTimeout`]. What a caller may do is tighten it, which is what
/// [`within`](Self::within) is for.
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
    /// Replaces the tool's declared timeout with `limit`.
    #[must_use]
    pub const fn within(mut self, limit: Duration) -> Self {
        self.timeout = Some(ToolTimeout::After(limit));
        self
    }

    /// Asks that only cancellation bound the call.
    ///
    /// Accepted only for a tool that declared
    /// [`ToolTimeout::OnlyByCancellation`] itself. A caller cannot lift a limit
    /// a tool asked for: the declaration is the author's claim that the body is
    /// stoppable, and lifting a timeout from a body that never polls its token
    /// produces a call with no way to end at all. Refused with
    /// [`ExecutionError::UnboundedNotDeclared`].
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

    /// The call is not in a state execution can begin from.
    #[error("tool call {call} is {state} and only a pending call can be dispatched")]
    NotDispatchable {
        /// Call that was handed to the executor.
        call: ToolCallId,
        /// State it was found in.
        state: ToolCallState,
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
        let record = self.store.load_tool_call(call)?;
        if record.state() != ToolCallState::Pending {
            return Err(ExecutionError::NotDispatchable {
                call,
                state: record.state(),
            });
        }

        // Resolution happens before anything is written, so a call naming a tool
        // that does not exist fails without ever having been `running`.
        let tool = match self.resolve(&record) {
            Ok(tool) => tool,
            Err(error) => {
                let failure = error.as_failure();
                return self.finish(call, None, CallOutcome::Failed { failure });
            }
        };
        let identity = tool.descriptor().identity().clone();
        let timeout = self
            .limits
            .timeout_for(&identity, tool.descriptor().timeout())?;

        // The context is built before the record moves, because building it can
        // still refuse the call — an unusable workspace root — and a refusal
        // after the dispatch would leave a `running` row for work that never
        // began. Its deadline is attached afterwards, so a tool is given the
        // limit it declared rather than that limit minus a database write.
        let (progress, reports) = progress_channel(self.limits.progress_capacity);
        let mut context = ExecutionContext::new(
            record.run_id(),
            record.step_id(),
            call,
            workspace_root,
            cancellation.clone(),
            Box::new(progress),
            Box::new(StoreArtifacts::new(
                Arc::clone(&self.store),
                record.run_id(),
                record.step_id(),
                call,
            )),
        )
        .map_err(|source| ExecutionError::Context { call, source })?
        .with_stream_tail_bytes(self.limits.stream_tail_bytes);

        // Everything that could refuse this call has now refused it, so the
        // record moves to `running` — pinning the version that was resolved —
        // before the body is allowed to start.
        let at = OffsetDateTime::now_utc();
        let version = identity.version.to_string();
        let (dispatched, _) = self.store.dispatch_tool_call_with_event(
            call,
            &version,
            at,
            RunEvent::new(EventKind::ToolCallStateChanged, at)
                .for_step(record.step_id())
                .for_tool_call(call)
                .with_payload(json!({
                    "state": ToolCallState::Running.as_str(),
                    "tool_id": identity.id.as_str(),
                    "tool_version": version,
                })),
        )?;
        let run_id = dispatched.run_id();

        let deadline = timeout.limit().map(Deadline::starting_now);
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

        let outcome = self.supervise(run_id, call, &awaiting, &reports, cancellation, deadline);
        self.finish(call, Some(identity), outcome)
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
    fn supervise(
        &self,
        run_id: RunId,
        call: ToolCallId,
        awaiting: &Receiver<Result<ToolOutcome, ToolError>>,
        reports: &ProgressReceiver,
        cancellation: &Cancellation,
        deadline: Option<Deadline>,
    ) -> CallOutcome {
        // `Some` once the executor has asked the call to stop: it holds the
        // verdict to record if the body does not come back, and the instant the
        // grace period is measured from.
        let mut stopping: Option<(CallOutcome, std::time::Instant)> = None;

        loop {
            self.record_progress(run_id, call, reports);

            // Waiting on the channel rather than sleeping beside it: a fast
            // tool is not made to pay a poll interval it had no reason to, and a
            // slow one still wakes this loop on the cadence progress draining
            // and the two limits are checked at.
            match awaiting.recv_timeout(POLL_INTERVAL) {
                Ok(result) => {
                    self.record_progress(run_id, call, reports);
                    let reported = outcome_of(result);
                    // A body that finished on its own terms outranks anything
                    // the executor could infer about work it could not see —
                    // including work completed just as a stop was requested,
                    // whose side effects have happened and must be recorded.
                    //
                    // What it does *not* outrank is the executor's own reason
                    // for stopping. Stopping means cancelling the token, so a
                    // tool killed by its deadline reports `cancelled`: that is
                    // the echo of this decision, not independent evidence, and
                    // recording it would tell a user their work was cancelled
                    // when in fact it ran out of time.
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
                    self.record_progress(run_id, call, reports);
                    return stopping.map_or_else(
                        || CallOutcome::Failed {
                            failure: ToolError::Interrupted.as_failure(),
                        },
                        |(verdict, _)| verdict,
                    );
                }
                Err(RecvTimeoutError::Timeout) => {}
            }

            if let Some((verdict, since)) = &stopping {
                if since.elapsed() >= TERMINATION_GRACE {
                    return verdict.clone();
                }
            } else if let Some(verdict) = self.reason_to_stop(cancellation, deadline) {
                // Cancelling the token is what actually stops the work: a
                // process-backed tool kills its child's group off the back of
                // it, and a cooperative body returns at its next check.
                cancellation.cancel();
                stopping = Some((verdict, std::time::Instant::now()));
            }
        }
    }

    /// The verdict to record, when there is a reason to stop the call.
    fn reason_to_stop(
        &self,
        cancellation: &Cancellation,
        deadline: Option<Deadline>,
    ) -> Option<CallOutcome> {
        if cancellation.is_cancelled() {
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
    fn record_progress(&self, run_id: RunId, call: ToolCallId, reports: &ProgressReceiver) {
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
                RunEvent::new(EventKind::ToolProgress, at)
                    .for_tool_call(call)
                    .with_payload(serde_json::to_value(&event).unwrap_or(Value::Null))
            }),
        );
    }

    /// Records the terminal state and its event, then reports the outcome.
    ///
    /// The commit happens here and nowhere else, so "persisted before delivered"
    /// is a property of one function rather than of every path that reaches one.
    fn finish(
        &self,
        call: ToolCallId,
        tool: Option<ToolIdentity>,
        outcome: CallOutcome,
    ) -> Result<CompletedCall, ExecutionError> {
        let at = OffsetDateTime::now_utc();
        let record = match self.persist(call, &outcome, at) {
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
                let record = self.persist(call, &outcome, at)?;
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
        outcome: &CallOutcome,
        at: OffsetDateTime,
    ) -> Result<ToolCall, StoreError> {
        let event = |state: ToolCallState, detail: Value| {
            RunEvent::new(EventKind::ToolCallStateChanged, at)
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
    fn a_caller_may_tighten_a_declared_limit_but_never_remove_one() {
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

        // Lifting a limit the tool asked for would produce a call with no way to
        // end: the declaration is the author's claim that the body is stoppable.
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
