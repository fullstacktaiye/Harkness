//! Newest-first run listing paged by key rather than by offset.
//!
//! An offset page is wrong for a run history that grows at the tip: inserting a
//! run between two requests shifts every later offset by one, so the next page
//! repeats a row the caller already saw. A keyset page addresses a position in
//! the data instead of a position in a result set, so the same continuation
//! keeps its meaning no matter how many runs arrive at the tip.

use rusqlite::{Connection, named_params};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use time::OffsetDateTime;

use harkness_core::ProjectId;

use crate::domain::{Run, RunId, ToolCallId};

use super::column::{decode_cursor_timestamp, decode_id, encode_timestamp};
use super::error::{StoreError, query_failed};
use super::repository::{RUN_COLUMNS, run_from_wire, run_wire};

/// Default number of runs a page returns when the caller states no preference.
pub const DEFAULT_RUN_PAGE_LIMIT: usize = 50;

/// Largest page the store will assemble in one query.
pub const MAX_RUN_PAGE_LIMIT: usize = 500;

/// An opaque continuation naming the first run a following page returns.
///
/// The token stays opaque so the ordering key can change without breaking
/// callers, and it is versioned so an older token is refused explicitly rather
/// than silently misread. Front ends serialize it into whatever transport token
/// they already use, exactly as the Git log cursor is serialized today.
///
/// # A cursor is a position, not a claim that a row exists
///
/// Validity is structural and is settled at deserialization: a supported
/// version, a parseable identifier, and a parseable timestamp normalized to
/// UTC. The store deliberately does not verify that the anchor row is still
/// present, because a keyset page addressing a pruned or deleted run must keep
/// working — resuming from the position it names is the whole reason the token
/// is a key rather than an offset. A caller that assembles its own coordinates
/// gets the page that key selects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct RunCursor {
    created_at: OffsetDateTime,
    id: RunId,
}

#[derive(Deserialize, Serialize)]
struct RunCursorWire {
    v: u8,
    created_at: String,
    id: String,
}

impl Serialize for RunCursor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let created_at = encode_timestamp("run_cursor", "created_at", self.created_at)
            .map_err(serde::ser::Error::custom)?;
        RunCursorWire {
            v: 1,
            created_at,
            id: self.id.to_string(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RunCursor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = RunCursorWire::deserialize(deserializer)?;
        if wire.v != 1 {
            return Err(D::Error::custom(format!(
                "unsupported run cursor version {}",
                wire.v
            )));
        }
        Ok(Self {
            created_at: decode_cursor_timestamp("run_cursor", "created_at", &wire.created_at)
                .map_err(D::Error::custom)?,
            id: decode_id("run_cursor", "id", &wire.id).map_err(D::Error::custom)?,
        })
    }
}

impl RunCursor {
    /// The run the continued page starts at.
    #[must_use]
    pub const fn anchor(&self) -> RunId {
        self.id
    }
}

/// Bounds and positions one page of run history.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct RunPage {
    /// Maximum number of runs in the returned page.
    pub limit: usize,
    /// Continuation returned by an earlier [`RunListing`].
    pub cursor: Option<RunCursor>,
}

impl RunPage {
    /// Requests the newest `limit` runs.
    #[must_use]
    pub const fn new(limit: usize) -> Self {
        Self {
            limit,
            cursor: None,
        }
    }

    /// Continues an earlier listing from its cursor.
    #[must_use]
    pub const fn after(cursor: RunCursor, limit: usize) -> Self {
        Self {
            limit,
            cursor: Some(cursor),
        }
    }
}

impl Default for RunPage {
    fn default() -> Self {
        Self::new(DEFAULT_RUN_PAGE_LIMIT)
    }
}

/// One page of run history, newest first.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct RunListing {
    /// At most [`RunPage::limit`] runs, newest first.
    pub runs: Vec<Run>,
    /// Continuation anchored at the first unreturned run, if one exists.
    pub next_cursor: Option<RunCursor>,
}

