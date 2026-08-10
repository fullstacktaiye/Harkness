//! Conversions between stored column values and durable record fields.
//!
//! Every conversion is fallible and names the record and column it failed on,
//! because a row can be older than the process, hand-edited, or written by a
//! build that understood a spelling this one does not.

use std::path::Path;
use std::str::FromStr;

use serde_json::Value;
use time::format_description::BorrowedFormatItem;
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, macros::format_description};

use crate::domain::{Approval, ExecutionState, Failure, ToolCallState};

use super::MAX_INLINE_PAYLOAD_BYTES;
use super::error::StoreError;

/// The one timestamp spelling the store writes.
///
/// RFC 3339 permits a variable number of fractional-second digits, and a
/// variable-width encoding does not sort lexicographically: `…:56.5Z` would
/// order before `…:56Z` because `.` precedes `Z`. Keyset paging compares these
/// strings directly in SQL, so the store always writes exactly nine fractional
/// digits and byte order becomes chronological order. Reading stays lenient and
/// accepts any RFC 3339 spelling, so a row written by a future format is still
/// readable.
const TIMESTAMP_FORMAT: &[BorrowedFormatItem<'_>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:9]Z");

/// Formats a lifecycle instant into its stored, order-preserving spelling.
///
/// The caller must have normalized the instant to UTC; every durable record
/// does so on construction.
pub(super) fn encode_timestamp(
    record: &'static str,
    field: &'static str,
    at: OffsetDateTime,
) -> Result<String, StoreError> {
    at.format(TIMESTAMP_FORMAT)
        .map_err(|error| encoding(record, field, error))
}

/// Formats an optional lifecycle instant.
pub(super) fn encode_optional_timestamp(
    record: &'static str,
    field: &'static str,
    at: Option<OffsetDateTime>,
) -> Result<Option<String>, StoreError> {
    at.map(|at| encode_timestamp(record, field, at)).transpose()
}

/// Parses a stored timestamp, accepting any RFC 3339 spelling.
pub(super) fn decode_timestamp(
    record: &'static str,
    field: &'static str,
    stored: &str,
) -> Result<OffsetDateTime, StoreError> {
    OffsetDateTime::parse(stored, &Rfc3339).map_err(|error| encoding(record, field, error))
}

/// Parses an optional stored timestamp.
pub(super) fn decode_optional_timestamp(
    record: &'static str,
    field: &'static str,
    stored: Option<String>,
) -> Result<Option<OffsetDateTime>, StoreError> {
    stored
        .map(|stored| decode_timestamp(record, field, &stored))
        .transpose()
}

/// Encodes a JSON payload and refuses anything above the inline threshold.
///
/// This is the single choke point every inline payload passes through, so the
/// artifact store and the redaction rules that follow it have exactly one place
/// to attach.
pub(super) fn encode_payload(
    record: &'static str,
    field: &'static str,
    payload: &Value,
) -> Result<String, StoreError> {
    let encoded = serde_json::to_string(payload).map_err(|error| encoding(record, field, error))?;
    if encoded.len() > MAX_INLINE_PAYLOAD_BYTES {
        return Err(StoreError::PayloadTooLarge {
            record,
            field,
            bytes: encoded.len(),
        });
    }
    Ok(encoded)
}

/// Encodes an optional JSON payload through the same threshold.
pub(super) fn encode_optional_payload(
    record: &'static str,
    field: &'static str,
    payload: Option<&Value>,
) -> Result<Option<String>, StoreError> {
    payload
        .map(|payload| encode_payload(record, field, payload))
        .transpose()
}

/// Parses a stored JSON payload.
pub(super) fn decode_payload(
    record: &'static str,
    field: &'static str,
    stored: &str,
) -> Result<Value, StoreError> {
    serde_json::from_str(stored).map_err(|error| encoding(record, field, error))
}

/// Parses an optional stored JSON payload.
pub(super) fn decode_optional_payload(
    record: &'static str,
    field: &'static str,
    stored: Option<String>,
) -> Result<Option<Value>, StoreError> {
    stored
        .map(|stored| decode_payload(record, field, &stored))
        .transpose()
}

/// Encodes the approval audit history as a JSON array.
///
/// Approvals stay with their record instead of in their own table: they are a
/// bounded, ordered part of the record's own audit trail, and the approval
/// request queue that a later change adds is a different thing from the
/// decisions already recorded here.
pub(super) fn encode_approvals(
    record: &'static str,
    approvals: &[Approval],
) -> Result<String, StoreError> {
    serde_json::to_string(approvals).map_err(|error| encoding(record, "approvals", error))
}

/// Parses the stored approval audit history.
pub(super) fn decode_approvals(
    record: &'static str,
    stored: &str,
) -> Result<Vec<Approval>, StoreError> {
    serde_json::from_str(stored).map_err(|error| encoding(record, "approvals", error))
}

/// Splits structured failure detail into its two stored columns.
pub(super) fn encode_failure(failure: Option<&Failure>) -> (Option<&str>, Option<&str>) {
    match failure {
        Some(failure) => (Some(failure.kind()), Some(failure.message())),
        None => (None, None),
    }
}

/// Rebuilds structured failure detail from its two stored columns.
///
/// One column present without the other is a corrupt row rather than a partial
/// failure, so it is refused instead of being filled in with a placeholder.
pub(super) fn decode_failure(
    record: &'static str,
    kind: Option<String>,
    message: Option<String>,
) -> Result<Option<Failure>, StoreError> {
    match (kind, message) {
        (Some(kind), Some(message)) => Ok(Some(Failure::new(kind, message))),
        (None, None) => Ok(None),
        _ => Err(StoreError::ColumnEncoding {
            record,
            field: "failure",
            reason: "failure_kind and failure_message must be stored together".to_owned(),
        }),
    }
}

/// Parses a stored identifier.
pub(super) fn decode_id<T>(
    record: &'static str,
    field: &'static str,
    stored: &str,
) -> Result<T, StoreError>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    T::from_str(stored).map_err(|error| encoding(record, field, error))
}

