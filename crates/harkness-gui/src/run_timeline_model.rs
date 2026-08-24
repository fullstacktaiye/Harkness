//! An incrementally appended list model for one run's event log.
//!
//! A timeline is the surface that grows while somebody is looking at it. A run
//! emitting hundreds of progress events would rebuild every delegate on every
//! one of them if the rows were a replaced `QVariantList`, so this is a real
//! `QAbstractListModel` that opens on the newest
//! [`TIMELINE_PAGE_SIZE`](self::TIMELINE_PAGE_SIZE) entries, pages backwards on
//! demand, and appends live events in whole batches.
//!
//! # Two sources, one order
//!
//! Rows are oldest-first. Two things put events into them and they overlap on
//! purpose: the initial page reads the newest entries out of the store, and the
//! live subscription replays from wherever it was opened. `Timeline::last_seq`
//! is what makes the overlap harmless — an arriving event at or below the
//! highest sequence already applied is a repeat and is dropped. Sequence numbers
//! are per run and allocated inside the transaction that inserts the row, so
//! "already applied" is a total statement about one run's log and never a guess.
//!
//! Bursts are coalesced. The subscriber thread blocks for one event and then
//! drains everything else already queued, so a hundred events delivered while
//! the Qt thread was busy cost one `beginInsertRows` span rather than a hundred.
//!
//! # Rows are summaries
//!
//! A row carries the event's kind, time, associations, and a bounded one-line
//! summary — never its payload, which may be 64 KiB per event. `loadDetail`
//! re-reads one event and attaches its rendering to that row alone, so a
//! timeline of a thousand events holds a thousand short strings and nothing
//! else. The model is additionally capped at
//! [`MAX_TIMELINE_ROWS`](self::MAX_TIMELINE_ROWS) rows: appending past it drops
//! from the oldest end, and paging backwards stops there and says which of the
//! two reasons stopped it — the run's first event, or the window.
//!
//! The window does not slide. A run longer than the cap keeps its newest
//! `MAX_TIMELINE_ROWS` entries, and the entries before them are read out of the
//! append-only log by something other than this model. Sliding it would mean
//! discarding from the newest end while a run is still emitting there, and what
//! a detached-then-reattached timeline should show is a presentation decision
//! that belongs with the QML rather than with the bridge.
//!
//! # Progress ticks fold
//!
//! Consecutive `tool_progress` events of one tool call occupy one row carrying
//! the newest line and how many ticks it stands for, and a tick arriving at the
//! tip rewrites that row instead of adding one. A tool that reports a line per
//! file would otherwise turn a run's timeline into a copy of its own output,
//! with the state changes either side of it pushed off the screen.
//!
//! It is a fold and not a filter: nothing is dropped from the log, the count is
//! on the row, and `harkness run show` still prints every tick. Only
//! *consecutive* ticks fold, so anything the run recorded between two of them
//! keeps them apart. The row remembers the oldest tick it absorbed as well as
//! the newest, because backwards paging continues from a position in the log and
//! resuming from the newest would re-read the ticks the row already stands for.
//!
//! # The Qt-thread mutation invariant
//!
//! Every `RunTimelineModelRust` field is read and written on the Qt thread. The
//! store read, the subscription, and the blocking wait all live on a
//! `std::thread::spawn` worker whose results re-enter through
//! `qt_thread().queue(...)`.
//!
//! # The staleness counter
//!
//! `Selection` is `HarknessBackend::next_review_request`'s mechanism with one
//! addition: the subscriber thread outlives the call that started it, so the
//! counter is shared and atomic so a worker can read it too. Selecting another
//! run advances it, which both drops every queued reply for the old run and ends
//! the thread that was producing them. `loadOlder` and `loadDetail` deliberately
//! do *not* advance it — they belong to the selection already in progress.

#[cxx_qt::bridge]
pub mod ffi {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qbytearray.h");
        type QByteArray = cxx_qt_lib::QByteArray;

        include!("cxx-qt-lib/qhash_i32_QByteArray.h");
        type QHash_i32_QByteArray = cxx_qt_lib::QHash<cxx_qt_lib::QHashPair_i32_QByteArray>;

        include!("cxx-qt-lib/qmodelindex.h");
        type QModelIndex = cxx_qt_lib::QModelIndex;

        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;

        include!("cxx-qt-lib/qvariant.h");
        type QVariant = cxx_qt_lib::QVariant;

        include!("listmodelbase.h");
        type RunTimelineModelBase;

        #[rust_name = "begin_insert"]
        fn beginInsert(self: Pin<&mut RunTimelineModelBase>, first: i32, last: i32);

        #[rust_name = "end_insert"]
        fn endInsert(self: Pin<&mut RunTimelineModelBase>);

        #[rust_name = "begin_remove"]
        fn beginRemove(self: Pin<&mut RunTimelineModelBase>, first: i32, last: i32);

        #[rust_name = "end_remove"]
        fn endRemove(self: Pin<&mut RunTimelineModelBase>);

        #[rust_name = "begin_reset"]
        fn beginReset(self: Pin<&mut RunTimelineModelBase>);

        #[rust_name = "end_reset"]
        fn endReset(self: Pin<&mut RunTimelineModelBase>);

        #[rust_name = "emit_changed"]
        fn changed(self: Pin<&mut RunTimelineModelBase>, first: i32, last: i32);
    }

    extern "RustQt" {
        /// One run's event log, oldest first.
        ///
        /// cxx-qt does not convert names to camel case, so property names are
        /// kept to a single word and every multi-word member names its Qt
        /// spelling explicitly. `run` is the selected run, `loading` is true
        /// while a page is in flight, `live` while the subscription is
        /// delivering, `more` says whether an older page can be loaded, and
        /// `status` carries the last failure or bound the model hit.
        #[qobject]
        #[qml_element]
        #[base = RunTimelineModelBase]
        #[qproperty(QString, run)]
        #[qproperty(bool, loading)]
        #[qproperty(bool, live)]
        #[qproperty(bool, more)]
        #[qproperty(QString, status)]
        #[qproperty(QString, kind)]
        type RunTimelineModel = super::RunTimelineModelRust;

        #[cxx_override]
        fn data(self: &RunTimelineModel, index: &QModelIndex, role: i32) -> QVariant;

        #[cxx_override]
        #[cxx_name = "rowCount"]
        fn row_count(self: &RunTimelineModel, parent: &QModelIndex) -> i32;

        #[cxx_override]
        #[cxx_name = "roleNames"]
        fn role_names(self: &RunTimelineModel) -> QHash_i32_QByteArray;

        /// Shows `run_id`'s timeline, discarding whatever was shown before.
        ///
        /// Passing an empty string clears the model and stops the subscription.
        #[qinvokable]
        fn select(self: Pin<&mut RunTimelineModel>, run_id: &QString);

        /// Reloads the selected run from its newest page.
        #[qinvokable]
        fn refresh(self: Pin<&mut RunTimelineModel>);

        /// Loads one page of events older than the oldest row held.
        #[qinvokable]
        #[cxx_name = "loadOlder"]
        fn load_older(self: Pin<&mut RunTimelineModel>);

        /// Attaches one event's full payload to its row.
        #[qinvokable]
        #[cxx_name = "loadDetail"]
        fn load_detail(self: Pin<&mut RunTimelineModel>, seq: i64);
    }

    impl cxx_qt::Threading for RunTimelineModel {}
}

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use cxx_qt::{CxxQtType, Threading, casting::Upcast};
use cxx_qt_lib::{QByteArray, QHash, QHashPair_i32_QByteArray, QModelIndex, QString, QVariant};
use serde_json::Value;

use harkness_runtime::coordinator::{
    EventDelivery, EventReceiver, ReceiveTimeoutError, TryReceiveError,
};
use harkness_runtime::domain::RunId;
use harkness_runtime::store::{EventListing, EventPage, EventSeq, RunEvent};

use super::runs_backend::{
    RunsFailure, cached_coordinator, data_dir, note_qt_thread, parse_run, read_store, rfc3339,
};

/// Events loaded per page, in either direction.
///
/// The run store's own `DEFAULT_EVENT_PAGE_LIMIT`: the timeline latency target
/// is stated for a thousand-event run, and five bounded queries meet it without
/// asking the store to assemble its maximum page.
pub const TIMELINE_PAGE_SIZE: usize = 200;

