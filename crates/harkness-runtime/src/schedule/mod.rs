//! Deciding *when* a recorded tool call runs, and how many run at once.
//!
//! # Why a scheduler exists at all
//!
//! [`tool`](crate::tool) answers "what does running this call mean" and
//! guarantees that one call reaches a terminal state whatever the tool does. It
//! deliberately answers nothing about a *second* call. Without a layer that
//! does, every caller would have to re-derive the same three facts — that two
//! mutations of one worktree must not overlap, that reads may, and that a
//! machine has a finite number of child processes it can usefully run — and
//! each would get them subtly differently. The property that matters is not
//! that some caller serializes correctly; it is that a caller *cannot* fail to.
//!
//! Submitting through [`Scheduler::submit`] is therefore the only thing a
//! coordinator does, and every concurrency question is answered once, here.
//!
//! # Serialization is a safety property
//!
//! Two concurrent mutations of one checkout interleave index writes and leave a
//! worktree in a state neither call asked for. The workspace mutation slot
//! makes that unrepresentable above the Git layer, and `RepositoryLock` remains
//! the backstop beneath it — the two are not redundant. The lock is keyed by
//! Git's common directory, so every *linked worktree* of one repository shares
//! it; the scheduler's [`WorkspaceKey`] is finer, naming one checkout, and
//! sits above it. Work in two worktrees of one repository therefore overlaps
//! here and serializes there, which is the correct answer to both questions.
//!
//! Ordering, top to bottom: **scheduler workspace slot → repository lock →
//! catalog lock**. The scheduler never calls catalog or Git code, so it has no
//! way to violate the two beneath it, and it holds none of its own locks across
//! an executor call, a store write, or a child wait.
//!
//! # Backpressure, never dropping
//!
//! Every queue here has a named capacity — [`WORKSPACE_QUEUE_CAPACITY`] for
//! submissions, [`OUTCOME_CAPACITY`] for a ticket's single result — above the
//! executor's own bounded progress channel and the store's event append. A full
//! queue slows its producer down. Nothing is discarded to make room, because a
//! discarded call is a run whose history omits work somebody asked for, and a
//! discarded progress event is an audit trail with a hole in it.
//!
//! # The cancellation chain
//!
//! [`Scheduler::cancel_run`] is one end of a chain that reaches the operating
//! system:
//!
//! ```text
//! cancel_run → queued calls swept and recorded `cancelled`, undispatched
//!            → running calls' caller tokens tripped
//!              → executor cancels each call's own token
//!                → cooperative body returns / ToolProcess kills the process group
//! ```
//!
//! Each link is owned by exactly one layer, which is why a queued call never
//! becomes `running` on its way to being cancelled: dispatching work in order
//! to stop it would start a body, take a process slot, and write a `running`
//! state for something that never began.
//!
//! # What this module does not do
//!
//! It decides *when*, never *whether*: there is no policy evaluation and no
//! approval flow here — a call arrives already authorized, or already decided
//! and marked as such. It sequences nothing above a call either; run-level
//! orchestration and step ordering belong to the coordinator that submits.
//! There are no priorities, no deadlines, and no preemption.
//!
//! Ordering is FIFO *within* a workspace, which is what makes starvation
//! unrepresentable there. Between workspaces there is exactly one contended
//! resource — the global process limit — and it needs its own answer, because
//! a fixed sweep order would give one key a permanent advantage over its
//! neighbours; a freed slot is therefore offered to each workspace first in
//! turn. Nothing else here orders one workspace against another.
//!
//! And it schedules only this process's work: a second Harkness process is
//! bounded by the repository lock and by nothing here.

mod error;
mod scheduler;
mod snapshot;
mod ticket;
mod workspace;

#[cfg(test)]
mod tests;

pub use error::ScheduleError;
pub use scheduler::{
    CancelReport, DEFAULT_SHUTDOWN_DEADLINE, MAX_PROCESS_CONCURRENCY, ScheduledCall, Scheduler,
    ShutdownReport, WORKSPACE_QUEUE_CAPACITY, WORKSPACE_READ_CONCURRENCY,
};
pub use snapshot::{ProcessSlots, ScheduleSnapshot, WorkspaceLoad};
pub use ticket::{CallTicket, OUTCOME_CAPACITY, Scheduled};
pub use workspace::WorkspaceKey;
