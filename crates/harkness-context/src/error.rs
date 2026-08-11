//! The one error namespace this crate raises.

use std::path::PathBuf;

use thiserror::Error;

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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::ContextDomainError;

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
}