/// Rows the model retains before it starts dropping the oldest.
///
/// Ten pages. A timeline is read from its newest end, and holding an unbounded
/// run's whole log to serve a scrollbar is how a long-running agent turns into a
/// memory leak. Passing it while appending drops from the oldest end and reopens
/// backwards paging from the new front, so nothing becomes unreachable — it is
/// re-read rather than retained.
pub const MAX_TIMELINE_ROWS: usize = 10 * TIMELINE_PAGE_SIZE;

/// Longest one-line summary a row carries.
const MAX_TIMELINE_SUMMARY_BYTES: usize = 240;

/// Longest payload rendering `loadDetail` hands to QML.
const MAX_TIMELINE_DETAIL_BYTES: usize = 8 * 1024;

/// How long the subscriber blocks before re-checking whether it is still wanted.
const SUBSCRIPTION_POLL: Duration = Duration::from_millis(100);

/// `Qt::DisplayRole`, so a row reads as its summary in accessibility tooling.
const DISPLAY_ROLE: i32 = 0;
/// `Qt::UserRole + 1` and up: the roles QML delegates bind to.
const SEQ_ROLE: i32 = 257;
const KIND_ROLE: i32 = 258;
const RECOGNIZED_ROLE: i32 = 259;
const AT_ROLE: i32 = 260;
const STEP_ROLE: i32 = 261;
const TOOL_CALL_ROLE: i32 = 262;
const ARTIFACT_ROLE: i32 = 263;
const SUMMARY_ROLE: i32 = 264;
const HAS_DETAIL_ROLE: i32 = 265;
const DETAIL_ROLE: i32 = 266;
const PROGRESS_COUNT_ROLE: i32 = 267;

/// The stored spelling of the one kind this model folds.
///
/// Named rather than written at each site because the folding rule below is the
/// only place in the front end that branches on a kind at all, and a typo in one
/// of three string literals would silently stop it folding.
const TOOL_PROGRESS_KIND: &str = "tool_progress";

fn model_roles() -> QHash<QHashPair_i32_QByteArray> {
    let mut roles = QHash::<QHashPair_i32_QByteArray>::default();
    roles.insert(DISPLAY_ROLE, QByteArray::from("display"));
    roles.insert(SEQ_ROLE, QByteArray::from("seq"));
    roles.insert(KIND_ROLE, QByteArray::from("kind"));
    roles.insert(RECOGNIZED_ROLE, QByteArray::from("recognized"));
    roles.insert(AT_ROLE, QByteArray::from("at"));
    roles.insert(STEP_ROLE, QByteArray::from("stepId"));
    roles.insert(TOOL_CALL_ROLE, QByteArray::from("toolCallId"));
    roles.insert(ARTIFACT_ROLE, QByteArray::from("artifactId"));
    roles.insert(SUMMARY_ROLE, QByteArray::from("summary"));
    roles.insert(HAS_DETAIL_ROLE, QByteArray::from("hasDetail"));
    roles.insert(DETAIL_ROLE, QByteArray::from("detail"));
    roles.insert(PROGRESS_COUNT_ROLE, QByteArray::from("progressCount"));
    roles
}

/// One event as a delegate draws it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct TimelineRow {
    seq: u64,
    /// Sequence of the oldest event this row stands for.
    ///
    /// Equal to `seq` for every row but a folded run of progress ticks, where
    /// it is the first tick's. Backwards paging continues from *this* number
    /// rather than from `seq`, because a cursor of the newest folded tick would
    /// re-read the ticks the row already absorbed and show them again as rows
    /// of their own.
    first_seq: u64,
    kind: String,
    recognized: bool,
    at: String,
    step_id: String,
    tool_call_id: String,
    artifact_id: String,
    summary: String,
    /// Whether the event carries a payload `loadDetail` could fetch.
    has_detail: bool,
    /// The payload rendering, empty until `loadDetail` attached it.
    detail: String,
    /// How many progress ticks this row stands for; zero for anything else.
    ///
    /// One for a single tick, so a reader can tell "a progress event" from "a
    /// row that is not about progress" without matching on the kind text.
    progress_count: u32,
}

/// Keeps a rendering inside its byte budget on a character boundary.
fn clamp(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_owned();
    }
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Renders one payload field, never recursing into a nested structure.
///
/// A summary says *what happened*, and a nested object rendered inline is how a
/// one-line summary becomes a screenful. The size of the structure is reported
/// instead, and the structure itself is what `loadDetail` is for.
fn field_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        Value::Array(items) => format!("[{} items]", items.len()),
        Value::Object(fields) => format!("{{{} fields}}", fields.len()),
    }
}

/// Renders a payload as one bounded line.
///
/// Object fields are emitted in sorted key order rather than in the map's own.
/// `serde_json::Map` is an insertion-ordered `IndexMap` in this workspace, so
/// map order is a property of whoever wrote the payload; two events saying the
/// same thing must not read differently because their fields were built in a
/// different order.
pub(crate) fn summarize(payload: &Value) -> String {
    let line = match payload {
        Value::Null => String::new(),
        Value::Object(fields) => {
            let mut parts: Vec<String> = fields
                .iter()
                .map(|(key, value)| format!("{key}={}", field_value(value)))
                .collect();
            parts.sort();
            parts.join(" ")
        }
        other => field_value(other),
    };
    clamp(&line, MAX_TIMELINE_SUMMARY_BYTES)
}

/// Projects one stored event into a row.
///
/// The payload becomes a summary and is then dropped: a row must not carry the
/// 64 KiB an event payload may hold, and `loadDetail` is how a reader asks for
/// one of them back.
pub(crate) fn event_row(seq: u64, event: &RunEvent) -> TimelineRow {
    TimelineRow {
        seq,
        first_seq: seq,
        progress_count: u32::from(event.kind().as_str() == TOOL_PROGRESS_KIND),
        kind: event.kind().as_str().to_owned(),
        recognized: event.kind().is_recognized(),
        at: rfc3339(event.at()),
        step_id: event.step_id().map(|id| id.to_string()).unwrap_or_default(),
        tool_call_id: event
            .tool_call_id()
            .map(|id| id.to_string())
            .unwrap_or_default(),
        artifact_id: event
            .artifact_id()
            .map(|id| id.to_string())
            .unwrap_or_default(),
        summary: summarize(event.payload()),
        has_detail: !event.payload().is_null(),
        detail: String::new(),
    }
}

/// Whether `row` is a progress tick that may fold into a neighbour.
///
/// A tick that names no tool call is not foldable: a row with nothing to fold
/// *into* would otherwise absorb the next unrelated tick and report a count
/// spanning two different pieces of work.
fn foldable(row: &TimelineRow) -> bool {
    row.progress_count > 0 && !row.tool_call_id.is_empty()
}

/// Whether `row` continues the run of ticks `previous` already stands for.
fn folds_into(previous: &TimelineRow, row: &TimelineRow) -> bool {
    foldable(previous) && foldable(row) && previous.tool_call_id == row.tool_call_id
}

/// Merges a later tick into the run of ticks an earlier row stands for.
///
/// The newest tick's text wins, because the line a reader wants from a running
/// tool is the one it printed last; the span the row covers keeps the oldest
/// tick's sequence so paging backwards resumes before it rather than inside it.
fn absorb(earlier: &TimelineRow, later: &TimelineRow) -> TimelineRow {
    let mut merged = later.clone();
    merged.first_seq = earlier.first_seq;
    merged.progress_count = earlier.progress_count.saturating_add(later.progress_count);
    merged
}

/// Collapses each run of consecutive progress ticks of one call into one row.
///
/// A tool that reports a line per file turns a timeline into its own output, so
/// consecutive ticks of one call become a single row carrying the newest line
/// and how many there were. Only *consecutive* ticks fold: anything else the run
/// recorded between two of them — a state change, an artifact, an approval — is
/// a thing that happened, and folding across it would put two ticks in one row
/// while claiming nothing happened in between.
///
/// This is presentation and not redaction. The events are in the append-only
/// log exactly as they were written, `harkness run show` prints every one of
/// them, and the folded row names the count so the reader knows what it stands
/// for.
fn fold(rows: Vec<TimelineRow>) -> Vec<TimelineRow> {
    let mut folded: Vec<TimelineRow> = Vec::with_capacity(rows.len());
    for row in rows {
        match folded.last() {
            Some(previous) if folds_into(previous, &row) => {
                let merged = absorb(previous, &row);
                *folded
                    .last_mut()
                    .expect("the branch above matched on the same last row") = merged;
            }
            _ => folded.push(row),
        }
    }
    folded
}

