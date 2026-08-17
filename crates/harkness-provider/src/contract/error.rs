//! The two error namespaces of the provider boundary.
//!
//! [`ContractError`] is refused *before* anything reaches a provider: a value
//! this crate would not be able to store or send. [`ProviderError`] is what a
//! turn failed with, and its ten [`KINDS`](ProviderError::KINDS) are the stable
//! vocabulary the CLI error namespace ([#136]) and run diagnostics publish
//! unchanged, following `GitError` and `TransportError`.
//!
//! The two do not overlap and their kind tables must not collide, because
//! `harkness contract` publishes their concatenation.
//!
//! [#136]: https://github.com/fullstacktaiye/harkness/issues/136

use std::{fmt, time::Duration};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{assemble::AssistantTurn, text::clamp};

/// Largest detail a [`ProviderError`] retains.
///
/// An error body is written by the endpoint, so it is bounded here for the
/// reason every peer-supplied string in the workspace is bounded: [#126]
/// persists this text beside a run.
///
/// [#126]: https://github.com/fullstacktaiye/harkness/issues/126
pub const MAX_ERROR_DETAIL_BYTES: usize = 2048;

/// Bounded free text attached to a [`ProviderError`].
///
/// The field is private and every construction path clamps, so no route
/// produces an unbounded one — the same reason `SchemaViolation`'s fields are
/// private in `harkness-runtime`. Clamping names the bytes it dropped rather
/// than leaving a sentence that reads as complete.
///
/// It clamps on the way in and on the way out, so a detail that arrived from a
/// record written by another build is bounded exactly as one built here is.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(from = "String", into = "String")]
pub struct ErrorDetail(String);

impl ErrorDetail {
    /// Clamps `text` to [`MAX_ERROR_DETAIL_BYTES`].
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self(clamp(text.into(), MAX_ERROR_DETAIL_BYTES))
    }

    /// Borrows the retained text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ErrorDetail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl From<&str> for ErrorDetail {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for ErrorDetail {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<ErrorDetail> for String {
    fn from(value: ErrorDetail) -> Self {
        value.0
    }
}

/// A value refused before it could be sent to a provider.
///
/// Distinct from [`ProviderError`] because nothing was attempted: no endpoint
/// was reached, no token was spent, and the caller's own value is what has to
/// change.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ContractError {
    /// An identity is outside its grammar.
    #[error("invalid {subject} {value:?}: {reason}")]
    InvalidIdentifier {
        /// Which identity was being built.
        subject: &'static str,
        /// The rejected spelling.
        value: String,
        /// Stable refusal reason.
        reason: &'static str,
    },

    /// A request field cannot be sent as written.
    #[error("invalid request field {field}: {reason}")]
    InvalidRequest {
        /// Field that was refused.
        field: &'static str,
        /// Stable refusal reason.
        reason: &'static str,
    },
}

impl ContractError {
    /// Every stable discriminant this namespace can emit.
    pub const KINDS: &'static [&'static str] = &["invalid_identifier", "invalid_request"];

    /// Stable machine-readable discriminant.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::InvalidIdentifier { .. } => "invalid_identifier",
            Self::InvalidRequest { .. } => "invalid_request",
        }
    }
}

