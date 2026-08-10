use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::RegistryError;

/// Longest accepted tool identifier or capability name.
///
/// The bound exists because these strings are persisted in
/// `tool_calls.tool_id`, published by `harkness contract`, and hashed into
/// approval scopes. A name long enough to need wrapping in any of those places
/// is a name nobody can match against reliably.
pub const MAX_IDENTIFIER_LENGTH: usize = 64;

/// Segments a tool identifier must have at minimum: a namespace and a verb.
const MINIMUM_TOOL_ID_SEGMENTS: usize = 2;

/// Segments a capability name must have at minimum.
const MINIMUM_CAPABILITY_SEGMENTS: usize = 1;

/// Reason reported when a name exceeds [`MAX_IDENTIFIER_LENGTH`].
///
/// A `&'static str` so every error variant can carry a stable reason without
/// allocating; `the_length_reason_states_the_actual_bound` keeps it honest.
const TOO_LONG: &str = "it is longer than 64 characters";

/// A stable dotted tool identifier such as `fs.read`.
///
/// The grammar is deliberately narrow: lowercase ASCII letters, digits, and
/// underscores, grouped into dot-separated segments that each start with a
/// letter, with at least a namespace and a verb. Every character it admits
/// survives a shell word, a SQL literal, a JSON key, and a URL path segment
/// unescaped, so the one identity that ties a descriptor, a persisted
/// `tool_calls` row, and an approval scope together never needs quoting rules
/// that differ between them.
///
/// An identifier is part of the public contract: renaming one is a breaking
/// change for every recorded call that referenced it, which is why the registry
/// pins `(id, version)` rather than `id` alone.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(into = "String", try_from = "String")]
pub struct ToolId(String);

impl ToolId {
    /// Parses a dotted tool identifier.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::InvalidToolId`] when the value is empty, longer
    /// than [`MAX_IDENTIFIER_LENGTH`], carries a character outside the grammar,
    /// or names fewer than two segments.
    pub fn new(value: impl Into<String>) -> Result<Self, RegistryError> {
        let value = value.into();
        if let Err(reason) = validate_identifier(&value, MINIMUM_TOOL_ID_SEGMENTS) {
            return Err(RegistryError::InvalidToolId { value, reason });
        }
        Ok(Self(value))
    }

    /// Borrows the identifier as its stable string spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Leading namespace segment, the part shared by a family of tools.
    #[must_use]
    pub fn namespace(&self) -> &str {
        self.0.split('.').next().unwrap_or_default()
    }
}

/// A tool version ordered by semantic-version precedence.
///
/// Versions are compared by precedence rather than by string, so `0.10.0`
/// follows `0.9.0` and a pre-release never outranks the release it precedes.
/// Resolving "the latest version of an id" therefore cannot be fooled by
/// lexicographic ordering, which is the whole reason the version is parsed
/// instead of stored as text.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(into = "String", try_from = "String")]
pub struct ToolVersion(semver::Version);

impl ToolVersion {
    /// Parses a semantic version.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::InvalidToolVersion`] when the value is not a
    /// complete `major.minor.patch` semantic version.
    pub fn new(value: impl Into<String>) -> Result<Self, RegistryError> {
        let value = value.into();
        match semver::Version::parse(&value) {
            Ok(version) => Ok(Self(version)),
            Err(error) => Err(RegistryError::InvalidToolVersion {
                value,
                reason: error.to_string(),
            }),
        }
    }

    /// Borrows the parsed semantic version.
    #[must_use]
    pub const fn as_semver(&self) -> &semver::Version {
        &self.0
    }
}

/// The `(id, version)` pair a caller resolves and a record pins.
///
/// This is the unit the registry keys on, the unit `tool_calls.tool_id` plus
/// `tool_calls.tool_version` reproduces, and the unit an approval is bound to.
/// Keeping it one type means none of those three can drift into recording only
/// half of it.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolIdentity {
    /// Stable dotted tool identifier.
    pub id: ToolId,
    /// Immutable version of that tool.
    pub version: ToolVersion,
}

impl ToolIdentity {
    /// Pairs an identifier with a version.
    #[must_use]
    pub const fn new(id: ToolId, version: ToolVersion) -> Self {
        Self { id, version }
    }

