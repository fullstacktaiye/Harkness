//! Statement-level persistence for workspace snapshots.
//!
//! A snapshot is evidence. It says exactly which workspace a run read — the
//! repository, the worktree, `HEAD`, the branch, the staged, dirty and
//! untracked path sets and their digests, and the configuration and index
//! generations it was taken under — and every context item audited later points
//! at one. ADR-0004 puts it here rather than in the disposable index cache for
//! that reason: deleting `<data_dir>/context/` must cost warm-up time and never
//! an audit trail.
//!
//! # Two version ladders in one row
//!
//! `schema_version` is the *runtime's*, exactly as every other table's is, and
//! it describes the envelope: the identity, the run association, and the
//! denormalized columns. The document in `payload_json` carries
//! `harkness-context`'s own `schema_version` inside it and is probed against
//! that crate's ladder when it is decoded. The two are independent on purpose —
//! a context record and a run record change for different reasons — and a
//! payload from a newer build therefore reads as an upgrade request rather than
//! as a corrupt column.
//!
//! # The envelope is derivable and stored anyway
//!
//! `id`, `project_id`, `snapshot_digest` and `captured_at` all appear inside
//! `payload_json`. They are lifted out so the table is legible from the database
//! alone and so a run's captures can be found without parsing every document —
//! the same trade an artifact's `storage_path` makes. And, as there, the stored
//! copies are *compared* against the payload on every read and a row where they
//! disagree is refused by name, so a hand-edited column cannot make a snapshot
//! answer for a workspace its own contents do not describe.
//!
//! # Nothing here is redacted
//!
//! The payload is bound by a digest that `harkness-context` re-derives on load.
//! Rewriting a path inside it would move that digest and refuse the very row the
//! rewrite was meant to protect, so redaction would not make the record safer,
//! it would make it unreadable. A snapshot holds hashes and paths and never file
//! contents, which is what makes that acceptable; the `snapshot_captured` event
//! that announces one goes through the store's redactor like every other event.

use harkness_context::{
    ContextDomainError, SnapshotDigest, SnapshotId, SnapshotWire, SnapshotWireRef,
    WorkspaceSnapshot, validate_record_schema_version,
};
use harkness_core::ProjectId;
use rusqlite::{Connection, OptionalExtension, named_params};
use serde_json::{Value, json};

use crate::domain::{RUNTIME_RECORD_SCHEMA_VERSION, RunId};

use super::column::{decode_id, decode_timestamp, encode_text, encode_timestamp};
use super::error::{Containment, StoreError, insert_failed, query_failed};
use super::event::{EventKind, RunEvent};
use super::repository::{optional_text, schema_version, text};

const SNAPSHOT: &str = "workspace_snapshot";

const SNAPSHOT_COLUMNS: &str = "schema_version, id, run_id, project_id, snapshot_digest, \
     payload_json, captured_at";

/// A workspace snapshot as it was recorded, with the run it belongs to.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct StoredSnapshot {
    /// Run the capture was recorded for; `None` for a standalone capture.
    pub run_id: Option<RunId>,
    /// The capture itself, re-validated on load.
    pub snapshot: WorkspaceSnapshot,
}

/// The event that announces a capture becoming durable evidence.
///
/// Derived from the record rather than supplied, so a caller cannot record a
/// snapshot under an event that describes a different one. The payload names
/// the capture and the workspace and nothing else: the run is already a column
/// on the event, and everything else is one read of the row away.
pub(super) fn captured_event(snapshot: &WorkspaceSnapshot) -> RunEvent {
    RunEvent::new(EventKind::SnapshotCaptured, snapshot.captured_at()).with_payload(json!({
        "snapshot_id": snapshot.id().to_string(),
        "snapshot_digest": snapshot.digest().to_string(),
    }))
}

pub(super) fn insert(
    connection: &Connection,
    run_id: Option<RunId>,
    snapshot: &WorkspaceSnapshot,
) -> Result<(), StoreError> {
    let payload = serde_json::to_string(&SnapshotWireRef::from(snapshot)).map_err(|error| {
        StoreError::ColumnEncoding {
            record: SNAPSHOT,
            field: "payload_json",
            reason: error.to_string(),
        }
    })?;
    let digest = snapshot.digest().to_string();
    connection
        .execute(
            &format!(
                "INSERT INTO workspace_snapshots ({SNAPSHOT_COLUMNS}) \
                 VALUES (:schema_version, :id, :run_id, :project_id, :snapshot_digest, \
                 :payload_json, :captured_at)"
            ),
            named_params! {
                ":schema_version": RUNTIME_RECORD_SCHEMA_VERSION,
                ":id": snapshot.id().to_string(),
                ":run_id": run_id.map(|run_id| run_id.to_string()),
                ":project_id": snapshot.project_id().to_string(),
                ":snapshot_digest": encode_text(SNAPSHOT, "snapshot_digest", &digest)?,
                // Held to the same inline bound every other caller-controlled
                // column keeps. A workspace with tens of thousands of dirty
                // paths is refused rather than truncated: a snapshot that
                // recorded only some of them would claim an identity the
                // workspace never had.
                ":payload_json": encode_text(SNAPSHOT, "payload_json", &payload)?,
                ":captured_at": encode_timestamp(SNAPSHOT, "captured_at", snapshot.captured_at())?,
            },
        )
        .map(|_| ())
        .map_err(|error| {
            insert_failed(
                Containment {
                    record: SNAPSHOT,
                    parent: "run",
                },
                &snapshot.id(),
                "recording a workspace snapshot",
                error,
            )
        })
}

