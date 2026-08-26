//! The two error namespaces this crate raises.
//!
//! [`ContextDomainError`] is what capturing, verifying, and decoding a record
//! can fail with. [`ContextEngineError`] is what the [engine](crate::engine)
//! facade fails with, and it is the *union* of its own table and the domain's:
//! a domain failure raised beneath a facade call is carried whole and keeps the
//! discriminant it was given, exactly as `InvocationError` delegates to
//! `ToolError` in the runtime. The two tables must stay disjoint, because
//! [`ContextEngineError::kinds`] publishes their concatenation.

use std::path::PathBuf;

use thiserror::Error;

use crate::inventory::InventoryError;
use crate::search::SearchError;
use crate::watch::WatchError;

/// Failures raised while capturing, verifying, or decoding context records.
///
/// Every variant maps to a stable discriminant in [`ContextDomainError::KINDS`],
/// which is the spelling a front end, the CLI envelope, or a persisted event may
/// depend on. Adding a variant requires adding its kind, and no kind here may
/// collide with another published namespace.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ContextDomainError {
    /// The repository backing the worktree could not be addressed.
    #[error("the repository at '{}' is unavailable: {reason}", path.display())]
    RepositoryUnavailable {
        /// Worktree root the operation was addressed to.
        path: PathBuf,
        /// Stable human-readable explanation from the Git layer.
        reason: String,
    },

    /// The worktree root does not exist, or is not a directory.
    #[error("the worktree root '{}' is missing", path.display())]
    WorktreeRootMissing {
        /// Worktree root the operation was addressed to.
        path: PathBuf,
    },

    /// A workspace probe reported a failure it declared fatal.
    ///
    /// An ordinary unreadable file is *not* this: it contributes the
    /// [`ContentDigest::Unreadable`] sentinel and is listed in capture
    /// diagnostics, so one permission-denied file never fails a whole capture.
    ///
    /// [`ContentDigest::Unreadable`]: crate::ContentDigest::Unreadable
    #[error("failed to hash '{path}': {reason}")]
    HashingFailed {
        /// Repository-relative path in its lossy display form.
        path: String,
        /// Stable human-readable explanation from the probe.
        reason: String,
    },

    /// Capture observed its [`Cancellation`] token before finishing.
    ///
    /// Capture never yields a half-built identity, so cancellation is a failure
    /// rather than a partial value. Verification answers
    /// [`FreshnessState::Unverifiable`] instead, because a cancelled check still
    /// has a defensible verdict: "this could not be told".
    ///
    /// [`Cancellation`]: harkness_git::Cancellation
    /// [`FreshnessState::Unverifiable`]: crate::FreshnessState::Unverifiable
    #[error("the snapshot operation was cancelled")]
    SnapshotCancelled,

    /// A digest or content-derived identifier is not in its documented form.
    #[error("'{value}' is not a valid {expected}: {reason}")]
    InvalidDigest {
        /// The rejected spelling.
        value: String,
        /// What the value was being read as.
        expected: &'static str,
        /// Stable human-readable explanation.
        reason: &'static str,
    },

    /// A snapshot wire record combines otherwise valid fields impossibly.
    #[error("workspace snapshot wire record is invalid: {reason}")]
    InvalidSnapshotWire {
        /// Stable human-readable explanation.
        reason: String,
    },

    /// A provenance wire record combines otherwise valid fields impossibly.
    #[error("provenance wire record is invalid: {reason}")]
    InvalidProvenanceWire {
        /// Stable human-readable explanation.
        reason: String,
    },

    /// A recorded digest disagrees with the digest its own components produce.
    ///
    /// Raised when a stored row is rebuilt: the entry lists are re-digested on
    /// load, so a hand-edited row fails to load rather than entering the process
    /// claiming an identity its contents do not support.
    #[error("{component} digest is {found} but its contents digest to {expected}")]
    DigestMismatch {
        /// Which digest disagreed, in its stable spelling.
        component: &'static str,
        /// Digest the record's own contents produce.
        expected: String,
        /// Digest the record claimed.
        found: String,
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
}

