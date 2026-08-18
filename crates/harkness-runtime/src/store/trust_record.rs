//! Statement-level persistence for per-subject trust records.
//!
//! One row per grant, addressed by its own row identity and never by a
//! projection of the record's fields. `TrustRecord::check` ignores the display
//! name and the executable path and accepts a semver-compatible upgrade, so
//! equality over those fields is a compatibility relation rather than a key —
//! and a revoked record and the later grant that replaced it would collide on
//! one, letting an upsert overwrite the single decision the state machine exists
//! to preserve.

use rusqlite::{Connection, OptionalExtension, named_params};
use time::OffsetDateTime;

use crate::domain::RUNTIME_RECORD_SCHEMA_VERSION;
use crate::integration::{SubjectKind, TrustRecord, TrustRecordId, TrustRecordWire};

use super::column::{decode_id, decode_timestamp, encode_timestamp};
use super::error::{Containment, StoreError, insert_failed, query_failed};
use super::repository::{missing_row, schema_version, text};

const RECORD: &str = "integration_trust_record";

const COLUMNS: &str = "schema_version, id, subject_kind, subject_ref, record_json, recorded_at";

/// One stored trust grant, with the row identity a later transition names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredTrustRecord {
    id: TrustRecordId,
    subject_kind: SubjectKind,
    subject_ref: String,
    record: TrustRecord,
    recorded_at: OffsetDateTime,
}

impl StoredTrustRecord {
    /// The row's identity, which is what a transition addresses.
    #[must_use]
    pub const fn id(&self) -> TrustRecordId {
        self.id
    }

    /// Which vocabulary [`subject_ref`](Self::subject_ref) is spelled in.
    #[must_use]
    pub const fn subject_kind(&self) -> SubjectKind {
        self.subject_kind
    }

    /// The subject this grant was made *for*.
    #[must_use]
    pub fn subject_ref(&self) -> &str {
        &self.subject_ref
    }

    /// The grant itself.
    #[must_use]
    pub const fn record(&self) -> &TrustRecord {
        &self.record
    }

    /// The grant itself, taken by value.
    #[must_use]
    pub fn into_record(self) -> TrustRecord {
        self.record
    }

    /// When this row was written, which is not when the grant was made.
    #[must_use]
    pub const fn recorded_at(&self) -> OffsetDateTime {
        self.recorded_at
    }
}

pub(super) fn insert(
    connection: &Connection,
    id: TrustRecordId,
    subject_kind: SubjectKind,
    subject_ref: &str,
    record: &TrustRecord,
    recorded_at: OffsetDateTime,
) -> Result<(), StoreError> {
    let encoded = encode(record)?;
    super::column::within_inline_limit(RECORD, "subject_ref", subject_ref.len())?;
    super::column::within_inline_limit(RECORD, "record_json", encoded.len())?;
    connection
        .execute(
            "INSERT INTO integration_trust_records \
             (schema_version, id, subject_kind, subject_ref, record_json, recorded_at) \
             VALUES (:schema_version, :id, :subject_kind, :subject_ref, :record_json, \
             :recorded_at)",
            named_params! {
                ":schema_version": RUNTIME_RECORD_SCHEMA_VERSION,
                ":id": id.to_string(),
                ":subject_kind": subject_kind.as_str(),
                ":subject_ref": subject_ref,
                ":record_json": encoded,
                ":recorded_at": encode_timestamp(RECORD, "recorded_at", recorded_at)?,
            },
        )
        .map(|_| ())
        .map_err(|error| {
            insert_failed(
                Containment {
                    record: RECORD,
                    // No foreign key: a trust record is about a subject the
                    // database does not hold a row for, so the only constraint
                    // an insert can break is its own primary key.
                    parent: "none",
                },
                &id,
                "recording an integration trust grant",
                error,
            )
        })
}

/// Rewrites the record one row holds, leaving its identity and subject alone.
///
/// This is how a revocation, an invalidation and a re-grant are stored: all
/// three are transitions of one record the user already made, and writing a new
/// row for each would make "the most recent record about this subject" a
/// question with two answers.
pub(super) fn update(
    connection: &Connection,
    id: TrustRecordId,
    record: &TrustRecord,
) -> Result<(), StoreError> {
    let encoded = encode(record)?;
    super::column::within_inline_limit(RECORD, "record_json", encoded.len())?;
    let updated = connection
        .execute(
            "UPDATE integration_trust_records SET schema_version = :schema_version, \
             record_json = :record_json WHERE id = :id",
            named_params! {
                ":schema_version": RUNTIME_RECORD_SCHEMA_VERSION,
                ":record_json": encoded,
                ":id": id.to_string(),
            },
        )
        .map_err(|error| query_failed("updating an integration trust grant", error))?;
    missing_row(RECORD, &id, updated)
}

/// How the records about one subject are ordered, oldest first.
///
/// `rowid` is the tiebreak rather than `id`, and the difference is load-bearing:
/// `id` is a random UUID, so two rows written in the same instant would order by
/// coin flip and "the most recent record about this subject" would have two
/// answers. `rowid` is insertion order, which is the thing actually being asked
/// about. Reversing this ordering is what
/// [`latest_for_subject`](latest_for_subject) does, so the two must stay one
/// definition.
const ORDER: &str = "recorded_at, rowid";