/// One step of a model mutation, in the row coordinates that hold when it is
/// applied. Steps are recorded in the order they must be applied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ModelEdit {
    Insert {
        first: usize,
        rows: Vec<TimelineRow>,
    },
    Remove {
        first: usize,
        last: usize,
    },
    Update {
        first: usize,
        rows: Vec<TimelineRow>,
    },
}

/// Filters, orders, and deduplicates an arriving batch.
///
/// Ordering here rather than trusting the producer is what makes an insert span
/// contiguous whatever order a batch was drained in; a repeated sequence is
/// dropped rather than inserted twice, because `(run_id, seq)` names one event
/// and a timeline showing it twice is a timeline that lies.
fn ordered(batch: Vec<TimelineRow>, keep: impl Fn(&TimelineRow) -> bool) -> Vec<TimelineRow> {
    let mut fresh: Vec<TimelineRow> = batch.into_iter().filter(|row| keep(row)).collect();
    fresh.sort_by_key(|row| row.seq);
    fresh.dedup_by_key(|row| row.seq);
    fresh
}

/// The rows a timeline holds and the two facts that bound them.
///
/// Free of Qt so every ordering, dedupe, and trimming rule is decided — and
/// tested — without a `QGuiApplication`.
#[derive(Debug, Default)]
pub(crate) struct Timeline {
    rows: Vec<TimelineRow>,
    /// Highest sequence ever applied. An event at or below it is a repeat.
    last_seq: u64,
    /// Whether a backwards page reported it reached the run's first event.
    beginning: bool,
}

impl Timeline {
    /// Replaces every row with a freshly loaded page.
    fn reset(&mut self, rows: Vec<TimelineRow>, beginning: bool) {
        self.last_seq = rows.iter().map(|row| row.seq).max().unwrap_or_default();
        self.rows = fold(rows);
        self.beginning = beginning;
    }

    /// Plans an append, dropping repeats and trimming past the retained bound.
    ///
    /// The trim is planned in the coordinates that hold *after* the insert,
    /// because the two are applied in order.
    pub(crate) fn plan_append(&self, batch: Vec<TimelineRow>) -> Vec<ModelEdit> {
        let applied = self.last_seq;
        let mut fresh = fold(ordered(batch, |row| row.seq > applied));
        if fresh.is_empty() {
            return Vec::new();
        }
        let mut edits = Vec::with_capacity(3);
        // A tick continuing the run of ticks already at the tip rewrites that
        // row instead of adding one, which is what makes a tool reporting a
        // hundred lines cost the reader no new rows and no scroll position.
        if self
            .rows
            .last()
            .is_some_and(|last| folds_into(last, &fresh[0]))
        {
            let head = fresh.remove(0);
            let last = self
                .rows
                .last()
                .expect("the condition above read the same last row");
            edits.push(ModelEdit::Update {
                first: self.rows.len() - 1,
                rows: vec![absorb(last, &head)],
            });
        }
        let length = self.rows.len() + fresh.len();
        if !fresh.is_empty() {
            edits.push(ModelEdit::Insert {
                first: self.rows.len(),
                rows: fresh,
            });
        }
        if length > MAX_TIMELINE_ROWS {
            edits.push(ModelEdit::Remove {
                first: 0,
                last: length - MAX_TIMELINE_ROWS - 1,
            });
        }
        edits
    }

    /// Plans a backwards page, dropping anything the model already holds.
    ///
    /// Ticks fold within the arriving page but never into the row above it: the
    /// row a backwards page would merge into is one the reader is already
    /// looking at, and rewriting it while they read is a worse outcome than one
    /// extra row at a page boundary.
    pub(crate) fn plan_prepend(&self, batch: Vec<TimelineRow>) -> Vec<ModelEdit> {
        let boundary = self.rows.first().map_or(u64::MAX, |row| row.first_seq);
        let older = fold(ordered(batch, |row| row.seq < boundary));
        if older.is_empty() {
            return Vec::new();
        }
        vec![ModelEdit::Insert {
            first: 0,
            rows: older,
        }]
    }

    /// Plans attaching one event's payload to the row that names it.
    pub(crate) fn plan_detail(&self, seq: u64, detail: String) -> Option<ModelEdit> {
        let first = self.rows.iter().position(|row| row.seq == seq)?;
        if self.rows[first].detail == detail {
            return None;
        }
        let mut row = self.rows[first].clone();
        row.detail = detail;
        Some(ModelEdit::Update {
            first,
            rows: vec![row],
        })
    }

    /// Applies one planned edit to the rows.
    pub(crate) fn apply(&mut self, edit: &ModelEdit) {
        match edit {
            ModelEdit::Insert { first, rows } => {
                self.last_seq = self
                    .last_seq
                    .max(rows.iter().map(|row| row.seq).max().unwrap_or_default());
                self.rows.splice(*first..*first, rows.iter().cloned());
            }
            ModelEdit::Remove { first, last } => {
                self.rows.drain(*first..=*last);
                // Only the oldest rows are ever dropped, so a model that had
                // walked back to the run's first event no longer holds it and
                // must stop claiming to. Without this a trimmed timeline would
                // report the log ended rather than that the bound was reached,
                // which is the difference `at_bound` exists to draw.
                self.beginning = false;
            }
            ModelEdit::Update { first, rows } => {
                // An update is not always a rewrite of what a row already
                // stood for: a folded progress row absorbs ticks the model has
                // not seen before, and forgetting to raise the watermark for
                // them would let the next delivery of the same ticks fold a
                // second time and double the count.
                self.last_seq = self
                    .last_seq
                    .max(rows.iter().map(|row| row.seq).max().unwrap_or_default());
                self.rows
                    .splice(*first..*first + rows.len(), rows.iter().cloned());
            }
        }
    }

    /// Whether a backwards page can still be loaded.
    ///
    /// False once the walk reached the run's first event, and false at the
    /// retained bound: prepending there would have to discard from the newest
    /// end, which is the live tail the reader is watching.
    pub(crate) fn more(&self) -> bool {
        !self.beginning && self.rows.len() < MAX_TIMELINE_ROWS
    }

    /// Whether the retained bound rather than the log's beginning stopped it.
    ///
    /// The two reasons `more` is false are not the same thing to say to a
    /// reader — one is "that is the whole run" and the other is "that is as far
    /// back as this window goes" — so the surface asks which one it was.
    pub(crate) fn at_bound(&self) -> bool {
        !self.beginning && self.rows.len() >= MAX_TIMELINE_ROWS
    }

    /// The exclusive boundary a backwards page continues from.
    fn older_cursor(&self) -> Option<EventSeq> {
        self.rows.first().map(|row| EventSeq::new(row.first_seq))
    }
}

/// One freshly loaded page and where it left the walk.
struct TimelinePage {
    rows: Vec<TimelineRow>,
    /// True only when the store said the walk reached the first event; an
    /// under-full page cannot say that on its own.
    beginning: bool,
    /// Whether a subscription is now delivering this run's later events.
    live: bool,
}

/// Turns a store listing into a page of rows.
///
/// The store returns a newest-first page in both directions; rows are
/// oldest-first, so the page is reversed exactly once, here.
fn timeline_page(listing: EventListing, live: bool) -> TimelinePage {
    TimelinePage {
        rows: listing
            .events
            .iter()
            .rev()
            .map(|stored| event_row(stored.seq.get(), &stored.event))
            .collect(),
        // Only the cursor may say the walk reached the run's first event; an
        // under-full page cannot say it on its own.
        beginning: listing.next_cursor.is_none(),
        live,
    }
}

/// Reads the newest page and, where there is one to have, opens the live stream.
fn open_timeline(run: RunId) -> Result<Option<(Option<EventReceiver>, TimelinePage)>, RunsFailure> {
    open_timeline_in(&data_dir()?, run)
}

