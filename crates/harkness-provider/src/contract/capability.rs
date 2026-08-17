//! What an endpoint says it can do, and what to believe when it says nothing.

use serde::{Deserialize, Serialize};

/// What one model at one provider supports.
///
/// Every field is *unknown by default*, and unknown is not "no" — it is "this
/// endpoint did not say". [`ProviderCapabilities::unknown`] is the value a
/// provider returns for a model it has never been told about, which is the
/// ordinary case for an OpenAI-compatible endpoint serving a local model.
///
/// Two rules follow, and both are load-bearing:
///
/// - A size that is `None` means the caller supplies its own conservative
///   floor, through [`context_window_or`](Self::context_window_or) and
///   [`max_output_tokens_or`](Self::max_output_tokens_or). Nothing in this
///   crate unwraps a capability field, and nothing downstream should either:
///   guessing a window is how a run discovers the real one by overflowing it.
/// - A `supports_*` flag is `false` when unknown, so an unannounced capability
///   is one a caller declines to rely on rather than one it assumes.
///
/// There is deliberately no field meaning "may write files". A model provider
/// cannot execute anything — ADR-0002 — so a capability describing what it may
/// do to a workspace would be describing something that does not exist.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct ProviderCapabilities {
    /// Total tokens one request may occupy, when the endpoint says.
    pub context_window: Option<u32>,
    /// Tokens one turn may produce, when the endpoint says.
    pub max_output_tokens: Option<u32>,
    /// Whether the endpoint streams a turn rather than returning it whole.
    pub supports_streaming: bool,
    /// Whether the endpoint accepts tool definitions and emits tool calls.
    pub supports_tool_calls: bool,
    /// Whether the endpoint can be asked for a schema-constrained answer.
    pub supports_structured_output: bool,
    /// Whether the endpoint can count tokens without running a turn.
    pub supports_token_counting: bool,
}

impl ProviderCapabilities {
    /// The value that claims nothing: no sizes, no supported features.
    #[must_use]
    pub const fn unknown() -> Self {
        Self {
            context_window: None,
            max_output_tokens: None,
            supports_streaming: false,
            supports_tool_calls: false,
            supports_structured_output: false,
            supports_token_counting: false,
        }
    }

    /// Declares the context window.
    #[must_use]
    pub const fn with_context_window(mut self, tokens: u32) -> Self {
        self.context_window = Some(tokens);
        self
    }

    /// Declares the per-turn output bound.
    #[must_use]
    pub const fn with_max_output_tokens(mut self, tokens: u32) -> Self {
        self.max_output_tokens = Some(tokens);
        self
    }

    /// Declares whether the endpoint streams.
    #[must_use]
    pub const fn with_streaming(mut self, supported: bool) -> Self {
        self.supports_streaming = supported;
        self
    }

    /// Declares whether the endpoint emits tool calls.
    #[must_use]
    pub const fn with_tool_calls(mut self, supported: bool) -> Self {
        self.supports_tool_calls = supported;
        self
    }

    /// Declares whether the endpoint can be constrained to a schema.
    #[must_use]
    pub const fn with_structured_output(mut self, supported: bool) -> Self {
        self.supports_structured_output = supported;
        self
    }

    /// Declares whether the endpoint counts tokens.
    #[must_use]
    pub const fn with_token_counting(mut self, supported: bool) -> Self {
        self.supports_token_counting = supported;
        self
    }

    /// The declared context window, or the caller's conservative floor.
    ///
    /// The floor belongs to the caller because only the caller knows what it is
    /// budgeting: [#122] estimates against it, and a shared default here would
    /// be a guess wearing the endpoint's authority.
    ///
    /// [#122]: https://github.com/fullstacktaiye/harkness/issues/122
    #[must_use]
    pub const fn context_window_or(self, conservative: u32) -> u32 {
        match self.context_window {
            Some(tokens) => tokens,
            None => conservative,
        }
    }

    /// The declared per-turn output bound, or the caller's conservative floor.
    #[must_use]
    pub const fn max_output_tokens_or(self, conservative: u32) -> u32 {
        match self.max_output_tokens {
            Some(tokens) => tokens,
            None => conservative,
        }
    }

    /// Whether this value claims nothing at all.
    #[must_use]
    pub const fn is_unknown(self) -> bool {
        self.context_window.is_none()
            && self.max_output_tokens.is_none()
            && !self.supports_streaming
            && !self.supports_tool_calls
            && !self.supports_structured_output
            && !self.supports_token_counting
    }
}

#[cfg(test)]
mod tests {
    use super::ProviderCapabilities;

    #[test]
    fn the_default_claims_nothing() {
        let capabilities = ProviderCapabilities::default();
        assert_eq!(capabilities, ProviderCapabilities::unknown());
        assert!(capabilities.is_unknown());
        assert_eq!(capabilities.context_window, None);
        assert!(!capabilities.supports_tool_calls);
    }

    /// The point of the accessor: an unknown window is answered by the caller's
    /// floor, so no code path needs an `unwrap` and none can invent a size.
    #[test]
    fn an_unknown_size_is_answered_by_the_callers_floor() {
        assert_eq!(
            ProviderCapabilities::unknown().context_window_or(8_192),
            8_192
        );
        assert_eq!(
            ProviderCapabilities::unknown()
                .with_context_window(200_000)
                .context_window_or(8_192),
            200_000
        );
        assert_eq!(
            ProviderCapabilities::unknown().max_output_tokens_or(1_024),
            1_024
        );
    }

    #[test]
    fn capabilities_round_trip_and_refuse_a_field_this_build_does_not_define() {
        let declared = ProviderCapabilities::unknown()
            .with_context_window(128_000)
            .with_max_output_tokens(4_096)
            .with_streaming(true)
            .with_tool_calls(true);
        let json = serde_json::to_string(&declared).unwrap();
        assert_eq!(
            serde_json::from_str::<ProviderCapabilities>(&json).unwrap(),
            declared
        );

        let absent: ProviderCapabilities = serde_json::from_str("{}").unwrap();
        assert!(
            absent.is_unknown(),
            "an omitted field is unknown, not false"
        );

        assert!(
            serde_json::from_str::<ProviderCapabilities>("{\"supports_vision\":true}").is_err(),
            "a capability this build does not define must not decode as silence"
        );
    }
}
