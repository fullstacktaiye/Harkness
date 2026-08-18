//! Statement-level persistence for what Harkness observed about one ACP agent.
//!
//! One row per registration, replaced whole. Nothing here is configuration:
//! `agents.json` says which agents exist and what they are launched with, and
//! this table says what happened the last time one was asked.

use rusqlite::{Connection, OptionalExtension, named_params};

use crate::agent_registry::{
    AgentId, AgentObservations, AuthStatus, CompatibilityStatus, decode_health, decode_initialize,
    encode_health, encode_initialize,
};
use crate::domain::RUNTIME_RECORD_SCHEMA_VERSION;

use super::column::{decode_timestamp, encode_timestamp, within_inline_limit};
use super::error::{StoreError, query_failed};
use super::repository::{optional_text, schema_version, text};

const RECORD: &str = "agent_runtime_state";

const COLUMNS: &str = "schema_version, agent_id, auth_status, compatibility, \
                       advertised_protocol_version, last_initialize_json, last_health_json, \
                       updated_at";

pub(super) fn put(
    connection: &Connection,
    id: &AgentId,
    observations: &AgentObservations,
) -> Result<(), StoreError> {
    // Held to the same bounds the load path holds it to, before anything is
    // written. Every field here is validated again on the way out, and a value
    // that satisfies one direction and not the other is a row that writes and
    // can never be read — which takes the agent with it.
    observations
        .validate()
        .map_err(|error| StoreError::ColumnEncoding {
            record: RECORD,
            field: "agent_runtime_state",
            reason: error.to_string(),
        })?;
    let initialize = observations
        .last_initialize()
        .map(|record| encode(RECORD, "last_initialize_json", &encode_initialize(record)))
        .transpose()?;
    let health = observations
        .last_health()
        .map(|record| encode(RECORD, "last_health_json", &encode_health(record)))
        .transpose()?;
    within_inline_limit(RECORD, "agent_id", id.as_str().len())?;

    connection
        .execute(
            "INSERT INTO agent_runtime_state \
             (schema_version, agent_id, auth_status, compatibility, \
              advertised_protocol_version, last_initialize_json, last_health_json, updated_at) \
             VALUES (:schema_version, :agent_id, :auth_status, :compatibility, \
              :advertised, :last_initialize_json, :last_health_json, :updated_at) \
             ON CONFLICT(agent_id) DO UPDATE SET \
             schema_version = excluded.schema_version, \
             auth_status = excluded.auth_status, \
             compatibility = excluded.compatibility, \
             advertised_protocol_version = excluded.advertised_protocol_version, \
             last_initialize_json = excluded.last_initialize_json, \
             last_health_json = excluded.last_health_json, \
             updated_at = excluded.updated_at",
            named_params! {
                ":schema_version": RUNTIME_RECORD_SCHEMA_VERSION,
                ":agent_id": id.as_str(),
                ":auth_status": observations.auth_status().as_str(),
                ":compatibility": observations.compatibility().as_str(),
                ":advertised": observations.compatibility().advertised(),
                ":last_initialize_json": initialize,
                ":last_health_json": health,
                ":updated_at": encode_timestamp(RECORD, "updated_at", observations.updated_at())?,
            },
        )
        .map(|_| ())
        .map_err(|error| query_failed("recording agent runtime state", error))
}

pub(super) fn load(
    connection: &Connection,
    id: &AgentId,
) -> Result<Option<AgentObservations>, StoreError> {
    let mut statement = connection
        .prepare_cached(&format!(
            "SELECT {COLUMNS} FROM agent_runtime_state WHERE agent_id = :agent_id"
        ))
        .map_err(|error| query_failed("preparing the agent runtime state query", error))?;
    let stored = statement
        .query_row(named_params! { ":agent_id": id.as_str() }, |row| {
            Ok(decode(row))
        })
        .optional()
        .map_err(|error| query_failed("loading agent runtime state", error))?;
    stored.transpose()
}