/// Opens one run's timeline in a named data directory.
///
/// Split from [`open_timeline`] so a test can seed a temporary store and read it
/// back without touching `HARKNESS_DATA_DIR`, which is process-wide. `None`
/// means the directory has recorded nothing at all, which is a read answering
/// honestly rather than a read creating a store.
///
/// # Why the receiver is optional
///
/// A subscription is worth opening for exactly one kind of run: a non-terminal
/// one this process is already driving. A finished run will never publish
/// again, and a run some *other* process is driving cannot publish here —
/// `RunCoordinator::subscribe` would hand back a receiver that replays the
/// durable history and then waits forever for a worker that does not exist in
/// this process. Both cases read their page and stop, which is also what keeps
/// looking at a timeline from attaching this process's lease to the store.
fn open_timeline_in(
    data_dir: &std::path::Path,
    run: RunId,
) -> Result<Option<(Option<EventReceiver>, TimelinePage)>, RunsFailure> {
    let Some(store) = read_store(data_dir)? else {
        return Ok(None);
    };
    // An unknown run is an error rather than an empty page. A timeline is
    // always asked for by a caller that believes the run exists, and answering
    // "no events" would let a mistyped identifier read as an empty run.
    let record = store.load_run(run)?;
    let coordinator = cached_coordinator(data_dir)?;
    // Subscribed before the page is read, so an event recorded between the two
    // arrives on the stream instead of falling into the gap between them.
    let receiver = match coordinator {
        Some(coordinator) if !record.state().is_terminal() => Some(coordinator.subscribe(run)?),
        _ => None,
    };
    let listing = store.event_page(run, EventPage::newest(TIMELINE_PAGE_SIZE))?;
    // `live` is whether something is actually delivering, not whether the run
    // looks unfinished: a subscription that was never opened publishes nothing
    // however the run is recorded.
    let live = receiver.is_some();
    Ok(Some((receiver, timeline_page(listing, live))))
}

/// Reads one page older than `cursor`.
fn load_older_page(run: RunId, cursor: EventSeq) -> Result<TimelinePage, RunsFailure> {
    load_older_page_in(&data_dir()?, run, cursor)
}

fn load_older_page_in(
    data_dir: &std::path::Path,
    run: RunId,
    cursor: EventSeq,
) -> Result<TimelinePage, RunsFailure> {
    let Some(store) = read_store(data_dir)? else {
        return Ok(TimelinePage {
            rows: Vec::new(),
            beginning: true,
            live: false,
        });
    };
    store.load_run(run)?;
    let listing = store.event_page(run, EventPage::newest(TIMELINE_PAGE_SIZE).after(cursor))?;
    Ok(timeline_page(listing, false))
}

/// Re-reads one event and renders its payload.
fn load_event_detail(run: RunId, seq: u64) -> Result<Option<String>, RunsFailure> {
    load_event_detail_in(&data_dir()?, run, seq)
}

/// Reads one event's payload from a named data directory.
///
/// A one-row page positioned just before the wanted sequence, which is the
/// narrowest read the store's paging offers; the returned event is checked
/// against the sequence that was asked for, because a gap in the log — which the
/// log deliberately permits — would otherwise return the next event's payload
/// under the wrong row.
fn load_event_detail_in(
    data_dir: &std::path::Path,
    run: RunId,
    seq: u64,
) -> Result<Option<String>, RunsFailure> {
    let Some(store) = read_store(data_dir)? else {
        return Ok(None);
    };
    store.load_run(run)?;
    let page = EventPage::oldest(1).after(EventSeq::new(seq.saturating_sub(1)));
    let listing = store.event_page(run, page)?;
    Ok(listing
        .events
        .first()
        .filter(|stored| stored.seq.get() == seq)
        .map(|stored| {
            let payload = stored.event.payload();
            let rendered =
                serde_json::to_string_pretty(payload).unwrap_or_else(|_| payload.to_string());
            clamp(&rendered, MAX_TIMELINE_DETAIL_BYTES)
        }))
}

/// Which selection the model is currently showing.
///
/// The staleness counter every reply is gated on, shared rather than held as a
/// plain field for one reason: the subscriber thread outlives the `select` that
/// started it, so it has to be able to notice that it is no longer wanted. The
/// Qt thread is the only writer; a worker only ever reads.
#[derive(Clone, Debug, Default)]
pub(crate) struct Selection(Arc<AtomicU64>);

impl Selection {
    /// Starts a new selection and returns the number it is known by.
    ///
    /// Called on the Qt thread before anything is queued or spawned, so every
    /// reply and every subscriber belonging to the previous selection is
    /// already superseded the next time it looks.
    pub(crate) fn advance(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// The number the current selection is known by.
    pub(crate) fn current(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    /// Whether work numbered `request` still describes what is being shown.
    pub(crate) fn describes(&self, request: u64) -> bool {
        self.current() == request
    }
}

#[derive(Default)]
pub struct RunTimelineModelRust {
    run: QString,
    loading: bool,
    live: bool,
    more: bool,
    status: QString,
    kind: QString,
    timeline: Timeline,
    selection: Selection,
}

impl ffi::RunTimelineModel {
    fn data(&self, index: &QModelIndex, role: i32) -> QVariant {
        if !index.is_valid() {
            return QVariant::default();
        }
        let Ok(row) = usize::try_from(index.row()) else {
            return QVariant::default();
        };
        let Some(entry) = self.rust().timeline.rows.get(row) else {
            return QVariant::default();
        };
        let text = |value: &str| QVariant::from(&QString::from(value));
        match role {
            DISPLAY_ROLE | SUMMARY_ROLE => text(&entry.summary),
            SEQ_ROLE => QVariant::from(&i64::try_from(entry.seq).unwrap_or(i64::MAX)),
            KIND_ROLE => text(&entry.kind),
            RECOGNIZED_ROLE => QVariant::from(&entry.recognized),
            AT_ROLE => text(&entry.at),
            STEP_ROLE => text(&entry.step_id),
            TOOL_CALL_ROLE => text(&entry.tool_call_id),
            ARTIFACT_ROLE => text(&entry.artifact_id),
            HAS_DETAIL_ROLE => QVariant::from(&entry.has_detail),
            DETAIL_ROLE => text(&entry.detail),
            PROGRESS_COUNT_ROLE => {
                QVariant::from(&i32::try_from(entry.progress_count).unwrap_or(i32::MAX))
            }
            _ => QVariant::default(),
        }
    }

    fn row_count(&self, parent: &QModelIndex) -> i32 {
        // A list model has rows only below its invisible root.
        if parent.is_valid() {
            return 0;
        }
        i32::try_from(self.rust().timeline.rows.len()).unwrap_or(i32::MAX)
    }

    fn role_names(&self) -> QHash<QHashPair_i32_QByteArray> {
        model_roles()
    }

    fn select(mut self: Pin<&mut Self>, run_id: &QString) {
        note_qt_thread();
        let run_id = run_id.to_string();
        self.as_mut().set_run(QString::from(run_id.as_str()));
        self.as_mut().clear();
        // Nothing is delivering until the new subscription opens, and the
        // previous run's is dead the moment the selection advances.
        self.as_mut().set_live(false);
        let selection = self.as_ref().rust().selection.clone();
        let request = selection.advance();
        if run_id.is_empty() {
            self.as_mut().set_loading(false);
            return;
        }
        self.as_mut().set_loading(true);
        self.as_mut().set_status(QString::default());
        self.as_mut().set_kind(QString::default());
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let run = match parse_run(&run_id) {
                Ok(run) => run,
                Err(failure) => {
                    let _ = qt_thread.queue(move |model| report(model, request, &failure));
                    return;
                }
            };
            let opened = match open_timeline(run) {
                Ok(opened) => opened,
                Err(failure) => {
                    let _ = qt_thread.queue(move |model| report(model, request, &failure));
                    return;
                }
            };
            let Some((receiver, page)) = opened else {
                let _ = qt_thread.queue(move |model| {
                    apply_page(
                        model,
                        request,
                        TimelinePage {
                            rows: Vec::new(),
                            beginning: true,
                            live: false,
                        },
                    );
                });
                return;
            };
            // Everything the page already shows is old news on the stream. The
            // model drops repeats too, but filtering here keeps a replayed
            // history off the Qt thread's queue entirely.
            let floor = page.rows.last().map_or(0, |row| row.seq);
            let _ = qt_thread.queue(move |model| apply_page(model, request, page));
            if let Some(receiver) = receiver {
                deliver(&receiver, run, floor, request, &selection, &qt_thread);
            }
        });
    }

    fn refresh(mut self: Pin<&mut Self>) {
        let run = self.as_ref().run().clone();
        self.as_mut().select(&run);
    }

