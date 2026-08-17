//! Marking one abandoned run, everything under it, and its unanswered
//! questions, in a single transaction.
//!
//! Recovery only ever *appends*. No event is deleted, no earlier event is
//! rewritten, and the run's history up to the moment its process stopped is
//! left exactly as that process wrote it. What this adds is the ending the dead
//! process never got to write: the run, its unfinished steps, its in-flight
//! calls, and the approvals nobody can answer any more all reach a terminal
//! state, each with its own event.
//!
//! # Why one transaction per run
//!
//! A store holding a hundred abandoned runs may hold one whose rows a hand edit
//! or an older bug made unloadable. Sweeping them all under one transaction
//! would let that single record block the recovery of every other. Each run is
//! therefore its own read-modify-write, and a run that cannot be recovered is
//! reported rather than retried forever or silently skipped.
//!
//! # Why the candidate set is re-read under the write lock
//!
//! The events are described *before* the transaction opens, exactly as every
//! other paired write in this module describes its own — redaction and encoding
//! must not happen under the write lock. A record that reached a terminal state
//! in between is then skipped along with the event that described it, so a
//! second sweeper racing the first appends nothing rather than appending a
//! second set of markings for work already marked.

use rusqlite::Connection;
use serde_json::json;
use time::OffsetDateTime;

use crate::approval::{ApprovalRequest, ApprovalState};
use crate::domain::{
    ApprovalId, ExecutionState, LeaseId, RunId, StepId, ToolCallId, ToolCallState,
};

use super::error::StoreError;
use super::{EventKind, PreparedEvent, RunEvent, Store, approval, lease, repository};

/// Why a run was found abandoned.
///
/// Carried into the `run_interrupted` event so a timeline says what was
/// detected rather than only that something was. The lock-file answer and the
/// row's own claim are separate variants because they are separate evidence:
/// one is the kernel reporting that a process is gone, the other is this build
/// reporting that a coordinator said so on its way out.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptionReason {
    /// The owning lease's advisory lock file was acquirable, so its holder is
    /// gone — whatever ended it, and whether or not it got to say anything.
    LeaseLockReleased,
    /// The owning lease's row already recorded that the claim was over.
    LeaseReleased,
    /// The run named no lease at all, so no process can be driving it.
    NoLease,
    /// The lease could not be probed and outlived the interval it is renewed
    /// on by the whole grace period.
    LeaseExpired,
}

impl InterruptionReason {
    /// Every reason in its stable declaration order.
    pub const ALL: &'static [Self] = &[
        Self::LeaseLockReleased,
        Self::LeaseReleased,
        Self::NoLease,
        Self::LeaseExpired,
    ];

    /// The stable spelling recorded in the `run_interrupted` payload.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LeaseLockReleased => "lease_lock_released",
            Self::LeaseReleased => "lease_released",
            Self::NoLease => "no_lease",
            Self::LeaseExpired => "lease_expired",
        }
    }
}

impl std::fmt::Display for InterruptionReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What one recovered run had to be marked.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunInterruption {
    run: RunId,
    steps: Vec<StepId>,
    tool_calls: Vec<ToolCallId>,
    approvals: Vec<ApprovalRequest>,
}

impl RunInterruption {
    /// The run that was marked.
    #[must_use]
    pub const fn run(&self) -> RunId {
        self.run
    }

    /// Steps this sweep terminalized, in ordinal order.
    #[must_use]
    pub fn steps(&self) -> &[StepId] {
        &self.steps
    }

    /// Tool calls this sweep terminalized, in creation order.
    #[must_use]
    pub fn tool_calls(&self) -> &[ToolCallId] {
        &self.tool_calls
    }

    /// The requests this sweep resolved, as they were stored.
    ///
    /// Returned whole rather than as identifiers because a caller has to wake
    /// whoever was parked on them, and the gate is keyed on the record.
    #[must_use]
    pub fn approvals(&self) -> &[ApprovalRequest] {
        &self.approvals
    }

    /// Identifiers of the requests this sweep resolved.
    #[must_use]
    pub fn approval_ids(&self) -> Vec<ApprovalId> {
        self.approvals
            .iter()
            .map(ApprovalRequest::id)
            .collect::<Vec<_>>()
    }
}

/// Everything the sweep needs about one run before it takes the write lock.
struct Planned {
    run: RunId,
    interrupted: PreparedEvent,
    run_state: PreparedEvent,
    steps: Vec<(StepId, PreparedEvent)>,
    tool_calls: Vec<(ToolCallId, PreparedEvent)>,
    approvals: Vec<(ApprovalRequest, PreparedEvent)>,
}

impl Planned {
    fn discard_spills(&self, store: &Store) {
        for event in [&self.interrupted, &self.run_state]
            .into_iter()
            .chain(self.steps.iter().map(|(_, event)| event))
            .chain(self.tool_calls.iter().map(|(_, event)| event))
            .chain(self.approvals.iter().map(|(_, event)| event))
        {
            event.discard_spill(store);
        }
    }
}

