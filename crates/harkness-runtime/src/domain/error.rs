use std::fmt;

use thiserror::Error;

use super::{ExecutionState, ToolCallState};

/// A requested lifecycle edge that is absent from its transition table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidTransition<S> {
    /// State held before the rejected request.
    pub from: S,
    /// State requested by the caller.
    pub to: S,
}

impl<S> fmt::Display for InvalidTransition<S>
where
    S: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "state {} cannot become {}", self.from, self.to)
    }
}

impl<S> std::error::Error for InvalidTransition<S> where S: fmt::Debug + fmt::Display {}

/// Invalid state changes or persisted record combinations in the runtime domain.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum RunDomainError {
    /// A run or step requested an edge absent from [`super::EXECUTION_TRANSITIONS`].
    #[error(transparent)]
    InvalidExecutionTransition(#[from] InvalidTransition<ExecutionState>),

    /// A tool call requested an edge absent from [`super::TOOL_CALL_TRANSITIONS`].
    #[error(transparent)]
    InvalidToolCallTransition(#[from] InvalidTransition<ToolCallState>),

    /// A lifecycle revision has no representable successor.
    #[error("{record} revision is exhausted")]
    RevisionExhausted {
        /// Kind of record being transitioned.
        record: &'static str,
    },

    /// A persisted record predates the oldest schema this build supports.
    #[error(
        "{record} schema version {found} is older than the minimum supported version {minimum}"
    )]
    SchemaVersionTooOld {
        /// Kind of record being decoded.
        record: &'static str,
        /// Version found in the record.
        found: u32,
        /// Oldest version understood by this build.
        minimum: u32,
    },

    /// A persisted record requires a newer build of Harkness.
    #[error(
        "{record} schema version {found} is newer than the maximum supported version {maximum}; upgrade Harkness to read it"
    )]
    SchemaVersionTooNew {
        /// Kind of record being decoded.
        record: &'static str,
        /// Version found in the record.
        found: u32,
        /// Newest version understood by this build.
        maximum: u32,
    },

    /// A wire timestamp is not a valid UTC lifecycle timestamp.
    #[error("{record}.{field} is invalid: {reason}")]
    InvalidTimestamp {
        /// Kind of record being decoded.
        record: &'static str,
        /// Timestamp field that violated the invariant.
        field: &'static str,
        /// Stable human-readable explanation.
        reason: &'static str,
    },

    /// A wire record combines otherwise valid fields into an impossible state.
    #[error("{record} has an invalid lifecycle: {reason}")]
    InvalidLifecycle {
        /// Kind of record being decoded.
        record: &'static str,
        /// Stable human-readable explanation.
        reason: &'static str,
    },
}

impl RunDomainError {
    /// Every stable discriminant this error namespace can emit.
    pub const KINDS: &'static [&'static str] = &[
        "invalid_execution_transition",
        "invalid_tool_call_transition",
        "revision_exhausted",
        "schema_version_too_old",
        "schema_version_too_new",
        "invalid_timestamp",
        "invalid_lifecycle",
    ];

    /// Stable machine-readable discriminant for caller-facing error handling.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::InvalidExecutionTransition(_) => "invalid_execution_transition",
            Self::InvalidToolCallTransition(_) => "invalid_tool_call_transition",
            Self::RevisionExhausted { .. } => "revision_exhausted",
            Self::SchemaVersionTooOld { .. } => "schema_version_too_old",
            Self::SchemaVersionTooNew { .. } => "schema_version_too_new",
            Self::InvalidTimestamp { .. } => "invalid_timestamp",
            Self::InvalidLifecycle { .. } => "invalid_lifecycle",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{InvalidTransition, RunDomainError};
    use crate::domain::{ExecutionState, ToolCallState};

    #[test]
    fn transition_errors_use_stable_state_spellings() {
        let error = InvalidTransition {
            from: ExecutionState::WaitingForApproval,
            to: ExecutionState::Succeeded,
        };
        assert_eq!(
            error.to_string(),
            "state waiting_for_approval cannot become succeeded"
        );
    }

    #[test]
    fn domain_error_kinds_round_trip_through_the_kinds_table() {
        let cases = [
            (
                RunDomainError::InvalidExecutionTransition(InvalidTransition {
                    from: ExecutionState::Queued,
                    to: ExecutionState::Succeeded,
                }),
                "invalid_execution_transition",
            ),
            (
                RunDomainError::InvalidToolCallTransition(InvalidTransition {
                    from: ToolCallState::Denied,
                    to: ToolCallState::Running,
                }),
                "invalid_tool_call_transition",
            ),
            (
                RunDomainError::RevisionExhausted { record: "run" },
                "revision_exhausted",
            ),
            (
                RunDomainError::SchemaVersionTooOld {
                    record: "task",
                    found: 0,
                    minimum: 1,
                },
                "schema_version_too_old",
            ),
            (
                RunDomainError::SchemaVersionTooNew {
                    record: "task",
                    found: 2,
                    maximum: 1,
                },
                "schema_version_too_new",
            ),
            (
                RunDomainError::InvalidTimestamp {
                    record: "run",
                    field: "created_at",
                    reason: "must use the UTC offset",
                },
                "invalid_timestamp",
            ),
            (
                RunDomainError::InvalidLifecycle {
                    record: "run",
                    reason: "a terminal state requires finished_at",
                },
                "invalid_lifecycle",
            ),
        ];

        let kinds = cases.iter().map(|(_, kind)| *kind).collect::<Vec<_>>();
        assert_eq!(kinds, RunDomainError::KINDS);
        for (error, expected) in cases {
            assert_eq!(error.kind(), expected, "unexpected kind for {error:?}");
        }
    }
}