    fn load_older(mut self: Pin<&mut Self>) {
        if self.as_ref().rust().loading {
            return;
        }
        if self.as_ref().rust().timeline.at_bound() {
            self.as_mut().set_status(QString::from(
                format!("Showing the most recent {MAX_TIMELINE_ROWS} entries of this run").as_str(),
            ));
            return;
        }
        let Some(cursor) = self.as_ref().rust().timeline.older_cursor() else {
            return;
        };
        let Ok(run) = self.as_ref().run().to_string().parse::<RunId>() else {
            return;
        };
        note_qt_thread();
        // Deliberately no advance: paging backwards belongs to the selection
        // already in progress and must not end its live subscription.
        let request = self.as_ref().rust().selection.current();
        self.as_mut().set_loading(true);
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let outcome = load_older_page(run, cursor);
            let _ = qt_thread.queue(move |model| match outcome {
                Ok(page) => prepend_page(model, request, page),
                Err(failure) => report(model, request, &failure),
            });
        });
    }

    fn load_detail(self: Pin<&mut Self>, seq: i64) {
        let Ok(seq) = u64::try_from(seq) else {
            return;
        };
        let Ok(run) = self.as_ref().run().to_string().parse::<RunId>() else {
            return;
        };
        note_qt_thread();
        let request = self.as_ref().rust().selection.current();
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let outcome = load_event_detail(run, seq);
            let _ = qt_thread.queue(move |model| match outcome {
                Ok(Some(detail)) => attach_detail(model, request, seq, detail),
                Ok(None) => {}
                Err(failure) => report(model, request, &failure),
            });
        });
    }

    /// Empties the model, announcing the reset Qt requires around it.
    fn clear(mut self: Pin<&mut Self>) {
        {
            let base: Pin<&mut ffi::RunTimelineModelBase> = self.as_mut().upcast_pin();
            base.begin_reset();
        }
        self.as_mut().rust_mut().get_mut().timeline = Timeline::default();
        {
            let base: Pin<&mut ffi::RunTimelineModelBase> = self.as_mut().upcast_pin();
            base.end_reset();
        }
        self.as_mut().set_more(false);
    }

    /// Applies one planned edit, wrapping the row mutation in the notification
    /// Qt requires for it.
    fn edit(mut self: Pin<&mut Self>, edit: &ModelEdit) {
        match edit {
            ModelEdit::Insert { first, rows } => {
                let last = first + rows.len() - 1;
                {
                    let base: Pin<&mut ffi::RunTimelineModelBase> = self.as_mut().upcast_pin();
                    base.begin_insert(*first as i32, last as i32);
                }
                self.as_mut().rust_mut().get_mut().timeline.apply(edit);
                let base: Pin<&mut ffi::RunTimelineModelBase> = self.as_mut().upcast_pin();
                base.end_insert();
            }
            ModelEdit::Remove { first, last } => {
                {
                    let base: Pin<&mut ffi::RunTimelineModelBase> = self.as_mut().upcast_pin();
                    base.begin_remove(*first as i32, *last as i32);
                }
                self.as_mut().rust_mut().get_mut().timeline.apply(edit);
                let base: Pin<&mut ffi::RunTimelineModelBase> = self.as_mut().upcast_pin();
                base.end_remove();
            }
            ModelEdit::Update { first, rows } => {
                let last = first + rows.len() - 1;
                self.as_mut().rust_mut().get_mut().timeline.apply(edit);
                let base: Pin<&mut ffi::RunTimelineModelBase> = self.as_mut().upcast_pin();
                base.emit_changed(*first as i32, last as i32);
            }
        }
    }
}

/// Blocks on the subscription, coalescing whatever else is already queued.
///
/// One `qt_thread.queue` per drained batch is the whole point: a hundred events
/// delivered while the Qt thread was busy become one insert span rather than a
/// hundred, and the Qt thread never blocks on the run.
fn deliver(
    receiver: &EventReceiver,
    run: RunId,
    floor: u64,
    request: u64,
    selection: &Selection,
    qt_thread: &cxx_qt::CxxQtThread<ffi::RunTimelineModel>,
) {
    loop {
        // Three things end this thread. Selecting another run is the ordinary
        // one. A destroyed model is the one that would otherwise leak: the
        // worker holds its own clone of the selection counter, so nothing about
        // dropping the model moves it, and a run that never reaches a terminal
        // state never disconnects either — the thread would poll for the life
        // of the process, one per timeline the user ever opened. The third is a
        // queue that fails below, which says the same thing without the race.
        if !selection.describes(request) || qt_thread.is_destroyed() {
            return;
        }
        let first = match receiver.recv_timeout(SUBSCRIPTION_POLL) {
            Ok(delivery) => delivery,
            Err(ReceiveTimeoutError::Timeout) => continue,
            Err(ReceiveTimeoutError::Disconnected) => {
                let _ = qt_thread.queue(move |model| settle(model, request));
                return;
            }
        };
        let mut rows = Vec::new();
        let mut lost = collect(first, &mut rows, run, floor);
        // Everything already queued joins this batch, which is what makes a
        // burst one insert span rather than one per event. The selection is
        // re-checked inside the drain because a subscription opened on a long
        // run replays its whole log, and that replay must not outlive the run
        // being deselected part-way through it.
        while !lost && selection.describes(request) {
            match receiver.try_recv() {
                Ok(delivery) => lost = collect(delivery, &mut rows, run, floor),
                Err(TryReceiveError::Empty | TryReceiveError::Disconnected) => break,
            }
        }
        if !rows.is_empty()
            && qt_thread
                .queue(move |model| append_batch(model, request, rows))
                .is_err()
        {
            // The model is gone; nothing will ever read this stream again.
            return;
        }
        if lost {
            // A subscriber is closed once it lags, so there is nothing left to
            // drain and nothing more this thread can do.
            let _ = qt_thread.queue(move |model| lagged(model, request));
            return;
        }
    }
}

/// Adds one delivery to a batch, reporting whether the stream was lost.
///
/// Projected and filtered here rather than after the drain, and that ordering
/// is what keeps the batch bounded. A subscription replays the run's whole log
/// before it reaches live events, so retaining the raw deliveries first would
/// hold every payload of a five-thousand-event run in memory — up to 64 KiB
/// each — only to discard all of them against `floor` a moment later. A row is
/// a handful of short strings and an event at or below `floor` becomes nothing
/// at all.
fn collect(delivery: EventDelivery, rows: &mut Vec<TimelineRow>, run: RunId, floor: u64) -> bool {
    match delivery {
        // A subscription is per run, but the run is checked rather than assumed:
        // a row rendered under the wrong run is not re-checked by anything.
        EventDelivery::Event(stored) if stored.run_id == run => {
            if stored.seq.get() > floor {
                rows.push(event_row(stored.seq.get(), &stored.event));
            }
            false
        }
        EventDelivery::Event(_) => false,
        _ => true,
    }
}

/// Whether a queued reply still describes the selection the model is showing.
fn is_current(model: &Pin<&mut ffi::RunTimelineModel>, request: u64) -> bool {
    model.as_ref().rust().selection.describes(request)
}

/// Records a failed load without disturbing the rows or the subscription.
///
/// `live` is deliberately untouched: a backwards page or a payload that could
/// not be read says nothing about whether the stream is still delivering, and
/// the paths where it genuinely is not — a selection that never opened one, a
/// disconnect, a lag — say so themselves.
fn report(mut model: Pin<&mut ffi::RunTimelineModel>, request: u64, failure: &RunsFailure) {
    if !is_current(&model, request) {
        return;
    }
    model.as_mut().set_loading(false);
    model
        .as_mut()
        .set_status(QString::from(failure.message.as_str()));
    // The discriminant travels beside the message, so a surface can tell a
    // run that is not stored from a store it could not read.
    model
        .as_mut()
        .set_kind(QString::from(failure.kind.as_str()));
}

/// Records that the run stopped publishing without disturbing its rows.
fn settle(mut model: Pin<&mut ffi::RunTimelineModel>, request: u64) {
    if !is_current(&model, request) {
        return;
    }
    model.as_mut().set_live(false);
}

/// Records that the live stream fell behind and was disconnected.
///
/// Deliberately does *not* reload. A subscriber lags because events arrive
/// faster than this process drains them, and a reload that re-subscribes to the
/// same flood lags again — an automatic one would be a loop of full page reads
/// for exactly the run that is already outrunning the reader. `live` going false
/// beside the message is what a surface offers a reload button on.
fn lagged(mut model: Pin<&mut ffi::RunTimelineModel>, request: u64) {
    if !is_current(&model, request) {
        return;
    }
    model.as_mut().set_live(false);
    model.as_mut().set_status(QString::from(
        "The live timeline fell behind; reload to see the rest of this run",
    ));
}