/// Marks one abandoned run and everything under it, or reports it was already
/// terminal.
///
/// `Ok(None)` means another sweeper — or the run's own dying process — got
/// there first. That is not a failure: "exactly one set of markings" is what it
/// looks like from the loser's side.
pub(super) fn interrupt(
    store: &Store,
    run: RunId,
    lease: Option<LeaseId>,
    reason: InterruptionReason,
    at: OffsetDateTime,
) -> Result<Option<RunInterruption>, StoreError> {
    let record = store.load_run(run)?;
    if record.state().is_terminal() {
        return Ok(None);
    }
    let planned = plan(store, run, lease, reason, at)?;
    let result = store.in_write_transaction("interrupting an abandoned run", |connection| {
        apply(connection, &planned, at)
    });
    // Every payload here is a fixed handful of fields, so none of them can
    // overflow into an artifact. Cleaning up anyway is what keeps that true by
    // construction rather than by inspection of the payloads above.
    if result.is_err() {
        planned.discard_spills(store);
    }
    result
}

fn plan(
    store: &Store,
    run: RunId,
    lease: Option<LeaseId>,
    reason: InterruptionReason,
    at: OffsetDateTime,
) -> Result<Planned, StoreError> {
    let interrupted = store.prepare_event(
        run,
        RunEvent::new(EventKind::RunInterrupted, at).with_payload(json!({
            "reason": reason.as_str(),
            "lease_id": lease.map(|lease| lease.to_string()),
        })),
    )?;
    let run_state = store.prepare_event(
        run,
        RunEvent::new(EventKind::RunStateChanged, at)
            .with_payload(json!({"state": ExecutionState::Interrupted.as_str()})),
    )?;

    let mut tool_calls = Vec::new();
    for call in store.load_run_tool_calls(run)? {
        if call.state().is_terminal() {
            continue;
        }
        let event = store.prepare_event(
            run,
            RunEvent::new(EventKind::ToolCallStateChanged, at)
                .for_step(call.step_id())
                .for_tool_call(call.id())
                .with_payload(json!({
                    "state": ToolCallState::Interrupted.as_str(),
                    "reason": reason.as_str(),
                })),
        )?;
        tool_calls.push((call.id(), event));
    }

    let mut steps = Vec::new();
    for step in store.load_run_steps(run)? {
        if step.state().is_terminal() {
            continue;
        }
        let event = store.prepare_event(
            run,
            RunEvent::new(EventKind::StepFinished, at)
                .for_step(step.id())
                .with_payload(json!({
                    "state": ExecutionState::Interrupted.as_str(),
                    "reason": reason.as_str(),
                })),
        )?;
        steps.push((step.id(), event));
    }

    let mut approvals = Vec::new();
    for request in store.run_approvals(run)? {
        if request.state() != ApprovalState::Pending {
            continue;
        }
        let event = store.prepare_event(
            run,
            approval::unanswered_event(&request, ApprovalState::Superseded, at),
        )?;
        approvals.push((request, event));
    }

    Ok(Planned {
        run,
        interrupted,
        run_state,
        steps,
        tool_calls,
        approvals,
    })
}

fn apply(
    connection: &Connection,
    planned: &Planned,
    at: OffsetDateTime,
) -> Result<Option<RunInterruption>, StoreError> {
    let mut run = repository::load_run(connection, planned.run)?;
    if run.state().is_terminal() {
        return Ok(None);
    }
    planned.interrupted.append(connection, planned.run)?;

    let mut tool_calls = Vec::new();
    for (id, event) in &planned.tool_calls {
        let mut call = repository::load_tool_call(connection, *id)?;
        if call.state().is_terminal() {
            continue;
        }
        call.transition(ToolCallState::Interrupted, at)
            .map_err(StoreError::InvalidTransition)?;
        repository::update_tool_call(connection, &call)?;
        event.append(connection, planned.run)?;
        tool_calls.push(*id);
    }

    let mut steps = Vec::new();
    for (id, event) in &planned.steps {
        let mut step = repository::load_step(connection, *id)?;
        if step.state().is_terminal() {
            continue;
        }
        step.transition(ExecutionState::Interrupted, at)
            .map_err(StoreError::InvalidTransition)?;
        repository::update_step(connection, &step)?;
        event.append(connection, planned.run)?;
        steps.push(*id);
    }

    let mut approvals = Vec::new();
    for (request, event) in &planned.approvals {
        let mut stored = approval::load(connection, request.id())?;
        if stored.state() != ApprovalState::Pending {
            continue;
        }
        // `Superseded` is the vocabulary #92 already defined for exactly this:
        // the run will not resume, so the question no longer has a subject. It
        // is terminal, which is what stops a prompt left open in a restarted
        // front end from authorizing anything.
        stored
            .resolve(ApprovalState::Superseded, at)
            .map_err(StoreError::Approval)?;
        approval::update_resolution(connection, &stored)?;
        event.append(connection, planned.run)?;
        approvals.push(stored);
    }

    run.transition(ExecutionState::Interrupted, at)
        .map_err(StoreError::InvalidTransition)?;
    repository::update_run(connection, &run)?;
    planned.run_state.append(connection, planned.run)?;
    // The dead owner is written off in the same transaction that marks its
    // last run, so a later start neither probes its lock file again nor finds
    // a live-looking claim with nothing left to claim.
    if let Some(id) = repository::run_lease_of(connection, planned.run)? {
        lease::release(connection, id, at)?;
    }

    Ok(Some(RunInterruption {
        run: planned.run,
        steps,
        tool_calls,
        approvals,
    }))
}