/// Why one model turn did not produce an assistant turn.
///
/// Every variant is a transport or protocol failure. None of them can leave a
/// workspace modified, because a model provider has no filesystem, Git, process,
/// or credential access — that is the whole of what ADR-0002 fixes about this
/// contract.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ProviderError {
    /// The endpoint could not be reached at all.
    #[error("the provider endpoint could not be reached: {detail}")]
    EndpointUnreachable {
        /// What the attempt reported.
        detail: ErrorDetail,
    },

    /// The endpoint refused the credentials it was offered.
    #[error("the provider rejected the credentials: {detail}")]
    AuthenticationFailed {
        /// What the endpoint said, with no credential material echoed back.
        detail: ErrorDetail,
    },

    /// The endpoint asked for the request to be made later.
    #[error("the provider is rate limiting this account: {detail}")]
    RateLimited {
        /// How long the endpoint asked for, when it said.
        retry_after: Option<Duration>,
        /// What the endpoint said.
        detail: ErrorDetail,
    },

    /// The request did not fit the model's context window.
    #[error("the request exceeds the model's context window: {detail}")]
    ContextOverflow {
        /// What the endpoint said.
        detail: ErrorDetail,
    },

    /// The endpoint did not answer inside the deadline the adapter allowed.
    #[error("the provider did not answer within its deadline: {detail}")]
    ProviderTimeout {
        /// Which deadline expired.
        detail: ErrorDetail,
    },

    /// The stream ended before the turn did.
    ///
    /// Carries the turn assembled so far, because a disconnect mid-arguments is
    /// diagnosed from what *had* arrived and nothing else can reconstruct it.
    /// Attached by [`TurnDriver::fail`](crate::assemble::TurnDriver::fail).
    #[error("the provider disconnected before completing the turn: {detail}")]
    Disconnected {
        /// What ended the stream.
        detail: ErrorDetail,
        /// The partial turn, for diagnostics only. Never executed.
        partial: Option<Box<AssistantTurn>>,
    },

    /// The stream was not one this contract can interpret.
    ///
    /// Raised by the assembler for a delta naming an index no call started, for
    /// a second call at one index, and for an accumulation past a cap; raised by
    /// an adapter for anything it cannot turn into a [`ModelEvent`](super::ModelEvent).
    #[error("the provider sent a response this contract cannot interpret: {detail}")]
    MalformedResponse {
        /// What about the stream was refused.
        detail: ErrorDetail,
    },

    /// The request asked for something the endpoint does not support.
    #[error("the provider does not support {capability}")]
    UnsupportedCapability {
        /// The capability that was asked for.
        capability: ErrorDetail,
    },

    /// The stream ended having produced no events at all.
    ///
    /// Told apart from [`Disconnected`](Self::Disconnected) because there is no
    /// partial turn to diagnose: nothing arrived.
    #[error("the provider produced no events")]
    EmptyResponse,

    /// The turn was stopped through its [`Cancellation`](crate::Cancellation).
    #[error("the model turn was cancelled")]
    Cancelled,
}

