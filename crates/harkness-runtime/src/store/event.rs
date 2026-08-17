//! The append-only run event log.
//!
//! A run record says what is true now. The event log says how the run got
//! there, and it is the only durable answer to that question: it is what the
//! GUI timeline renders, what the CLI prints, and what an approval audit is
//! read out of. It is therefore append-only in the strict sense — this module
//! contains no `UPDATE` and no `DELETE` against `run_events`, and adding one
//! would make every consumer's trust in the log unearned.
//!
//! # Sequence numbers
//!
//! Each event carries an [`EventSeq`] that is monotonic within its run and
//! starts at one. The number is allocated as `1 + MAX(seq)` *inside* the same
//! transaction that inserts the row, and every write goes through the store's
//! single writer connection under `BEGIN IMMEDIATE`, so two appenders cannot
//! read the same maximum. `(run_id, seq)` is the primary key, so even a caller
//! reaching the table another way cannot produce a duplicate.
//!
//! Gaps are permitted and monotonicity is not. A gap costs a reader nothing —
//! pagination is `seq > last`, not `seq = last + 1` — while a repeated or
//! reordered number would silently change what a timeline says happened.
//!
//! # Atomicity with the state it describes
//!
//! [`Store::transition_run_with_event`](super::Store::transition_run_with_event)
//! and its tool-call sibling apply the lifecycle change and append its event in
//! one transaction. Either both are visible or neither is; there is no window
//! in which a run has moved and its history does not say so.
//!
//! # Oversized payloads
//!
//! A payload above [`MAX_INLINE_PAYLOAD_BYTES`] is not refused and not
//! truncated. It is written to an artifact and the stored event carries a
//! [reference](RunEvent::overflowed_payload) to it under the
//! [`OVERFLOW_PAYLOAD_FIELD`] key, so the full bytes stay recoverable while the
//! row stays small. That keeps one promise the whole store makes — no column
//! holds more than the threshold — without making a caller decide in advance
//! how big its own diagnostics are going to be.
//!
//! The threshold is measured *after* redaction, and the artifact holds the
//! redacted encoding: exactly the bytes the row would have held had they fit.
//! Spilling the caller's original and leaving the artifact stream's wrapper to
//! scrub it would make redaction depend on payload size, since a rule may
//! legitimately be implemented in `redact_text` alone.
//!
//! # Unknown kinds
//!
//! [`EventKind`] is extensible the way the project catalog is extensible: a
//! spelling this build does not define decodes to
//! [`Unrecognized`](EventKind::Unrecognized) and renders as an opaque timeline
//! entry. Adding a kind is not a migration, and an older binary reading a newer
//! database shows a plain entry rather than refusing the run.

use std::fmt;

use rusqlite::{Connection, Row, named_params};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use time::{OffsetDateTime, UtcOffset};

use crate::domain::{ArtifactId, RUNTIME_RECORD_SCHEMA_VERSION, RunId, StepId, ToolCallId};

use super::MAX_INLINE_PAYLOAD_BYTES;
use super::column::{
    decode_id, decode_payload, decode_timestamp, encode_text, encode_timestamp, within_inline_limit,
};
use super::error::{Containment, StoreError, insert_failed, query_failed};
use super::repository::{optional_text, schema_version, text};

const RUN_EVENT: &str = "run_event";

const EVENT_COLUMNS: &str = "schema_version, run_id, seq, at, kind, step_id, tool_call_id, \
     artifact_id, payload_json";

/// Default number of events a page returns when the caller states no preference.
pub const DEFAULT_EVENT_PAGE_LIMIT: usize = 200;

