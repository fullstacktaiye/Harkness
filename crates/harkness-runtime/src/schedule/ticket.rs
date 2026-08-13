//! The handle a submitter keeps while its call waits its turn.

use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::time::Duration;

use crate::domain::ToolCallId;
use crate::tool::{CompletedCall, ExecutionError};

use super::ScheduleError;

/// How many outcomes one ticket's channel holds.
///
/// One, because a ticket carries exactly one call and a call reaches exactly
/// one terminal state. The capacity is named rather than written inline so the
/// module's rule — every queue has a named capacity — has no exception, and so
/// a reader can see that a reporting worker never blocks: the buffer is always
/// large enough for everything that will ever be sent through it, whether or
/// not anybody is still holding the ticket.
pub const OUTCOME_CAPACITY: usize = 1;

/// What one scheduled call ultimately produced.
///
/// The nesting is the point. The outer result says whether the call ever
/// reached the executor; the inner one, whether the executor could drive it —
/// and a call that ran and *failed* is neither, it is a
/// [`CompletedCall`](crate::tool::CompletedCall) carrying a
/// [`Failed`](crate::tool::CallOutcome::Failed) outcome.
pub type Scheduled = Result<CompletedCall, ScheduleError>;

/// The end of a ticket's channel a worker reports through.
pub(super) type Report = SyncSender<Result<CompletedCall, ExecutionError>>;

/// Creates the one-slot channel joining a ticket to the worker that fills it.
pub(super) fn outcome_channel(call: ToolCallId) -> (Report, CallTicket) {
    let (report, outcome) = sync_channel(OUTCOME_CAPACITY);
    (report, CallTicket { call, outcome })
}

/// A claim on the outcome of one submitted call.
///
/// Handed back by [`submit`](super::Scheduler::submit) as soon as the call is
/// *accepted*, which is deliberately earlier than when it starts: a submitter
/// that had to wait for dispatch to learn the call's identity could not cancel
/// it, render it as queued, or hold it while doing anything else.
///
/// Dropping a ticket abandons the outcome, never the call. The work still runs
/// to a terminal state and is still recorded — the scheduler's channel has room
/// for the result whether or not a receiver survives to read it, exactly as a
/// tool's progress is not an error to stop listening to.
#[derive(Debug)]
pub struct CallTicket {
    call: ToolCallId,
    outcome: Receiver<Result<CompletedCall, ExecutionError>>,
}

impl CallTicket {
    /// The recorded call this ticket is a claim on.
    #[must_use]
    pub const fn call(&self) -> ToolCallId {
        self.call
    }

    /// Blocks until the call reaches a terminal state.
    ///
    /// Returns the record the store committed, however the call ended —
    /// succeeded, failed, timed out, or cancelled before it was ever
    /// dispatched.
    ///
    /// # Errors
    ///
    /// Returns [`ScheduleError::WorkerLost`] when the worker ended without
    /// reporting, which the executor's own contract makes reachable only by the
    /// layer beneath it going away.
    pub fn wait(self) -> Scheduled {
        settle(self.call, self.outcome.recv().ok())
    }

    /// Blocks for at most `limit`, handing the ticket back if nothing settled.
    ///
    /// The ticket is returned rather than consumed on a timeout so that a
    /// caller polling a queued call — a front end drawing a progress surface,
    /// a test unwilling to hang — cannot accidentally discard its only claim on
    /// an outcome that has not arrived yet.
    ///
    /// # Errors
    ///
    /// As [`wait`](Self::wait), inside the outer `Ok`. The outer `Err` is the
    /// ticket itself and means only that `limit` passed.
    pub fn wait_for(self, limit: Duration) -> Result<Scheduled, Self> {
        match self.outcome.recv_timeout(limit) {
            Ok(settled) => Ok(settle(self.call, Some(settled))),
            Err(RecvTimeoutError::Disconnected) => Ok(settle(self.call, None)),
            Err(RecvTimeoutError::Timeout) => Err(self),
        }
    }
}

/// Projects what a worker reported — or its silence — into one outcome.
fn settle(call: ToolCallId, reported: Option<Result<CompletedCall, ExecutionError>>) -> Scheduled {
    match reported {
        Some(Ok(completed)) => Ok(completed),
        // An executor fault is not a scheduling fault, and flattening the two
        // would lose which layer refused. It travels inside the ticket rather
        // than out of `submit`, because by the time it happens the call has
        // been accepted and the submitter has gone on to something else.
        Some(Err(source)) => Err(ScheduleError::Execution {
            call,
            source: Box::new(source),
        }),
        None => Err(ScheduleError::WorkerLost { call }),
    }
}