pub(super) fn load(
    connection: &Connection,
    id: SnapshotId,
) -> Result<Option<StoredSnapshot>, StoreError> {
    let mut statement = connection
        .prepare_cached(&format!(
            "SELECT {SNAPSHOT_COLUMNS} FROM workspace_snapshots WHERE id = :id"
        ))
        .map_err(|error| query_failed("preparing the workspace snapshot query", error))?;
    let stored = statement
        .query_row(named_params! { ":id": id.to_string() }, |row| {
            Ok(decode(row))
        })
        .optional()
        .map_err(|error| query_failed("loading a workspace snapshot", error))?;
    stored.transpose()
}

pub(super) fn for_run(
    connection: &Connection,
    run_id: RunId,
) -> Result<Vec<StoredSnapshot>, StoreError> {
    let mut statement = connection
        .prepare_cached(&format!(
            "SELECT {SNAPSHOT_COLUMNS} FROM workspace_snapshots WHERE run_id = :run_id \
             ORDER BY captured_at, id"
        ))
        .map_err(|error| query_failed("preparing the run snapshot query", error))?;
    let rows = statement
        .query_map(named_params! { ":run_id": run_id.to_string() }, |row| {
            Ok(decode(row))
        })
        .map_err(|error| query_failed("listing a run's workspace snapshots", error))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| query_failed("reading a run's workspace snapshots", error))?
        .into_iter()
        .collect()
}

fn decode(row: &rusqlite::Row<'_>) -> Result<StoredSnapshot, StoreError> {
    // Probe first. A future row may use columns this build cannot decode and
    // must be reported as an upgrade request rather than as a corrupt column.
    schema_version(row, SNAPSHOT)?;
    let run_id = optional_text(row, SNAPSHOT, "run_id")?
        .map(|stored| decode_id::<RunId>(SNAPSHOT, "run_id", &stored))
        .transpose()?;

    let payload = text(row, SNAPSHOT, "payload_json")?;
    let document: Value = serde_json::from_str(&payload).map_err(payload_encoding)?;
    // Probed before the strict body is parsed, exactly as every other durable
    // record's version is. Letting the strict decode raise it instead would
    // fold "this build is too old" into "this column is malformed", and the two
    // ask a user for opposite things.
    let found = document
        .get("schema_version")
        .and_then(Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .ok_or_else(|| StoreError::ColumnEncoding {
            record: SNAPSHOT,
            field: "payload_json",
            reason: "the payload states no readable schema_version".to_owned(),
        })?;
    validate_record_schema_version(SNAPSHOT, found).map_err(context_record)?;
    let wire: SnapshotWire = serde_json::from_value(document).map_err(payload_encoding)?;
    // The context crate re-derives all three content digests plus the composite
    // from the entry lists and refuses a document whose claimed identity its
    // own contents do not support.
    let snapshot = WorkspaceSnapshot::try_from(wire).map_err(context_record)?;

    // The denormalized columns are compared, never trusted. A row whose
    // `snapshot_digest` was edited would otherwise let a search by workspace
    // identity return a capture of a different workspace.
    let id = decode_id::<SnapshotId>(SNAPSHOT, "id", &text(row, SNAPSHOT, "id")?)?;
    require(SNAPSHOT, "id", id == snapshot.id())?;
    let project_id =
        decode_id::<ProjectId>(SNAPSHOT, "project_id", &text(row, SNAPSHOT, "project_id")?)?;
    require(SNAPSHOT, "project_id", project_id == snapshot.project_id())?;
    let digest = decode_id::<SnapshotDigest>(
        SNAPSHOT,
        "snapshot_digest",
        &text(row, SNAPSHOT, "snapshot_digest")?,
    )?;
    require(SNAPSHOT, "snapshot_digest", digest == snapshot.digest())?;
    let captured_at = decode_timestamp(
        SNAPSHOT,
        "captured_at",
        &text(row, SNAPSHOT, "captured_at")?,
    )?;
    require(
        SNAPSHOT,
        "captured_at",
        captured_at == snapshot.captured_at(),
    )?;

    Ok(StoredSnapshot { run_id, snapshot })
}

/// Refuses a denormalized column that disagrees with the payload it came from.
fn require(record: &'static str, field: &'static str, agrees: bool) -> Result<(), StoreError> {
    if agrees {
        return Ok(());
    }
    Err(StoreError::ColumnEncoding {
        record,
        field,
        reason: "the stored column disagrees with the payload it was derived from".to_owned(),
    })
}

fn context_record(source: ContextDomainError) -> StoreError {
    StoreError::InvalidContextRecord {
        record: SNAPSHOT,
        source,
    }
}

fn payload_encoding(error: serde_json::Error) -> StoreError {
    StoreError::ColumnEncoding {
        record: SNAPSHOT,
        field: "payload_json",
        reason: error.to_string(),
    }
}