fn apply_page(mut model: Pin<&mut ffi::RunTimelineModel>, request: u64, page: TimelinePage) {
    if !is_current(&model, request) {
        return;
    }
    {
        let base: Pin<&mut ffi::RunTimelineModelBase> = model.as_mut().upcast_pin();
        base.begin_reset();
    }
    model
        .as_mut()
        .rust_mut()
        .get_mut()
        .timeline
        .reset(page.rows, page.beginning);
    {
        let base: Pin<&mut ffi::RunTimelineModelBase> = model.as_mut().upcast_pin();
        base.end_reset();
    }
    model.as_mut().set_loading(false);
    model.as_mut().set_live(page.live);
    let more = model.as_ref().rust().timeline.more();
    model.as_mut().set_more(more);
}

fn prepend_page(mut model: Pin<&mut ffi::RunTimelineModel>, request: u64, page: TimelinePage) {
    if !is_current(&model, request) {
        return;
    }
    model.as_mut().set_loading(false);
    // Bound before the loop: a temporary in a `for` head lives for the whole
    // loop, and this one borrows the object the body mutates.
    let edits = model.as_ref().rust().timeline.plan_prepend(page.rows);
    for edit in edits {
        model.as_mut().edit(&edit);
    }
    if page.beginning {
        model.as_mut().rust_mut().get_mut().timeline.beginning = true;
    }
    let more = model.as_ref().rust().timeline.more();
    model.as_mut().set_more(more);
}

fn append_batch(mut model: Pin<&mut ffi::RunTimelineModel>, request: u64, rows: Vec<TimelineRow>) {
    if !is_current(&model, request) {
        return;
    }
    let edits = model.as_ref().rust().timeline.plan_append(rows);
    for edit in edits {
        model.as_mut().edit(&edit);
    }
    let more = model.as_ref().rust().timeline.more();
    model.as_mut().set_more(more);
}

fn attach_detail(
    mut model: Pin<&mut ffi::RunTimelineModel>,
    request: u64,
    seq: u64,
    detail: String,
) {
    if !is_current(&model, request) {
        return;
    }
    let planned = model.as_ref().rust().timeline.plan_detail(seq, detail);
    if let Some(edit) = planned {
        model.as_mut().edit(&edit);
    }
}

#[cfg(test)]
mod tests {
    use cxx_qt_lib::QByteArray;
    use serde_json::json;
    use time::OffsetDateTime;

    use harkness_runtime::domain::{
        ArtifactId, ExecutionState, Run, RunId, StepId, Task, TaskId, ToolCallId,
    };
    use harkness_runtime::store::{EventKind, EventSeq, RunEvent, Store};
    use tempfile::TempDir;