/// Largest event page the store will assemble in one query.
///
/// Higher than the run listing's limit because a timeline is read in bulk: the
/// latency target is stated for a thousand-event run, and a consumer meeting it
/// should not have to pay for three round trips to do so.
///
/// # This is a row bound, not a byte bound
///
/// Each payload may be up to [`MAX_INLINE_PAYLOAD_BYTES`], so a full page can in
/// principle materialize `MAX_EVENT_PAGE_LIMIT * MAX_INLINE_PAYLOAD_BYTES` — 64
/// MiB — of events at once. Real payloads are progress counters and state
/// spellings, and anything genuinely large has already been spilled to an
/// artifact and replaced by a reference of a couple of hundred bytes, so the
/// worst case is not the expected one. It is still the caller's arithmetic to
/// do: a consumer with a memory budget should size its page against the
/// payloads it expects rather than asking for the maximum.
pub const MAX_EVENT_PAGE_LIMIT: usize = 1_000;

/// Payload field naming the artifact that holds an overflowed payload.
pub const OVERFLOW_PAYLOAD_FIELD: &str = "payload_artifact";

/// Media type an overflowed event payload is stored under.
pub const OVERFLOW_PAYLOAD_MEDIA_TYPE: &str = "application/json";

/// Artifact name an overflowed event payload is stored under.
pub const OVERFLOW_PAYLOAD_NAME: &str = "payload.json";

/// A position in one run's event log.
///
/// Monotonic within a run and meaningless across runs. It is the pagination key
/// as well as the identity, which is why it is a number a caller can carry in a
/// continuation token rather than an opaque blob.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct EventSeq(u64);

impl EventSeq {
    /// The sequence number the first event of a run receives.
    pub const FIRST: Self = Self(1);

    /// Names an arbitrary position, for a caller rebuilding one from a token.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The underlying number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for EventSeq {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// What an event says happened.
///
/// The variants name the transitions and observations v0.3 records. The
/// [`Unrecognized`](Self::Unrecognized) variant is not an error case: it is how
/// a build reads a log written by a newer one, and it is why adding a kind
/// never needs a migration.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum EventKind {
    /// A run entered a new lifecycle state.
    RunStateChanged,
    /// A recovery sweep found the run's owning process gone.
    ///
    /// Distinct from the `run_state_changed` entry that follows it: that one
    /// says the run reached `interrupted`, this one says why it was detected
    /// and which claim was found dead.
    RunInterrupted,
    /// A later run was created to re-attempt this one.
    ///
    /// Recorded on the *original*, which is never otherwise touched again — a
    /// terminal run's timeline is evidence, and a retry is a new run rather
    /// than a continuation of it.
    RunRetried,
    /// A step began executing.
    StepStarted,
    /// A step reached a terminal state.
    StepFinished,
    /// A tool call entered a new lifecycle state.
    ToolCallStateChanged,
    /// A validated tool request received its binding policy decision.
    PolicyDecision,
    /// A running tool reported progress.
    ToolProgress,
    /// Work paused for a human decision.
    ApprovalRequested,
    /// A human decision was recorded.
    ApprovalDecided,
    /// An otherwise applicable approval no longer matched external identity.
    ApprovalIdentityDrift,
    /// Content was stored outside the log.
    ArtifactCreated,
    /// A redacted observation was delivered to the run's agent.
    AgentObservation,
    /// A redacted action was returned by the run's agent.
    AgentAction,
    /// The agent's resumable session checkpoint was recorded.
    AgentCheckpoint,
    /// Anything a run wants to say that no other kind covers.
    Diagnostic,
    /// A kind this build does not define, preserved exactly as stored.
    ///
    /// Sealed against construction from outside this crate so
    /// [`parse`](Self::parse) is the only way to reach it. Without that seal a
    /// caller could write `Unrecognized("diagnostic".into())`, which stores as
    /// `diagnostic` and reads back as [`Diagnostic`](Self::Diagnostic) — one
    /// meaning with two in-memory spellings, and an appended event that no
    /// longer equals the one that comes back. Read the text with
    /// [`as_str`](Self::as_str).
    #[non_exhaustive]
    Unrecognized(String),
}