impl ProviderError {
    /// Every stable discriminant this error namespace can emit.
    pub const KINDS: &'static [&'static str] = &[
        "endpoint_unreachable",
        "authentication_failed",
        "rate_limited",
        "context_overflow",
        "provider_timeout",
        "disconnected",
        "malformed_response",
        "unsupported_capability",
        "empty_response",
        "cancelled",
    ];

    /// Stable machine-readable discriminant.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::EndpointUnreachable { .. } => "endpoint_unreachable",
            Self::AuthenticationFailed { .. } => "authentication_failed",
            Self::RateLimited { .. } => "rate_limited",
            Self::ContextOverflow { .. } => "context_overflow",
            Self::ProviderTimeout { .. } => "provider_timeout",
            Self::Disconnected { .. } => "disconnected",
            Self::MalformedResponse { .. } => "malformed_response",
            Self::UnsupportedCapability { .. } => "unsupported_capability",
            Self::EmptyResponse => "empty_response",
            Self::Cancelled => "cancelled",
        }
    }

    /// When an identical request could be sent again, if the caller chooses to.
    ///
    /// A hint answers *when*, never *whether*: retry policy, budgets, and
    /// attempt counts are the loop's ([#126]). That split is what keeps this
    /// classification free of policy — the same failure means "wait two
    /// seconds" here and "give up, we are out of budget" there.
    ///
    /// [#126]: https://github.com/fullstacktaiye/harkness/issues/126
    #[must_use]
    pub fn retry_hint(&self) -> RetryHint {
        match self {
            // The provider named a time. Nothing else in the namespace can.
            Self::RateLimited {
                retry_after: Some(after),
                ..
            } => RetryHint::After(*after),
            // Nothing about the failure implies waiting: the same request may
            // succeed the moment it is sent again.
            Self::Disconnected { .. } | Self::EmptyResponse | Self::MalformedResponse { .. } => {
                RetryHint::Immediate
            }
            // An identical request cannot succeed. `Cancelled` belongs here
            // because the token that stopped this turn is still cancelled —
            // starting the work again is a new decision, not a retry.
            Self::AuthenticationFailed { .. }
            | Self::ContextOverflow { .. }
            | Self::UnsupportedCapability { .. }
            | Self::Cancelled => RetryHint::Never,
            // The failure says nothing about when.
            Self::EndpointUnreachable { .. }
            | Self::ProviderTimeout { .. }
            | Self::RateLimited {
                retry_after: None, ..
            } => RetryHint::Unknown,
        }
    }

    /// Builds an [`EndpointUnreachable`](Self::EndpointUnreachable).
    #[must_use]
    pub fn endpoint_unreachable(detail: impl Into<String>) -> Self {
        Self::EndpointUnreachable {
            detail: ErrorDetail::new(detail),
        }
    }

    /// Builds an [`AuthenticationFailed`](Self::AuthenticationFailed).
    #[must_use]
    pub fn authentication_failed(detail: impl Into<String>) -> Self {
        Self::AuthenticationFailed {
            detail: ErrorDetail::new(detail),
        }
    }

    /// Builds a [`RateLimited`](Self::RateLimited).
    #[must_use]
    pub fn rate_limited(retry_after: Option<Duration>, detail: impl Into<String>) -> Self {
        Self::RateLimited {
            retry_after,
            detail: ErrorDetail::new(detail),
        }
    }

    /// Builds a [`ContextOverflow`](Self::ContextOverflow).
    #[must_use]
    pub fn context_overflow(detail: impl Into<String>) -> Self {
        Self::ContextOverflow {
            detail: ErrorDetail::new(detail),
        }
    }

    /// Builds a [`ProviderTimeout`](Self::ProviderTimeout).
    #[must_use]
    pub fn provider_timeout(detail: impl Into<String>) -> Self {
        Self::ProviderTimeout {
            detail: ErrorDetail::new(detail),
        }
    }

    /// Builds a [`Disconnected`](Self::Disconnected) with no partial turn.
    ///
    /// The partial is attached by
    /// [`TurnDriver::fail`](crate::assemble::TurnDriver::fail), which is the
    /// only thing that holds one.
    #[must_use]
    pub fn disconnected(detail: impl Into<String>) -> Self {
        Self::Disconnected {
            detail: ErrorDetail::new(detail),
            partial: None,
        }
    }

    /// Builds a [`MalformedResponse`](Self::MalformedResponse).
    #[must_use]
    pub fn malformed_response(detail: impl Into<String>) -> Self {
        Self::MalformedResponse {
            detail: ErrorDetail::new(detail),
        }
    }

    /// Builds an [`UnsupportedCapability`](Self::UnsupportedCapability).
    #[must_use]
    pub fn unsupported_capability(capability: impl Into<String>) -> Self {
        Self::UnsupportedCapability {
            capability: ErrorDetail::new(capability),
        }
    }

    /// The turn assembled before the failure, when the failure carries one.
    #[must_use]
    pub fn partial_turn(&self) -> Option<&AssistantTurn> {
        match self {
            Self::Disconnected { partial, .. } => partial.as_deref(),
            _ => None,
        }
    }
}