/// Forgets everything observed about one registration.
///
/// Called when a registration is removed. State left behind for an identifier a
/// user later reuses would answer for a program nobody checked, which is the
/// worst kind of stale: it looks like evidence.
pub(super) fn delete(connection: &Connection, id: &AgentId) -> Result<usize, StoreError> {
    connection
        .execute(
            "DELETE FROM agent_runtime_state WHERE agent_id = :agent_id",
            named_params! { ":agent_id": id.as_str() },
        )
        .map_err(|error| query_failed("removing agent runtime state", error))
}

fn encode(
    record: &'static str,
    field: &'static str,
    value: &serde_json::Value,
) -> Result<String, StoreError> {
    let encoded = serde_json::to_string(value).map_err(|error| StoreError::ColumnEncoding {
        record,
        field,
        reason: error.to_string(),
    })?;
    within_inline_limit(record, field, encoded.len())?;
    Ok(encoded)
}

fn decode(row: &rusqlite::Row<'_>) -> Result<AgentObservations, StoreError> {
    // Probe first, exactly as every other stored record does: a future row may
    // spell a status this build cannot decode, and it must read as an upgrade
    // request rather than as a corrupt column.
    schema_version(row, RECORD)?;
    let stored_auth = text(row, RECORD, "auth_status")?;
    let auth_status =
        AuthStatus::from_stored(&stored_auth).ok_or_else(|| StoreError::ColumnEncoding {
            record: RECORD,
            field: "auth_status",
            reason: format!("{stored_auth:?} is not a known authentication status"),
        })?;

    let stored_compatibility = text(row, RECORD, "compatibility")?;
    let advertised: Option<i64> =
        row.get("advertised_protocol_version")
            .map_err(|error| StoreError::ColumnEncoding {
                record: RECORD,
                field: "advertised_protocol_version",
                reason: error.to_string(),
            })?;
    let advertised = advertised
        .map(|stored| {
            u16::try_from(stored).map_err(|_| StoreError::ColumnEncoding {
                record: RECORD,
                field: "advertised_protocol_version",
                reason: format!("{stored} is not a representable protocol version"),
            })
        })
        .transpose()?;
    // The tag and its payload are validated together, so a row claiming
    // compatibility while carrying a refused version — or claiming a refusal
    // while carrying none — is refused rather than half-read.
    let compatibility = CompatibilityStatus::from_stored(&stored_compatibility, advertised)
        .ok_or_else(|| StoreError::ColumnEncoding {
            record: RECORD,
            field: "compatibility",
            reason: format!(
                "{stored_compatibility:?} with advertised version {advertised:?} is not a \
                 compatibility status this build wrote"
            ),
        })?;

    let last_initialize = optional_text(row, RECORD, "last_initialize_json")?
        .map(|encoded| decode_json(RECORD, "last_initialize_json", &encoded))
        .transpose()?
        .map(|value| {
            decode_initialize(&value).map_err(|error| StoreError::ColumnEncoding {
                record: RECORD,
                field: "last_initialize_json",
                reason: error.to_string(),
            })
        })
        .transpose()?;
    let last_health = optional_text(row, RECORD, "last_health_json")?
        .map(|encoded| decode_json(RECORD, "last_health_json", &encoded))
        .transpose()?
        .map(|value| {
            decode_health(&value).map_err(|error| StoreError::ColumnEncoding {
                record: RECORD,
                field: "last_health_json",
                reason: error.to_string(),
            })
        })
        .transpose()?;

    let updated_at = decode_timestamp(RECORD, "updated_at", &text(row, RECORD, "updated_at")?)?;
    Ok(AgentObservations::from_parts(
        auth_status,
        compatibility,
        last_initialize,
        last_health,
        updated_at,
    ))
}

fn decode_json(
    record: &'static str,
    field: &'static str,
    encoded: &str,
) -> Result<serde_json::Value, StoreError> {
    serde_json::from_str(encoded).map_err(|error| StoreError::ColumnEncoding {
        record,
        field,
        reason: error.to_string(),
    })
}