impl EventKind {
    /// Every kind this build defines, in declaration order.
    pub const KINDS: &'static [&'static str] = &[
        "run_state_changed",
        "run_interrupted",
        "run_retried",
        "step_started",
        "step_finished",
        "tool_call_state_changed",
        "policy_decision",
        "tool_progress",
        "approval_requested",
        "approval_decided",
        "approval_identity_drift",
        "artifact_created",
        "agent_observation",
        "agent_action",
        "agent_checkpoint",
        "diagnostic",
    ];

    /// The stable spelling stored in the `kind` column.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::RunStateChanged => Self::KINDS[0],
            Self::RunInterrupted => Self::KINDS[1],
            Self::RunRetried => Self::KINDS[2],
            Self::StepStarted => Self::KINDS[3],
            Self::StepFinished => Self::KINDS[4],
            Self::ToolCallStateChanged => Self::KINDS[5],
            Self::PolicyDecision => Self::KINDS[6],
            Self::ToolProgress => Self::KINDS[7],
            Self::ApprovalRequested => Self::KINDS[8],
            Self::ApprovalDecided => Self::KINDS[9],
            Self::ApprovalIdentityDrift => Self::KINDS[10],
            Self::ArtifactCreated => Self::KINDS[11],
            Self::AgentObservation => Self::KINDS[12],
            Self::AgentAction => Self::KINDS[13],
            Self::AgentCheckpoint => Self::KINDS[14],
            Self::Diagnostic => Self::KINDS[15],
            Self::Unrecognized(spelling) => spelling,
        }
    }

    /// Interprets a stored spelling, keeping an unknown one verbatim.
    ///
    /// Normalizing here rather than only at the decode site means a caller
    /// cannot construct an `Unrecognized` that shadows a defined kind, so the
    /// column has one spelling per meaning however the value was built.
    #[must_use]
    pub fn parse(spelling: &str) -> Self {
        match spelling {
            "run_state_changed" => Self::RunStateChanged,
            "run_interrupted" => Self::RunInterrupted,
            "run_retried" => Self::RunRetried,
            "step_started" => Self::StepStarted,
            "step_finished" => Self::StepFinished,
            "tool_call_state_changed" => Self::ToolCallStateChanged,
            "policy_decision" => Self::PolicyDecision,
            "tool_progress" => Self::ToolProgress,
            "approval_requested" => Self::ApprovalRequested,
            "approval_decided" => Self::ApprovalDecided,
            "approval_identity_drift" => Self::ApprovalIdentityDrift,
            "artifact_created" => Self::ArtifactCreated,
            "agent_observation" => Self::AgentObservation,
            "agent_action" => Self::AgentAction,
            "agent_checkpoint" => Self::AgentCheckpoint,
            "diagnostic" => Self::Diagnostic,
            other => Self::Unrecognized(other.to_owned()),
        }
    }

    /// Whether this build knows what the kind means.
    ///
    /// A timeline renders a recognized kind with its own presentation and an
    /// unrecognized one as a plain entry; nothing else should branch on it.
    #[must_use]
    pub const fn is_recognized(&self) -> bool {
        !matches!(self, Self::Unrecognized(_))
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for EventKind {
    /// Serializes as the stored spelling, so a projection of the log is the same
    /// text whether it came from a row or from a value in memory.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for EventKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::parse(&String::deserialize(deserializer)?))
    }
}

/// Which end of a run's log a page is read from.
///
/// Both directions page by [`EventSeq`] and neither repeats or skips an event
/// however many arrive at the tip between requests. They exist separately
/// because the two readers of a timeline start at opposite ends: a replay
/// follows the log forwards from where it left off, while a surface opens on
/// the newest entries and asks for older ones as the reader scrolls. Serving
/// the second from the first would mean reading the whole run to reach its end.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum EventOrder {
    /// Oldest first, continuing towards the tip.
    #[default]
    Oldest,
    /// Newest first, continuing towards the beginning of the run.
    Newest,
}

/// Bounds and positions one page of a run's event log.
///
/// `from` is the exclusive boundary the page starts after, in the direction
/// `order` names: the last sequence already seen reading forwards, or the
/// oldest already seen reading backwards. Absent, the page starts at whichever
/// end `order` selects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct EventPage {
    /// Maximum number of events in the returned page.
    pub limit: usize,
    /// Direction the page is read in, and which end an unbounded page starts at.
    pub order: EventOrder,
    /// Exclusive boundary an earlier [`EventListing`] returned.
    pub from: Option<EventSeq>,
}