pub(super) fn list_runs(connection: &Connection, page: RunPage) -> Result<RunListing, StoreError> {
    if page.limit == 0 || page.limit > MAX_RUN_PAGE_LIMIT {
        return Err(StoreError::InvalidPageLimit {
            limit: page.limit,
            maximum: MAX_RUN_PAGE_LIMIT,
        });
    }

    // One row beyond the page decides both whether a continuation exists and
    // where it is anchored, without a second count query that a concurrent
    // insert could invalidate.
    let probe = i64::try_from(page.limit)
        .unwrap_or(i64::MAX)
        .saturating_add(1);
    let mut wires = Vec::with_capacity(page.limit + 1);
    match page.cursor {
        None => {
            let mut statement = connection
                .prepare_cached(&format!(
                    "SELECT {RUN_COLUMNS} FROM runs ORDER BY created_at DESC, id DESC LIMIT :limit"
                ))
                .map_err(|error| query_failed("preparing the run listing", error))?;
            let rows = statement
                .query_map(named_params! { ":limit": probe }, |row| Ok(run_wire(row)))
                .map_err(|error| query_failed("listing runs", error))?;
            for row in rows {
                wires.push(row.map_err(|error| query_failed("reading a run row", error))??);
            }
        }
        Some(cursor) => {
            let mut statement = connection
                .prepare_cached(&format!(
                    "SELECT {RUN_COLUMNS} FROM runs \
                     WHERE (created_at, id) <= (:created_at, :id) \
                     ORDER BY created_at DESC, id DESC LIMIT :limit"
                ))
                .map_err(|error| query_failed("preparing the continued run listing", error))?;
            let rows = statement
                .query_map(
                    named_params! {
                        ":created_at": encode_timestamp("run_cursor", "created_at", cursor.created_at)?,
                        ":id": cursor.id.to_string(),
                        ":limit": probe,
                    },
                    |row| Ok(run_wire(row)),
                )
                .map_err(|error| query_failed("listing runs", error))?;
            for row in rows {
                wires.push(row.map_err(|error| query_failed("reading a run row", error))??);
            }
        }
    }

    let next_cursor = wires.get(page.limit).map(|wire| RunCursor {
        created_at: wire.created_at,
        id: wire.id,
    });
    wires.truncate(page.limit);
    let runs = wires
        .into_iter()
        .map(run_from_wire)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(RunListing { runs, next_cursor })
}

pub(super) fn project_latest_tool_call_ids_by_check(
    connection: &Connection,
    project_id: ProjectId,
    check_ids: &[String],
) -> Result<Vec<ToolCallId>, StoreError> {
    if check_ids.is_empty() {
        return Ok(Vec::new());
    }
    let check_ids =
        serde_json::to_string(check_ids).expect("a string array is always representable as JSON");
    let mut statement = connection
        .prepare_cached(
            "WITH matching AS (\
                 SELECT tool_calls.id, tool_calls.created_at, \
                        ROW_NUMBER() OVER (\
                            PARTITION BY json_extract(tool_calls.input_json, '$.check_id') \
                            ORDER BY tool_calls.created_at DESC, tool_calls.id DESC\
                        ) AS newest \
                 FROM tool_calls \
                 JOIN runs ON runs.id = tool_calls.run_id \
                 JOIN tasks ON tasks.id = runs.task_id \
                 WHERE tasks.project_id = :project_id \
                   AND tool_calls.tool_id = 'check.run' \
                   AND json_type(tool_calls.input_json, '$.check_id') = 'text' \
                   AND json_extract(tool_calls.input_json, '$.check_id') IN (\
                       SELECT value FROM json_each(:check_ids)\
                   )\
             ) \
             SELECT id FROM matching WHERE newest = 1 \
             ORDER BY created_at DESC, id DESC",
        )
        .map_err(|error| query_failed("preparing the latest project checks", error))?;
    let rows = statement
        .query_map(
            named_params! {
                ":project_id": project_id.to_string(),
                ":check_ids": check_ids,
            },
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| query_failed("listing the latest project checks", error))?;
    rows.map(|row| {
        let stored = row.map_err(|error| query_failed("reading a project check call id", error))?;
        decode_id("tool call", "id", &stored)
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    use crate::domain::RunId;

    use super::RunCursor;

    #[test]
    fn a_cursor_round_trips_through_its_opaque_token() {
        let cursor = RunCursor {
            created_at: OffsetDateTime::parse("2026-08-10T12:34:56.5Z", &Rfc3339).unwrap(),
            id: RunId::new(),
        };

        let token = serde_json::to_string(&cursor).unwrap();
        assert_eq!(
            serde_json::from_str::<RunCursor>(&token).unwrap(),
            cursor,
            "the token lost information"
        );
        assert!(token.contains("\"v\":1"), "the token must stay versioned");
    }

    #[test]
    fn a_cursor_from_an_unknown_version_is_refused() {
        let token = format!(
            "{{\"v\":2,\"created_at\":\"2026-08-10T12:34:56.000000000Z\",\"id\":\"{}\"}}",
            RunId::new()
        );
        let error = serde_json::from_str::<RunCursor>(&token).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported run cursor version 2"),
            "unexpected refusal: {error}"
        );
    }
}