    /// Parses both halves from their string spellings.
    ///
    /// # Errors
    ///
    /// Returns the first of [`RegistryError::InvalidToolId`] or
    /// [`RegistryError::InvalidToolVersion`] that applies.
    pub fn parse(id: &str, version: &str) -> Result<Self, RegistryError> {
        Ok(Self::new(ToolId::new(id)?, ToolVersion::new(version)?))
    }
}

/// A named capability a tool requires in order to run.
///
/// Capabilities are what the policy engine grants and what an approval is
/// scoped to. A tool declares them once, in its descriptor, so a grant can be
/// reasoned about before anything executes rather than discovered while it
/// runs. Unlike a tool identifier a single segment is allowed, because the
/// broadest capabilities genuinely have no namespace to sit under.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(into = "String", try_from = "String")]
pub struct Capability(String);

impl Capability {
    /// Parses a capability name.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::InvalidCapability`] when the value is empty,
    /// longer than [`MAX_IDENTIFIER_LENGTH`], or carries a character outside
    /// the identifier grammar.
    pub fn new(value: impl Into<String>) -> Result<Self, RegistryError> {
        let value = value.into();
        if let Err(reason) = validate_identifier(&value, MINIMUM_CAPABILITY_SEGMENTS) {
            return Err(RegistryError::InvalidCapability { value, reason });
        }
        Ok(Self(value))
    }

    /// Borrows the capability as its stable string spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Checks a value against the shared identifier grammar.
///
/// Returns a stable reason rather than a formatted message so the caller can
/// attach it to the error variant that names the field being rejected.
fn validate_identifier(value: &str, minimum_segments: usize) -> Result<(), &'static str> {
    if value.is_empty() {
        return Err("it must not be empty");
    }
    // Counted in characters rather than bytes so the reason a non-ASCII name is
    // refused is its grammar, reported below, and not a length it never exceeded.
    if value.chars().count() > MAX_IDENTIFIER_LENGTH {
        return Err(TOO_LONG);
    }

    let mut segments = 0;
    for segment in value.split('.') {
        segments += 1;
        let mut characters = segment.chars();
        match characters.next() {
            None => return Err("every dot-separated segment must be non-empty"),
            Some(first) if !first.is_ascii_lowercase() => {
                return Err("every segment must start with a lowercase ASCII letter");
            }
            Some(_) => {}
        }
        if !characters.all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
        }) {
            return Err("segments accept only lowercase ASCII letters, digits, and underscores");
        }
    }

    if segments < minimum_segments {
        return Err("it must name at least a namespace and a verb, as in fs.read");
    }
    Ok(())
}

macro_rules! impl_string_newtype {
    ($name:ident) => {
        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = RegistryError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = RegistryError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.to_string()
            }
        }
    };
}

impl_string_newtype!(ToolId);
impl_string_newtype!(ToolVersion);
impl_string_newtype!(Capability);

impl fmt::Display for ToolIdentity {
    /// Renders the pair as `id@version`, the spelling used in log lines and
    /// error messages so a report never names a tool without its version.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}@{}", self.id, self.version)
    }
}

#[cfg(test)]
mod tests {
    use super::{Capability, MAX_IDENTIFIER_LENGTH, ToolId, ToolIdentity, ToolVersion};

    #[test]
    fn tool_identifiers_accept_dotted_lowercase_names() {
        for accepted in ["fs.read", "git.commit", "process.spawn_shell", "a.b.c9"] {
            let id = ToolId::new(accepted).unwrap();
            assert_eq!(id.as_str(), accepted);
            assert_eq!(id.to_string(), accepted);
        }
        assert_eq!(ToolId::new("fs.read").unwrap().namespace(), "fs");
    }

