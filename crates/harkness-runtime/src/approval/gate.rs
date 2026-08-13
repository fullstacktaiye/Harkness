//! The wake-up channel a call parks on while a human decides.
//!
//! # Why this is not a database poll
//!
//! The call waiting for an answer holds no transaction — it cannot, because the
//! store serializes every write through one connection and a transaction held
//! across a human's attention span would stop every other run in the process.
//! What it holds instead is a parked thread and a condition variable keyed by
//! [`ApprovalId`]. The decision is a short write that commits and *then* wakes
//! the waiter, so the wait costs no lock and the wake costs no poll interval.
//!
//! # A decision may arrive before anyone waits
//!
//! Persisting the request, notifying the surfaces, and parking are three
//! separate steps, and a fast answer can land between the second and the third.
//! [`ApprovalGate::resolve`] therefore records the observation whether or not a
//! waiter exists, and [`ApprovalTicket::wait`] takes an already-recorded one
//! without blocking. A gate that only signalled live waiters would hang exactly
//! the runs whose approvals were answered fastest.
//!
//! # Every way out is an observation
//!
//! A denial, an expiry, and a run cancellation all wake the waiter with an
//! [`ApprovalObservation`], never with a bare boolean and never with a hang. The
//! observation carries the terminal state as well as the verdict, so a caller
//! can tell "a human said no" from "the run was cancelled underneath you" —
//! which are the same verdict and very different things to report.

use std::collections::HashMap;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::domain::ApprovalId;

use super::{ApprovalRequest, ApprovalScope, ApprovalState, ApprovalVerdict};

/// What a waiting call learns when its approval resolves.
///
/// Structured rather than boolean because an agent that is told only "no" cannot
/// say why it stopped, and a timeline that records only "no" cannot be audited.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalObservation {
    approval_id: ApprovalId,
    state: ApprovalState,
    verdict: ApprovalVerdict,
    scope: Option<ApprovalScope>,
    reason: Option<String>,
}

impl ApprovalObservation {
    /// Reads the observation out of a resolved request.
    ///
    /// `None` while the request is still pending: there is nothing to observe,
    /// and returning a denial-shaped value for a question nobody has answered is
    /// exactly the "absence of an answer is consent" mistake in reverse — it
    /// would report a refusal that was never made.
    #[must_use]
    pub fn of(request: &ApprovalRequest) -> Option<Self> {
        if !request.state().is_terminal() {
            return None;
        }
        let (verdict, scope, reason) = match request.decision() {
            Some(decision) => (
                decision.verdict(),
                decision.scope(),
                decision.reason().map(ToOwned::to_owned),
            ),
            // Expired, cancelled, and superseded requests were never answered.
            // They still owe the waiter a verdict, and the only safe one is a
            // refusal that names the state it came from.
            None => (
                ApprovalVerdict::Denied,
                None,
                Some(unanswered_reason(request.state()).to_owned()),
            ),
        };
        Some(Self {
            approval_id: request.id(),
            state: request.state(),
            verdict,
            scope,
            reason,
        })
    }

    /// Approval this observation resolves.
    #[must_use]
    pub const fn approval_id(&self) -> ApprovalId {
        self.approval_id
    }

    /// Terminal state the request reached.
    #[must_use]
    pub const fn state(&self) -> ApprovalState {
        self.state
    }

    /// Whether the work may proceed.
    #[must_use]
    pub const fn verdict(&self) -> ApprovalVerdict {
        self.verdict
    }

    /// Scope authorized, present only when a human granted one.
    #[must_use]
    pub const fn scope(&self) -> Option<ApprovalScope> {
        self.scope
    }

    /// Explanation suitable for a timeline entry or an agent observation.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    /// Whether the observation authorizes the call that was waiting.
    #[must_use]
    pub fn is_granted(&self) -> bool {
        self.verdict == ApprovalVerdict::Granted
    }
}

const fn unanswered_reason(state: ApprovalState) -> &'static str {
    match state {
        ApprovalState::Expired => "the approval request expired before it was answered",
        ApprovalState::Cancelled => "the run was cancelled while the approval was pending",
        ApprovalState::Superseded => "the run will not resume, so the approval authorizes nothing",
        // `of` only reaches this arm for a terminal state with no decision, and
        // `granted`/`denied` are unreachable without one.
        _ => "the approval was resolved without a decision",
    }
}

/// The rendezvous between a waiting call and whichever surface answers it.
///
/// One gate is shared by every run in a process. It holds no database handle and
/// takes no store lock: resolving is what the decision writer does *after* its
/// transaction commits.
#[derive(Debug, Default)]
pub struct ApprovalGate {
    resolved: Mutex<HashMap<ApprovalId, ApprovalObservation>>,
    decided: Condvar,
}