/// Parses a stored run or step state spelling.
pub(super) fn decode_execution_state(
    record: &'static str,
    stored: &str,
) -> Result<ExecutionState, StoreError> {
    ExecutionState::ALL
        .iter()
        .copied()
        .find(|state| state.as_str() == stored)
        .ok_or_else(|| unknown_state(record, stored))
}

/// Parses a stored tool-call state spelling.
pub(super) fn decode_tool_call_state(
    record: &'static str,
    stored: &str,
) -> Result<ToolCallState, StoreError> {
    ToolCallState::ALL
        .iter()
        .copied()
        .find(|state| state.as_str() == stored)
        .ok_or_else(|| unknown_state(record, stored))
}

/// Borrows a path as the UTF-8 text the store can hold.
///
/// Platform paths are not guaranteed to be UTF-8, and the durable record format
/// already requires UTF-8 for the same field, so an unrepresentable workspace
/// path is reported rather than stored lossily.
pub(super) fn encode_path<'a>(
    record: &'static str,
    field: &'static str,
    path: &'a Path,
) -> Result<&'a str, StoreError> {
    path.to_str()
        .ok_or(StoreError::NonUtf8Path { record, field })
}

/// Narrows a revision into the signed integer SQLite stores.
///
/// The domain refuses to advance a revision past `u64::MAX`, so a value this
/// conversion cannot represent means the record was already beyond anything a
/// database could hold.
pub(super) fn encode_revision(record: &'static str, revision: u64) -> Result<i64, StoreError> {
    i64::try_from(revision).map_err(|_| StoreError::ColumnEncoding {
        record,
        field: "revision",
        reason: format!("{revision} is beyond the storable revision range"),
    })
}

/// Widens a stored revision back to its domain type.
pub(super) fn decode_revision(record: &'static str, stored: i64) -> Result<u64, StoreError> {
    u64::try_from(stored).map_err(|_| StoreError::ColumnEncoding {
        record,
        field: "revision",
        reason: format!("{stored} is not a representable revision"),
    })
}

/// Narrows a stored ordinal back to its domain type.
pub(super) fn decode_ordinal(record: &'static str, stored: i64) -> Result<u32, StoreError> {
    u32::try_from(stored).map_err(|_| StoreError::ColumnEncoding {
        record,
        field: "ordinal",
        reason: format!("{stored} is not a representable ordinal"),
    })
}

/// Narrows a stored owner process identifier back to its platform type.
pub(super) fn decode_owner_pid(
    record: &'static str,
    stored: Option<i64>,
) -> Result<Option<u32>, StoreError> {
    stored
        .map(|stored| {
            u32::try_from(stored).map_err(|_| StoreError::ColumnEncoding {
                record,
                field: "owner_pid",
                reason: format!("{stored} is not a representable process identifier"),
            })
        })
        .transpose()
}