    #[test]
    fn tool_identifiers_refuse_names_outside_the_grammar() {
        let rejected = [
            ("", "it must not be empty"),
            (
                "fs",
                "it must name at least a namespace and a verb, as in fs.read",
            ),
            ("fs.", "every dot-separated segment must be non-empty"),
            (".read", "every dot-separated segment must be non-empty"),
            (
                "FS.read",
                "every segment must start with a lowercase ASCII letter",
            ),
            (
                "9fs.read",
                "every segment must start with a lowercase ASCII letter",
            ),
            (
                "fs.read-file",
                "segments accept only lowercase ASCII letters, digits, and underscores",
            ),
            (
                "fs.read file",
                "segments accept only lowercase ASCII letters, digits, and underscores",
            ),
        ];
        for (value, reason) in rejected {
            let error = ToolId::new(value).unwrap_err();
            assert_eq!(error.kind(), "invalid_tool_id", "accepted {value:?}");
            assert!(
                error.to_string().contains(reason),
                "{value:?} reported {error}"
            );
        }

        let overlong = format!("fs.{}", "a".repeat(MAX_IDENTIFIER_LENGTH));
        assert!(
            ToolId::new(overlong)
                .unwrap_err()
                .to_string()
                .contains(super::TOO_LONG)
        );
        // A name exactly at the bound is accepted; the bound is inclusive.
        let at_bound = format!("fs.{}", "a".repeat(MAX_IDENTIFIER_LENGTH - 3));
        assert!(ToolId::new(at_bound).is_ok());
    }

    #[test]
    fn the_length_reason_states_the_actual_bound() {
        // The message is a `&'static str`, so it cannot interpolate the constant;
        // this is what keeps the two from drifting apart.
        assert!(
            super::TOO_LONG.contains(&MAX_IDENTIFIER_LENGTH.to_string()),
            "{} does not name {MAX_IDENTIFIER_LENGTH}",
            super::TOO_LONG
        );
    }

    #[test]
    fn a_non_ascii_name_is_refused_for_its_grammar_not_its_length() {
        let error = ToolId::new("fs.léer").unwrap_err();
        assert!(
            error.to_string().contains("lowercase ASCII"),
            "a short non-ASCII name must not be blamed on length: {error}"
        );
    }

    #[test]
    fn capability_names_allow_a_single_segment_but_share_the_grammar() {
        assert_eq!(Capability::new("network").unwrap().as_str(), "network");
        assert_eq!(Capability::new("fs.write").unwrap().as_str(), "fs.write");
        assert_eq!(
            Capability::new("Network").unwrap_err().kind(),
            "invalid_capability"
        );
    }

    #[test]
    fn tool_versions_order_by_precedence_and_not_by_string() {
        let mut versions = [
            ToolVersion::new("0.9.0").unwrap(),
            ToolVersion::new("0.10.0").unwrap(),
            ToolVersion::new("1.0.0-alpha.1").unwrap(),
            ToolVersion::new("1.0.0").unwrap(),
        ];
        versions.sort();
        let spellings = versions.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(spellings, ["0.9.0", "0.10.0", "1.0.0-alpha.1", "1.0.0"]);
    }

    #[test]
    fn tool_versions_refuse_partial_semantic_versions() {
        for rejected in ["", "1", "1.0", "latest", "v1.0.0"] {
            assert_eq!(
                ToolVersion::new(rejected).unwrap_err().kind(),
                "invalid_tool_version",
                "accepted {rejected:?}"
            );
        }
    }

    #[test]
    fn identifiers_round_trip_through_json_as_bare_strings() {
        let identity = ToolIdentity::parse("fs.read", "1.2.3").unwrap();
        let json = serde_json::to_string(&identity).unwrap();
        assert_eq!(json, r#"{"id":"fs.read","version":"1.2.3"}"#);
        assert_eq!(
            serde_json::from_str::<ToolIdentity>(&json).unwrap(),
            identity
        );
        assert_eq!(identity.to_string(), "fs.read@1.2.3");
    }

    #[test]
    fn deserialization_enforces_the_same_grammar_as_the_constructor() {
        let error = serde_json::from_str::<ToolId>(r#""FS.read""#).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("must start with a lowercase ASCII letter"),
            "{error}"
        );
        assert!(
            serde_json::from_str::<ToolVersion>(r#""1.0""#)
                .unwrap_err()
                .to_string()
                .contains("is not a valid tool version"),
        );
    }

    #[test]
    fn identities_sort_by_identifier_then_by_version_precedence() {
        let mut identities = [
            ToolIdentity::parse("fs.write", "1.0.0").unwrap(),
            ToolIdentity::parse("fs.read", "1.10.0").unwrap(),
            ToolIdentity::parse("fs.read", "1.9.0").unwrap(),
        ];
        identities.sort();
        let rendered = identities
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert_eq!(
            rendered,
            ["fs.read@1.9.0", "fs.read@1.10.0", "fs.write@1.0.0"]
        );
    }
}
