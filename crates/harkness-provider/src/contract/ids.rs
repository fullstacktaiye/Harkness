//! The three identities the contract carries.
//!
//! Each is a validated newtype rather than a `String`, for the reason
//! `ProjectId` and `ToolId` are: these are persisted by [#126] beside run
//! records and named in configuration by [#124], so a spelling that cannot be
//! stored has to fail where it was written rather than where it is read.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::error::ContractError;

/// Largest accepted [`ProviderId`] spelling.
pub const MAX_PROVIDER_ID_BYTES: usize = 64;

/// Largest accepted [`ModelId`] spelling.
pub const MAX_MODEL_ID_BYTES: usize = 256;

/// Largest accepted [`ProviderToolCallId`] spelling.
///
/// A provider chooses this value, so it is bounded for the same reason every
/// other peer-supplied string in the workspace is: it ends up in a record.
pub const MAX_TOOL_CALL_ID_BYTES: usize = 512;

/// Prefix every synthesized tool-call identity carries.
///
/// Synthesis is recorded, never inferred from the spelling: a provider is free
/// to issue an id that happens to start this way, and
/// [`IdProvenance`](crate::assemble::IdProvenance) still says where the id came
/// from. The prefix exists so a human reading a transcript can tell at a glance
/// which ids Harkness invented.
pub const SYNTHESIZED_TOOL_CALL_ID_PREFIX: &str = "harkness-synth-";

/// Stable identity of one provider adapter, such as `openai_compat`.
///
/// Grammar: lowercase ASCII letters, digits, and interior underscores, starting
/// with a letter. Narrow because it is written by hand in configuration
/// ([#124]) and matched against records written by older builds.
///
/// [#124]: https://github.com/fullstacktaiye/harkness/issues/124
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(into = "String", try_from = "String")]
pub struct ProviderId(String);

impl ProviderId {
    /// Parses and validates a provider identity.
    ///
    /// # Errors
    ///
    /// Returns [`ContractError::InvalidIdentifier`] for an empty, overlong, or
    /// non-snake-case spelling.
    pub fn new(value: impl Into<String>) -> Result<Self, ContractError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= MAX_PROVIDER_ID_BYTES
            && value.bytes().enumerate().all(|(index, byte)| match byte {
                b'a'..=b'z' => true,
                b'0'..=b'9' => index > 0,
                b'_' => index > 0 && index + 1 < value.len(),
                _ => false,
            })
            && !value.contains("__");
        if !valid {
            return Err(ContractError::InvalidIdentifier {
                subject: "provider id",
                value,
                reason: "expected at most 64 lowercase snake-case characters starting with a letter",
            });
        }
        Ok(Self(value))
    }

    /// Borrows the stable spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Provider-scoped model name, such as `gpt-4o-mini` or `qwen2.5:7b`.
///
/// Deliberately permissive: the vocabulary belongs to the endpoint, and a
/// grammar tight enough to be meaningful for one provider refuses another's
/// legitimate names. What is enforced is what a record requires — a bound, and
/// no control characters or surrounding whitespace, because two spellings that
/// differ only by a trailing space must not compare as one model.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(into = "String", try_from = "String")]
pub struct ModelId(String);

impl ModelId {
    /// Parses and validates a model name.
    ///
    /// # Errors
    ///
    /// Returns [`ContractError::InvalidIdentifier`] for an empty or overlong
    /// name, one carrying a control character, or one padded with whitespace.
    pub fn new(value: impl Into<String>) -> Result<Self, ContractError> {
        let value = value.into();
        if let Some(reason) = refuse_opaque_identifier(&value, MAX_MODEL_ID_BYTES) {
            return Err(ContractError::InvalidIdentifier {
                subject: "model id",
                value,
                reason,
            });
        }
        Ok(Self(value))
    }

    /// Borrows the model name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Identity of one tool call within one assistant turn.
///
/// Either issued by the provider or synthesized by the assembler when the
/// provider issued none. Which of the two it is is recorded on the assembled
/// call, never read back out of the spelling.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(into = "String", try_from = "String")]
pub struct ProviderToolCallId(String);

impl ProviderToolCallId {
    /// Parses and validates a provider-issued tool-call identity.
    ///
    /// # Errors
    ///
    /// Returns [`ContractError::InvalidIdentifier`] for an empty or overlong
    /// id, one carrying a control character, or one padded with whitespace. An
    /// adapter turns that refusal into
    /// [`ProviderError::MalformedResponse`](super::ProviderError::MalformedResponse),
    /// because an id it cannot store is a response it cannot use.
    pub fn new(value: impl Into<String>) -> Result<Self, ContractError> {
        let value = value.into();
        if let Some(reason) = refuse_opaque_identifier(&value, MAX_TOOL_CALL_ID_BYTES) {
            return Err(ContractError::InvalidIdentifier {
                subject: "tool call id",
                value,
                reason,
            });
        }
        Ok(Self(value))
    }