impl ContextDomainError {
    /// Every stable discriminant this error namespace can emit.
    pub const KINDS: &'static [&'static str] = &[
        "repository_unavailable",
        "worktree_root_missing",
        "hashing_failed",
        "snapshot_cancelled",
        "invalid_digest",
        "invalid_snapshot_wire",
        "invalid_provenance_wire",
        "digest_mismatch",
        "schema_version_too_old",
        "schema_version_too_new",
    ];

    /// Stable machine-readable discriminant for caller-facing error handling.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::RepositoryUnavailable { .. } => "repository_unavailable",
            Self::WorktreeRootMissing { .. } => "worktree_root_missing",
            Self::HashingFailed { .. } => "hashing_failed",
            Self::SnapshotCancelled => "snapshot_cancelled",
            Self::InvalidDigest { .. } => "invalid_digest",
            Self::InvalidSnapshotWire { .. } => "invalid_snapshot_wire",
            Self::InvalidProvenanceWire { .. } => "invalid_provenance_wire",
            Self::DigestMismatch { .. } => "digest_mismatch",
            Self::SchemaVersionTooOld { .. } => "schema_version_too_old",
            Self::SchemaVersionTooNew { .. } => "schema_version_too_new",
        }
    }
}

/// Failures raised by the [`ContextEngine`](crate::ContextEngine) facade.
///
/// Two absences here are deliberate rather than oversights.
///
/// There is no `repository_unavailable` variant: [`ContextDomainError`] already
/// publishes that kind, and re-spelling it would put one meaning in two
/// namespaces whose concatenation [`kinds`](Self::kinds) publishes. A worktree
/// that is not a repository, or one that has gone away, is reported as
/// [`ContextDomainError::RepositoryUnavailable`] carried by
/// [`Domain`](Self::Domain).
///
/// [`Cancelled`](Self::Cancelled) is *not* the same as
/// [`ContextDomainError::SnapshotCancelled`], which is why the spellings
/// differ. The domain kind says a workspace read was abandoned half-way; this
/// one says the facade observed the token before it got that far, or that
/// something with no workspace read in it — an index refresh, say — was
/// stopped.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ContextEngineError {
    /// A facade method whose implementation has not landed yet.
    ///
    /// A real, tested refusal rather than a `todo!()`: a caller written against
    /// the seam today gets a typed answer naming what is missing, and the issue
    /// that implements the method deletes this path rather than a panic.
    #[error("{feature} is not available in this build yet")]
    NotYetAvailable {
        /// Stable name of the missing feature.
        feature: &'static str,
    },

    /// A built-in language adapter could not be registered at startup.
    #[error("the symbol adapter registry is unavailable: {reason}")]
    SymbolAdapterUnavailable {
        /// Query-compilation or registration diagnostic.
        reason: String,
    },

    /// The index cache could not be opened, created, or written.
    ///
    /// The engine still serves everything that does not read the cache — the
    /// workspace snapshot above all — so a read-only or missing cache directory
    /// degrades retrieval instead of failing the engine.
    #[error("the index cache at '{}' is unusable: {reason}", path.display())]
    CacheOpenFailed {
        /// Cache database the failure is about.
        path: PathBuf,
        /// Stable human-readable explanation.
        reason: String,
    },

    /// The cache on disk was written by a newer build.
    ///
    /// Refused read-only and left byte-identical rather than downgraded, which
    /// mirrors the run store's `schema_too_new`. A newer sibling process is
    /// still using that cache; truncating it would be this build corrupting a
    /// working one.
    #[error(
        "the index cache at '{}' is at schema version {found}, newer than the maximum supported version {maximum}; upgrade Harkness to use it",
        path.display()
    )]
    CacheVersionConflict {
        /// Cache database that was refused.
        path: PathBuf,
        /// Schema version found in `index_meta`.
        found: u32,
        /// Newest cache schema this build understands.
        maximum: u32,
    },

    /// A cache found unusable mid-life was set aside and recreated.
    ///
    /// The call that met the fault does not succeed — the cache it addressed is
    /// gone — but the engine is healthy again afterwards, and the quarantined
    /// file is named so a person can look at it or delete it. `quarantined_to`
    /// is absent when there was nothing left to keep, which is what a cache
    /// another process deleted underneath this one looks like.
    #[error(
        "the index cache at '{}' was unusable and has been replaced: {reason}",
        path.display()
    )]
    CacheCorruptQuarantined {
        /// Cache database that was recreated.
        path: PathBuf,
        /// Where the unusable bytes were moved, when there were any.
        quarantined_to: Option<PathBuf>,
        /// Stable human-readable explanation.
        reason: String,
    },

    /// Another process held the cache past the busy timeout.
    ///
    /// Deliberately its own discriminant rather than a
    /// [`CacheOpenFailed`](Self::CacheOpenFailed) with a contention-shaped
    /// message. The two lead to opposite responses: a caller met by this one
    /// degrades to reading the workspace live and tries again later, while a
    /// permission bit or an exhausted descriptor table is not going to clear on
    /// a retry. It is never a reason to quarantine a cache — a busy cache is not
    /// a corrupt one, and treating it as one would let a slow front end destroy
    /// the other's index.
    #[error("the index cache at '{}' is busy: {reason}", path.display())]
    IndexBusy {
        /// Cache database the contention is on.
        path: PathBuf,
        /// Stable human-readable explanation from SQLite.
        reason: String,
    },

    /// A batch would have grown the cache past its per-repository cap.
    ///
    /// The batch is refused whole and the previous generation stays usable. The
    /// alternative — storing what fits — makes retrieval answer "no match" for
    /// content the cache simply never held, which a caller cannot tell from a
    /// repository that does not contain it.
    #[error(
        "the index cache at '{}' holds {bytes} bytes, past its {limit}-byte limit",
        path.display()
    )]
    IndexBudgetExhausted {
        /// Cache database that reached its cap.
        path: PathBuf,
        /// Bytes the cache occupies.
        bytes: u64,
        /// Bytes it may occupy.
        limit: u64,
    },

    /// Another batch published this worktree while this one was open.
    ///
    /// Two front ends indexing one repository is a supported situation, and one
    /// of them has to lose. The loser is refused rather than allowed to move the
    /// visibility watermark backwards, which would hide every row the winner
    /// committed — a batch that reported success while making the index smaller
    /// is the worst of the available outcomes.
    #[error(
        "the index cache at '{}' moved to generation {watermark} while a batch at generation {generation} was open",
        path.display()
    )]
    IndexBatchSuperseded {
        /// Cache database the batch was writing.
        path: PathBuf,
        /// Generation the refused batch held.
        generation: u64,
        /// Generation the cache actually reached.
        watermark: u64,
    },

    /// A batch was given rows it cannot store as presented.
    ///
    /// A caller mistake rather than a fault in the cache — an entry paired with
    /// another file's bytes, symbols attached to a file version the batch never
    /// records. It is spelled apart from [`CacheOpenFailed`](Self::CacheOpenFailed)
    /// because a front end must not tell a user their index is broken when the
    /// code above it built the batch wrong.
    #[error("the index batch cannot be stored as presented: {reason}")]
    IndexBatchInvalid {
        /// Stable human-readable explanation.
        reason: String,
    },

    /// A capture handed to the facade is of a different checkout.
    ///
    /// Every method that accepts a snapshot rather than taking one is accepting
    /// a caller's answer to "which workspace is this", and a capture of another
    /// checkout would stamp results with a workspace state they were not read
    /// from — provenance that is well formed and false. The root is what is
    /// compared, and not the project id: two catalog entries may name one
    /// checkout, which is the same reason a worktree's index rows are keyed by
    /// its canonical root.
    #[error(
        "the snapshot describes the worktree at '{}', not '{}'",
        found.display(),
        expected.display()
    )]
    ForeignSnapshot {
        /// The worktree root this engine serves.
        expected: PathBuf,
        /// The worktree root the snapshot describes.
        found: PathBuf,
    },

    /// The operation observed its cancellation token.
    #[error("the context engine operation was cancelled")]
    Cancelled,

    /// A domain failure raised beneath the facade.
    #[error(transparent)]
    Domain(#[from] ContextDomainError),

    /// An inventory walk failed beneath the facade.
    ///
    /// Carried whole rather than re-spelled, exactly as [`Self::Domain`] is: a
    /// caller that needs to tell a missing worktree root from an invalid ignore
    /// rule needs the discriminant the walk gave it, and re-mapping onto the
    /// engine's own vocabulary would erase the distinction the walk was careful
    /// to draw.
    #[error(transparent)]
    Inventory(InventoryError),

    /// Establishing a filesystem watch failed beneath the facade.
    ///
    /// Carried whole for the same reason the walk's failures are. The
    /// distinction that matters here is between a root that is not there and a
    /// backend that could not be established: the first is a real failure and
    /// the second costs latency only, because events were never what decided
    /// whether the index was current.
    #[error(transparent)]
    Watch(WatchError),

    /// A search refused beneath the facade.
    ///
    /// Carried whole for the same reason the other two are, and the distinction
    /// that matters here is between a query a caller can fix and a repository
    /// state it cannot: a malformed pattern, a path filter outside the
    /// workspace, and a withheld capability are all answered by changing the
    /// request, while an unindexed worktree is answered by building the index.
    /// Folding them into one engine-level spelling would leave a front end
    /// offering "rebuild the index" to somebody who typed an unbalanced
    /// bracket.
    #[error(transparent)]
    Search(SearchError),
}