impl ApprovalGate {
    /// Creates an empty gate.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Claims the right to wait for `id`.
    ///
    /// Take the ticket *before* persisting the request. Registering afterwards
    /// would still be correct — [`resolve`](Self::resolve) records an answer
    /// nobody is waiting for — but taking it first is what makes the
    /// already-answered path a fast return rather than a lucky one.
    ///
    /// One approval has one waiter: it holds exactly one tool call, and that
    /// call is what parks. An observation is *taken* by the waiter that receives
    /// it rather than broadcast, so a second ticket for the same approval would
    /// wait for an answer the first already consumed. Do not hand one approval's
    /// ticket to two threads.
    #[must_use]
    pub const fn ticket(&self, id: ApprovalId) -> ApprovalTicket<'_> {
        ApprovalTicket { gate: self, id }
    }

    /// Records an observation and wakes whoever is waiting for it.
    ///
    /// Idempotent by last write: a run cancellation racing a human decision
    /// resolves the same approval twice in memory, and the store has already
    /// decided which of the two is the real one. Waking with either is safe
    /// because the waiter re-reads the record it is resuming.
    pub fn resolve(&self, observation: ApprovalObservation) {
        guard(&self.resolved).insert(observation.approval_id, observation);
        // Every waiter checks its own key, so a broadcast costs one spurious
        // wake per unrelated waiter and needs no per-approval condition
        // variable.
        self.decided.notify_all();
    }

    /// Resolves whichever approval a stored record just reached a terminal state
    /// in, doing nothing while the record is still pending.
    pub fn resolve_from(&self, request: &ApprovalRequest) {
        if let Some(observation) = ApprovalObservation::of(request) {
            self.resolve(observation);
        }
    }

    /// Whether an answer for `id` is already recorded.
    #[must_use]
    pub fn is_resolved(&self, id: ApprovalId) -> bool {
        guard(&self.resolved).contains_key(&id)
    }

    /// Discards a recorded answer nobody is going to wait for.
    fn forget(&self, id: ApprovalId) {
        guard(&self.resolved).remove(&id);
    }
}

/// One call's claim on the answer to one approval.
///
/// Dropping a ticket without waiting discards any answer recorded for it, so a
/// caller that gives up — because its run was torn down, or because the store
/// refused the request it was about to persist — leaves nothing behind in the
/// gate.
#[derive(Debug)]
pub struct ApprovalTicket<'a> {
    gate: &'a ApprovalGate,
    id: ApprovalId,
}