impl EventPage {
    /// Requests the oldest `limit` events.
    #[must_use]
    pub const fn oldest(limit: usize) -> Self {
        Self {
            limit,
            order: EventOrder::Oldest,
            from: None,
        }
    }

    /// Requests the newest `limit` events.
    #[must_use]
    pub const fn newest(limit: usize) -> Self {
        Self {
            limit,
            order: EventOrder::Newest,
            from: None,
        }
    }

    /// Continues this page's direction from `boundary`, exclusive.
    #[must_use]
    pub const fn after(mut self, boundary: EventSeq) -> Self {
        self.from = Some(boundary);
        self
    }
}

impl Default for EventPage {
    fn default() -> Self {
        Self::oldest(DEFAULT_EVENT_PAGE_LIMIT)
    }
}

/// One page of a run's event log, ordered as its [`EventPage`] asked for.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct EventListing {
    /// At most [`EventPage::limit`] events, in the requested order.
    pub events: Vec<StoredEvent>,
    /// Boundary to continue from, present only when a further event exists.
    ///
    /// This is the sequence of the last event in `events`, so continuing is
    /// [`EventPage::after`] with it. It is `None` for a page that reached the
    /// end of the log in its direction — which is a stronger statement than an
    /// under-full page, because a page can be exactly full and still be last.
    pub next_cursor: Option<EventSeq>,
}

/// One thing worth recording about a run.
///
/// The associations are optional because they are genuinely optional: a run
/// state change belongs to no step, and a diagnostic may belong to no tool
/// call. Each one that *is* present is enforced by a foreign key composite with
/// the run, so an event cannot name a step, call, or artifact that was never
/// stored — nor one belonging to a different run. A timeline naming another
/// run's step would be a worse outcome than a refused write, because nothing
/// downstream re-checks it; the wrong step would simply be rendered.
#[derive(Clone, Debug, PartialEq)]
pub struct RunEvent {
    kind: EventKind,
    at: OffsetDateTime,
    step_id: Option<StepId>,
    tool_call_id: Option<ToolCallId>,
    artifact_id: Option<ArtifactId>,
    payload: Value,
}

impl RunEvent {
    /// Records `kind` happening at `at`, with no associations and no payload.
    ///
    /// The instant is normalized to UTC, because that is the only spelling the
    /// column holds and a value that changed on the way to storage would not
    /// compare equal to the one that comes back.
    #[must_use]
    pub fn new(kind: EventKind, at: OffsetDateTime) -> Self {
        Self {
            kind,
            at: at.to_offset(UtcOffset::UTC),
            step_id: None,
            tool_call_id: None,
            artifact_id: None,
            payload: Value::Null,
        }
    }

    /// Associates the event with a step of the same run.
    #[must_use]
    pub fn for_step(mut self, step_id: StepId) -> Self {
        self.step_id = Some(step_id);
        self
    }

    /// Associates the event with a tool call of the same run.
    #[must_use]
    pub fn for_tool_call(mut self, tool_call_id: ToolCallId) -> Self {
        self.tool_call_id = Some(tool_call_id);
        self
    }

    /// Associates the event with a stored artifact.
    #[must_use]
    pub fn for_artifact(mut self, artifact_id: ArtifactId) -> Self {
        self.artifact_id = Some(artifact_id);
        self
    }

    /// Attaches structured detail.
    #[must_use]
    pub fn with_payload(mut self, payload: Value) -> Self {
        self.payload = payload;
        self
    }

    /// What happened.
    #[must_use]
    pub const fn kind(&self) -> &EventKind {
        &self.kind
    }

    /// When it happened, in UTC.
    #[must_use]
    pub const fn at(&self) -> OffsetDateTime {
        self.at
    }

    /// Step the event belongs to, when it belongs to one.
    #[must_use]
    pub const fn step_id(&self) -> Option<StepId> {
        self.step_id
    }

