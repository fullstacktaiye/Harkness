//! Stable identifiers for everything the context engine names.
//!
//! Two families live here and they are not interchangeable.
//!
//! **Minted identities** — [`SnapshotId`], [`ContextPackId`], [`ContextItemId`],
//! [`ContextQueryId`] — are random v4 UUIDs on the [`ProjectId`] pattern. They
//! name an *event*: this capture, this assembled pack, this query. Capturing the
//! same workspace twice yields two snapshots with two ids and one
//! [`SnapshotDigest`](crate::SnapshotDigest); the digest is what answers "is this
//! the same workspace", and the id is what a run correlates by.
//!
//! **Content-derived identities** — [`FileVersionId`], [`ChunkId`],
//! [`SymbolId`] — are SHA-256 digests over the content they name, spelled
//! `sha256:<hex>`. They are deterministic: the same inputs produce the same id
//! on every machine and after every restart, which is what lets a cache built in
//! one process be trusted by another.
//!
//! Every content-derived id absorbs a *content digest* rather than the content
//! itself, so a large file can be hashed in fixed-size blocks and combined
//! afterwards without a second derivation rule that could drift from the first.
//!
//! [`ProjectId`]: harkness_core::ProjectId

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use uuid::Uuid;

use crate::digest::{DOMAIN_CHUNK, DOMAIN_FILE_VERSION, DOMAIN_SYMBOL, DigestWriter, Sha256Hex};
use crate::error::ContextDomainError;
use crate::path::RepoPath;

/// The algorithm prefix every content-derived identifier carries.
const CONTENT_ID_PREFIX: &str = "sha256:";

macro_rules! define_minted_id {
    ($(#[$metadata:meta])* $name:ident) => {
        $(#[$metadata])*
        #[derive(
            Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Generates a new random identifier.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl Default for $name {
            /// Generates a fresh random identifier; there is no empty ID value.
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            /// Accepts every UUID spelling supported by [`Uuid::parse_str`].
            /// Display and serialization always return canonical hyphenated form.
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value).map(Self)
            }
        }
    };
}

define_minted_id!(
    /// A stable identifier for one capture of a workspace's state.
    ///
    /// Every retrieved piece of context names the snapshot it came from, so a
    /// run inspected later can prove which workspace state it described.
    ///
    /// [`Default`] generates a fresh random identity rather than an empty value.
    SnapshotId
);
define_minted_id!(
    /// A stable identifier for one assembled context pack.
    ///
    /// The pack aggregate itself is defined where it is built ([#122]); the
    /// identity lives here so records that reference a pack can be typed before
    /// then.
    ///
    /// [`Default`] generates a fresh random identity rather than an empty value.
    ///
    /// [#122]: https://github.com/fullstacktaiye/harkness/issues/122
    ContextPackId
);
define_minted_id!(
    /// A stable identifier for one item inside a context pack.
    ///
    /// [`Default`] generates a fresh random identity rather than an empty value.
    ContextItemId
);
define_minted_id!(
    /// A stable identifier for one retrieval query.
    ///
    /// [`Default`] generates a fresh random identity rather than an empty value.
    ContextQueryId
);

macro_rules! define_content_id {
    ($(#[$metadata:meta])* $name:ident, $expected:literal) => {
        $(#[$metadata])*
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(Sha256Hex);

        impl $name {
            /// The underlying digest, without its algorithm prefix.
            #[must_use]
            pub fn digest(&self) -> &Sha256Hex {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "{CONTENT_ID_PREFIX}{}", self.0)
            }
        }

        impl FromStr for $name {
            type Err = ContextDomainError;

            /// Requires the `sha256:` prefix, so an identifier can never be
            /// confused with a bare digest column.
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                let digest = value.strip_prefix(CONTENT_ID_PREFIX).ok_or_else(|| {
                    ContextDomainError::InvalidDigest {
                        value: value.to_owned(),
                        expected: $expected,
                        reason: "must begin with the 'sha256:' algorithm prefix",
                    }
                })?;
                digest
                    .parse()
                    .map(Self)
                    .map_err(|_| ContextDomainError::InvalidDigest {
                        value: value.to_owned(),
                        expected: $expected,
                        reason: "must be 'sha256:' followed by 64 lowercase hexadecimal characters",
                    })
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.to_string())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                value.parse().map_err(de::Error::custom)
            }
        }
    };
}

