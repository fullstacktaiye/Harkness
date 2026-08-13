use std::path::PathBuf;

use rusqlite::ErrorCode;
use thiserror::Error;

use crate::approval::ApprovalError;
use crate::domain::RunDomainError;

use super::MAX_INLINE_PAYLOAD_BYTES;

/// Failures raised by the durable run store.
///
/// Every variant carries a stable [`kind`](StoreError::kind) discriminant so a
/// front end can branch on the failure without matching Rust types, mirroring
/// [`GitError`](harkness_core::GitError) and
/// [`RunDomainError`](crate::domain::RunDomainError).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StoreError {
    /// The platform exposed no data directory and no override was set.
    #[error("no Harkness data directory is available for the run store")]
    DataDirectoryUnavailable,

    /// The database file could not be created, opened, or prepared.
    #[error("failed to open the run store at {}: {source}", .path.display())]
    Open {
        /// Database path that could not be opened.
        path: PathBuf,
        /// Underlying SQLite or filesystem failure.
        #[source]
        source: OpenFailure,
    },

    /// A writer held the database past the busy timeout.
    #[error("the run store stayed busy during {operation}")]
    Busy {
        /// Statement or transaction that could not acquire the database.
        operation: &'static str,
    },

    /// A checkpoint left frames in the write-ahead log.
    ///
    /// The database is intact; only the assumption that `runtime.db` now holds
    /// every commit is not. Retry once the reader holding the log has finished,
    /// or copy all three WAL files together instead.
    #[error(
        "the write-ahead log was not fully checkpointed: {checkpointed_frames} of {log_frames} frames were folded in; copy runtime.db, runtime.db-wal and runtime.db-shm together, or retry once other connections are idle"
    )]
    IncompleteCheckpoint {
        /// Frames the log held when the checkpoint gave up, or `-1` when
        /// another connection held the log outright.
        log_frames: i64,
        /// Frames actually folded into the database file.
        checkpointed_frames: i64,
    },

    /// The database was written by a newer build of Harkness.
    #[error(
        "run store schema version {found} is newer than the maximum supported version {maximum}; upgrade Harkness to read it"
    )]
    SchemaTooNew {
        /// Version recorded in `PRAGMA user_version`.
        found: i64,
        /// Newest schema this build can apply.
        maximum: i64,
    },

    /// A migration failed and was rolled back.
    #[error("run store migration {version} failed: {source}")]
    Migration {
        /// Migration that failed to apply.
        version: i64,
        /// Underlying SQLite failure.
        #[source]
        source: rusqlite::Error,
    },

    /// The requested record is absent.
    #[error("no {record} is stored with id {id}")]
    NotFound {
        /// Kind of record that was requested.
        record: &'static str,
        /// Identifier that matched no row.
        id: String,
    },

    /// A record with the same identity is already stored.
    #[error("a {record} with id {id} is already stored")]
    AlreadyExists {
        /// Kind of record being inserted.
        record: &'static str,
        /// Identifier that already exists.
        id: String,
    },

    /// A record referenced a container that is not stored.
    #[error("{record} {id} references a {parent} that is not stored")]
    MissingParent {
        /// Kind of record being inserted.
        record: &'static str,
        /// Identifier of the record being inserted.
        id: String,
        /// Kind of container the record referenced.
        parent: &'static str,
    },

    /// A step reused an ordinal already taken within its run.
    #[error("run {run_id} already contains a step with ordinal {ordinal}")]
    DuplicateStepOrdinal {
        /// Run whose ordinal space was violated.
        run_id: String,
        /// Ordinal that is already taken.
        ordinal: u32,
    },

    /// Caller data for one column exceeded [`MAX_INLINE_PAYLOAD_BYTES`].
    ///
    /// The remedy depends on the column, so the message names both: a tool
    /// payload belongs in the artifact store, while a title, a failure message,
    /// or an approval identity is simply too long and should be shortened.
    #[error(
        "{record}.{field} is {bytes} bytes, above the {MAX_INLINE_PAYLOAD_BYTES} byte inline limit; shorten it, or store large content as an artifact instead"
    )]
    PayloadTooLarge {
        /// Kind of record being persisted.
        record: &'static str,
        /// Column that would have held the payload.
        field: &'static str,
        /// Encoded size of the refused payload.
        bytes: usize,
    },

    /// An artifact's content could not be written, synced, renamed, or read.
    ///
    /// On a *write*, the file and its metadata row are recorded in that order —
    /// file first — so this failure means no row was written and nothing refers
    /// to whatever bytes reached the disk.
    ///
    /// On a *read* it means the opposite: the row is there and its content is
    /// not, because something outside Harkness removed or replaced it. The
    /// metadata still loads, reporting
    /// [`Availability::Missing`](crate::store::Availability::Missing); only
    /// asking for the bytes fails.
    #[error("the run store failed while {operation} at {}: {source}", .path.display())]
    ArtifactIo {
        /// What the store was doing to the file.
        operation: &'static str,
        /// File the operation was performed on.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },

    /// A stored artifact row named a location outside its reserved place.
    ///
    /// An artifact's path is derivable from its run and its own identity, so a
    /// row disagreeing with that derivation was edited outside Harkness.
    /// Refusing it is what stops a tampered row from making the store read or
    /// overwrite an arbitrary file.
    ///
    /// The run itself stays readable: loading it and paging its events resolve
    /// no artifact path at all, and every other artifact still reads on its own.
    /// A *listing* of the run's artifacts does fail, deliberately — a row that
    /// cannot be true is reported rather than quietly dropped from a list the
    /// caller would then read as complete.
    #[error(
        "artifact {id} is recorded at {path}, which is not the location reserved for it; refusing to resolve it"
    )]
    ForbiddenArtifactPath {
        /// Artifact whose row names the wrong place.
        id: String,
        /// Path the row named.
        path: String,
    },

    /// A stored path is not valid UTF-8 on this platform.
    #[error("{record}.{field} is not valid UTF-8 and cannot be stored")]
    NonUtf8Path {
        /// Kind of record being persisted.
        record: &'static str,
        /// Field holding the unrepresentable path.
        field: &'static str,
    },

    /// A requested lifecycle change is absent from the domain transition table.
    #[error(transparent)]
    InvalidTransition(RunDomainError),

    /// An approval rule refused the change.
    ///
    /// Carried whole rather than flattened, because the specific refusal is what
    /// a surface has to render: "somebody already answered this" and "that scope
    /// is broader than the request allows" are the same failed write and very
    /// different things to tell a user. Branch on
    /// [`ApprovalError::kind`](crate::approval::ApprovalError::kind) for those;
    /// `approval_refused` is the discriminant the store namespace publishes.
    #[error(transparent)]
    Approval(#[from] ApprovalError),

    /// A stored row could not be decoded into a valid domain record.
    #[error("a stored {record} row is not a valid record: {source}")]
    InvalidRecord {
        /// Kind of record being decoded.
        record: &'static str,
        /// Domain rule the row violated.
        #[source]
        source: RunDomainError,
    },

    /// A column value could not be encoded to or decoded from its stored form.
    #[error("{record}.{field} cannot be exchanged with the run store: {reason}")]
    ColumnEncoding {
        /// Kind of record being exchanged.
        record: &'static str,
        /// Column that could not be represented.
        field: &'static str,
        /// Stable human-readable explanation.
        reason: String,
    },

    /// A page asked for no runs at all, or for more than one query may return.
    #[error("a run page of {limit} is outside the supported range 1..={maximum}")]
    InvalidPageLimit {
        /// Limit the caller requested.
        limit: usize,
        /// Largest page the store assembles in one query.
        maximum: usize,
    },

    /// A statement failed for a reason the store does not model.
    #[error("the run store failed during {operation}: {source}")]
    Query {
        /// Statement or transaction that failed.
        operation: &'static str,
        /// Underlying SQLite failure.
        #[source]
        source: rusqlite::Error,
    },
}