    /// Tool call the event belongs to, when it belongs to one.
    #[must_use]
    pub const fn tool_call_id(&self) -> Option<ToolCallId> {
        self.tool_call_id
    }

    /// Artifact the event refers to, when it refers to one.
    #[must_use]
    pub const fn artifact_id(&self) -> Option<ArtifactId> {
        self.artifact_id
    }

    /// Structured detail, or [`Value::Null`] when there is none.
    #[must_use]
    pub const fn payload(&self) -> &Value {
        &self.payload
    }

    /// The artifact holding this event's payload, when it overflowed inline.
    ///
    /// Present only on an event read back from the store: a payload above the
    /// inline threshold is written to an artifact on the way in, and this is the
    /// reference that replaced it.
    #[must_use]
    pub fn overflowed_payload(&self) -> Option<OverflowedPayload> {
        serde_json::from_value::<OverflowMarker>(self.payload.clone())
            .ok()
            .map(|marker| marker.payload_artifact)
    }
}

/// The reference standing in for a payload that outgrew its column.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OverflowedPayload {
    /// Artifact holding the full payload bytes.
    pub id: ArtifactId,
    /// Media type the payload was stored under.
    pub media_type: String,
    /// Size of the stored payload in bytes.
    pub byte_size: u64,
    /// Hex SHA-256 of the stored payload.
    pub sha256: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OverflowMarker {
    payload_artifact: OverflowedPayload,
}

/// Builds the inline payload that replaces one written to an artifact.
pub(super) fn overflow_payload(reference: OverflowedPayload) -> Value {
    serde_json::to_value(OverflowMarker {
        payload_artifact: reference,
    })
    .expect("an overflow reference is always representable as JSON")
}

/// An event as the store holds it: a run, a position, and what happened.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct StoredEvent {
    /// Run whose log the event belongs to.
    pub run_id: RunId,
    /// Position within that log.
    pub seq: EventSeq,
    /// What was recorded.
    pub event: RunEvent,
}

/// Appends one event, allocating its sequence number in the same transaction.
///
/// The caller has already redacted the payload and spilled it to an artifact if
/// it was too large; this function is the statement pair and nothing else.
pub(super) fn append_event(
    connection: &Connection,
    run_id: RunId,
    event: &RunEvent,
    payload_json: &str,
) -> Result<EventSeq, StoreError> {
    within_inline_limit(RUN_EVENT, "payload", payload_json.len())?;
    let seq = next_sequence(connection, run_id)?;
    let stored = i64::try_from(seq.get()).map_err(|_| StoreError::ColumnEncoding {
        record: RUN_EVENT,
        field: "seq",
        reason: format!("{seq} is beyond the storable sequence range"),
    })?;

    connection
        .execute(
            &format!(
                "INSERT INTO run_events ({EVENT_COLUMNS}) VALUES (:schema_version, :run_id, :seq, \
                 :at, :kind, :step_id, :tool_call_id, :artifact_id, :payload_json)"
            ),
            named_params! {
                ":schema_version": RUNTIME_RECORD_SCHEMA_VERSION,
                ":run_id": run_id.to_string(),
                ":seq": stored,
                ":at": encode_timestamp(RUN_EVENT, "at", event.at)?,
                ":kind": encode_text(RUN_EVENT, "kind", event.kind.as_str())?,
                ":step_id": event.step_id.map(|id| id.to_string()),
                ":tool_call_id": event.tool_call_id.map(|id| id.to_string()),
                ":artifact_id": event.artifact_id.map(|id| id.to_string()),
                ":payload_json": payload_json,
            },
        )
        .map_err(|error| {
            insert_failed(
                Containment {
                    record: RUN_EVENT,
                    // Four foreign keys reach out of this row and SQLite names
                    // none of them, so the refusal names the set rather than
                    // guessing which one the caller broke. Three of them are
                    // composite with `run_id`, so "not stored" covers both an
                    // absent record and one belonging to a different run.
                    parent: "run, or a step, tool call, or artifact of that run,",
                },
                &format!("{run_id}:{seq}"),
                "appending a run event",
                error,
            )
        })?;
    Ok(seq)
}

