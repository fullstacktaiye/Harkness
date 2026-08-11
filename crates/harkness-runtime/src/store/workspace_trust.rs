//! Statement-level persistence for exact workspace trust decisions.

use std::path::PathBuf;

use harkness_core::ProjectId;
use rusqlite::{Connection, OptionalExtension, named_params};

use crate::domain::RUNTIME_RECORD_SCHEMA_VERSION;
use crate::trust::{TrustState, WorkspaceTrust};

use super::column::{decode_id, decode_timestamp, encode_path, encode_timestamp};
use super::error::{StoreError, query_failed};
use super::repository::{schema_version, text};

const TRUST: &str = "workspace_trust";

pub(super) fn put(connection: &Connection, trust: &WorkspaceTrust) -> Result<(), StoreError> {
    connection
        .execute(
            "INSERT INTO workspace_trust \
             (schema_version, project_id, canonical_root, state, decided_at) \
             VALUES (:schema_version, :project_id, :canonical_root, :state, :decided_at) \
             ON CONFLICT(project_id) DO UPDATE SET \
             schema_version = excluded.schema_version, \
             canonical_root = excluded.canonical_root, \
             state = excluded.state, \
             decided_at = excluded.decided_at",
            named_params! {
                ":schema_version": RUNTIME_RECORD_SCHEMA_VERSION,
                ":project_id": trust.project_id().to_string(),
                ":canonical_root": encode_path(TRUST, "canonical_root", trust.canonical_root())?,
                ":state": trust.state().as_str(),
                ":decided_at": encode_timestamp(TRUST, "decided_at", trust.decided_at())?,
            },
        )
        .map(|_| ())
        .map_err(|error| query_failed("recording workspace trust", error))
}

pub(super) fn load(
    connection: &Connection,
    project_id: ProjectId,
) -> Result<Option<WorkspaceTrust>, StoreError> {
    let mut statement = connection
        .prepare_cached(
            "SELECT schema_version, project_id, canonical_root, state, decided_at \
             FROM workspace_trust WHERE project_id = :project_id",
        )
        .map_err(|error| query_failed("preparing the workspace trust query", error))?;
    let stored = statement
        .query_row(
            named_params! { ":project_id": project_id.to_string() },
            |row| Ok(decode(row)),
        )
        .optional()
        .map_err(|error| query_failed("loading workspace trust", error))?;
    stored.transpose()
}

fn decode(row: &rusqlite::Row<'_>) -> Result<WorkspaceTrust, StoreError> {
    // Probe first. A future row may use state or column spellings this build
    // cannot decode, and must be reported as an upgrade request instead.
    schema_version(row, TRUST)?;
    let project_id = decode_id(TRUST, "project_id", &text(row, TRUST, "project_id")?)?;
    let canonical_root = PathBuf::from(text(row, TRUST, "canonical_root")?);
    if !canonical_root.is_absolute() {
        return Err(StoreError::ColumnEncoding {
            record: TRUST,
            field: "canonical_root",
            reason: "a stored canonical root must be absolute".to_owned(),
        });
    }
    let stored_state = text(row, TRUST, "state")?;
    let state =
        TrustState::from_stored(&stored_state).ok_or_else(|| StoreError::ColumnEncoding {
            record: TRUST,
            field: "state",
            reason: format!("{stored_state:?} is not a known trust state"),
        })?;
    let decided_at = decode_timestamp(TRUST, "decided_at", &text(row, TRUST, "decided_at")?)?;
    Ok(WorkspaceTrust::from_stored(
        project_id,
        canonical_root,
        state,
        decided_at,
    ))
}