    use super::{
        ARTIFACT_ROLE, AT_ROLE, DETAIL_ROLE, DISPLAY_ROLE, HAS_DETAIL_ROLE, KIND_ROLE,
        MAX_TIMELINE_ROWS, MAX_TIMELINE_SUMMARY_BYTES, ModelEdit, PROGRESS_COUNT_ROLE,
        RECOGNIZED_ROLE, SEQ_ROLE, STEP_ROLE, SUMMARY_ROLE, Selection, TIMELINE_PAGE_SIZE,
        TOOL_CALL_ROLE, Timeline, TimelineRow, event_row, model_roles, summarize,
    };
    use super::{load_event_detail_in, load_older_page_in, open_timeline_in};

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_755_000_000 + seconds).unwrap()
    }

    fn row(seq: u64) -> TimelineRow {
        event_row(seq, &RunEvent::new(EventKind::ToolProgress, at(seq as i64)))
    }

    fn rows(range: std::ops::RangeInclusive<u64>) -> Vec<TimelineRow> {
        range.map(row).collect()
    }

    /// One progress tick attributed to `call`, which is what makes it foldable.
    fn tick(seq: u64, call: ToolCallId) -> TimelineRow {
        event_row(
            seq,
            &RunEvent::new(EventKind::ToolProgress, at(seq as i64))
                .for_step(StepId::new())
                .for_tool_call(call),
        )
    }

    /// Anything that is not a progress tick, on the same call.
    fn other(seq: u64, call: ToolCallId) -> TimelineRow {
        event_row(
            seq,
            &RunEvent::new(EventKind::ToolCallStateChanged, at(seq as i64))
                .for_step(StepId::new())
                .for_tool_call(call)
                .with_payload(json!({"state": "running"})),
        )
    }

    fn populated(range: std::ops::RangeInclusive<u64>) -> Timeline {
        let mut timeline = Timeline::default();
        timeline.reset(rows(range), false);
        timeline
    }

    #[test]
    fn qml_roles_have_stable_names() {
        let roles = model_roles();

        for (role, name) in [
            (DISPLAY_ROLE, "display"),
            (SEQ_ROLE, "seq"),
            (KIND_ROLE, "kind"),
            (RECOGNIZED_ROLE, "recognized"),
            (AT_ROLE, "at"),
            (STEP_ROLE, "stepId"),
            (TOOL_CALL_ROLE, "toolCallId"),
            (ARTIFACT_ROLE, "artifactId"),
            (SUMMARY_ROLE, "summary"),
            (HAS_DETAIL_ROLE, "hasDetail"),
            (DETAIL_ROLE, "detail"),
            (PROGRESS_COUNT_ROLE, "progressCount"),
        ] {
            assert_eq!(roles.get(&role), Some(QByteArray::from(name)));
        }
    }

    #[test]
    fn a_row_carries_every_association_the_event_recorded() {
        let step = StepId::new();
        let call = ToolCallId::new();
        let artifact = ArtifactId::new();
        let event = RunEvent::new(EventKind::ArtifactCreated, at(1))
            .for_step(step)
            .for_tool_call(call)
            .for_artifact(artifact);

        let row = event_row(7, &event);

        assert_eq!(row.seq, 7);
        assert_eq!(row.kind, "artifact_created");
        assert!(row.recognized);
        assert_eq!(row.at, "2025-08-12T12:00:01Z");
        assert_eq!(row.step_id, step.to_string());
        assert_eq!(row.tool_call_id, call.to_string());
        assert_eq!(row.artifact_id, artifact.to_string());
    }

    #[test]
    fn an_unrecognized_kind_keeps_its_spelling_and_says_it_is_unknown() {
        let event = RunEvent::new(EventKind::parse("from_a_newer_build"), at(1));

        let row = event_row(1, &event);

        assert_eq!(row.kind, "from_a_newer_build");
        assert!(!row.recognized);
    }

    #[test]
    fn event_detail_is_absent_until_explicitly_loaded() {
        let event = RunEvent::new(EventKind::ToolProgress, at(1))
            .with_payload(json!({"stage": "compiling", "percent": 40}));

        let row = event_row(1, &event);

        assert_eq!(row.summary, "percent=40 stage=compiling");
        assert!(row.has_detail, "the payload is there to be loaded");
        assert_eq!(
            row.detail, "",
            "but no row carries it before it is asked for"
        );
    }

    #[test]
    fn an_event_with_no_payload_has_nothing_to_load() {
        let row = event_row(1, &RunEvent::new(EventKind::StepStarted, at(1)));

        assert_eq!(row.summary, "");
        assert!(!row.has_detail);
    }

    #[test]
    fn a_summary_reports_nested_structure_by_size_rather_than_inline() {
        let summary = summarize(&json!({
            "artifacts": [1, 2, 3],
            "result": {"passed": true, "code": 0},
        }));

        assert_eq!(summary, "artifacts=[3 items] result={2 fields}");
    }

    #[test]
    fn a_summary_is_bounded_however_long_the_payload_is() {
        let summary = summarize(&json!({"stderr": "x".repeat(64 * 1024)}));

        assert!(
            summary.len() <= MAX_TIMELINE_SUMMARY_BYTES + '…'.len_utf8(),
            "{} bytes",
            summary.len()
        );
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn a_summary_does_not_depend_on_the_order_the_payload_was_built_in() {
        let one = summarize(&json!({"b": 2, "a": 1}));
        let other = summarize(&json!({"a": 1, "b": 2}));

        assert_eq!(one, other);
    }

    #[test]
    fn a_burst_of_events_becomes_one_contiguous_insert() {
        let timeline = populated(1..=3);

        let edits = timeline.plan_append(rows(4..=103));

        assert_eq!(edits.len(), 1, "one drained batch is one insert range");
        match &edits[0] {
            ModelEdit::Insert { first, rows } => {
                assert_eq!(*first, 3);
                assert_eq!(rows.len(), 100);
                assert!(
                    rows.windows(2).all(|pair| pair[1].seq == pair[0].seq + 1),
                    "the inserted span is contiguous"
                );
            }
            other => panic!("unexpected edit: {other:?}"),
        }
    }

    #[test]
    fn duplicate_event_sequences_are_dropped_on_the_stream_page_overlap() {
        let timeline = populated(1..=10);

        let edits = timeline.plan_append(rows(6..=12));

        match &edits[0] {
            ModelEdit::Insert { first, rows } => {
                assert_eq!(*first, 10);
                assert_eq!(
                    rows.iter().map(|row| row.seq).collect::<Vec<_>>(),
                    vec![11, 12],
                    "everything the page already showed is a repeat"
                );
            }
            other => panic!("unexpected edit: {other:?}"),
        }
    }

    #[test]
    fn a_batch_of_nothing_new_plans_no_edit_at_all() {
        let timeline = populated(1..=10);

        assert!(timeline.plan_append(rows(1..=10)).is_empty());
    }

    #[test]
    fn consecutive_progress_ticks_of_one_call_occupy_one_row() {
        let call = ToolCallId::new();
        let mut timeline = Timeline::default();

        timeline.reset((1..=5).map(|seq| tick(seq, call)).collect(), true);

        assert_eq!(timeline.rows.len(), 1, "five ticks of one call are one row");
        assert_eq!(timeline.rows[0].progress_count, 5);
        assert_eq!(
            timeline.rows[0].seq, 5,
            "the newest tick's line is the one shown"
        );
        assert_eq!(
            timeline.rows[0].first_seq, 1,
            "the row still names where it began"
        );
    }

    #[test]
    fn progress_ticks_of_different_calls_stay_apart() {
        let first = ToolCallId::new();
        let second = ToolCallId::new();
        let mut timeline = Timeline::default();

        timeline.reset(vec![tick(1, first), tick(2, second), tick(3, first)], true);

        assert_eq!(
            timeline.rows.len(),
            3,
            "no two of these ticks are one call's"
        );
    }

    #[test]
    fn a_tick_after_another_event_starts_a_new_row() {
        let call = ToolCallId::new();
        let mut timeline = Timeline::default();

        timeline.reset(
            vec![tick(1, call), tick(2, call), other(3, call), tick(4, call)],
            true,
        );

        assert_eq!(
            timeline.rows.len(),
            3,
            "the state change keeps the ticks apart"
        );
        assert_eq!(timeline.rows[0].progress_count, 2);
        assert_eq!(timeline.rows[2].progress_count, 1);
    }

    #[test]
    fn a_tick_that_names_no_tool_call_is_never_folded() {
        let mut timeline = Timeline::default();

        // `row` builds a progress event with no association at all, which is
        // what a run-level progress line looks like. Folding those together
        // would report one count across two unrelated pieces of work.
        timeline.reset(rows(1..=4), true);

        assert_eq!(timeline.rows.len(), 4);
    }

    #[test]
    fn a_hundred_progress_ticks_add_one_row_and_then_none() {
        let call = ToolCallId::new();
        let mut timeline = Timeline::default();
        timeline.reset(vec![other(1, call)], true);
        let before = timeline.rows.len();

        for seq in 2..=101 {
            for edit in timeline.plan_append(vec![tick(seq, call)]) {
                timeline.apply(&edit);
            }
        }

        assert_eq!(
            timeline.rows.len(),
            before + 1,
            "the first tick added its row and the other ninety-nine rewrote it"
        );
        assert_eq!(timeline.rows.last().unwrap().progress_count, 100);
        assert_eq!(timeline.rows.last().unwrap().seq, 101);
    }

    #[test]
    fn a_progress_tick_at_the_tip_updates_the_row_rather_than_adding_one() {
        let call = ToolCallId::new();
        let mut timeline = Timeline::default();
        timeline.reset(vec![other(1, call), tick(2, call)], true);

        let edits = timeline.plan_append(vec![tick(3, call)]);

        assert!(
            matches!(edits.as_slice(), [ModelEdit::Update { first: 1, rows }] if rows[0].progress_count == 2),
            "{edits:?}"
        );
    }

    #[test]
    fn a_batch_that_both_extends_a_tick_and_moves_on_updates_before_it_inserts() {
        let call = ToolCallId::new();
        let mut timeline = Timeline::default();
        timeline.reset(vec![tick(1, call)], true);

        let edits = timeline.plan_append(vec![tick(2, call), other(3, call)]);

        assert!(
            matches!(
                edits.as_slice(),
                [
                    ModelEdit::Update { first: 0, .. },
                    ModelEdit::Insert { first: 1, rows }
                ] if rows.len() == 1
            ),
            "{edits:?}"
        );
    }

    #[test]
    fn a_tick_folded_into_a_row_is_not_counted_a_second_time() {
        let call = ToolCallId::new();
        let mut timeline = Timeline::default();
        timeline.reset(vec![tick(1, call)], true);
        for edit in timeline.plan_append(vec![tick(2, call)]) {
            timeline.apply(&edit);
        }

        // The subscription replays from where it was opened, so the same tick
        // arriving twice is ordinary rather than exceptional.
        let repeat = timeline.plan_append(vec![tick(2, call)]);

        assert!(repeat.is_empty(), "{repeat:?}");
        assert_eq!(timeline.rows[0].progress_count, 2);
    }

    #[test]
    fn paging_backwards_resumes_before_the_oldest_tick_a_folded_row_absorbed() {
        let call = ToolCallId::new();
        let mut timeline = Timeline::default();
        timeline.reset((5..=8).map(|seq| tick(seq, call)).collect(), false);

        // The store is asked for events older than this, and answers with the
        // page ending at it. A cursor of the newest folded tick would hand back
        // the ticks the row already stands for and show them all over again.
        assert_eq!(timeline.older_cursor().map(EventSeq::get), Some(5));
        let edits = timeline.plan_prepend(vec![other(4, call), tick(5, call)]);
        assert!(
            matches!(edits.as_slice(), [ModelEdit::Insert { first: 0, rows }] if rows.len() == 1),
            "the repeated tick is dropped and only the older event lands: {edits:?}"
        );
    }

    #[test]
    fn a_backwards_page_folds_within_itself_and_never_into_the_row_above() {
        let call = ToolCallId::new();
        let mut timeline = Timeline::default();
        timeline.reset((5..=8).map(|seq| tick(seq, call)).collect(), false);

        // Four older ticks of the same call. They are one row between
        // themselves, and deliberately stay a second row rather than merging
        // into the one already on screen: rewriting a row the reader is looking
        // at, and moving everything under it, is worse than a seam at a page
        // boundary that only costs them one row.
        let edits = timeline.plan_prepend((1..=4).map(|seq| tick(seq, call)).collect());

        assert!(
            matches!(
                edits.as_slice(),
                [ModelEdit::Insert { first: 0, rows }]
                    if rows.len() == 1
                        && rows[0].progress_count == 4
                        && rows[0].first_seq == 1
                        && rows[0].seq == 4
            ),
            "{edits:?}"
        );
        for edit in &edits {
            timeline.apply(edit);
        }
        assert_eq!(timeline.rows.len(), 2);
        assert_eq!(
            timeline.rows[1].progress_count, 4,
            "the page above is intact"
        );
    }

    #[test]
    fn a_batch_drained_out_of_order_still_inserts_in_sequence_order() {
        let timeline = populated(1..=2);
        let mut batch = rows(3..=6);
        batch.reverse();
        batch.push(row(4));

        let edits = timeline.plan_append(batch);

        match &edits[0] {
            ModelEdit::Insert { rows, .. } => assert_eq!(
                rows.iter().map(|row| row.seq).collect::<Vec<_>>(),
                vec![3, 4, 5, 6]
            ),
            other => panic!("unexpected edit: {other:?}"),
        }
    }

    #[test]
    fn appending_past_the_retained_bound_drops_the_oldest_rows() {
        let mut timeline = populated(1..=MAX_TIMELINE_ROWS as u64);
        let next = MAX_TIMELINE_ROWS as u64 + 1;

        let edits = timeline.plan_append(rows(next..=next + 9));

        assert_eq!(edits.len(), 2);
        assert_eq!(edits[1], ModelEdit::Remove { first: 0, last: 9 });
        for edit in &edits {
            timeline.apply(edit);
        }
        assert_eq!(timeline.rows.len(), MAX_TIMELINE_ROWS);
        assert_eq!(timeline.rows[0].seq, 11);
        assert_eq!(timeline.last_seq, next + 9);
    }

    #[test]
    fn a_trimmed_timeline_stops_claiming_it_holds_the_first_event() {
        let mut timeline = Timeline::default();
        timeline.reset(rows(1..=MAX_TIMELINE_ROWS as u64), true);
        assert!(!timeline.more(), "the walk had reached the first event");
        assert!(!timeline.at_bound(), "and the bound was not the reason");

        let next = MAX_TIMELINE_ROWS as u64 + 1;
        for edit in timeline.plan_append(rows(next..=next)) {
            timeline.apply(&edit);
        }

        assert!(!timeline.more());
        assert!(
            timeline.at_bound(),
            "the first event was dropped, so the window is what stops it now"
        );
    }

    #[test]
    fn a_backwards_page_lands_in_front_of_the_oldest_row() {
        let mut timeline = populated(201..=400);

        let edits = timeline.plan_prepend(rows(1..=200));

        assert_eq!(edits.len(), 1);
        for edit in &edits {
            timeline.apply(edit);
        }
        assert_eq!(timeline.rows.first().map(|row| row.seq), Some(1));
        assert_eq!(timeline.rows.len(), 400);
        assert_eq!(
            timeline.last_seq, 400,
            "an older page never moves the newest position"
        );
    }

    #[test]
    fn a_backwards_page_overlapping_what_is_held_inserts_only_the_older_part() {
        let timeline = populated(5..=10);

        let edits = timeline.plan_prepend(rows(1..=7));

        match &edits[0] {
            ModelEdit::Insert { first, rows } => {
                assert_eq!(*first, 0);
                assert_eq!(
                    rows.iter().map(|row| row.seq).collect::<Vec<_>>(),
                    vec![1, 2, 3, 4]
                );
            }
            other => panic!("unexpected edit: {other:?}"),
        }
    }

    #[test]
    fn reaching_the_retained_bound_stops_paging_backwards_without_claiming_the_end() {
        let mut timeline = Timeline::default();
        timeline.reset(rows(1..=MAX_TIMELINE_ROWS as u64), false);

        assert!(!timeline.more());
        assert!(timeline.at_bound(), "the bound stopped it, not the log");
    }

    #[test]
    fn a_page_that_reached_the_first_event_offers_no_older_page() {
        let mut timeline = Timeline::default();
        timeline.reset(rows(1..=3), true);

        assert!(!timeline.more());
        assert!(!timeline.at_bound(), "the log ended, the bound did not");
    }

    #[test]
    fn an_under_full_page_still_offers_an_older_one() {
        let mut timeline = Timeline::default();
        // Fewer rows than a page holds, and the store still reported a
        // continuation: only the cursor may say the walk reached the beginning.
        timeline.reset(rows(1..=3), false);

        assert!(timeline.more());
        assert!(timeline.rows.len() < TIMELINE_PAGE_SIZE);
    }

    #[test]
    fn loading_a_payload_changes_exactly_the_row_that_names_it() {
        let mut timeline = populated(1..=5);

        let edit = timeline
            .plan_detail(3, "{\n  \"stage\": \"linking\"\n}".to_owned())
            .expect("row 3 is held");
        timeline.apply(&edit);

        assert_eq!(
            edit,
            ModelEdit::Update {
                first: 2,
                rows: vec![timeline.rows[2].clone()]
            }
        );
        assert_eq!(timeline.rows[2].detail, "{\n  \"stage\": \"linking\"\n}");
        assert!(
            timeline
                .rows
                .iter()
                .enumerate()
                .all(|(index, row)| index == 2 || row.detail.is_empty()),
            "no other row gained a payload"
        );
    }

    #[test]
    fn loading_a_payload_for_an_event_no_longer_held_plans_nothing() {
        let timeline = populated(10..=20);

        assert!(timeline.plan_detail(1, "{}".to_owned()).is_none());
    }

    #[test]
    fn late_timeline_batches_for_a_deselected_run_are_discarded() {
        let selection = Selection::default();
        let first = selection.advance();
        assert!(selection.describes(first));

        let second = selection.advance();

        assert!(
            !selection.describes(first),
            "a reply for the run that was showing before must be dropped"
        );
        assert!(selection.describes(second));
    }

    #[test]
    fn paging_backwards_stays_inside_the_selection_it_belongs_to() {
        let selection = Selection::default();
        let request = selection.advance();

        // `loadOlder` and `loadDetail` read the current number rather than
        // taking a new one, so neither ends the live subscription `select`
        // started, and both are still superseded by the next selection.
        assert_eq!(selection.current(), request);
        assert!(selection.describes(selection.current()));
        selection.advance();
        assert!(!selection.describes(request));
    }

    #[test]
    fn attaching_the_payload_a_row_already_carries_plans_nothing() {
        let mut timeline = populated(1..=3);
        let edit = timeline.plan_detail(2, "{}".to_owned()).unwrap();
        timeline.apply(&edit);

        assert!(timeline.plan_detail(2, "{}".to_owned()).is_none());
    }

    /// Records one finished run carrying `events` progress entries.
    fn seed(data_dir: &std::path::Path, events: u64) -> RunId {
        let store = Store::open(data_dir).unwrap();
        let task = Task::with_id(
            TaskId::new(),
            "Check: cargo test",
            "/workspace/harkness",
            None,
            at(0),
        );
        store.insert_task(&task).unwrap();
        let run = Run::with_id(RunId::new(), task.id(), at(1));
        store.insert_run(&run).unwrap();
        for index in 0..events {
            store
                .append_event(
                    run.id(),
                    RunEvent::new(EventKind::ToolProgress, at(index as i64 + 2))
                        .with_payload(json!({"step": index})),
                )
                .unwrap();
        }
        store
            .transition_run(run.id(), ExecutionState::Running, at(2))
            .unwrap();
        store
            .transition_run(run.id(), ExecutionState::Succeeded, at(3))
            .unwrap();
        run.id()
    }

    #[test]
    fn a_timeline_opens_on_its_newest_page_in_oldest_first_order() {
        let fixture = TempDir::new().unwrap();
        let data_dir = fixture.path().join("data");
        let total = TIMELINE_PAGE_SIZE as u64 + 20;
        let run = seed(&data_dir, total);

        let (_receiver, page) = open_timeline_in(&data_dir, run).unwrap().expect("a store");

        assert_eq!(page.rows.len(), TIMELINE_PAGE_SIZE);
        assert_eq!(
            page.rows.first().map(|row| row.seq),
            Some(total - TIMELINE_PAGE_SIZE as u64 + 1),
            "the page opens on the newest entries, oldest of them first"
        );
        assert_eq!(page.rows.last().map(|row| row.seq), Some(total));
        assert!(!page.beginning, "there are older entries behind it");
        assert!(
            !page.live,
            "the seeded run already succeeded, so nothing is delivering for it"
        );
    }

    #[test]
    fn paging_backwards_reaches_the_first_event_and_says_so() {
        let fixture = TempDir::new().unwrap();
        let data_dir = fixture.path().join("data");
        let total = TIMELINE_PAGE_SIZE as u64 + 20;
        let run = seed(&data_dir, total);
        let (_receiver, page) = open_timeline_in(&data_dir, run).unwrap().expect("a store");
        let cursor = super::EventSeq::new(page.rows.first().expect("a page").seq);

        let older = load_older_page_in(&data_dir, run, cursor).unwrap();

        assert_eq!(older.rows.len(), 20);
        assert_eq!(older.rows.first().map(|row| row.seq), Some(1));
        assert_eq!(
            older.rows.last().map(|row| row.seq),
            Some(total - TIMELINE_PAGE_SIZE as u64),
            "the page stops exactly before the entry the cursor named"
        );
        assert!(older.beginning, "the walk reached the run's first event");
        assert!(!older.live);
    }

    #[test]
    fn a_payload_is_read_back_for_exactly_the_sequence_that_was_asked_for() {
        let fixture = TempDir::new().unwrap();
        let data_dir = fixture.path().join("data");
        let run = seed(&data_dir, 5);

        let detail = load_event_detail_in(&data_dir, run, 3)
            .unwrap()
            .expect("event three is stored");

        assert_eq!(detail, "{\n  \"step\": 2\n}");
        assert!(
            load_event_detail_in(&data_dir, run, 99).unwrap().is_none(),
            "an event nothing recorded has no payload to attach"
        );
    }

    #[test]
    fn a_data_directory_that_recorded_nothing_opens_no_timeline() {
        let fixture = TempDir::new().unwrap();

        let opened = open_timeline_in(&fixture.path().join("never-used"), RunId::new()).unwrap();

        assert!(opened.is_none());
        assert!(
            !fixture.path().join("never-used").exists(),
            "a read must not be what creates the run store"
        );
    }
}
