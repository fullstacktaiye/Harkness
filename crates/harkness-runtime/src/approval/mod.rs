//! Durable approval requests, their decisions, and exact-request binding.
//!
//! [`policy`](crate::policy) can answer `Ask`. This module is what happens next:
//! the question becomes a record before anybody is shown it, an answer becomes a
//! grant bound to the exact call it was given for, and the call that was waiting
//! learns what happened through a structured observation rather than a hang.
//!
//! # Persist, then present
//!
//! An [`ApprovalRequest`] is written and committed *before* any surface is
//! notified, because a pause that only exists in one process's memory is not a
//! pause a user can be asked to survive. The row and the `approval_requested`
//! event share one transaction, so a timeline that says a question was asked and
//! a store with no question in it are not states this module can be found in.
//! Restarting lists the pending requests with every binding field intact.
//!
//! # Nothing is held while a human thinks
//!
//! The waiting call holds no database transaction. It holds an
//! [`ApprovalTicket`] on the shared [`ApprovalGate`], and the decision is a
//! short write that commits and then wakes it. The store's single writer stays
//! free for every other run for as long as the user takes to answer.
//!
//! # An answer authorizes exactly what was asked
//!
//! [`grant_applies`] binds a grant to the run, the workspace identity, the tool
//! id, the tool version, and — at [`ExactCall`](ApprovalScope::ExactCall) scope
//! — the canonical hash of the validated input. Any single mismatch defeats the
//! match; there is no partial application and no "close enough". That is what
//! stops an agent obtaining approval for an innocuous patch and applying a
//! different one.
//!
//! Scope ceilings are applied when the request is *created*, not when a grant is
//! matched, so a [`RemoteWrite`](crate::tool::RiskLevel::RemoteWrite) or
//! [`Destructive`](crate::tool::RiskLevel::Destructive) request that asked for a
//! run-wide scope is *stored* as an exact call and shows both spellings. A
//! record that claimed a breadth the matcher would never honor would be a lie in
//! the audit trail rather than a defence in depth.
//!
//! # Absence of an answer is never consent
//!
//! Closing a window, dismissing a dialog, and losing a surface all leave the
//! request `pending`. Only an explicit decision, an expiry, or a run
//! cancellation resolves one, and the last two record
//! [`Expired`](ApprovalState::Expired) or
//! [`Cancelled`](ApprovalState::Cancelled) rather than synthesizing a refusal
//! nobody made — the waiter still observes a denial, and the record still says
//! that no human answered.
//!
//! # Two things called an approval
//!
//! [`domain::Approval`](crate::domain::Approval) is the audit entry appended to
//! a run, step, or tool-call record: it says *that* a record was approved, and
//! it travels with the record. [`ApprovalRequest`] is the question itself, with
//! an identity, a lifecycle, and the binding fields a grant is matched on. The
//! first is a line in a record's history; the second is the thing this module is
//! about.

mod canonical;
mod error;
mod gate;
mod matcher;
mod record;

/// Re-exported beside the records it names: an approval's identity is generated
/// by the same `define_id!` contract every other runtime identifier is, and a
/// caller working with approvals should not have to reach into [`crate::domain`]
/// for it.
pub use crate::domain::ApprovalId;
pub use canonical::{CANONICAL_INPUT_DOMAIN, InputHash, canonical_input, canonical_input_hash};
pub use error::ApprovalError;
pub use gate::{ApprovalGate, ApprovalObservation, ApprovalTicket};
pub use matcher::{
    ApprovalGrant, CandidateCall, GrantMatch, GrantMatches, IntegrationIdentityDrift,
    IntegrationIdentityField, grant_applies, matching_grants, matching_grants_detailed,
};
pub use record::{
    APPROVAL_TRANSITIONS, ApprovalDecision, ApprovalRequest, ApprovalScope, ApprovalState,
    ApprovalVerdict, DecidedVia, PendingApproval, WorkspaceBinding,
};
