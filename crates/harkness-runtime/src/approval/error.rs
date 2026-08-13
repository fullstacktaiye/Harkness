use thiserror::Error;
use time::OffsetDateTime;

use crate::domain::ApprovalId;

use super::{ApprovalScope, ApprovalState};

/// Failures raised while creating, matching, or resolving an approval.
///
/// Every variant carries a stable [`kind`](ApprovalError::kind) discriminant, so
/// a front end can branch on a refusal without matching Rust types, exactly as
/// it already does for [`GitError`](harkness_core::GitError) and
/// [`StoreError`](crate::store::StoreError).
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ApprovalError {
    /// A lifecycle edge absent from [`APPROVAL_TRANSITIONS`](super::APPROVAL_TRANSITIONS).
    #[error("approval {id} is {from} and cannot become {to}")]
    InvalidTransition {
        /// Approval whose state change was refused.
        id: ApprovalId,
        /// State the record held.
        from: ApprovalState,
        /// State the caller requested.
        to: ApprovalState,
    },

    /// A second decision arrived for a request that is no longer pending.
    ///
    /// Both front ends can answer the same question, and a run can be cancelled
    /// while a human is still looking at it, so losing this race is ordinary
    /// rather than exceptional. It is reported separately from a generic invalid
    /// transition so a surface can say "somebody already answered this" instead
    /// of describing a state machine.
    #[error("approval {id} was already resolved as {state}")]
    AlreadyResolved {
        /// Approval that was already decided.
        id: ApprovalId,
        /// State it was resolved into.
        state: ApprovalState,
    },

    /// A decision named an approval other than the one being resolved.
    #[error("a decision for approval {decision} cannot resolve approval {request}")]
    DecisionIdentityMismatch {
        /// Approval the record identifies.
        request: ApprovalId,
        /// Approval the decision claims to answer.
        decision: ApprovalId,
    },

    /// A grant asked for more than the request it answers permits.
    ///
    /// A human may always narrow a decision to the single call in front of them.
    /// Anything broader than the scope the request was stored with — including
    /// anything the risk ceiling already reduced — is a different question and
    /// needs its own request.
    #[error("approval {id} may be granted as {effective} or exact_call, not as {requested}")]
    ScopeExceedsRequest {
        /// Approval whose ceiling was exceeded.
        id: ApprovalId,
        /// Scope the stored request was reduced to.
        effective: ApprovalScope,
        /// Scope the decision asked for.
        requested: ApprovalScope,
    },

    /// An answer arrived after the request's deadline for answering it.
    ///
    /// A lapsed request is a question that is over. Granting one would make the
    /// deadline advisory, and it is the only thing `expires_at` is for.
    #[error("approval {id} lapsed at {expires_at} and can no longer be decided")]
    Expired {
        /// Approval whose deadline had passed.
        id: ApprovalId,
        /// Deadline the request carried.
        expires_at: OffsetDateTime,
    },

    /// A capability-scoped request named no capability to scope to.
    #[error("a {scope} request must name at least one capability to grant")]
    ScopeRequiresCapability {
        /// Scope that cannot be expressed without a capability.
        scope: ApprovalScope,
    },

    /// A tool input could not be reduced to its canonical form.
    ///
    /// The pointer is an RFC 6901 JSON Pointer, the same way the tool contract's
    /// schema validators locate a finding, so a caller can name the field that
    /// has to change.
    #[error("the tool input at {pointer} cannot be canonicalized: {reason}")]
    UncanonicalizableInput {
        /// RFC 6901 pointer to the offending value.
        pointer: String,
        /// Stable human-readable explanation.
        reason: &'static str,
    },

    /// A stored row describes an approval that cannot be true.
    ///
    /// The store rebuilds every row through the same rules a fresh record
    /// passes, so a database edited outside Harkness fails to load rather than
    /// entering the process as an impossible approval. The edit this matters
    /// most for is widening `effective_scope` past the risk ceiling: nothing
    /// downstream re-derives that column, so a row claiming a breadth the
    /// ceiling would never have allowed would simply be honored.
    #[error("stored approval {id} is not a valid record: {reason}")]
    InconsistentRecord {
        /// Approval whose row was refused.
        id: ApprovalId,
        /// Stable human-readable explanation.
        reason: &'static str,
    },

    /// A stored or transported input hash is not 64 lowercase hex characters.
    #[error("{value:?} is not a canonical input hash: {reason}")]
    MalformedInputHash {
        /// Spelling that was refused.
        value: String,
        /// Stable human-readable explanation.
        reason: &'static str,
    },
}