/// Why [`StoreError::Open`] could not prepare the database.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OpenFailure {
    /// The data directory could not be created.
    #[error("the data directory could not be created: {0}")]
    DataDirectory(#[source] std::io::Error),

    /// SQLite refused to open the file or apply a connection pragma.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),

    /// SQLite accepted `journal_mode=WAL` but did not enter it.
    #[error("the connection reports journal mode {mode} instead of wal")]
    JournalMode {
        /// Journal mode the connection actually reports.
        mode: String,
    },
}

impl StoreError {
    /// Every stable discriminant this error namespace can emit.
    pub const KINDS: &'static [&'static str] = &[
        "data_directory_unavailable",
        "store_open",
        "store_busy",
        "incomplete_checkpoint",
        "schema_too_new",
        "migration_failed",
        "not_found",
        "already_exists",
        "missing_parent",
        "duplicate_step_ordinal",
        "payload_too_large",
        "artifact_io",
        "forbidden_artifact_path",
        "non_utf8_path",
        "invalid_transition",
        "approval_refused",
        "invalid_record",
        "column_encoding",
        "invalid_page_limit",
        "query_failed",
    ];

    /// Stable machine-readable discriminant for caller-facing error handling.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::DataDirectoryUnavailable => "data_directory_unavailable",
            Self::Open { .. } => "store_open",
            Self::Busy { .. } => "store_busy",
            Self::IncompleteCheckpoint { .. } => "incomplete_checkpoint",
            Self::SchemaTooNew { .. } => "schema_too_new",
            Self::Migration { .. } => "migration_failed",
            Self::NotFound { .. } => "not_found",
            Self::AlreadyExists { .. } => "already_exists",
            Self::MissingParent { .. } => "missing_parent",
            Self::DuplicateStepOrdinal { .. } => "duplicate_step_ordinal",
            Self::PayloadTooLarge { .. } => "payload_too_large",
            Self::ArtifactIo { .. } => "artifact_io",
            Self::ForbiddenArtifactPath { .. } => "forbidden_artifact_path",
            Self::NonUtf8Path { .. } => "non_utf8_path",
            Self::InvalidTransition(_) => "invalid_transition",
            Self::Approval(_) => "approval_refused",
            Self::InvalidRecord { .. } => "invalid_record",
            Self::ColumnEncoding { .. } => "column_encoding",
            Self::InvalidPageLimit { .. } => "invalid_page_limit",
            Self::Query { .. } => "query_failed",
        }
    }
}

