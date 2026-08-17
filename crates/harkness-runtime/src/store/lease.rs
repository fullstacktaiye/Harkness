//! Durable records of which process claims which runs.
//!
//! A lease row is half of a liveness answer. It says a coordinator existed,
//! which runs it took, and when it last said it was still there. What it cannot
//! say is whether that process is *now* alive: a row is bytes, and a `SIGKILL`
//! writes nothing. The other half lives in
//! [`coordinator::lease`](crate::coordinator), where an advisory lock file the
//! kernel releases on death answers exactly that question.
//!
//! Keeping the two apart is deliberate. This module opens no lock file and
//! probes no process; it stores and reads rows. Nothing here decides that a
//! coordinator is gone.

use rusqlite::{Connection, Row, named_params};
use time::OffsetDateTime;

use crate::domain::{LeaseId, RUNTIME_RECORD_SCHEMA_VERSION};

use super::column::{
    decode_id, decode_optional_timestamp, decode_timestamp, encode_optional_timestamp,
    encode_timestamp,
};
use super::error::{StoreError, query_failed};
use super::repository::{optional_text, schema_version, text};

const LEASE: &str = "runtime_lease";

const LEASE_COLUMNS: &str = "schema_version, id, pid, acquired_at, renewed_at, released_at";

/// One coordinator's durable claim on the runs it drives.
///
/// The record carries no lock path. The file a lease is proved alive through is
/// derived from the identity and the data directory, never read from a column,
/// so a rewritten row cannot redirect a liveness probe at a file somebody else
/// holds — and answer "alive" for a process that is gone.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeaseRecord {
    id: LeaseId,
    pid: u32,
    acquired_at: OffsetDateTime,
    renewed_at: OffsetDateTime,
    released_at: Option<OffsetDateTime>,
}

impl LeaseRecord {
    /// Describes a lease taken now by this process.
    #[must_use]
    pub fn acquired(id: LeaseId, pid: u32, at: OffsetDateTime) -> Self {
        let at = at.to_offset(time::UtcOffset::UTC);
        Self {
            id,
            pid,
            acquired_at: at,
            renewed_at: at,
            released_at: None,
        }
    }

    /// Rebuilds a record from the columns it was stored as.
    ///
    /// Crate-visible rather than public: a `LeaseRecord` a caller assembled
    /// would be a claim nothing is holding, and the only honest sources of one
    /// are [`acquired`](Self::acquired) and a row.
    pub(crate) const fn from_stored(
        id: LeaseId,
        pid: u32,
        acquired_at: OffsetDateTime,
        renewed_at: OffsetDateTime,
        released_at: Option<OffsetDateTime>,
    ) -> Self {
        Self {
            id,
            pid,
            acquired_at,
            renewed_at,
            released_at,
        }
    }

    /// Stable identity of the claim.
    #[must_use]
    pub const fn id(&self) -> LeaseId {
        self.id
    }

    /// Process that took the lease, for an operator reading a log.
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// UTC time the lease was taken.
    #[must_use]
    pub const fn acquired_at(&self) -> OffsetDateTime {
        self.acquired_at
    }

    /// UTC time the holder last said it was still there.
    #[must_use]
    pub const fn renewed_at(&self) -> OffsetDateTime {
        self.renewed_at
    }

    /// UTC time the lease was given up, or found dead by a recovery sweep.
    #[must_use]
    pub const fn released_at(&self) -> Option<OffsetDateTime> {
        self.released_at
    }

    /// Whether the row itself already says the claim is over.
    ///
    /// A released lease is never probed again: the file lock would answer
    /// "acquirable" for it anyway, and asking costs an open.
    #[must_use]
    pub const fn is_released(&self) -> bool {
        self.released_at.is_some()
    }
}

/// Records the claim if this is the first run it has taken.
///
/// Written on first use rather than at acquisition, so a coordinator that opens
/// a store and records nothing — which every read-only front end does — leaves
/// no row behind. There is then nothing to collect: a lease row exists exactly
/// when some run names it, which is also the only time anything asks about one.
pub(super) fn ensure(connection: &Connection, lease: &LeaseRecord) -> Result<(), StoreError> {
    connection
        .execute(
            &format!(
                "INSERT INTO runtime_leases ({LEASE_COLUMNS}) VALUES (:schema_version, :id, :pid, \
                 :acquired_at, :renewed_at, :released_at) ON CONFLICT (id) DO NOTHING"
            ),
            named_params! {
                ":schema_version": RUNTIME_RECORD_SCHEMA_VERSION,
                ":id": lease.id.to_string(),
                ":pid": i64::from(lease.pid),
                ":acquired_at": encode_timestamp(LEASE, "acquired_at", lease.acquired_at)?,
                ":renewed_at": encode_timestamp(LEASE, "renewed_at", lease.renewed_at)?,
                ":released_at": encode_optional_timestamp(LEASE, "released_at", lease.released_at)?,
            },
        )
        .map(|_| ())
        .map_err(|error| query_failed("recording a runtime lease", error))
}