define_content_id!(
    /// Identifies one version of one file's contents.
    ///
    /// # Derivation
    ///
    /// `sha256:` over the [file-version domain](crate::FileVersionId), then the
    /// path's exact bytes, then the SHA-256 of the file's bytes — each
    /// length-framed. The path participates so that two files sharing content
    /// keep separate identities, which is what makes "this content came from
    /// this path" checkable rather than assumed.
    ///
    /// Absorbing the *content digest* rather than the content lets a large file
    /// be hashed in blocks: [`FileVersionId::derive`] and
    /// [`FileVersionId::from_content_digest`] are the same derivation reached
    /// two ways, never two rules.
    FileVersionId,
    "file version identifier"
);
define_content_id!(
    /// Identifies one chunk of one file.
    ///
    /// # Derivation
    ///
    /// `sha256:` over the chunk domain, the path's exact bytes, the chunk's
    /// structural anchor, and the SHA-256 of the chunk's bytes — each
    /// length-framed. The anchor is a stable description of *where* the chunk
    /// sits (an enclosing item's qualified name, not a byte offset), which is
    /// what keeps a chunk's identity unchanged when an unrelated region of the
    /// same file moves. The anchor vocabulary is fixed by [#113]; this type
    /// fixes only how an anchor is absorbed.
    ///
    /// [#113]: https://github.com/fullstacktaiye/harkness/issues/113
    ChunkId,
    "chunk identifier"
);
define_content_id!(
    /// Identifies one symbol declaration.
    ///
    /// # Derivation
    ///
    /// `sha256:` over the symbol domain, the path's exact bytes, the language,
    /// the qualified symbol name, and the symbol kind — each length-framed. No
    /// content participates: a symbol keeps its identity while its body changes,
    /// which is what makes a symbol index diffable. The language and kind
    /// vocabularies are fixed by [#117].
    ///
    /// [#117]: https://github.com/fullstacktaiye/harkness/issues/117
    SymbolId,
    "symbol identifier"
);

impl FileVersionId {
    /// Derives the identity of `content` stored at `path`.
    #[must_use]
    pub fn derive(path: &RepoPath, content: &[u8]) -> Self {
        Self::from_content_digest(path, &Sha256Hex::of(content))
    }

    /// Derives the same identity from a content digest hashed elsewhere.
    #[must_use]
    pub fn from_content_digest(path: &RepoPath, content: &Sha256Hex) -> Self {
        let mut writer = DigestWriter::new(DOMAIN_FILE_VERSION);
        writer
            .field(path.as_bytes())
            .field(content.as_str().as_bytes());
        Self(writer.finish())
    }
}

impl ChunkId {
    /// Derives the identity of a chunk anchored at `anchor` within `path`.
    #[must_use]
    pub fn derive(path: &RepoPath, anchor: &str, content: &[u8]) -> Self {
        Self::from_content_digest(path, anchor, &Sha256Hex::of(content))
    }

    /// Derives the same identity from a content digest hashed elsewhere.
    #[must_use]
    pub fn from_content_digest(path: &RepoPath, anchor: &str, content: &Sha256Hex) -> Self {
        let mut writer = DigestWriter::new(DOMAIN_CHUNK);
        writer
            .field(path.as_bytes())
            .field(anchor.as_bytes())
            .field(content.as_str().as_bytes());
        Self(writer.finish())
    }
}