impl From<SearchError> for ContextEngineError {
    /// Written by hand for the one variant [`From<InventoryError>`] is.
    ///
    /// [`From<InventoryError>`]: Self::from
    fn from(error: SearchError) -> Self {
        match error {
            SearchError::Cancelled => Self::Cancelled,
            carried => Self::Search(carried),
        }
    }
}

impl From<WatchError> for ContextEngineError {
    /// Written by hand for the same one variant [`From<InventoryError>`] is.
    ///
    /// [`From<InventoryError>`]: Self::from
    fn from(error: WatchError) -> Self {
        match error {
            WatchError::Cancelled => Self::Cancelled,
            carried => Self::Watch(carried),
        }
    }
}

impl From<InventoryError> for ContextEngineError {
    /// Written by hand for one variant's sake.
    ///
    /// A cancelled walk and a cancelled facade call are one event, and a caller
    /// polling its own token wants one answer: `#[from]` would carry a second
    /// `cancelled` spelling into the published namespace and make the two
    /// indistinguishable-but-unequal.
    fn from(error: InventoryError) -> Self {
        match error {
            InventoryError::Cancelled => Self::Cancelled,
            carried => Self::Inventory(carried),
        }
    }
}

impl ContextEngineError {
    /// Every stable discriminant this namespace defines on its own.
    ///
    /// [`kinds`](Self::kinds) is what a caller enumerating the facade's whole
    /// error surface wants; this table is the half that belongs to the engine.
    pub const KINDS: &'static [&'static str] = &[
        "not_yet_available",
        "symbol_adapter_unavailable",
        "cache_open_failed",
        "cache_version_conflict",
        "cache_corrupt_quarantined",
        "index_busy",
        "index_budget_exhausted",
        "index_batch_superseded",
        "index_batch_invalid",
        "foreign_snapshot",
        "cancelled",
    ];

    /// Stable machine-readable discriminant for caller-facing error handling.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::NotYetAvailable { .. } => "not_yet_available",
            Self::SymbolAdapterUnavailable { .. } => "symbol_adapter_unavailable",
            Self::CacheOpenFailed { .. } => "cache_open_failed",
            Self::CacheVersionConflict { .. } => "cache_version_conflict",
            Self::CacheCorruptQuarantined { .. } => "cache_corrupt_quarantined",
            Self::IndexBusy { .. } => "index_busy",
            Self::IndexBudgetExhausted { .. } => "index_budget_exhausted",
            Self::IndexBatchSuperseded { .. } => "index_batch_superseded",
            Self::IndexBatchInvalid { .. } => "index_batch_invalid",
            Self::ForeignSnapshot { .. } => "foreign_snapshot",
            Self::Cancelled => "cancelled",
            Self::Domain(error) => error.kind(),
            Self::Inventory(error) => error.kind(),
            Self::Watch(error) => error.kind(),
            Self::Search(error) => error.kind(),
        }
    }

    /// Every discriminant a facade call can report, in declaration order.
    ///
    /// The engine's own table, then the domain's, then the walk's, then the
    /// watch's, then the search's. A caller building an exit-code or
    /// presentation table needs the union, because a facade call can fail at
    /// any of the five layers and the caller cannot tell which one decided.
    ///
    /// `cancelled` is published once. A walk that observed the token, a watch
    /// that observed it, a search that observed it, and a facade call that
    /// observed it are the same answer, which is why the hand-written `From`
    /// implementations map onto the engine's own spelling rather than letting
    /// four tables spell it four times.
    #[must_use]
    pub fn kinds() -> Vec<&'static str> {
        Self::KINDS
            .iter()
            .copied()
            .chain(ContextDomainError::KINDS.iter().copied())
            .chain(
                InventoryError::KINDS
                    .iter()
                    .copied()
                    .filter(|kind| !Self::KINDS.contains(kind)),
            )
            .chain(
                WatchError::KINDS
                    .iter()
                    .copied()
                    .filter(|kind| !Self::KINDS.contains(kind)),
            )
            .chain(
                SearchError::KINDS
                    .iter()
                    .copied()
                    .filter(|kind| !Self::KINDS.contains(kind)),
            )
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{ContextDomainError, ContextEngineError};

    #[test]
    fn every_variant_maps_to_a_listed_kind_in_declaration_order() {
        let cases = [
            (
                ContextDomainError::RepositoryUnavailable {
                    path: PathBuf::from("/tmp/gone"),
                    reason: "not a repository".to_owned(),
                },
                "repository_unavailable",
            ),
            (
                ContextDomainError::WorktreeRootMissing {
                    path: PathBuf::from("/tmp/gone"),
                },
                "worktree_root_missing",
            ),
            (
                ContextDomainError::HashingFailed {
                    path: "src/main.rs".to_owned(),
                    reason: "permission denied".to_owned(),
                },
                "hashing_failed",
            ),
            (ContextDomainError::SnapshotCancelled, "snapshot_cancelled"),
            (
                ContextDomainError::InvalidDigest {
                    value: "nothex".to_owned(),
                    expected: "SHA-256 digest",
                    reason: "must be 64 lowercase hexadecimal characters",
                },
                "invalid_digest",
            ),
            (
                ContextDomainError::InvalidSnapshotWire {
                    reason: "captured_at must use the UTC offset".to_owned(),
                },
                "invalid_snapshot_wire",
            ),
            (
                ContextDomainError::InvalidProvenanceWire {
                    reason: "range end precedes its start".to_owned(),
                },
                "invalid_provenance_wire",
            ),
            (
                ContextDomainError::DigestMismatch {
                    component: "untracked",
                    expected: "a".repeat(64),
                    found: "b".repeat(64),
                },
                "digest_mismatch",
            ),
            (
                ContextDomainError::SchemaVersionTooOld {
                    record: "workspace_snapshot",
                    found: 0,
                    minimum: 1,
                },
                "schema_version_too_old",
            ),
            (
                ContextDomainError::SchemaVersionTooNew {
                    record: "workspace_snapshot",
                    found: 2,
                    maximum: 1,
                },
                "schema_version_too_new",
            ),
        ];

        let kinds = cases.iter().map(|(_, kind)| *kind).collect::<Vec<_>>();
        assert_eq!(kinds, ContextDomainError::KINDS);
        for (error, expected) in cases {
            assert_eq!(error.kind(), expected, "unexpected kind for {error:?}");
        }
    }

    #[test]
    fn kinds_are_unique() {
        let mut sorted = ContextDomainError::KINDS.to_vec();
        sorted.sort_unstable();
        let count = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), count);
    }

    #[test]
    fn every_engine_variant_maps_to_a_listed_kind_in_declaration_order() {
        let cases = [
            (
                ContextEngineError::NotYetAvailable { feature: "search" },
                "not_yet_available",
            ),
            (
                ContextEngineError::SymbolAdapterUnavailable {
                    reason: "invalid query".to_owned(),
                },
                "symbol_adapter_unavailable",
            ),
            (
                ContextEngineError::CacheOpenFailed {
                    path: PathBuf::from("/data/context/key/index.db"),
                    reason: "permission denied".to_owned(),
                },
                "cache_open_failed",
            ),
            (
                ContextEngineError::CacheVersionConflict {
                    path: PathBuf::from("/data/context/key/index.db"),
                    found: 2,
                    maximum: 1,
                },
                "cache_version_conflict",
            ),
            (
                ContextEngineError::CacheCorruptQuarantined {
                    path: PathBuf::from("/data/context/key/index.db"),
                    quarantined_to: Some(PathBuf::from("/data/context/key/index.db.corrupt-0")),
                    reason: "file is not a database".to_owned(),
                },
                "cache_corrupt_quarantined",
            ),
            (
                ContextEngineError::IndexBusy {
                    path: PathBuf::from("/data/context/key/index.db"),
                    reason: "database is locked".to_owned(),
                },
                "index_busy",
            ),
            (
                ContextEngineError::IndexBudgetExhausted {
                    path: PathBuf::from("/data/context/key/index.db"),
                    bytes: 1,
                    limit: 0,
                },
                "index_budget_exhausted",
            ),
            (
                ContextEngineError::IndexBatchSuperseded {
                    path: PathBuf::from("/data/context/key/index.db"),
                    generation: 1,
                    watermark: 2,
                },
                "index_batch_superseded",
            ),
            (
                ContextEngineError::IndexBatchInvalid {
                    reason: "symbols name a file version this batch never records".to_owned(),
                },
                "index_batch_invalid",
            ),
            (
                ContextEngineError::ForeignSnapshot {
                    expected: PathBuf::from("/w/this"),
                    found: PathBuf::from("/w/other"),
                },
                "foreign_snapshot",
            ),
            (ContextEngineError::Cancelled, "cancelled"),
        ];

        let kinds = cases.iter().map(|(_, kind)| *kind).collect::<Vec<_>>();
        assert_eq!(kinds, ContextEngineError::KINDS);
        for (error, expected) in cases {
            assert_eq!(error.kind(), expected, "unexpected kind for {error:?}");
        }
    }

    /// A domain failure raised beneath the facade keeps its own discriminant.
    /// Re-spelling it would make one meaning have two names depending on which
    /// layer a caller happened to reach it through.
    #[test]
    fn a_carried_domain_failure_keeps_its_own_kind() {
        let carried = ContextEngineError::from(ContextDomainError::RepositoryUnavailable {
            path: PathBuf::from("/tmp/gone"),
            reason: "not a repository".to_owned(),
        });

        assert_eq!(carried.kind(), "repository_unavailable");
        assert!(!ContextEngineError::KINDS.contains(&carried.kind()));
        assert!(ContextEngineError::kinds().contains(&"repository_unavailable"));
    }

    /// A search refusal is carried the same way, and its `cancelled` is the
    /// engine's own: a caller polling one token wants one answer whichever
    /// layer noticed first.
    #[test]
    fn a_carried_search_refusal_keeps_its_own_kind_but_not_its_own_cancellation() {
        let carried = ContextEngineError::from(crate::SearchError::RegexNotPermitted);
        assert_eq!(carried.kind(), "regex_not_permitted");
        assert!(matches!(carried, ContextEngineError::Search(_)));
        assert!(ContextEngineError::kinds().contains(&"regex_not_permitted"));

        let cancelled = ContextEngineError::from(crate::SearchError::Cancelled);
        assert_eq!(cancelled.kind(), "cancelled");
        assert!(matches!(cancelled, ContextEngineError::Cancelled));
    }

    /// The published namespace is a concatenation, so a spelling appearing in
    /// both tables would make `kind()` ambiguous about which layer answered.
    #[test]
    fn the_engine_and_domain_kind_tables_are_disjoint_and_unique() {
        let published = ContextEngineError::kinds();
        assert_eq!(
            published.len(),
            ContextEngineError::KINDS.len()
                + ContextDomainError::KINDS.len()
                // Every walk and watch kind but `cancelled`, which the engine's
                // own table already publishes for the same event.
                + crate::InventoryError::KINDS.len()
                - 1
                + crate::watch::WatchError::KINDS.len()
                - 1
                + crate::SearchError::KINDS.len()
                - 1
        );

        let mut sorted = published;
        sorted.sort_unstable();
        let count = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), count, "the two kind tables collide");
    }
}