/// Reads the next free position for a run.
///
/// `MAX(seq)` over a `WITHOUT ROWID` primary key is an index end-scan, so this
/// costs one seek rather than a count of the log.
fn next_sequence(connection: &Connection, run_id: RunId) -> Result<EventSeq, StoreError> {
    let highest: i64 = connection
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM run_events WHERE run_id = :run_id",
            named_params! { ":run_id": run_id.to_string() },
            |row| row.get(0),
        )
        .map_err(|error| query_failed("allocating an event sequence number", error))?;
    let highest = u64::try_from(highest).map_err(|_| StoreError::ColumnEncoding {
        record: RUN_EVENT,
        field: "seq",
        reason: format!("{highest} is not a representable sequence number"),
    })?;
    Ok(EventSeq(highest + 1))
}

/// Returns the durable tip of one run's log without materializing its history.
pub(super) fn latest_sequence(
    connection: &Connection,
    run_id: RunId,
) -> Result<Option<EventSeq>, StoreError> {
    let highest: Option<i64> = connection
        .query_row(
            "SELECT MAX(seq) FROM run_events WHERE run_id = :run_id",
            named_params! { ":run_id": run_id.to_string() },
            |row| row.get(0),
        )
        .map_err(|error| query_failed("reading the latest event sequence", error))?;
    highest
        .map(|highest| {
            u64::try_from(highest)
                .map(EventSeq)
                .map_err(|_| StoreError::ColumnEncoding {
                    record: RUN_EVENT,
                    field: "seq",
                    reason: format!("{highest} is not a representable sequence number"),
                })
        })
        .transpose()
}

/// Returns one page of a run's log, ordered by sequence.
pub(super) fn events(
    connection: &Connection,
    run_id: RunId,
    after: Option<EventSeq>,
    limit: usize,
) -> Result<Vec<StoredEvent>, StoreError> {
    if limit == 0 || limit > MAX_EVENT_PAGE_LIMIT {
        return Err(StoreError::InvalidPageLimit {
            limit,
            maximum: MAX_EVENT_PAGE_LIMIT,
        });
    }

    let mut statement = connection
        .prepare_cached(&format!(
            "SELECT {EVENT_COLUMNS} FROM run_events WHERE run_id = :run_id AND seq > :after \
             ORDER BY seq LIMIT :limit"
        ))
        .map_err(|error| query_failed("preparing the run event query", error))?;
    let rows = statement
        .query_map(
            named_params! {
                ":run_id": run_id.to_string(),
                ":after": i64::try_from(after.map_or(0, EventSeq::get)).unwrap_or(i64::MAX),
                ":limit": i64::try_from(limit).unwrap_or(i64::MAX),
            },
            |row| Ok(stored_event(row)),
        )
        .map_err(|error| query_failed("listing the events of a run", error))?;

    let mut events = Vec::with_capacity(limit.min(DEFAULT_EVENT_PAGE_LIMIT));
    for row in rows {
        events.push(row.map_err(|error| query_failed("reading a run event row", error))??);
    }
    Ok(events)
}