impl SymbolId {
    /// Derives the identity of a symbol declared in `path`.
    #[must_use]
    pub fn derive(path: &RepoPath, language: &str, qualified_name: &str, kind: &str) -> Self {
        let mut writer = DigestWriter::new(DOMAIN_SYMBOL);
        writer
            .field(path.as_bytes())
            .field(language.as_bytes())
            .field(qualified_name.as_bytes())
            .field(kind.as_bytes());
        Self(writer.finish())
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::{Debug, Display};
    use std::str::FromStr;

    use serde::Serialize;
    use serde::de::DeserializeOwned;

    use super::{
        ChunkId, ContextItemId, ContextPackId, ContextQueryId, FileVersionId, SnapshotId, SymbolId,
    };
    use crate::digest::Sha256Hex;
    use crate::path::RepoPath;

    const FIXTURE_UUID: &str = "123e4567-e89b-42d3-a456-426614174000";

    fn assert_minted_contract<T>()
    where
        T: Copy + Debug + Display + Eq + FromStr<Err = uuid::Error> + Serialize + DeserializeOwned,
    {
        let id = T::from_str(FIXTURE_UUID).unwrap();
        assert_eq!(id.to_string(), FIXTURE_UUID);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{FIXTURE_UUID}\""));
        assert_eq!(serde_json::from_str::<T>(&json).unwrap(), id);
    }

    fn assert_content_contract<T>(id: &T)
    where
        T: Clone + Debug + Display + Eq + FromStr + Serialize + DeserializeOwned,
        T::Err: Debug,
    {
        let spelled = id.to_string();
        assert!(spelled.starts_with("sha256:"), "{spelled}");
        assert_eq!(spelled.len(), "sha256:".len() + 64);
        assert_eq!(&T::from_str(&spelled).unwrap(), id);
        let json = serde_json::to_string(id).unwrap();
        assert_eq!(json, format!("\"{spelled}\""));
        assert_eq!(&serde_json::from_str::<T>(&json).unwrap(), id);
    }

    fn path(text: &str) -> RepoPath {
        RepoPath::from_bytes(text.as_bytes().to_vec())
    }

    #[test]
    fn minted_ids_parse_display_and_serde_round_trip_like_project_id() {
        assert_minted_contract::<SnapshotId>();
        assert_minted_contract::<ContextPackId>();
        assert_minted_contract::<ContextItemId>();
        assert_minted_contract::<ContextQueryId>();
    }

    #[test]
    fn minted_ids_default_to_a_fresh_random_identity() {
        assert_ne!(SnapshotId::default(), SnapshotId::default());
        assert_ne!(ContextPackId::default(), ContextPackId::default());
        assert_ne!(ContextItemId::default(), ContextItemId::default());
        assert_ne!(ContextQueryId::default(), ContextQueryId::default());
    }

    #[test]
    fn content_ids_parse_display_and_serde_round_trip() {
        assert_content_contract(&FileVersionId::derive(&path("a.rs"), b"body"));
        assert_content_contract(&ChunkId::derive(&path("a.rs"), "fn main", b"body"));
        assert_content_contract(&SymbolId::derive(&path("a.rs"), "rust", "main", "function"));
    }

    #[test]
    fn content_ids_reject_a_bare_digest_or_a_foreign_algorithm() {
        let bare = Sha256Hex::of(b"body").to_string();
        assert_eq!(
            bare.parse::<FileVersionId>().unwrap_err().kind(),
            "invalid_digest"
        );
        assert_eq!(
            format!("md5:{bare}").parse::<ChunkId>().unwrap_err().kind(),
            "invalid_digest"
        );
        assert_eq!(
            "sha256:short".parse::<SymbolId>().unwrap_err().kind(),
            "invalid_digest"
        );
    }

    #[test]
    fn a_file_version_is_the_same_derivation_reached_two_ways() {
        assert_eq!(
            FileVersionId::derive(&path("a.rs"), b"body"),
            FileVersionId::from_content_digest(&path("a.rs"), &Sha256Hex::of(b"body"))
        );
        assert_eq!(
            ChunkId::derive(&path("a.rs"), "fn main", b"body"),
            ChunkId::from_content_digest(&path("a.rs"), "fn main", &Sha256Hex::of(b"body"))
        );
    }

    #[test]
    fn identical_content_at_two_paths_keeps_two_identities() {
        assert_ne!(
            FileVersionId::derive(&path("a.rs"), b"body"),
            FileVersionId::derive(&path("b.rs"), b"body")
        );
        assert_ne!(
            ChunkId::derive(&path("a.rs"), "one", b"body"),
            ChunkId::derive(&path("a.rs"), "two", b"body")
        );
    }

    #[test]
    fn a_symbol_identity_ignores_its_body_and_separates_its_components() {
        let symbol = SymbolId::derive(&path("a.rs"), "rust", "module::main", "function");
        assert_eq!(
            symbol,
            SymbolId::derive(&path("a.rs"), "rust", "module::main", "function")
        );
        // Framing keeps a shifted component boundary from colliding.
        assert_ne!(
            symbol,
            SymbolId::derive(&path("a.rs"), "rust", "module::mainfunction", "")
        );
    }

    #[test]
    fn non_utf8_paths_produce_distinct_content_identities() {
        let left = RepoPath::from_bytes(vec![0xff, b'a']);
        let right = RepoPath::from_bytes(vec![0xfe, b'a']);
        assert_eq!(
            left.display(),
            right.display(),
            "the fixture must be two paths a lossy conversion folds together"
        );
        assert_ne!(
            FileVersionId::derive(&left, b"body"),
            FileVersionId::derive(&right, b"body"),
            "a lossy conversion folded two distinct paths together"
        );
    }
}
