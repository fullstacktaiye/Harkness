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
//! # A decision may arrive before anyone parks
//!
//! Persisting the request, notifying the surfaces, and parking are three
//! separate steps, and a fast answer can land between the second and the third.
//! The ticket is therefore taken *first*, before the request is persisted: from
//! that moment [`ApprovalGate::resolve`] has somewhere to put the answer, and
//! [`ApprovalTicket::wait`] takes an already-recorded one without blocking.
//!
//! An answer for an approval with **no live ticket is discarded**, which is what
//! keeps the gate bounded by the calls currently waiting rather than by the
//! approvals ever resolved. That case is common and is not a lost wake-up: a
//! restart supersedes the requests an interrupted run left behind, and a
//! cancellation resolves approvals whose callers have already exited. Neither
//! has anybody to wake, and an answer kept for a waiter that will never arrive
//! is a leak rather than a safety net.
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
    /// One entry per *live ticket*, holding its answer once one arrives.
    ///
    /// Keyed on the ticket rather than on the answer, so the map is bounded by
    /// the calls currently waiting and not by the approvals ever resolved. A map
    /// that recorded every answer would grow without limit in exactly the case
    /// that has no waiter at all: a restart superseding the requests an
    /// interrupted run left behind, or a cancellation resolving approvals whose
    /// callers are long gone.
    waiting: Mutex<HashMap<ApprovalId, Option<ApprovalObservation>>>,
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
    /// **Take the ticket before persisting the request.** An answer for an
    /// approval with no live ticket is discarded, so registering afterwards
    /// leaves a window in which a fast decision is dropped and the waiter parks
    /// forever. Taking it first closes that window and costs nothing: the
    /// already-answered path becomes a return without blocking.
    ///
    /// Discarding is the right default for everything else. A restart that
    /// supersedes the requests an interrupted run left behind, and a
    /// cancellation that resolves approvals whose callers have exited, both
    /// resolve approvals nobody is waiting for — and an answer kept for a waiter
    /// that will never arrive is a leak, not a safety net.
    ///
    /// One approval has one waiter: it holds exactly one tool call, and that
    /// call is what parks. An observation is *taken* by the waiter that receives
    /// it rather than broadcast, so a second ticket for the same approval would
    /// replace the first's registration. Do not hand one approval's ticket to
    /// two threads.
    #[must_use]
    pub fn ticket(&self, id: ApprovalId) -> ApprovalTicket<'_> {
        guard(&self.waiting).entry(id).or_default();
        ApprovalTicket { gate: self, id }
    }

    /// Records an observation and wakes whoever is waiting for it.
    ///
    /// Does nothing when no ticket is outstanding for the approval; see
    /// [`ticket`](Self::ticket) for why that is the safe default and why a
    /// waiter must register before its request is persisted.
    ///
    /// Idempotent by last write: a run cancellation racing a human decision
    /// resolves the same approval twice in memory, and the store has already
    /// decided which of the two is the real one. Waking with either is safe
    /// because the waiter re-reads the record it is resuming.
    pub fn resolve(&self, observation: ApprovalObservation) {
        let mut waiting = guard(&self.waiting);
        let Some(slot) = waiting.get_mut(&observation.approval_id) else {
            return;
        };
        *slot = Some(observation);
        drop(waiting);
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

    /// Whether an answer for `id` has arrived and not yet been taken.
    #[must_use]
    pub fn is_resolved(&self, id: ApprovalId) -> bool {
        guard(&self.waiting).get(&id).is_some_and(Option::is_some)
    }

    /// Whether a ticket for `id` is outstanding.
    #[must_use]
    pub fn is_waiting(&self, id: ApprovalId) -> bool {
        guard(&self.waiting).contains_key(&id)
    }

    /// Releases a ticket's registration, with any answer it never took.
    fn forget(&self, id: ApprovalId) {
        guard(&self.waiting).remove(&id);
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
        let mut waiting = guard(&self.gate.waiting);
        loop {
            if let Some(observation) = waiting.get_mut(&self.id).and_then(Option::take) {
                // Released before the ticket's own drop runs, which takes the
                // same lock: `Mutex` is not reentrant, so returning while
                // holding this guard would deadlock on the way out.
                drop(waiting);
                return observation;
            }
            waiting = self
                .gate
                .decided
                .wait(waiting)
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
        let mut waiting = guard(&self.gate.waiting);
        loop {
            if let Some(observation) = waiting.get_mut(&self.id).and_then(Option::take) {
                drop(waiting);
                return Ok(observation);
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                drop(waiting);
                // The registration stays, so an answer arriving while the caller
                // decides whether to keep waiting is still delivered to the
                // ticket it hands back.
                return Err(self);
            };
            waiting = self
                .gate
                .decided
                .wait_timeout(waiting, remaining)
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
        let gate = ApprovalGate::new();
        let mut record = request();
        let id = record.id();

        // The ticket is taken here, on the thread that is about to persist the
        // request, and *then* handed to the waiter — which is the order the
        // contract requires and the reason a decision cannot outrun it.
        let ticket = gate.ticket(id);
        thread::scope(|scope| {
            let waiter = scope.spawn(move || ticket.wait());

            record
                .decide(ApprovalDecision::deny(id, DecidedVia::Cli, at(2)))
                .unwrap();
            gate.resolve_from(&record);

            let observation = waiter.join().unwrap();
            assert_eq!(observation.approval_id(), id);
            assert!(!observation.is_granted());
        });
    }

    #[test]
    fn an_answer_that_arrives_before_the_wait_begins_is_not_lost() {
        // The window the registration order closes: the request is persisted and
        // answered before its caller ever parks.
        let gate = ApprovalGate::new();
        let mut record = request();
        let ticket = gate.ticket(record.id());
        record.cancel(at(3)).unwrap();

        gate.resolve_from(&record);
        assert!(gate.is_resolved(record.id()));

        let observation = ticket.wait();
        assert_eq!(observation.state(), ApprovalState::Cancelled);
        assert!(
            !gate.is_waiting(record.id()),
            "taking an observation releases its registration"
        );
    }

    #[test]
    fn an_answer_for_an_approval_nobody_is_waiting_for_is_discarded() {
        // The shape a restart has: pending requests left by an interrupted run
        // are superseded, and none of them has a caller. Keeping those answers
        // would grow the gate for the life of the process.
        let gate = ApprovalGate::new();
        let mut abandoned = request();
        abandoned.supersede(at(3)).unwrap();

        gate.resolve_from(&abandoned);

        assert!(!gate.is_waiting(abandoned.id()));
        assert!(!gate.is_resolved(abandoned.id()));
    }

    #[test]
    fn a_run_cancellation_wakes_a_waiter_rather_than_leaving_it_parked() {
        let gate = ApprovalGate::new();
        let mut record = request();
        let ticket = gate.ticket(record.id());

        thread::scope(|scope| {
            let waiter = scope.spawn(move || ticket.wait());
            record.cancel(at(4)).unwrap();
            gate.resolve_from(&record);

            let observation = waiter.join().unwrap();
            assert_eq!(observation.state(), ApprovalState::Cancelled);
            assert_eq!(observation.verdict(), ApprovalVerdict::Denied);
        });
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
        assert!(gate.is_waiting(id));
        drop(ticket);

        assert!(!gate.is_resolved(id));
        assert!(
            !gate.is_waiting(id),
            "a released ticket takes its registration with it"
        );
    }

    #[test]
    fn one_gate_serves_many_approvals_without_crossing_their_answers() {
        let gate = ApprovalGate::new();
        let ids = (0..8).map(|_| ApprovalId::new()).collect::<Vec<_>>();
        let tickets = ids.iter().map(|id| gate.ticket(*id)).collect::<Vec<_>>();

        thread::scope(|scope| {
            let waiters = tickets
                .into_iter()
                .map(|ticket| scope.spawn(move || ticket.wait()))
                .collect::<Vec<_>>();

            for id in &ids {
                let mut record = request();
                // Every observation names its own approval, so a waiter woken by
                // a broadcast for somebody else must stay parked.
                record.cancel(at(7)).unwrap();
                let mut observation = ApprovalObservation::of(&record).unwrap();
                observation.approval_id = *id;
                gate.resolve(observation);
            }

            for (waiter, id) in waiters.into_iter().zip(&ids) {
                assert_eq!(waiter.join().unwrap().approval_id(), *id);
            }
        });
    }
}