impl ApprovalError {
    /// Every stable discriminant this error namespace can emit.
    pub const KINDS: &'static [&'static str] = &[
        "approval_invalid_transition",
        "approval_already_resolved",
        "approval_decision_identity_mismatch",
        "approval_scope_exceeds_request",
        "approval_expired",
        "approval_scope_requires_capability",
        "approval_uncanonicalizable_input",
        "approval_inconsistent_record",
        "approval_malformed_input_hash",
    ];

    /// Stable machine-readable discriminant for caller-facing error handling.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::InvalidTransition { .. } => Self::KINDS[0],
            Self::AlreadyResolved { .. } => Self::KINDS[1],
            Self::DecisionIdentityMismatch { .. } => Self::KINDS[2],
            Self::ScopeExceedsRequest { .. } => Self::KINDS[3],
            Self::Expired { .. } => Self::KINDS[4],
            Self::ScopeRequiresCapability { .. } => Self::KINDS[5],
            Self::UncanonicalizableInput { .. } => Self::KINDS[6],
            Self::InconsistentRecord { .. } => Self::KINDS[7],
            Self::MalformedInputHash { .. } => Self::KINDS[8],
        }
    }
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;

    use crate::domain::ApprovalId;

    use super::super::{ApprovalScope, ApprovalState};
    use super::ApprovalError;

    #[test]
    fn approval_error_kinds_round_trip_through_the_kinds_table() {
        let id = ApprovalId::new();
        let cases = [
            ApprovalError::InvalidTransition {
                id,
                from: ApprovalState::Granted,
                to: ApprovalState::Denied,
            },
            ApprovalError::AlreadyResolved {
                id,
                state: ApprovalState::Denied,
            },
            ApprovalError::DecisionIdentityMismatch {
                request: id,
                decision: ApprovalId::new(),
            },
            ApprovalError::ScopeExceedsRequest {
                id,
                effective: ApprovalScope::ExactCall,
                requested: ApprovalScope::ToolForRun,
            },
            ApprovalError::Expired {
                id,
                expires_at: OffsetDateTime::UNIX_EPOCH,
            },
            ApprovalError::ScopeRequiresCapability {
                scope: ApprovalScope::CapabilityForRun,
            },
            ApprovalError::UncanonicalizableInput {
                pointer: "/limit".to_owned(),
                reason: "it is not a finite number",
            },
            ApprovalError::InconsistentRecord {
                id,
                reason: "its effective scope is broader than its risk allows",
            },
            ApprovalError::MalformedInputHash {
                value: "nope".to_owned(),
                reason: "it is not 64 characters long",
            },
        ];

        let kinds = cases.iter().map(ApprovalError::kind).collect::<Vec<_>>();
        assert_eq!(kinds, ApprovalError::KINDS);
    }

    #[test]
    fn the_scope_refusals_name_the_scopes_involved() {
        let error = ApprovalError::ScopeExceedsRequest {
            id: ApprovalId::new(),
            effective: ApprovalScope::ExactCall,
            requested: ApprovalScope::ToolForRun,
        };
        assert!(error.to_string().contains("exact_call"));
        assert!(error.to_string().contains("tool_for_run"));
    }
}