    /// Builds the deterministic identity for the `counter`-th call of a turn
    /// that the provider left unnamed.
    ///
    /// Turn-scoped and one-based, so replaying one event sequence twice
    /// synthesizes the same ids. Nothing outside a turn may assume they are
    /// unique — two turns of one run both start at one.
    #[must_use]
    pub fn synthesized(counter: u32) -> Self {
        Self(format!("{SYNTHESIZED_TOOL_CALL_ID_PREFIX}{counter}"))
    }

    /// Borrows the identity.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Shared refusal for the two identities whose vocabulary belongs to a provider.
fn refuse_opaque_identifier(value: &str, limit: usize) -> Option<&'static str> {
    if value.is_empty() {
        return Some("it cannot be empty");
    }
    if value.len() > limit {
        return Some("it is longer than the bound its column can hold");
    }
    if value.chars().any(char::is_control) {
        return Some("it cannot contain a control character");
    }
    if value.trim() != value {
        return Some("it cannot begin or end with whitespace");
    }
    None
}

macro_rules! identity_conversions {
    ($name:ident) => {
        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl TryFrom<String> for $name {
            type Error = ContractError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

identity_conversions!(ProviderId);
identity_conversions!(ModelId);
identity_conversions!(ProviderToolCallId);

#[cfg(test)]
mod tests {
    use super::{
        MAX_MODEL_ID_BYTES, MAX_TOOL_CALL_ID_BYTES, ModelId, ProviderId, ProviderToolCallId,
        SYNTHESIZED_TOOL_CALL_ID_PREFIX,
    };

    #[test]
    fn a_provider_id_accepts_the_snake_case_grammar_and_nothing_else() {
        for accepted in ["openai_compat", "scripted", "a", "vendor2_beta"] {
            assert!(ProviderId::new(accepted).is_ok(), "rejected {accepted}");
        }
        for refused in [
            "",
            "OpenAI",
            "openai-compat",
            "_leading",
            "trailing_",
            "double__underscore",
            "1numeric",
            "openai compat",
        ] {
            assert!(ProviderId::new(refused).is_err(), "accepted {refused}");
        }
        assert!(ProviderId::new("a".repeat(65)).is_err());
    }

    #[test]
    fn a_model_id_keeps_the_vocabulary_the_endpoint_chose() {
        for accepted in [
            "gpt-4o-mini",
            "claude-opus-5",
            "meta-llama/Llama-3-70B-Instruct",
            "qwen2.5:7b",
            "local model",
        ] {
            assert!(ModelId::new(accepted).is_ok(), "rejected {accepted}");
        }
        for refused in ["", " padded ", "with\nnewline", "with\ttab"] {
            assert!(ModelId::new(refused).is_err(), "accepted {refused:?}");
        }
        assert!(ModelId::new("m".repeat(MAX_MODEL_ID_BYTES)).is_ok());
        assert!(ModelId::new("m".repeat(MAX_MODEL_ID_BYTES + 1)).is_err());
    }

    #[test]
    fn a_tool_call_id_is_bounded_by_what_a_record_can_hold() {
        assert!(ProviderToolCallId::new("call_abc123").is_ok());
        assert!(ProviderToolCallId::new("c".repeat(MAX_TOOL_CALL_ID_BYTES)).is_ok());
        assert!(ProviderToolCallId::new("c".repeat(MAX_TOOL_CALL_ID_BYTES + 1)).is_err());
        assert!(ProviderToolCallId::new("").is_err());
    }

    /// The prefix is a courtesy to a human reading a transcript. Provenance is
    /// recorded on the assembled call, so a provider that issues an id shaped
    /// like a synthesized one changes nothing about what the record says.
    #[test]
    fn a_synthesized_id_is_deterministic_and_marked() {
        assert_eq!(
            ProviderToolCallId::synthesized(1).as_str(),
            "harkness-synth-1"
        );
        assert_eq!(
            ProviderToolCallId::synthesized(2),
            ProviderToolCallId::synthesized(2)
        );
        let forged = ProviderToolCallId::new(format!("{SYNTHESIZED_TOOL_CALL_ID_PREFIX}1"))
            .expect("the prefix is an ordinary id spelling");
        assert_eq!(forged, ProviderToolCallId::synthesized(1));
    }

    #[test]
    fn identities_round_trip_through_their_transparent_spelling() {
        let provider = ProviderId::new("openai_compat").unwrap();
        let json = serde_json::to_string(&provider).unwrap();
        assert_eq!(json, "\"openai_compat\"");
        assert_eq!(serde_json::from_str::<ProviderId>(&json).unwrap(), provider);

        let model = ModelId::new("gpt-4o-mini").unwrap();
        assert_eq!(
            serde_json::from_str::<ModelId>(&serde_json::to_string(&model).unwrap()).unwrap(),
            model
        );

        let call = ProviderToolCallId::new("call_1").unwrap();
        assert_eq!(
            serde_json::from_str::<ProviderToolCallId>(&serde_json::to_string(&call).unwrap())
                .unwrap(),
            call
        );
    }

    #[test]
    fn a_refused_spelling_fails_deserialization_rather_than_entering_the_process() {
        let error = serde_json::from_str::<ProviderId>("\"Not Snake Case\"").unwrap_err();
        assert!(error.to_string().contains("provider id"), "{error}");
    }
}
