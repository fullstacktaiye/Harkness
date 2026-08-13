use thiserror::Error;

use crate::domain::InvalidTransition;

use super::{SubjectKind, TrustState};

/// Invalid trust transitions, malformed identities, and impossible persisted
/// integration records.
///
/// The namespace is separate from [`RunDomainError`](crate::domain::RunDomainError)
/// for the same reason the schema version is: a trust record and a run record
/// evolve independently, and a caller handling one should not have to match on
/// discriminants belonging to the other.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum IntegrationDomainError {
    /// A trust record requested an edge absent from [`TRUST_TRANSITIONS`].
    ///
    /// [`TRUST_TRANSITIONS`]: super::TRUST_TRANSITIONS
    #[error(transparent)]
    InvalidTrustTransition(#[from] InvalidTransition<TrustState>),

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

    /// A hash was not spelled as 64 lowercase hexadecimal characters.
    #[error("{value} is not a SHA-256 digest: {reason}")]
    MalformedDigest {
        /// Value that was refused.
        value: String,
        /// Stable human-readable explanation.
        reason: &'static str,
    },

    /// An identity field violated its grammar or its length bound.
    #[error("identity {field} is invalid: {reason}")]
    InvalidIdentity {
        /// Identity field that violated the invariant.
        field: &'static str,
        /// Stable human-readable explanation.
        reason: &'static str,
    },

    /// A grant was made against an identity carrying nothing its subject kind
    /// could be recognized by.
    ///
    /// Every field of an identity basis is optional, because no subject has all
    /// of them. A basis with *none* of the ones its kind is identified by would
    /// leave [`TrustRecord::check`](super::TrustRecord::check) with nothing to
    /// compare, so it would answer `Valid` for any observation at all — the
    /// exact drift the record exists to catch, passing silently.
    #[error("a {subject_kind} grant requires {required}")]
    MissingIdentityEvidence {
        /// Kind of subject the grant was made about.
        subject_kind: SubjectKind,
        /// Evidence that kind is recognized by.
        required: &'static str,
    },

    /// A wire record combines otherwise valid fields into an impossible record.
    #[error("{record} is invalid: {reason}")]
    InvalidRecord {
        /// Kind of record being decoded.
        record: &'static str,
        /// Stable human-readable explanation.
        reason: &'static str,
    },

    /// A trust timestamp is not a valid UTC timestamp.
    #[error("{record}.{field} is invalid: {reason}")]
    InvalidTimestamp {
        /// Kind of record being decoded.
        record: &'static str,
        /// Timestamp field that violated the invariant.
        field: &'static str,
        /// Stable human-readable explanation.
        reason: &'static str,
    },
}

impl IntegrationDomainError {
    /// Every stable discriminant this error namespace can emit.
    pub const KINDS: &'static [&'static str] = &[
        "invalid_trust_transition",
        "integration_schema_version_too_old",
        "integration_schema_version_too_new",
        "malformed_digest",
        "invalid_identity",
        "missing_identity_evidence",
        "invalid_integration_record",
        "invalid_integration_timestamp",
    ];

    /// Stable machine-readable discriminant for caller-facing error handling.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::InvalidTrustTransition(_) => "invalid_trust_transition",
            Self::SchemaVersionTooOld { .. } => "integration_schema_version_too_old",
            Self::SchemaVersionTooNew { .. } => "integration_schema_version_too_new",
            Self::MalformedDigest { .. } => "malformed_digest",
            Self::InvalidIdentity { .. } => "invalid_identity",
            Self::MissingIdentityEvidence { .. } => "missing_identity_evidence",
            Self::InvalidRecord { .. } => "invalid_integration_record",
            Self::InvalidTimestamp { .. } => "invalid_integration_timestamp",
        }
    }
}

pub(super) const fn invalid_identity(
    field: &'static str,
    reason: &'static str,
) -> IntegrationDomainError {
    IntegrationDomainError::InvalidIdentity { field, reason }
}

pub(super) const fn invalid_record(
    record: &'static str,
    reason: &'static str,
) -> IntegrationDomainError {
    IntegrationDomainError::InvalidRecord { record, reason }
}

#[cfg(test)]
mod tests {
    use super::IntegrationDomainError;
    use crate::domain::InvalidTransition;
    use crate::integration::TrustState;

    #[test]
    fn trust_transition_errors_use_stable_state_spellings() {
        let error = InvalidTransition {
            from: TrustState::Revoked,
            to: TrustState::Trusted,
        };
        assert_eq!(error.to_string(), "state revoked cannot become trusted");
    }

    #[test]
    fn integration_error_kinds_round_trip_through_the_kinds_table() {
        let cases = [
            (
                IntegrationDomainError::InvalidTrustTransition(InvalidTransition {
                    from: TrustState::Untrusted,
                    to: TrustState::Revoked,
                }),
                "invalid_trust_transition",
            ),
            (
                IntegrationDomainError::SchemaVersionTooOld {
                    record: "trust_record",
                    found: 0,
                    minimum: 1,
                },
                "integration_schema_version_too_old",
            ),
            (
                IntegrationDomainError::SchemaVersionTooNew {
                    record: "trust_record",
                    found: 2,
                    maximum: 1,
                },
                "integration_schema_version_too_new",
            ),
            (
                IntegrationDomainError::MalformedDigest {
                    value: "abc".to_owned(),
                    reason: "it is not 64 hexadecimal characters long",
                },
                "malformed_digest",
            ),
            (
                IntegrationDomainError::InvalidIdentity {
                    field: "display_name",
                    reason: "it cannot be empty",
                },
                "invalid_identity",
            ),
            (
                IntegrationDomainError::MissingIdentityEvidence {
                    subject_kind: crate::integration::SubjectKind::Recipe,
                    required: "a recipe content hash",
                },
                "missing_identity_evidence",
            ),
            (
                IntegrationDomainError::InvalidRecord {
                    record: "trust_record",
                    reason: "an untrusted subject has no record",
                },
                "invalid_integration_record",
            ),
            (
                IntegrationDomainError::InvalidTimestamp {
                    record: "trust_record",
                    field: "granted_at",
                    reason: "must use the UTC offset",
                },
                "invalid_integration_timestamp",
            ),
        ];

        let kinds = cases.iter().map(|(_, kind)| *kind).collect::<Vec<_>>();
        assert_eq!(kinds, IntegrationDomainError::KINDS);
        for (error, expected) in cases {
            assert_eq!(error.kind(), expected, "unexpected kind for {error:?}");
        }
    }

    #[test]
    fn integration_kinds_do_not_collide_with_the_run_domain_namespace() {
        for kind in IntegrationDomainError::KINDS {
            assert!(
                !crate::domain::RunDomainError::KINDS.contains(kind),
                "{kind} is declared by both error namespaces"
            );
        }
    }
}