/// When an identical request could be sent again.
///
/// Deliberately four answers rather than a boolean: "wait exactly this long"
/// and "nothing is known" are different pieces of advice, and collapsing them
/// makes a caller invent a delay the provider never asked for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryHint {
    /// The provider asked for this delay.
    After(Duration),
    /// Nothing about the failure implies waiting.
    Immediate,
    /// An identical request cannot succeed.
    Never,
    /// The failure says nothing about when.
    Unknown,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{ContractError, ErrorDetail, MAX_ERROR_DETAIL_BYTES, ProviderError, RetryHint};

    #[test]
    fn every_provider_error_kind_is_declared_in_order() {
        let cases = [
            (
                ProviderError::endpoint_unreachable("fixture"),
                "endpoint_unreachable",
            ),
            (
                ProviderError::authentication_failed("fixture"),
                "authentication_failed",
            ),
            (ProviderError::rate_limited(None, "fixture"), "rate_limited"),
            (
                ProviderError::context_overflow("fixture"),
                "context_overflow",
            ),
            (
                ProviderError::provider_timeout("fixture"),
                "provider_timeout",
            ),
            (ProviderError::disconnected("fixture"), "disconnected"),
            (
                ProviderError::malformed_response("fixture"),
                "malformed_response",
            ),
            (
                ProviderError::unsupported_capability("tool calls"),
                "unsupported_capability",
            ),
            (ProviderError::EmptyResponse, "empty_response"),
            (ProviderError::Cancelled, "cancelled"),
        ];

        let kinds = cases.iter().map(|(_, kind)| *kind).collect::<Vec<_>>();
        assert_eq!(kinds, ProviderError::KINDS);
        assert_eq!(
            ProviderError::KINDS.len(),
            10,
            "the ten kinds are the published namespace"
        );
        for (error, expected) in cases {
            assert_eq!(error.kind(), expected, "unexpected kind for {error:?}");
        }
    }

    #[test]
    fn every_contract_error_kind_is_declared_in_order() {
        let cases = [
            (
                ContractError::InvalidIdentifier {
                    subject: "provider id",
                    value: "Nope".to_owned(),
                    reason: "fixture",
                },
                "invalid_identifier",
            ),
            (
                ContractError::InvalidRequest {
                    field: "temperature",
                    reason: "fixture",
                },
                "invalid_request",
            ),
        ];

        let kinds = cases.iter().map(|(_, kind)| *kind).collect::<Vec<_>>();
        assert_eq!(kinds, ContractError::KINDS);
        for (error, expected) in cases {
            assert_eq!(error.kind(), expected);
        }
    }

    /// `harkness contract` publishes the concatenation of every stable kind
    /// namespace, so two of them meaning different things under one spelling
    /// would make a caller's mapping ambiguous.
    #[test]
    fn the_two_namespaces_do_not_collide() {
        for kind in ContractError::KINDS {
            assert!(
                !ProviderError::KINDS.contains(kind),
                "{kind} appears in both namespaces"
            );
        }
    }

    #[test]
    fn a_retry_hint_says_when_and_never_whether() {
        assert_eq!(
            ProviderError::rate_limited(Some(Duration::from_secs(2)), "slow down").retry_hint(),
            RetryHint::After(Duration::from_secs(2))
        );
        assert_eq!(
            ProviderError::rate_limited(None, "slow down").retry_hint(),
            RetryHint::Unknown,
            "a rate limit with no window says nothing about when"
        );
        assert_eq!(
            ProviderError::disconnected("mid stream").retry_hint(),
            RetryHint::Immediate
        );
        assert_eq!(
            ProviderError::EmptyResponse.retry_hint(),
            RetryHint::Immediate
        );
        assert_eq!(
            ProviderError::context_overflow("too long").retry_hint(),
            RetryHint::Never,
            "an identical request cannot fit a window it already overflowed"
        );
        assert_eq!(ProviderError::Cancelled.retry_hint(), RetryHint::Never);
        assert_eq!(
            ProviderError::endpoint_unreachable("dns").retry_hint(),
            RetryHint::Unknown
        );
    }

    #[test]
    fn a_detail_is_clamped_however_it_was_built() {
        let long = "x".repeat(MAX_ERROR_DETAIL_BYTES * 2);
        let error = ProviderError::malformed_response(long.clone());
        assert!(
            error.to_string().len() < MAX_ERROR_DETAIL_BYTES + 128,
            "an unbounded provider body must not reach a record"
        );
        assert!(error.to_string().contains("bytes)"));
        assert_eq!(
            ErrorDetail::from(long.as_str()).as_str(),
            ErrorDetail::new(long).as_str(),
            "every construction path clamps identically"
        );
    }

    #[test]
    fn only_a_disconnect_carries_a_partial_turn() {
        assert!(
            ProviderError::disconnected("mid stream")
                .partial_turn()
                .is_none()
        );
        assert!(ProviderError::EmptyResponse.partial_turn().is_none());
    }
}