/// Returns one page of a run's log in either direction, with a continuation.
///
/// The bare [`events`] query above stays the replay primitive it was written to
/// be: it answers "what came after this" and nothing else, which is all a
/// subscriber catching up needs. This one adds the two things a surface needs
/// and a replay does not — a page anchored at the newest end, and a
/// continuation that distinguishes a full page with more behind it from a full
/// page that happens to be last.
pub(super) fn event_page(
    connection: &Connection,
    run_id: RunId,
    page: EventPage,
) -> Result<EventListing, StoreError> {
    if page.limit == 0 || page.limit > MAX_EVENT_PAGE_LIMIT {
        return Err(StoreError::InvalidPageLimit {
            limit: page.limit,
            maximum: MAX_EVENT_PAGE_LIMIT,
        });
    }

    // One row beyond the page decides whether a continuation exists, without a
    // second count query that a concurrent append could invalidate.
    let probe = i64::try_from(page.limit)
        .unwrap_or(i64::MAX)
        .saturating_add(1);
    let sql = match page.order {
        EventOrder::Oldest => format!(
            "SELECT {EVENT_COLUMNS} FROM run_events WHERE run_id = :run_id AND seq > :from \
             ORDER BY seq LIMIT :limit"
        ),
        EventOrder::Newest => format!(
            "SELECT {EVENT_COLUMNS} FROM run_events WHERE run_id = :run_id AND seq < :from \
             ORDER BY seq DESC LIMIT :limit"
        ),
    };
    // An absent boundary means "start at the end this order names", so it
    // becomes the sentinel that admits every row in that direction.
    let from = match (page.order, page.from) {
        (_, Some(seq)) => i64::try_from(seq.get()).unwrap_or(i64::MAX),
        (EventOrder::Oldest, None) => 0,
        (EventOrder::Newest, None) => i64::MAX,
    };

    let mut statement = connection
        .prepare_cached(&sql)
        .map_err(|error| query_failed("preparing the run event page", error))?;
    let rows = statement
        .query_map(
            named_params! {
                ":run_id": run_id.to_string(),
                ":from": from,
                ":limit": probe,
            },
            |row| Ok(stored_event(row)),
        )
        .map_err(|error| query_failed("reading a page of the events of a run", error))?;

    let mut events = Vec::with_capacity(page.limit.min(DEFAULT_EVENT_PAGE_LIMIT) + 1);
    for row in rows {
        events.push(row.map_err(|error| query_failed("reading a run event row", error))??);
    }

    // The probe row is discarded; what it proves is that the last event kept is
    // a position worth handing back, so the continuation is that event's own
    // sequence rather than the unreturned one's.
    let has_more = events.len() > page.limit;
    events.truncate(page.limit);
    let next_cursor = has_more
        .then(|| events.last().map(|event| event.seq))
        .flatten();
    Ok(EventListing {
        events,
        next_cursor,
    })
}

fn stored_event(row: &Row<'_>) -> Result<StoredEvent, StoreError> {
    schema_version(row, RUN_EVENT)?;
    let seq = row
        .get::<_, i64>("seq")
        .map_err(|error| StoreError::ColumnEncoding {
            record: RUN_EVENT,
            field: "seq",
            reason: error.to_string(),
        })?;
    let seq = u64::try_from(seq).map_err(|_| StoreError::ColumnEncoding {
        record: RUN_EVENT,
        field: "seq",
        reason: format!("{seq} is not a representable sequence number"),
    })?;

    let payload_json = text(row, RUN_EVENT, "payload_json")?;
    let event = RunEvent {
        // A spelling this build does not define is an opaque entry, never a
        // failed read: the log has to stay legible to the binary that finds it.
        kind: EventKind::parse(&text(row, RUN_EVENT, "kind")?),
        at: decode_timestamp(RUN_EVENT, "at", &text(row, RUN_EVENT, "at")?)?,
        step_id: optional_id(row, "step_id")?,
        tool_call_id: optional_id(row, "tool_call_id")?,
        artifact_id: optional_id(row, "artifact_id")?,
        payload: decode_payload(RUN_EVENT, "payload", &payload_json)?,
    };
    Ok(StoredEvent {
        run_id: decode_id(RUN_EVENT, "run_id", &text(row, RUN_EVENT, "run_id")?)?,
        seq: EventSeq(seq),
        event,
    })
}

fn optional_id<T>(row: &Row<'_>, field: &'static str) -> Result<Option<T>, StoreError>
where
    T: std::str::FromStr,
    T::Err: fmt::Display,
{
    optional_text(row, RUN_EVENT, field)?
        .map(|stored| decode_id(RUN_EVENT, field, &stored))
        .transpose()
}