/// Whether SQLite gave up waiting for another connection's lock.
///
/// The busy timeout has already elapsed by the time either code reaches the
/// caller, so both spellings mean the same thing to a front end: retry later.
pub(super) fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

/// Classifies a statement failure that is not tied to one record's identity.
pub(super) fn query_failed(operation: &'static str, error: rusqlite::Error) -> StoreError {
    if is_busy(&error) {
        return StoreError::Busy { operation };
    }
    StoreError::Query {
        operation,
        source: error,
    }
}

/// Names the container a record referenced.
#[derive(Clone, Copy, Debug)]
pub(super) struct Containment {
    /// Kind of record being written.
    pub(super) record: &'static str,
    /// Kind of container a foreign key points at.
    pub(super) parent: &'static str,
}

/// Classifies an insert failure against the identity being written.
///
/// SQLite reports a rejected foreign key and a duplicate primary key with the
/// same primary code, so the extended code decides which invariant the caller
/// actually broke.
pub(super) fn insert_failed(
    containment: Containment,
    id: &dyn std::fmt::Display,
    operation: &'static str,
    error: rusqlite::Error,
) -> StoreError {
    if is_busy(&error) {
        return StoreError::Busy { operation };
    }
    if let rusqlite::Error::SqliteFailure(failure, _) = &error {
        match failure.extended_code {
            rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY => {
                return StoreError::MissingParent {
                    record: containment.record,
                    id: id.to_string(),
                    parent: containment.parent,
                };
            }
            rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY => {
                return StoreError::AlreadyExists {
                    record: containment.record,
                    id: id.to_string(),
                };
            }
            _ => {}
        }
    }
    StoreError::Query {
        operation,
        source: error,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::approval::ApprovalError;
    use crate::domain::{ExecutionState, InvalidTransition, RunDomainError};

    use super::{OpenFailure, StoreError};

    #[test]
    fn store_error_kinds_round_trip_through_the_kinds_table() {
        let cases = [
            (
                StoreError::DataDirectoryUnavailable,
                "data_directory_unavailable",
            ),
            (
                StoreError::Open {
                    path: PathBuf::from("runtime.db"),
                    source: OpenFailure::JournalMode {
                        mode: "delete".to_owned(),
                    },
                },
                "store_open",
            ),
            (
                StoreError::Busy {
                    operation: "insert run",
                },
                "store_busy",
            ),
            (
                StoreError::IncompleteCheckpoint {
                    log_frames: 12,
                    checkpointed_frames: 4,
                },
                "incomplete_checkpoint",
            ),
            (
                StoreError::SchemaTooNew {
                    found: 2,
                    maximum: 1,
                },
                "schema_too_new",
            ),
            (
                StoreError::Migration {
                    version: 1,
                    source: rusqlite::Error::ExecuteReturnedResults,
                },
                "migration_failed",
            ),
            (
                StoreError::NotFound {
                    record: "run",
                    id: "id".to_owned(),
                },
                "not_found",
            ),
            (
                StoreError::AlreadyExists {
                    record: "run",
                    id: "id".to_owned(),
                },
                "already_exists",
            ),
            (
                StoreError::MissingParent {
                    record: "run",
                    id: "id".to_owned(),
                    parent: "task",
                },
                "missing_parent",
            ),
            (
                StoreError::DuplicateStepOrdinal {
                    run_id: "id".to_owned(),
                    ordinal: 0,
                },
                "duplicate_step_ordinal",
            ),
            (
                StoreError::PayloadTooLarge {
                    record: "tool_call",
                    field: "input",
                    bytes: 65_537,
                },
                "payload_too_large",
            ),
            (
                StoreError::ArtifactIo {
                    operation: "renaming an artifact",
                    path: PathBuf::from("artifacts/run/artifact"),
                    source: std::io::Error::other("disk full"),
                },
                "artifact_io",
            ),
            (
                StoreError::ForbiddenArtifactPath {
                    id: "artifact".to_owned(),
                    path: "../../.ssh/id_rsa".to_owned(),
                },
                "forbidden_artifact_path",
            ),
            (
                StoreError::NonUtf8Path {
                    record: "task",
                    field: "workspace_root",
                },
                "non_utf8_path",
            ),
            (
                StoreError::InvalidTransition(RunDomainError::InvalidExecutionTransition(
                    InvalidTransition {
                        from: ExecutionState::Queued,
                        to: ExecutionState::Succeeded,
                    },
                )),
                "invalid_transition",
            ),
            (
                StoreError::Approval(ApprovalError::AlreadyResolved {
                    id: crate::domain::ApprovalId::new(),
                    state: crate::approval::ApprovalState::Denied,
                }),
                "approval_refused",
            ),
            (
                StoreError::InvalidRecord {
                    record: "run",
                    source: RunDomainError::InvalidLifecycle {
                        record: "run",
                        reason: "a terminal state requires finished_at",
                    },
                },
                "invalid_record",
            ),
            (
                StoreError::ColumnEncoding {
                    record: "run",
                    field: "created_at",
                    reason: "not an RFC 3339 timestamp".to_owned(),
                },
                "column_encoding",
            ),
            (
                StoreError::InvalidPageLimit {
                    limit: 0,
                    maximum: 500,
                },
                "invalid_page_limit",
            ),
            (
                StoreError::Query {
                    operation: "list runs",
                    source: rusqlite::Error::ExecuteReturnedResults,
                },
                "query_failed",
            ),
        ];

        let kinds = cases.iter().map(|(_, kind)| *kind).collect::<Vec<_>>();
        assert_eq!(kinds, StoreError::KINDS);
        for (error, expected) in cases {
            assert_eq!(error.kind(), expected, "unexpected kind for {error:?}");
        }
    }

    #[test]
    fn the_oversized_payload_message_names_the_threshold() {
        let error = StoreError::PayloadTooLarge {
            record: "tool_call",
            field: "input",
            bytes: 70_000,
        };
        assert_eq!(
            error.to_string(),
            "tool_call.input is 70000 bytes, above the 65536 byte inline limit; \
             shorten it, or store large content as an artifact instead"
        );
    }
}
