use std::fmt;

use thiserror::Error;

use super::{RunState, ToolCallState};

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
    S: fmt::Debug,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "state {:?} cannot become {:?}",
            self.from, self.to
        )
    }
}

impl<S> std::error::Error for InvalidTransition<S> where S: fmt::Debug {}

/// Invalid state changes or persisted record combinations in the run domain.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum RunDomainError {
    /// A run or step requested an edge absent from [`super::RUN_TRANSITIONS`].
    #[error(transparent)]
    InvalidRunTransition(#[from] InvalidTransition<RunState>),

    /// A tool call requested an edge absent from [`super::TOOL_CALL_TRANSITIONS`].
    #[error(transparent)]
    InvalidToolCallTransition(#[from] InvalidTransition<ToolCallState>),

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
        "invalid_run_transition",
        "invalid_tool_call_transition",
        "invalid_timestamp",
        "invalid_lifecycle",
    ];

    /// Stable machine-readable discriminant for caller-facing error handling.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::InvalidRunTransition(_) => "invalid_run_transition",
            Self::InvalidToolCallTransition(_) => "invalid_tool_call_transition",
            Self::InvalidTimestamp { .. } => "invalid_timestamp",
            Self::InvalidLifecycle { .. } => "invalid_lifecycle",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{InvalidTransition, RunDomainError};
    use crate::domain::{RunState, ToolCallState};

    #[test]
    fn domain_error_kinds_round_trip_through_the_kinds_table() {
        let cases = [
            (
                RunDomainError::InvalidRunTransition(InvalidTransition {
                    from: RunState::Queued,
                    to: RunState::Succeeded,
                }),
                "invalid_run_transition",
            ),
            (
                RunDomainError::InvalidToolCallTransition(InvalidTransition {
                    from: ToolCallState::Denied,
                    to: ToolCallState::Running,
                }),
                "invalid_tool_call_transition",
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
