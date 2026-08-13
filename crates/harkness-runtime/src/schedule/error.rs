//! Faults of the scheduler, as distinct from outcomes of a scheduled call.

use crate::domain::ToolCallId;
use crate::tool::ExecutionError;

/// Why a call could not be scheduled, or why its ticket carries no outcome.
///
/// The same separation [`ExecutionError`] draws, one layer up: a call that
/// failed, timed out, or was cancelled is an *outcome* and arrives as a
/// [`CompletedCall`](crate::tool::CompletedCall). This namespace is for the
/// cases in which no such record exists at all.
///
/// Two of the three variants are the scheduler's own. The third is the
/// executor's, carried rather than flattened, because a submission has to read
/// the recorded call before it can queue it — and "the store refused that read"
/// is the same fault, about the same connection, that the executor already
/// names. Inventing a second spelling for it would make a consumer branch on
/// which layer happened to be holding the connection.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ScheduleError {
    /// The scheduler is shutting down and accepts no further work.
    ///
    /// Raised by a submission that arrives after
    /// [`shutdown`](super::Scheduler::shutdown) has begun, and by one already
    /// blocked on a full queue when it begins — a producer waiting for room
    /// that will never be made must be told rather than left parked.
    #[error("the scheduler is shutting down and is not accepting tool call {call}")]
    Shutdown {
        /// Call that was refused.
        call: ToolCallId,
    },

    /// The call is already queued or running in this scheduler.
    ///
    /// One recorded call has one claim on it at a time. Two would be two
    /// tickets, two admissions, and two workspace slots for one row, and the
    /// executor's refusal of the second arrives too late to prevent any of
    /// that: it comes *after* dispatch, so the loser has already occupied a
    /// mutation slot and possibly a process slot on the way to being told no.
    ///
    /// The refusal also protects the run store. A resolution that checks a
    /// call's state and then writes its terminal state does so in two steps,
    /// which is sound for one claim and racy for two — a queued claim being
    /// cancelled could read `pending`, be overtaken by the other claim's
    /// dispatch, and record `cancelled` over a body that had just started.
    #[error("tool call {call} is already scheduled")]
    AlreadyScheduled {
        /// Call that was submitted twice.
        call: ToolCallId,
    },

    /// The worker carrying the call ended without reporting an outcome.
    ///
    /// The executor promises a terminal state on every path it returns from, so
    /// this is the layer underneath going away — an abort, a failed allocation.
    /// It is reported rather than waited on forever, because a ticket with no
    /// sender left is a wait with no end.
    #[error("the worker running tool call {call} ended without reporting an outcome")]
    WorkerLost {
        /// Call whose outcome will never arrive.
        call: ToolCallId,
    },

    /// The executor refused the call, before or instead of running it.
    ///
    /// Boxed because [`ExecutionError`] is much the larger of the two types and
    /// this is the rarest variant; a `Result` returned from every submission and
    /// every wait should not be sized by its least likely arm.
    #[error("the executor refused tool call {call}: {source}")]
    Execution {
        /// Call the executor refused.
        call: ToolCallId,
        /// What it refused with.
        #[source]
        source: Box<ExecutionError>,
    },
}

impl ScheduleError {
    /// Every stable discriminant this namespace defines *itself*.
    ///
    /// [`kinds`](Self::kinds) is what a consumer matches against, because
    /// [`Execution`](Self::Execution) reports the executor's spelling rather
    /// than one of these. The split follows
    /// [`InvocationError`](crate::tool::InvocationError), which publishes a
    /// union for the same reason: a wrapper that renamed what it wrapped would
    /// make one refusal answer to two names.
    pub const KINDS: &'static [&'static str] = &[
        "scheduler_shutting_down",
        "already_scheduled",
        "worker_lost",
    ];

    /// Every discriminant a `ScheduleError` can report, wrapped ones included.
    #[must_use]
    pub fn kinds() -> Vec<&'static str> {
        Self::KINDS
            .iter()
            .chain(ExecutionError::KINDS)
            .copied()
            .collect()
    }

    /// Stable machine-readable discriminant for caller-facing error handling.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Shutdown { .. } => "scheduler_shutting_down",
            Self::AlreadyScheduled { .. } => "already_scheduled",
            Self::WorkerLost { .. } => "worker_lost",
            Self::Execution { source, .. } => source.kind(),
        }
    }

    /// The call this refusal is about.
    ///
    /// Always known: nothing is refused here except on behalf of one recorded
    /// call, which is what lets a caller holding several tickets attribute a
    /// failure without tracking which submission produced it.
    #[must_use]
    pub const fn call(&self) -> ToolCallId {
        match self {
            Self::Shutdown { call }
            | Self::AlreadyScheduled { call }
            | Self::WorkerLost { call }
            | Self::Execution { call, .. } => *call,
        }
    }
}