/// Every record about one subject, oldest first.
pub(super) fn for_subject(
    connection: &Connection,
    subject_kind: SubjectKind,
    subject_ref: &str,
) -> Result<Vec<StoredTrustRecord>, StoreError> {
    let mut statement = connection
        .prepare_cached(&format!(
            "SELECT {COLUMNS} FROM integration_trust_records \
             WHERE subject_kind = :subject_kind AND subject_ref = :subject_ref \
             ORDER BY {ORDER}"
        ))
        .map_err(|error| query_failed("preparing the trust record query", error))?;
    let rows = statement
        .query_map(
            named_params! {
                ":subject_kind": subject_kind.as_str(),
                ":subject_ref": subject_ref,
            },
            |row| Ok(decode(row)),
        )
        .map_err(|error| query_failed("listing trust records", error))?;
    let mut records = Vec::new();
    for row in rows {
        records.push(row.map_err(|error| query_failed("reading a trust record", error))??);
    }
    Ok(records)
}

/// The most recently recorded grant about one subject.
///
/// One indexed seek rather than the whole history. Deciding this by loading
/// every record and dropping all but the last is the query a launch runs, and a
/// subject re-trusted weekly for a year would make it decode and re-validate
/// fifty grants to answer one question.
pub(super) fn latest_for_subject(
    connection: &Connection,
    subject_kind: SubjectKind,
    subject_ref: &str,
) -> Result<Option<StoredTrustRecord>, StoreError> {
    let mut statement = connection
        .prepare_cached(&format!(
            "SELECT {COLUMNS} FROM integration_trust_records \
             WHERE subject_kind = :subject_kind AND subject_ref = :subject_ref \
             ORDER BY recorded_at DESC, rowid DESC LIMIT 1"
        ))
        .map_err(|error| query_failed("preparing the latest trust record query", error))?;
    let stored = statement
        .query_row(
            named_params! {
                ":subject_kind": subject_kind.as_str(),
                ":subject_ref": subject_ref,
            },
            |row| Ok(decode(row)),
        )
        .optional()
        .map_err(|error| query_failed("loading the latest trust record", error))?;
    stored.transpose()
}

pub(super) fn delete_for_subject(
    connection: &Connection,
    subject_kind: SubjectKind,
    subject_ref: &str,
) -> Result<usize, StoreError> {
    connection
        .execute(
            "DELETE FROM integration_trust_records \
             WHERE subject_kind = :subject_kind AND subject_ref = :subject_ref",
            named_params! {
                ":subject_kind": subject_kind.as_str(),
                ":subject_ref": subject_ref,
            },
        )
        .map_err(|error| query_failed("removing trust records", error))
}

fn encode(record: &TrustRecord) -> Result<String, StoreError> {
    serde_json::to_string(&crate::integration::TrustRecordWireRef::from(record)).map_err(|error| {
        StoreError::ColumnEncoding {
            record: RECORD,
            field: "record_json",
            reason: error.to_string(),
        }
    })
}

fn decode(row: &rusqlite::Row<'_>) -> Result<StoredTrustRecord, StoreError> {
    // Probe first. A future row may spell a state or a subject kind this build
    // cannot decode, and must read as an upgrade request rather than as a
    // corrupt column.
    schema_version(row, RECORD)?;
    let id = decode_id(RECORD, "id", &text(row, RECORD, "id")?)?;
    let stored_kind = text(row, RECORD, "subject_kind")?;
    let subject_kind =
        SubjectKind::from_stored(&stored_kind).ok_or_else(|| StoreError::ColumnEncoding {
            record: RECORD,
            field: "subject_kind",
            reason: format!("{stored_kind:?} is not a known subject kind"),
        })?;
    let subject_ref = text(row, RECORD, "subject_ref")?;
    let encoded = text(row, RECORD, "record_json")?;
    let wire: TrustRecordWire =
        serde_json::from_str(&encoded).map_err(|error| StoreError::ColumnEncoding {
            record: RECORD,
            field: "record_json",
            reason: error.to_string(),
        })?;
    // The record is rebuilt through the domain rather than trusted as decoded,
    // so a hand-edited row fails to load instead of entering the process as an
    // impossible grant. The refusal is a column failure rather than
    // `InvalidRecord`, which is typed to the *run* domain's namespace: the two
    // are deliberately separate, and widening one to carry the other's errors
    // would undo that.
    let record = TrustRecord::try_from(wire).map_err(|source| StoreError::ColumnEncoding {
        record: RECORD,
        field: "record_json",
        reason: source.to_string(),
    })?;
    // The denormalized kind is what the index is built on, so it has to agree
    // with the record it points at; a row where the two disagree would be found
    // by one lookup and answer for another subject entirely.
    if record.subject_kind() != subject_kind {
        return Err(StoreError::ColumnEncoding {
            record: RECORD,
            field: "subject_kind",
            reason: format!(
                "the column says {subject_kind} and the record says {}",
                record.subject_kind()
            ),
        });
    }
    let recorded_at = decode_timestamp(RECORD, "recorded_at", &text(row, RECORD, "recorded_at")?)?;
    Ok(StoredTrustRecord {
        id,
        subject_kind,
        subject_ref,
        record,
        recorded_at,
    })
}