fn encoding(
    record: &'static str,
    field: &'static str,
    reason: impl std::fmt::Display,
) -> StoreError {
    StoreError::ColumnEncoding {
        record,
        field,
        reason: reason.to_string(),
    }
}

fn unknown_state(record: &'static str, stored: &str) -> StoreError {
    StoreError::ColumnEncoding {
        record,
        field: "state",
        reason: format!("{stored} is not a state this build understands"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    use crate::domain::{ExecutionState, Failure};
    use crate::store::MAX_INLINE_PAYLOAD_BYTES;

    use super::{
        decode_execution_state, decode_failure, decode_timestamp, encode_payload, encode_timestamp,
    };

    fn at(spelling: &str) -> OffsetDateTime {
        OffsetDateTime::parse(spelling, &Rfc3339).unwrap()
    }

    #[test]
    fn timestamps_are_stored_at_fixed_width_so_byte_order_is_time_order() {
        let whole = encode_timestamp("run", "created_at", at("2026-08-10T12:34:56Z")).unwrap();
        let fractional =
            encode_timestamp("run", "created_at", at("2026-08-10T12:34:56.5Z")).unwrap();

        assert_eq!(whole, "2026-08-10T12:34:56.000000000Z");
        assert_eq!(fractional, "2026-08-10T12:34:56.500000000Z");
        assert!(
            whole < fractional,
            "{whole} should sort before {fractional}"
        );
        assert_eq!(whole.len(), fractional.len());
    }

    #[test]
    fn stored_timestamps_round_trip_through_the_lenient_reader() {
        let original = at("2026-08-10T12:34:56.123456789Z");
        let stored = encode_timestamp("run", "created_at", original).unwrap();
        assert_eq!(
            decode_timestamp("run", "created_at", &stored).unwrap(),
            original
        );
        // A shorter spelling written by another producer still reads back.
        assert_eq!(
            decode_timestamp("run", "created_at", "2026-08-10T12:34:56Z").unwrap(),
            at("2026-08-10T12:34:56Z")
        );
    }

    #[test]
    fn oversized_inline_payloads_are_refused_at_the_threshold() {
        let inside = Value::String("a".repeat(MAX_INLINE_PAYLOAD_BYTES - 2));
        let encoded = encode_payload("tool_call", "input", &inside).unwrap();
        assert_eq!(encoded.len(), MAX_INLINE_PAYLOAD_BYTES);

        let outside = Value::String("a".repeat(MAX_INLINE_PAYLOAD_BYTES - 1));
        let error = encode_payload("tool_call", "input", &outside).unwrap_err();
        assert_eq!(error.kind(), "payload_too_large");
        assert!(
            error
                .to_string()
                .contains(&MAX_INLINE_PAYLOAD_BYTES.to_string()),
            "the refusal should name the threshold: {error}"
        );
    }

    #[test]
    fn a_half_written_failure_is_refused_instead_of_guessed() {
        assert_eq!(decode_failure("run", None, None).unwrap(), None);
        assert_eq!(
            decode_failure("run", Some("kind".to_owned()), Some("message".to_owned())).unwrap(),
            Some(Failure::new("kind", "message"))
        );

        let error = decode_failure("run", Some("kind".to_owned()), None).unwrap_err();
        assert_eq!(error.kind(), "column_encoding");
    }

    #[test]
    fn an_unknown_state_spelling_is_refused_by_name() {
        assert_eq!(
            decode_execution_state("run", "waiting_for_approval").unwrap(),
            ExecutionState::WaitingForApproval
        );

        let error = decode_execution_state("run", "WaitingForApproval").unwrap_err();
        assert_eq!(error.kind(), "column_encoding");
        assert!(
            error.to_string().contains("WaitingForApproval"),
            "the refusal should quote the spelling: {error}"
        );
    }

    #[test]
    fn payload_encoding_preserves_json_shape() {
        let payload = json!({"path": "src/lib.rs", "lines": [1, 2, 3]});
        let encoded = encode_payload("tool_call", "input", &payload).unwrap();
        assert_eq!(
            super::decode_payload("tool_call", "input", &encoded).unwrap(),
            payload
        );
    }
}