/// Refreshes the holder's "still here" timestamp.
///
/// Two absences are deliberately not errors. A lease that has taken no run yet
/// has no row, and a released one must not come back: naming the release column
/// is what stops a housekeeping tick that raced a shutdown from putting a dead
/// claim into service. Both cases update nothing and report how many rows moved,
/// so a caller that cares can tell "not recorded yet" from "refreshed".
pub(super) fn renew(
    connection: &Connection,
    id: LeaseId,
    at: OffsetDateTime,
) -> Result<usize, StoreError> {
    connection
        .execute(
            "UPDATE runtime_leases SET renewed_at = :renewed_at \
             WHERE id = :id AND released_at IS NULL",
            named_params! {
                ":id": id.to_string(),
                ":renewed_at": encode_timestamp(LEASE, "renewed_at", at)?,
            },
        )
        .map_err(|error| query_failed("renewing a runtime lease", error))
}

/// Records that a lease is over, whether it was given up or found dead.
///
/// Idempotent by construction: the first release wins and a second changes
/// nothing, because the statement only ever moves a row out of the live set.
pub(super) fn release(
    connection: &Connection,
    id: LeaseId,
    at: OffsetDateTime,
) -> Result<(), StoreError> {
    connection
        .execute(
            "UPDATE runtime_leases SET released_at = :released_at \
             WHERE id = :id AND released_at IS NULL",
            named_params! {
                ":id": id.to_string(),
                ":released_at": encode_timestamp(LEASE, "released_at", at)?,
            },
        )
        .map(|_| ())
        .map_err(|error| query_failed("releasing a runtime lease", error))
}

/// Loads one claim, reporting absence rather than failing.
///
/// A run can only name a lease whose row was written in the same transaction,
/// so absence here means the row was removed outside Harkness. That is a claim
/// nobody is making, which is what the caller needs to know — not an error that
/// would stop every other run of the sweep from being recovered.
pub(super) fn load(
    connection: &Connection,
    id: LeaseId,
) -> Result<Option<LeaseRecord>, StoreError> {
    let mut statement = connection
        .prepare_cached(&format!(
            "SELECT {LEASE_COLUMNS} FROM runtime_leases WHERE id = :id"
        ))
        .map_err(|error| query_failed("preparing the runtime lease query", error))?;
    match statement.query_row(named_params! { ":id": id.to_string() }, |row| {
        Ok(lease_record(row))
    }) {
        Ok(record) => record.map(Some),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(query_failed("loading a runtime lease", error)),
    }
}

/// Lists every lease that has not been released, oldest first.
pub(super) fn list_live(connection: &Connection) -> Result<Vec<LeaseRecord>, StoreError> {
    let mut statement = connection
        .prepare_cached(&format!(
            "SELECT {LEASE_COLUMNS} FROM runtime_leases WHERE released_at IS NULL \
             ORDER BY acquired_at, id"
        ))
        .map_err(|error| query_failed("preparing the live lease listing", error))?;
    let rows = statement
        .query_map([], |row| Ok(lease_record(row)))
        .map_err(|error| query_failed("listing live runtime leases", error))?;
    let mut leases = Vec::new();
    for row in rows {
        leases.push(row.map_err(|error| query_failed("reading a runtime lease row", error))??);
    }
    Ok(leases)
}

fn lease_record(row: &Row<'_>) -> Result<LeaseRecord, StoreError> {
    // Probed first, exactly as every other record's is: a row a newer build
    // wrote may spell a column in a way this one cannot read.
    let _ = schema_version(row, LEASE)?;
    let stored_pid: i64 = row.get("pid").map_err(|error| StoreError::ColumnEncoding {
        record: LEASE,
        field: "pid",
        reason: error.to_string(),
    })?;
    Ok(LeaseRecord::from_stored(
        decode_id(LEASE, "id", &text(row, LEASE, "id")?)?,
        u32::try_from(stored_pid).map_err(|_| StoreError::ColumnEncoding {
            record: LEASE,
            field: "pid",
            reason: format!("{stored_pid} is not a representable process identifier"),
        })?,
        decode_timestamp(LEASE, "acquired_at", &text(row, LEASE, "acquired_at")?)?,
        decode_timestamp(LEASE, "renewed_at", &text(row, LEASE, "renewed_at")?)?,
        decode_optional_timestamp(
            LEASE,
            "released_at",
            optional_text(row, LEASE, "released_at")?,
        )?,
    ))
}
