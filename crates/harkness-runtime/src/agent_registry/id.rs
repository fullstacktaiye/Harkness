use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use super::error::{AgentRegistryError, invalid_registration};

/// Longest a registration identifier may be, in bytes.
///
/// It is a `runtime.db` primary key, an `agents.json` object field, a path
/// component of nothing, and the thing a person types into a command line. The
/// bound is what any of those can carry comfortably, not what the largest of
/// them could survive.
pub const MAX_AGENT_ID_LENGTH: usize = 64;

/// The identity of one registration in `agents.json`.
///
/// Chosen by the user rather than generated, because it is the name they type
/// and the name that appears in a diff of their own configuration file:
/// `gemini-cli`, `gemini-cli-dev`, `claude-code-acp`. Two builds of one program
/// are two registrations with two identifiers and two independent trust, health
/// and capability records, which is what makes keeping a development build
/// beside a release build an ordinary thing to do rather than a workaround.
///
/// It is *not* the identity of the executable. Replacing the binary at the
/// configured path keeps this value and changes the
/// [`IdentityBasis`](crate::integration::IdentityBasis) the grant was bound to,
/// which is exactly what makes the swap visible instead of silent.
///
/// # Grammar
///
/// One to [`MAX_AGENT_ID_LENGTH`] bytes of lowercase ASCII letters, digits, and
/// the separators `-`, `_` and `.`, beginning and ending with a letter or a
/// digit. Uppercase is refused rather than folded: the value is compared as
/// text in a `STRICT` SQLite column and written verbatim into a JSON file, and
/// two spellings of one identifier would be two rows claiming one registration.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AgentId(String);

impl AgentId {
    /// Validates a registration identifier.
    ///
    /// # Errors
    ///
    /// Returns [`AgentRegistryError::InvalidRegistration`] when the value is
    /// empty, longer than [`MAX_AGENT_ID_LENGTH`], or violates the grammar
    /// above.
    pub fn new(id: impl Into<String>) -> Result<Self, AgentRegistryError> {
        let id = id.into();
        let refuse = |reason| Err(invalid_registration("id", reason));
        if id.is_empty() {
            return refuse("it cannot be empty");
        }
        if id.len() > MAX_AGENT_ID_LENGTH {
            return refuse("it is longer than the maximum agent identifier length");
        }
        // Compared as bytes rather than as characters: the length bound above
        // counts bytes, so a multi-byte character would otherwise be measured
        // by one rule and validated by another.
        let bytes = id.as_bytes();
        let is_alphanumeric = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
        if !bytes
            .iter()
            .copied()
            .all(|byte| is_alphanumeric(byte) || byte == b'-' || byte == b'_' || byte == b'.')
        {
            return refuse("it must be lowercase ASCII letters, digits, '-', '_' or '.'");
        }
        let ends_well = is_alphanumeric(bytes[0]) && is_alphanumeric(bytes[bytes.len() - 1]);
        if !ends_well {
            return refuse("it must begin and end with a lowercase letter or a digit");
        }
        Ok(Self(id))
    }

    /// The identifier as the user spelled it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for AgentId {
    type Err = AgentRegistryError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for AgentId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for AgentId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentId, MAX_AGENT_ID_LENGTH};

    #[test]
    fn ordinary_registration_identifiers_are_accepted() {
        for spelling in [
            "gemini-cli",
            "gemini-cli-dev",
            "claude-code-acp",
            "agent.v2",
            "a",
            "opencode_1",
        ] {
            assert_eq!(AgentId::new(spelling).unwrap().as_str(), spelling);
        }
    }

    #[test]
    fn a_spelling_outside_the_grammar_is_refused_rather_than_folded() {
        for spelling in [
            "",
            "Gemini",
            "gemini cli",
            "-gemini",
            "gemini-",
            ".gemini",
            "gemini/cli",
            "géminí",
        ] {
            assert!(
                AgentId::new(spelling).is_err(),
                "{spelling:?} should not be a registration identifier"
            );
        }
    }

    #[test]
    fn the_length_bound_counts_bytes() {
        assert!(AgentId::new("a".repeat(MAX_AGENT_ID_LENGTH)).is_ok());
        assert!(AgentId::new("a".repeat(MAX_AGENT_ID_LENGTH + 1)).is_err());
    }

    #[test]
    fn serde_round_trips_through_the_validating_constructor() {
        let id = AgentId::new("gemini-cli").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"gemini-cli\"");
        assert_eq!(serde_json::from_str::<AgentId>(&json).unwrap(), id);
        assert!(serde_json::from_str::<AgentId>("\"Gemini\"").is_err());
    }
}