impl ApprovalTicket<'_> {
    /// Approval this ticket waits for.
    #[must_use]
    pub const fn approval_id(&self) -> ApprovalId {
        self.id
    }

    /// Parks until the approval resolves.
    ///
    /// Returns immediately when the answer already arrived. There is no timeout
    /// here on purpose: an approval's deadline is its `expires_at`, which is
    /// enforced by *resolving the request* so the record says what happened.
    /// A waiter that gave up on its own clock would leave a pending row behind
    /// and a run that looks like it is still asking.
    #[must_use]
    pub fn wait(self) -> ApprovalObservation {
        let mut resolved = guard(&self.gate.resolved);
        loop {
            if let Some(observation) = resolved.remove(&self.id) {
                // Released before the ticket's own drop runs, which takes the
                // same lock: `Mutex` is not reentrant, so returning while
                // holding this guard would deadlock on the way out.
                drop(resolved);
                return observation;
            }
            resolved = self
                .gate
                .decided
                .wait(resolved)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Parks until the approval resolves or `timeout` elapses.
    ///
    /// `None` means the wait gave up. It does **not** mean the request is
    /// resolved: the caller owes it an `Expired` or `Cancelled` resolution, and
    /// the ticket is handed back so the same wait can be resumed if the caller
    /// decides to keep waiting instead.
    pub fn wait_for(self, timeout: Duration) -> Result<ApprovalObservation, Self> {
        let deadline = Instant::now() + timeout;
        let mut resolved = guard(&self.gate.resolved);
        loop {
            if let Some(observation) = resolved.remove(&self.id) {
                drop(resolved);
                return Ok(observation);
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                drop(resolved);
                return Err(self);
            };
            resolved = self
                .gate
                .decided
                .wait_timeout(resolved, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

impl Drop for ApprovalTicket<'_> {
    fn drop(&mut self) {
        self.gate.forget(self.id);
    }
}

/// Takes a lock, adopting the contents even if a previous holder panicked.
///
/// The map is a rendezvous, not a resource: a panic while holding it leaves an
/// observation either recorded or not, and refusing to use the gate afterwards
/// would strand every waiting run rather than protect anything.
fn guard<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use crate::approval::record::tests::{at, pending};
    use crate::approval::{
        ApprovalDecision, ApprovalRequest, ApprovalScope, ApprovalState, ApprovalVerdict,
        DecidedVia,
    };
    use crate::domain::ApprovalId;
    use crate::tool::RiskLevel;

    use super::{ApprovalGate, ApprovalObservation};

    fn request() -> ApprovalRequest {
        ApprovalRequest::open(pending(RiskLevel::Execute)).unwrap()
    }

    #[test]
    fn a_pending_request_has_nothing_to_observe() {
        assert!(ApprovalObservation::of(&request()).is_none());
    }

    #[test]
    fn a_grant_observation_carries_its_scope_and_reason() {
        let mut record = request();
        record
            .decide(
                ApprovalDecision::grant(
                    record.id(),
                    ApprovalScope::ExactCall,
                    DecidedVia::Gui,
                    at(1),
                )
                .because("reviewed the diff"),
            )
            .unwrap();

        let observation = ApprovalObservation::of(&record).unwrap();

        assert!(observation.is_granted());
        assert_eq!(observation.state(), ApprovalState::Granted);
        assert_eq!(observation.scope(), Some(ApprovalScope::ExactCall));
        assert_eq!(observation.reason(), Some("reviewed the diff"));
    }

    #[test]
    fn every_unanswered_resolution_observes_a_denial_that_names_its_state() {
        for (state, fragment) in [
            (ApprovalState::Expired, "expired"),
            (ApprovalState::Cancelled, "cancelled"),
            (ApprovalState::Superseded, "will not resume"),
        ] {
            let mut record = request();
            record.resolve(state, at(9)).unwrap();

            let observation = ApprovalObservation::of(&record).unwrap();

            assert!(!observation.is_granted(), "{state}");
            assert_eq!(observation.verdict(), ApprovalVerdict::Denied);
            assert_eq!(observation.state(), state);
            assert!(observation.scope().is_none());
            assert!(observation.reason().unwrap().contains(fragment), "{state}");
        }
    }

    #[test]
    fn a_waiter_parks_until_another_thread_decides() {
        let gate = Arc::new(ApprovalGate::new());
        let mut record = request();
        let id = record.id();
        let ticket_gate = Arc::clone(&gate);

        let waiter = thread::spawn(move || ticket_gate.ticket(id).wait());

        // The decision happens on this thread, after the waiter has had a
        // chance to park; the gate is correct either way, which is what the
        // already-answered test below covers.
        record
            .decide(ApprovalDecision::deny(id, DecidedVia::Cli, at(2)))
            .unwrap();
        gate.resolve_from(&record);

        let observation = waiter.join().unwrap();
        assert_eq!(observation.approval_id(), id);
        assert!(!observation.is_granted());
    }

    #[test]
    fn an_answer_that_arrives_before_the_wait_is_not_lost() {
        let gate = ApprovalGate::new();
        let mut record = request();
        record.cancel(at(3)).unwrap();

        gate.resolve_from(&record);
        assert!(gate.is_resolved(record.id()));

        let observation = gate.ticket(record.id()).wait();
        assert_eq!(observation.state(), ApprovalState::Cancelled);
        assert!(
            !gate.is_resolved(record.id()),
            "a taken observation must not be delivered twice"
        );
    }

    #[test]
    fn a_run_cancellation_wakes_a_waiter_rather_than_leaving_it_parked() {
        let gate = Arc::new(ApprovalGate::new());
        let mut record = request();
        let id = record.id();
        let waiting = Arc::clone(&gate);

        let waiter = thread::spawn(move || waiting.ticket(id).wait());
        record.cancel(at(4)).unwrap();
        gate.resolve_from(&record);

        let observation = waiter.join().unwrap();
        assert_eq!(observation.state(), ApprovalState::Cancelled);
        assert_eq!(observation.verdict(), ApprovalVerdict::Denied);
    }

    #[test]
    fn a_bounded_wait_hands_the_ticket_back_instead_of_resolving_anything() {
        let gate = ApprovalGate::new();
        let mut record = request();

        let ticket = gate
            .ticket(record.id())
            .wait_for(Duration::from_millis(20))
            .expect_err("nothing has answered yet");
        assert_eq!(
            record.state(),
            ApprovalState::Pending,
            "a waiter giving up must not resolve the record"
        );

        record.expire(at(5)).unwrap();
        gate.resolve_from(&record);
        let observation = ticket.wait_for(Duration::from_secs(5)).unwrap();
        assert_eq!(observation.state(), ApprovalState::Expired);
    }

    #[test]
    fn a_dropped_ticket_leaves_nothing_behind_in_the_gate() {
        let gate = ApprovalGate::new();
        let mut record = request();
        let id = record.id();

        let ticket = gate.ticket(id);
        record.expire(at(6)).unwrap();
        gate.resolve_from(&record);
        drop(ticket);

        assert!(!gate.is_resolved(id));
    }

    #[test]
    fn one_gate_serves_many_approvals_without_crossing_their_answers() {
        let gate = Arc::new(ApprovalGate::new());
        let ids = (0..8).map(|_| ApprovalId::new()).collect::<Vec<_>>();

        let waiters = ids
            .iter()
            .map(|id| {
                let gate = Arc::clone(&gate);
                let id = *id;
                thread::spawn(move || gate.ticket(id).wait())
            })
            .collect::<Vec<_>>();

        for id in &ids {
            let mut record = request();
            // Every observation names its own approval, so a waiter woken by a
            // broadcast for somebody else must stay parked.
            record.cancel(at(7)).unwrap();
            let mut observation = ApprovalObservation::of(&record).unwrap();
            observation.approval_id = *id;
            gate.resolve(observation);
        }

        for (waiter, id) in waiters.into_iter().zip(&ids) {
            assert_eq!(waiter.join().unwrap().approval_id(), *id);
        }
    }
}