/// Whether an encoded payload has to be spilled into an artifact.
pub(super) const fn overflows_inline(payload_json: &str) -> bool {
    payload_json.len() > MAX_INLINE_PAYLOAD_BYTES
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::OffsetDateTime;

    use crate::domain::ArtifactId;

    use super::{EventKind, EventSeq, OverflowedPayload, RunEvent, overflow_payload};

    #[test]
    fn event_kinds_round_trip_through_the_kinds_table() {
        let defined = [
            EventKind::RunStateChanged,
            EventKind::RunInterrupted,
            EventKind::RunRetried,
            EventKind::StepStarted,
            EventKind::StepFinished,
            EventKind::ToolCallStateChanged,
            EventKind::PolicyDecision,
            EventKind::ToolProgress,
            EventKind::ApprovalRequested,
            EventKind::ApprovalDecided,
            EventKind::ApprovalIdentityDrift,
            EventKind::ArtifactCreated,
            EventKind::AgentObservation,
            EventKind::AgentAction,
            EventKind::AgentCheckpoint,
            EventKind::Diagnostic,
        ];

        let spellings = defined.iter().map(EventKind::as_str).collect::<Vec<_>>();
        assert_eq!(spellings, EventKind::KINDS);
        for kind in &defined {
            assert_eq!(&EventKind::parse(kind.as_str()), kind);
            assert!(kind.is_recognized());
            assert_eq!(
                serde_json::to_string(kind).unwrap(),
                format!("\"{kind}\""),
                "a kind must serialize as its stored spelling"
            );
        }
    }

    #[test]
    fn an_unknown_spelling_becomes_an_opaque_kind_instead_of_an_error() {
        let future = EventKind::parse("sandbox_escaped");

        assert!(!future.is_recognized());
        assert_eq!(future.as_str(), "sandbox_escaped");
        assert_eq!(
            serde_json::from_str::<EventKind>("\"sandbox_escaped\"").unwrap(),
            future
        );
    }

    #[test]
    fn parsing_a_defined_spelling_never_yields_the_opaque_variant() {
        // Otherwise one meaning would have two representations, and a consumer
        // matching on the enum would miss the one that came in as text.
        for spelling in EventKind::KINDS {
            assert!(
                EventKind::parse(spelling).is_recognized(),
                "{spelling} decoded as unrecognized"
            );
        }
    }

    #[test]
    fn an_event_normalizes_its_instant_to_utc() {
        let shifted = OffsetDateTime::from_unix_timestamp(1_700_000_000)
            .unwrap()
            .to_offset(time::UtcOffset::from_hms(-5, 0, 0).unwrap());

        let event = RunEvent::new(EventKind::Diagnostic, shifted);

        assert_eq!(event.at().offset(), time::UtcOffset::UTC);
        assert_eq!(event.at(), shifted);
    }

    #[test]
    fn an_overflow_marker_round_trips_into_a_typed_reference() {
        let reference = OverflowedPayload {
            id: ArtifactId::new(),
            media_type: "application/json".to_owned(),
            byte_size: 70_000,
            sha256: "abc".to_owned(),
        };
        let event = RunEvent::new(EventKind::Diagnostic, OffsetDateTime::UNIX_EPOCH)
            .with_payload(overflow_payload(reference.clone()));

        assert_eq!(event.overflowed_payload(), Some(reference));
    }

    #[test]
    fn the_published_overflow_key_is_the_one_a_marker_actually_uses() {
        // The constant is what #99 and #100 key on; serde spells the field
        // independently, so the two are asserted to agree rather than assumed to.
        let marker = overflow_payload(OverflowedPayload {
            id: ArtifactId::new(),
            media_type: "application/json".to_owned(),
            byte_size: 1,
            sha256: "ab".to_owned(),
        });

        assert!(
            marker.get(super::OVERFLOW_PAYLOAD_FIELD).is_some(),
            "{marker}"
        );
    }

    #[test]
    fn an_ordinary_payload_is_not_mistaken_for_an_overflow_marker() {
        let event = RunEvent::new(EventKind::Diagnostic, OffsetDateTime::UNIX_EPOCH)
            .with_payload(json!({"payload_artifact": "not a reference"}));

        assert_eq!(event.overflowed_payload(), None);
    }

    #[test]
    fn the_first_sequence_number_is_one() {
        assert_eq!(EventSeq::FIRST.get(), 1);
        assert_eq!(EventSeq::new(7).to_string(), "7");
    }
}
