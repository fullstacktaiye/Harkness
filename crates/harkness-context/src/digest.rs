//! Deterministic hashing primitives shared by every context identity.
//!
//! Two rules make every digest in this crate reproducible across machines and
//! unambiguous across inputs:
//!
//! - **Domain separation.** Every digest starts by absorbing a constant naming
//!   what is being hashed and its version, so a file-version hash can never
//!   collide with a chunk hash that happens to absorb the same bytes.
//! - **Length framing.** Every component is absorbed as its length, as eight
//!   little-endian bytes, followed by its bytes. Concatenation is therefore
//!   injective: `("ab", "c")` and `("a", "bc")` produce different digests, which
//!   a naive concatenation would not.
//!
//! Nothing here reads a clock, a locale, or a path separator, so the same inputs
//! digest identically on every platform.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use sha2::{Digest, Sha256};

use crate::error::ContextDomainError;

/// Domain tag for an unordered set of `(path, content digest)` pairs.
pub(crate) const DOMAIN_PATH_SET: &str = "harkness.context.path_set.v1";
/// Domain tag for the composite workspace identity.
pub(crate) const DOMAIN_SNAPSHOT: &str = "harkness.context.workspace_snapshot.v1";
/// Domain tag for [`FileVersionId`](crate::FileVersionId).
pub(crate) const DOMAIN_FILE_VERSION: &str = "harkness.context.file_version.v1";
/// Domain tag for [`ChunkId`](crate::ChunkId).
pub(crate) const DOMAIN_CHUNK: &str = "harkness.context.chunk.v1";
/// Domain tag for [`SymbolId`](crate::SymbolId).
pub(crate) const DOMAIN_SYMBOL: &str = "harkness.context.symbol.v1";

/// Number of characters in a hexadecimal SHA-256 digest.
const DIGEST_HEX_LENGTH: usize = 64;

/// A SHA-256 digest in its canonical lowercase hexadecimal spelling.
///
/// The spelling is the value: an uppercase or truncated hexadecimal string is
/// refused rather than normalized, because these digests are compared as strings
/// once they reach a database column.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Sha256Hex(String);

impl Sha256Hex {
    /// Hashes `bytes` and returns the digest.
    #[must_use]
    pub fn of(bytes: impl AsRef<[u8]>) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes.as_ref());
        Self::finish(hasher)
    }

    /// Finalizes an in-progress hasher, for content streamed in blocks.
    #[must_use]
    pub(crate) fn finish(hasher: Sha256) -> Self {
        Self(format!("{:x}", hasher.finalize()))
    }

    /// The canonical lowercase hexadecimal spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Sha256Hex {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for Sha256Hex {
    type Err = ContextDomainError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != DIGEST_HEX_LENGTH
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ContextDomainError::InvalidDigest {
                value: value.to_owned(),
                expected: "SHA-256 digest",
                reason: "must be exactly 64 lowercase hexadecimal characters",
            });
        }
        Ok(Self(value.to_owned()))
    }
}

impl Serialize for Sha256Hex {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Sha256Hex {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(de::Error::custom)
    }
}

/// Absorbs domain-separated, length-framed components into one SHA-256 digest.
///
/// Callers append components in a fixed order; the framing is what makes that
/// order recoverable from the digest's point of view, so two different field
/// layouts can never produce the same value.
pub(crate) struct DigestWriter {
    hasher: Sha256,
}

impl DigestWriter {
    /// Starts a digest in `domain`, which is absorbed as the first component.
    pub(crate) fn new(domain: &str) -> Self {
        let mut writer = Self {
            hasher: Sha256::new(),
        };
        writer.field(domain.as_bytes());
        writer
    }

    /// Absorbs one length-framed component.
    pub(crate) fn field(&mut self, bytes: &[u8]) -> &mut Self {
        self.hasher.update((bytes.len() as u64).to_le_bytes());
        self.hasher.update(bytes);
        self
    }

    /// Absorbs an optional component, distinguishing absent from empty.
    pub(crate) fn optional_field(&mut self, bytes: Option<&[u8]>) -> &mut Self {
        match bytes {
            Some(bytes) => {
                self.field(b"some");
                self.field(bytes)
            }
            None => self.field(b"none"),
        }
    }

    /// Absorbs a counter or generation as eight little-endian bytes.
    pub(crate) fn integer(&mut self, value: u64) -> &mut Self {
        self.field(&value.to_le_bytes())
    }

    /// Finalizes the digest.
    pub(crate) fn finish(self) -> Sha256Hex {
        Sha256Hex::finish(self.hasher)
    }
}

/// The digest of a path set holding no paths.
///
/// This is the value a snapshot records for a component nothing has produced
/// yet — the instruction-set digest before [#120] discovers instruction files —
/// so "no instructions" is a stated fact rather than an absent field.
///
/// [#120]: https://github.com/fullstacktaiye/harkness/issues/120
#[must_use]
pub fn empty_path_set_digest() -> Sha256Hex {
    let mut writer = DigestWriter::new(DOMAIN_PATH_SET);
    writer.integer(0);
    writer.finish()
}

#[cfg(test)]
mod tests {
    use super::{DOMAIN_PATH_SET, DigestWriter, Sha256Hex, empty_path_set_digest};

    /// Frozen so a refactor that changes the framing fails here rather than
    /// silently invalidating every persisted identity on the next release.
    const EMPTY_INPUT_DIGEST: &str =
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn digests_of_known_bytes_are_frozen_across_platforms() {
        assert_eq!(Sha256Hex::of(b"").as_str(), EMPTY_INPUT_DIGEST);
        assert_eq!(
            Sha256Hex::of(b"harkness").as_str(),
            "3d85355468a6c0d4a393eb0a11982efe38adc4bebe649d2a3992a5dcdbe1edad"
        );
    }

    #[test]
    fn framing_distinguishes_differently_split_components() {
        let mut left = DigestWriter::new("test");
        left.field(b"ab").field(b"c");
        let mut right = DigestWriter::new("test");
        right.field(b"a").field(b"bc");
        assert_ne!(left.finish(), right.finish());
    }

    #[test]
    fn domain_separation_distinguishes_identical_components() {
        let mut left = DigestWriter::new("one");
        left.field(b"same");
        let mut right = DigestWriter::new("two");
        right.field(b"same");
        assert_ne!(left.finish(), right.finish());
    }

    #[test]
    fn an_absent_component_differs_from_an_empty_one() {
        let mut absent = DigestWriter::new("test");
        absent.optional_field(None);
        let mut empty = DigestWriter::new("test");
        empty.optional_field(Some(b""));
        assert_ne!(absent.finish(), empty.finish());
    }

    #[test]
    fn the_empty_path_set_digest_is_stable() {
        let mut expected = DigestWriter::new(DOMAIN_PATH_SET);
        expected.integer(0);
        assert_eq!(empty_path_set_digest(), expected.finish());
        assert_eq!(empty_path_set_digest(), empty_path_set_digest());
    }

    #[test]
    fn digest_spellings_round_trip_and_reject_malformed_input() {
        let digest = Sha256Hex::of(b"round trip");
        let json = serde_json::to_string(&digest).unwrap();
        assert_eq!(json, format!("\"{digest}\""));
        assert_eq!(serde_json::from_str::<Sha256Hex>(&json).unwrap(), digest);

        for rejected in [
            "",
            "abc",
            &"A".repeat(64),
            &"g".repeat(64),
            &format!("{digest}0"),
        ] {
            assert_eq!(
                rejected.parse::<Sha256Hex>().unwrap_err().kind(),
                "invalid_digest",
                "accepted '{rejected}'"
            );
        }
    }
}
