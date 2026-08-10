//! The durable run store: `runtime.db` under the Harkness data directory.
//!
//! # Why SQLite and not another JSON file
//!
//! The project catalog is one small document rewritten whole under an exclusive
//! lock. Run history is the opposite shape: append-heavy, queried by recency,
//! and unbounded. Keeping it in its own SQLite database means a run log cannot
//! corrupt the catalog, its growth is prunable independently, and listing the
//! newest runs costs one index seek instead of a full parse.
//!
//! # Connection discipline
//!
//! Every connection this module opens applies the same pragmas before it is
//! used: `journal_mode=WAL`, `foreign_keys=ON`, `busy_timeout=5000`, and
//! `synchronous=NORMAL`. WAL lets readers work while a writer commits;
//! `synchronous=NORMAL` trades power-loss durability for commit latency, which
//! is the right trade for a record of work a user can re-run — process-crash
//! consistency is the guarantee this store makes, not power-loss durability.
//!
//! The schema version is probed before WAL is requested, so refusing a database
//! written by a newer build leaves its bytes exactly as they were found. Each
//! migration then re-reads that version under its own write lock, so two
//! processes opening the same new database climb one ladder instead of both
//! replaying the same step.
//!
//! # Single writer, short transactions
//!
//! Every write goes through one mutex-guarded connection, and every
//! read-modify-write runs in one `BEGIN IMMEDIATE` transaction, so a lifecycle
//! change cannot interleave with another writer in this process or in another.
//! Reads use separate pooled connections and are never blocked by a writer.
//!
//! **No transaction is ever held across a user wait.** Work that needs a human
//! decision persists the request, commits, and only then waits; resuming is a
//! second, equally short transaction. A transaction held open across an
//! approval prompt would block every other writer for as long as the user takes
//! to answer.
//!
//! # Lock ordering
//!
//! The store takes neither the repository lock nor the catalog lock, and no
//! caller may hold a store transaction while acquiring either. The existing
//! ordering — repository lock before catalog lock — is therefore unchanged by
//! this module.
//!
//! # Inline payload threshold
//!
//! No column holds more than [`MAX_INLINE_PAYLOAD_BYTES`] of caller data, and
//! the bound covers every caller-controlled column rather than tool payloads
//! alone: titles, workspace paths, tool identifiers, failure detail, and the
//! approval history are each held to it. A row is the wrong home for a large
//! value whatever its column name — it inflates every query that touches the
//! table and defeats the page cache — and a limit with exceptions is not a
//! limit anyone can rely on. Oversized data is refused with
//! [`StoreError::PayloadTooLarge`] naming the threshold; large content belongs
//! in the artifact store instead.
//!
//! The refusal is symmetric: a row that arrived from outside Harkness holding
//! more than the threshold fails to load, because reading it back would import
//! the very cost the bound exists to prevent.
//!
//! One consequence is worth stating plainly. A caller recording a failure whose
//! message exceeds the threshold is refused, and the record keeps its previous
//! state; the caller must summarize the detail and retry. Truncating silently
//! would store something the caller never wrote, and storing it whole would
//! break the bound every other column keeps.
//!
//! The one caller-supplied value that is *not* refused for being too large is an
//! event payload, because an event describes something that already happened
//! and refusing to record it loses history rather than protecting it. It is
//! spilled into the artifact store and replaced by a reference instead; see
//! [`RunEvent::overflowed_payload`] for the reference it leaves behind.
//!
//! # Events and artifacts
//!
//! The records above say what a run *is*. [`RunEvent`] says how it got there,
//! in an append-only per-run log whose sequence numbers are allocated inside the
//! transaction that writes them, and which commits together with the state
//! change it describes. [`Artifact`] is where content too large for any row
//! lives: files under `<data_dir>/artifacts/`, written before their metadata row
//! so no row can point at bytes that were never made durable, hashed so a
//! consumer can prove they have not changed, and probed on read so deleting one
//! degrades an artifact rather than breaking a run.
//!
//! Every byte either of them persists passes through a [`Redactor`]. The v0.3
//! default changes nothing; the point of the hook is that supplying real rules
//! is a change in one place rather than an audit of every caller.
//!
//! # Backups
//!
//! A WAL database is three files: `runtime.db`, `runtime.db-wal`, and
//! `runtime.db-shm`. Copying only `runtime.db` from a running Harkness loses
//! every commit still in the log. Either copy all three, or call
//! [`Store::checkpoint`] first and copy `runtime.db` alone — and check that it
//! returned `Ok`, because a reader on another connection can leave frames
//! behind, which is reported as [`StoreError::IncompleteCheckpoint`] rather
//! than by failing the statement.

mod artifact;
mod column;
mod error;
mod event;
mod listing;
mod migration;
mod redaction;
mod repository;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::{Connection, TransactionBehavior};
use serde_json::Value;
use time::OffsetDateTime;

use crate::domain::{
    ArtifactId, Failure, Run, RunDomainError, RunId, Step, StepId, Task, TaskId, ToolCall,
    ToolCallId,
};

pub use artifact::{ARTIFACTS_DIRECTORY, Artifact, ArtifactSink, Availability, StoreArtifacts};
pub use error::{OpenFailure, StoreError};
pub use event::{
    DEFAULT_EVENT_PAGE_LIMIT, EventKind, EventSeq, MAX_EVENT_PAGE_LIMIT, OVERFLOW_PAYLOAD_FIELD,
    OVERFLOW_PAYLOAD_MEDIA_TYPE, OVERFLOW_PAYLOAD_NAME, OverflowedPayload, RunEvent, StoredEvent,
};
pub use listing::{DEFAULT_RUN_PAGE_LIMIT, MAX_RUN_PAGE_LIMIT, RunCursor, RunListing, RunPage};
pub use migration::SCHEMA_VERSION;
pub use redaction::{PassThrough, Redactor};

/// Name of the run database inside the Harkness data directory.
pub const DATABASE_FILE: &str = "runtime.db";

/// Largest inline payload any single column will hold.
///
/// The artifact store and the redaction rules that follow it enforce the same
/// number, so it is named once here.
pub const MAX_INLINE_PAYLOAD_BYTES: usize = 64 * 1024;

/// How long a connection waits for another writer before giving up.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the write-ahead-log transition re-checks a contended database.
const WAL_RETRY_INTERVAL: Duration = Duration::from_millis(20);

/// Read connections retained for reuse; extra readers are opened and dropped.
const POOLED_READERS: usize = 4;

/// The durable record of tasks, runs, steps, and tool calls.
///
/// A `Store` is safe to share across threads. Writes serialize through one
/// connection; reads borrow their own.
#[derive(Debug)]
pub struct Store {
    data_dir: PathBuf,
    path: PathBuf,
    writer: Mutex<Connection>,
    readers: Mutex<Vec<Connection>>,
    redactor: Arc<dyn Redactor>,
}

impl Store {
    /// Opens, creating and migrating when needed, `<data_dir>/runtime.db`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::SchemaTooNew`] when the database was written by a
    /// newer build, leaving the file untouched, and [`StoreError::Open`] when
    /// the directory or the connection could not be prepared.
    pub fn open(data_dir: &Path) -> Result<Self, StoreError> {
        let path = data_dir.join(DATABASE_FILE);
        fs::create_dir_all(data_dir)
            .map_err(|error| open_failed(&path, OpenFailure::DataDirectory(error)))?;

        let mut connection = connect(&path).map_err(|error| open_failed(&path, error))?;
        // Before WAL: a refused database must keep the bytes it arrived with.
        migration::refuse_newer_schema(&connection, migration::MIGRATIONS)?;
        enable_wal(&connection).map_err(|error| open_failed(&path, error))?;
        migration::apply(&mut connection, migration::MIGRATIONS)?;

        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            path,
            writer: Mutex::new(connection),
            readers: Mutex::new(Vec::new()),
            redactor: Arc::new(PassThrough),
        })
    }

    /// Routes every event payload and artifact stream through `redactor`.
    ///
    /// Consuming rather than mutating is deliberate: a store is shared across
    /// threads, and a redactor that could be swapped underneath a running write
    /// would make "this content was scrubbed" a claim about timing rather than
    /// about the store.
    #[must_use]
    pub fn redacting(mut self, redactor: Arc<dyn Redactor>) -> Self {
        self.redactor = redactor;
        self
    }

    /// Opens the run store in the platform data directory.
    ///
    /// `HARKNESS_DATA_DIR` replaces that location entirely when it is set, so a
    /// front end or a test can run against its own database.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::DataDirectoryUnavailable`] when the platform
    /// exposes no data directory and no override is set, and otherwise the
    /// errors of [`Store::open`].
    pub fn open_default() -> Result<Self, StoreError> {
        let data_dir =
            harkness_core::data_directory().ok_or(StoreError::DataDirectoryUnavailable)?;
        Self::open(&data_dir)
    }

    /// Path of the database file this store owns.
    ///
    /// Its write-ahead log and shared-memory sidecars sit beside it under the
    /// same name; see the module documentation on backups.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Folds the write-ahead log back into the database file.
    ///
    /// Call this before copying `runtime.db` on its own; otherwise the copy is
    /// missing every commit still held in the log.
    ///
    /// A checkpoint reports how far it got in a result row rather than by
    /// failing: a reader on another connection can leave frames behind while
    /// SQLite still returns success. Discarding that row would let a backup
    /// procedure copy `runtime.db` alone on the strength of a checkpoint that
    /// never finished, so the row is read and an incomplete fold is refused.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::IncompleteCheckpoint`] when frames remain in the
    /// log, and [`StoreError::Busy`] when another connection holds the database
    /// past the busy timeout.
    pub fn checkpoint(&self) -> Result<(), StoreError> {
        let writer = guard(&self.writer);
        let (busy, log_frames, checkpointed_frames) = writer
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(|error| error::query_failed("checkpointing the write-ahead log", error))?;
        // A busy checkpoint reports -1 frames, so the counts are only
        // meaningful once the busy flag is clear.
        if busy != 0 || checkpointed_frames != log_frames {
            return Err(StoreError::IncompleteCheckpoint {
                log_frames,
                checkpointed_frames,
            });
        }
        Ok(())
    }

    // -- tasks --------------------------------------------------------------

    /// Stores a task.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::AlreadyExists`] when the identity is taken and
    /// [`StoreError::NonUtf8Path`] when the workspace path cannot be stored.
    pub fn insert_task(&self, task: &Task) -> Result<(), StoreError> {
        repository::insert_task(&guard(&self.writer), task)
    }

    /// Loads one task.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] when no task has that identity.
    pub fn load_task(&self, id: TaskId) -> Result<Task, StoreError> {
        self.with_reader(|connection| repository::load_task(connection, id))
    }

    // -- runs ---------------------------------------------------------------

    /// Stores a run against an already-stored task.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::MissingParent`] when the task is not stored and
    /// [`StoreError::AlreadyExists`] when the identity is taken.
    pub fn insert_run(&self, run: &Run) -> Result<(), StoreError> {
        repository::insert_run(&guard(&self.writer), run)
    }

    /// Loads one run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] when no run has that identity.
    pub fn load_run(&self, id: RunId) -> Result<Run, StoreError> {
        self.with_reader(|connection| repository::load_run(connection, id))
    }

    /// Applies an outcome-free run transition and returns the stored result.
    ///
    /// The stored record is loaded, transitioned through the domain's own
    /// table, and written back in one transaction. A rejected transition leaves
    /// the row exactly as it was.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidTransition`] when the domain refuses the
    /// edge and [`StoreError::NotFound`] when no run has that identity.
    pub fn transition_run(
        &self,
        id: RunId,
        to: crate::domain::ExecutionState,
        at: OffsetDateTime,
    ) -> Result<Run, StoreError> {
        self.mutate_run(id, |run| run.transition(to, at))
    }

    /// Fails a run with structured detail, atomically with its transition.
    ///
    /// # Errors
    ///
    /// As [`Store::transition_run`].
    pub fn fail_run(
        &self,
        id: RunId,
        failure: Failure,
        at: OffsetDateTime,
    ) -> Result<Run, StoreError> {
        self.mutate_run(id, |run| run.fail(failure, at))
    }

    /// Records an approval and resumes a run awaiting one.
    ///
    /// The decision is persisted in its own short transaction; the wait that
    /// preceded it held no transaction at all.
    ///
    /// # Errors
    ///
    /// As [`Store::transition_run`].
    pub fn approve_run(
        &self,
        id: RunId,
        decided_by: &str,
        at: OffsetDateTime,
    ) -> Result<Run, StoreError> {
        self.mutate_run(id, |run| run.approve(decided_by, at))
    }

    /// Records a denied approval and fails a run awaiting one.
    ///
    /// # Errors
    ///
    /// As [`Store::transition_run`].
    pub fn reject_run_approval(
        &self,
        id: RunId,
        decided_by: &str,
        failure: Failure,
        at: OffsetDateTime,
    ) -> Result<Run, StoreError> {
        self.mutate_run(id, |run| run.reject_approval(decided_by, failure, at))
    }

    /// Records which process currently owns a run, or clears the claim.
    ///
    /// This is the column a later interruption-recovery pass reads to tell a
    /// run abandoned by a dead process from one still executing. Deciding what
    /// to do about a stale owner is deliberately not this module's job.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] when no run has that identity.
    pub fn set_run_owner(&self, id: RunId, owner_pid: Option<u32>) -> Result<(), StoreError> {
        repository::set_run_owner(&guard(&self.writer), id, owner_pid)
    }

    /// Reads the process that currently claims a run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] when no run has that identity.
    pub fn run_owner(&self, id: RunId) -> Result<Option<u32>, StoreError> {
        self.with_reader(|connection| repository::run_owner(connection, id))
    }

    /// Returns one newest-first page of run history.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidPageLimit`] when the page size is zero or
    /// above [`MAX_RUN_PAGE_LIMIT`].
    pub fn list_runs(&self, page: RunPage) -> Result<RunListing, StoreError> {
        self.with_reader(|connection| listing::list_runs(connection, page))
    }

    // -- steps --------------------------------------------------------------

    /// Stores a step against an already-stored run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::MissingParent`] when the run is not stored and
    /// [`StoreError::DuplicateStepOrdinal`] when the ordinal is taken.
    pub fn insert_step(&self, step: &Step) -> Result<(), StoreError> {
        repository::insert_step(&guard(&self.writer), step)
    }

    /// Loads one step.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] when no step has that identity.
    pub fn load_step(&self, id: StepId) -> Result<Step, StoreError> {
        self.with_reader(|connection| repository::load_step(connection, id))
    }

    /// Loads every step of a run in ordinal order.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Query`] when the listing statement fails.
    pub fn load_run_steps(&self, run_id: RunId) -> Result<Vec<Step>, StoreError> {
        self.with_reader(|connection| repository::load_run_steps(connection, run_id))
    }

    /// Applies an outcome-free step transition and returns the stored result.
    ///
    /// # Errors
    ///
    /// As [`Store::transition_run`], for steps.
    pub fn transition_step(
        &self,
        id: StepId,
        to: crate::domain::ExecutionState,
        at: OffsetDateTime,
    ) -> Result<Step, StoreError> {
        self.mutate_step(id, |step| step.transition(to, at))
    }

    /// Fails a step with structured detail, atomically with its transition.
    ///
    /// # Errors
    ///
    /// As [`Store::transition_run`], for steps.
    pub fn fail_step(
        &self,
        id: StepId,
        failure: Failure,
        at: OffsetDateTime,
    ) -> Result<Step, StoreError> {
        self.mutate_step(id, |step| step.fail(failure, at))
    }

    /// Records an approval and resumes a step awaiting one.
    ///
    /// # Errors
    ///
    /// As [`Store::transition_run`], for steps.
    pub fn approve_step(
        &self,
        id: StepId,
        decided_by: &str,
        at: OffsetDateTime,
    ) -> Result<Step, StoreError> {
        self.mutate_step(id, |step| step.approve(decided_by, at))
    }

    /// Records a denied approval and fails a step awaiting one.
    ///
    /// # Errors
    ///
    /// As [`Store::transition_run`], for steps.
    pub fn reject_step_approval(
        &self,
        id: StepId,
        decided_by: &str,
        failure: Failure,
        at: OffsetDateTime,
    ) -> Result<Step, StoreError> {
        self.mutate_step(id, |step| step.reject_approval(decided_by, failure, at))
    }

    // -- tool calls ---------------------------------------------------------

    /// Stores a tool call against an already-stored step of the same run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::MissingParent`] when the step is not stored or
    /// belongs to a different run, and [`StoreError::PayloadTooLarge`] when the
    /// input exceeds [`MAX_INLINE_PAYLOAD_BYTES`].
    pub fn insert_tool_call(&self, call: &ToolCall) -> Result<(), StoreError> {
        repository::insert_tool_call(&guard(&self.writer), call)
    }

    /// Loads one tool call.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] when no call has that identity.
    pub fn load_tool_call(&self, id: ToolCallId) -> Result<ToolCall, StoreError> {
        self.with_reader(|connection| repository::load_tool_call(connection, id))
    }

    /// Loads every tool call of a run in creation order.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Query`] when the listing statement fails.
    pub fn load_run_tool_calls(&self, run_id: RunId) -> Result<Vec<ToolCall>, StoreError> {
        self.with_reader(|connection| repository::load_run_tool_calls(connection, run_id))
    }

    /// Applies an outcome-free tool-call transition.
    ///
    /// # Errors
    ///
    /// As [`Store::transition_run`], for tool calls.
    pub fn transition_tool_call(
        &self,
        id: ToolCallId,
        to: crate::domain::ToolCallState,
        at: OffsetDateTime,
    ) -> Result<ToolCall, StoreError> {
        self.mutate_tool_call(id, |call| call.transition(to, at))
    }

    /// Succeeds a tool call, atomically with its output.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::PayloadTooLarge`] when the output exceeds
    /// [`MAX_INLINE_PAYLOAD_BYTES`], and otherwise as
    /// [`Store::transition_run`].
    pub fn succeed_tool_call(
        &self,
        id: ToolCallId,
        output: Value,
        at: OffsetDateTime,
    ) -> Result<ToolCall, StoreError> {
        self.mutate_tool_call(id, |call| call.succeed(output, at))
    }

    /// Fails a tool call with structured detail.
    ///
    /// # Errors
    ///
    /// As [`Store::transition_run`], for tool calls.
    pub fn fail_tool_call(
        &self,
        id: ToolCallId,
        failure: Failure,
        at: OffsetDateTime,
    ) -> Result<ToolCall, StoreError> {
        self.mutate_tool_call(id, |call| call.fail(failure, at))
    }

    /// Records a policy denial of a pending tool call.
    ///
    /// # Errors
    ///
    /// As [`Store::transition_run`], for tool calls.
    pub fn deny_tool_call(
        &self,
        id: ToolCallId,
        failure: Failure,
        at: OffsetDateTime,
    ) -> Result<ToolCall, StoreError> {
        self.mutate_tool_call(id, |call| call.deny(failure, at))
    }

    /// Records an approval and resumes a tool call awaiting one.
    ///
    /// # Errors
    ///
    /// As [`Store::transition_run`], for tool calls.
    pub fn approve_tool_call(
        &self,
        id: ToolCallId,
        decided_by: &str,
        at: OffsetDateTime,
    ) -> Result<ToolCall, StoreError> {
        self.mutate_tool_call(id, |call| call.approve(decided_by, at))
    }

    /// Records a denied approval of a tool call awaiting one.
    ///
    /// # Errors
    ///
    /// As [`Store::transition_run`], for tool calls.
    pub fn reject_tool_call_approval(
        &self,
        id: ToolCallId,
        decided_by: &str,
        failure: Failure,
        at: OffsetDateTime,
    ) -> Result<ToolCall, StoreError> {
        self.mutate_tool_call(id, |call| call.reject_approval(decided_by, failure, at))
    }

    // -- events ---------------------------------------------------------------

    /// Appends one event to a run's log and returns the position it took.
    ///
    /// The sequence number is allocated inside the same transaction as the
    /// insert, so two threads appending to one run cannot be handed the same
    /// number. A payload above [`MAX_INLINE_PAYLOAD_BYTES`] is written to an
    /// artifact and replaced by a reference to it rather than refused.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::MissingParent`] when the run, or a step, tool call
    /// or artifact the event names, is not stored, and
    /// [`StoreError::ArtifactIo`] when an oversized payload cannot be spilled.
    pub fn append_event(&self, run_id: RunId, event: RunEvent) -> Result<EventSeq, StoreError> {
        let prepared = self.prepare_event(run_id, event)?;
        self.in_write_transaction("appending a run event", |connection| {
            prepared.append(connection, run_id)
        })
    }

    /// Applies a run transition and appends its event in one transaction.
    ///
    /// This is the pairing the whole log depends on: a run whose state moved
    /// without its history saying so, or a history claiming a move that was
    /// rolled back, are both worse than a failed transition. Either both rows
    /// are visible or neither is.
    ///
    /// # Errors
    ///
    /// As [`Store::transition_run`] and [`Store::append_event`]; a failure of
    /// either half leaves the run exactly as it was.
    pub fn transition_run_with_event(
        &self,
        id: RunId,
        to: crate::domain::ExecutionState,
        at: OffsetDateTime,
        event: RunEvent,
    ) -> Result<(Run, EventSeq), StoreError> {
        let prepared = self.prepare_event(id, event)?;
        self.in_write_transaction("transitioning a run with its event", |connection| {
            let mut run = repository::load_run(connection, id)?;
            run.transition(to, at)
                .map_err(StoreError::InvalidTransition)?;
            repository::update_run(connection, &run)?;
            let seq = prepared.append(connection, id)?;
            Ok((run, seq))
        })
    }

    /// Applies a tool-call transition and appends its event in one transaction.
    ///
    /// # Errors
    ///
    /// As [`Store::transition_run_with_event`], for tool calls.
    pub fn transition_tool_call_with_event(
        &self,
        id: ToolCallId,
        to: crate::domain::ToolCallState,
        at: OffsetDateTime,
        event: RunEvent,
    ) -> Result<(ToolCall, EventSeq), StoreError> {
        // The run is read before the transaction so the payload can be spilled
        // and redacted outside it: no transaction is held across work that
        // touches the filesystem.
        let run_id = self.load_tool_call(id)?.run_id();
        let prepared = self.prepare_event(run_id, event)?;
        self.in_write_transaction("transitioning a tool call with its event", |connection| {
            let mut call = repository::load_tool_call(connection, id)?;
            call.transition(to, at)
                .map_err(StoreError::InvalidTransition)?;
            repository::update_tool_call(connection, &call)?;
            let seq = prepared.append(connection, call.run_id())?;
            Ok((call, seq))
        })
    }

    /// Returns one page of a run's event log, oldest first.
    ///
    /// `after` is exclusive, so paging is `after = the last sequence seen`. A
    /// page never repeats or skips an event however many arrive at the tip
    /// between requests, because it addresses a position in the log rather than
    /// a position in a result set.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidPageLimit`] when the page size is zero or
    /// above [`MAX_EVENT_PAGE_LIMIT`].
    pub fn events(
        &self,
        run_id: RunId,
        after: Option<EventSeq>,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        self.with_reader(|connection| event::events(connection, run_id, after, limit))
    }

    // -- artifacts ------------------------------------------------------------

    /// Opens a streaming write into the artifact store.
    ///
    /// The returned sink is an [`std::io::Write`] and offers no whole-content
    /// method, so a caller cannot accidentally buffer an artifact in order to
    /// store it. Nothing is recorded until
    /// [`ArtifactSink::finish`] is called; an abandoned sink leaves neither a
    /// row nor a file a reader would resolve.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::MissingParent`] when the run is not stored,
    /// [`StoreError::PayloadTooLarge`] when the name or media type would not fit
    /// its column, and [`StoreError::ArtifactIo`] when the file cannot be
    /// created.
    pub fn create_artifact(
        &self,
        run_id: RunId,
        name: &str,
        media_type: &str,
        at: OffsetDateTime,
    ) -> Result<ArtifactSink<'_>, StoreError> {
        ArtifactSink::create(self, run_id, name, media_type, at)
    }

    /// Loads one artifact's metadata, probing whether its bytes are still there.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] when no artifact has that identity and
    /// [`StoreError::ForbiddenArtifactPath`] when the row names a location
    /// outside the artifacts directory. A *missing file* is not an error: it is
    /// reported as [`Availability::Missing`].
    pub fn artifact(&self, id: ArtifactId) -> Result<Artifact, StoreError> {
        self.with_reader(|connection| artifact::load_artifact(connection, &self.data_dir, id))
    }

    /// Loads every artifact recorded against a run, oldest first.
    ///
    /// # Errors
    ///
    /// As [`Store::artifact`], for any row of the run.
    pub fn run_artifacts(&self, run_id: RunId) -> Result<Vec<Artifact>, StoreError> {
        self.with_reader(|connection| {
            artifact::load_run_artifacts(connection, &self.data_dir, run_id)
        })
    }

    /// Opens an artifact's content for reading.
    ///
    /// Returned as a handle rather than as bytes because an artifact is the
    /// thing that did not fit in memory; a caller that wants it whole says so
    /// with [`Store::read_artifact`].
    ///
    /// # Errors
    ///
    /// As [`Store::artifact`], plus [`StoreError::ArtifactIo`] when the content
    /// cannot be opened — which includes the file having been deleted.
    pub fn open_artifact(&self, id: ArtifactId) -> Result<fs::File, StoreError> {
        let artifact = self.artifact(id)?;
        let path = artifact::artifact_path(&self.data_dir, artifact.run_id(), artifact.id());
        fs::File::open(&path).map_err(|source| StoreError::ArtifactIo {
            operation: "opening an artifact",
            path,
            source,
        })
    }

    /// Reads an artifact's content into memory.
    ///
    /// # Errors
    ///
    /// As [`Store::open_artifact`].
    pub fn read_artifact(&self, id: ArtifactId) -> Result<Vec<u8>, StoreError> {
        let artifact = self.artifact(id)?;
        let path = artifact::artifact_path(&self.data_dir, artifact.run_id(), artifact.id());
        fs::read(&path).map_err(|source| StoreError::ArtifactIo {
            operation: "reading an artifact",
            path,
            source,
        })
    }

    // -- internals ----------------------------------------------------------

    /// The data directory this store's artifacts live under.
    pub(super) fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The redactor every recorded byte passes through.
    pub(super) fn redactor(&self) -> &Arc<dyn Redactor> {
        &self.redactor
    }

    /// Redacts an event's payload and spills it to an artifact when oversized.
    ///
    /// Everything expensive happens here, before any transaction opens: the
    /// redaction, the encoding, and — for an overflowing payload — writing and
    /// syncing a whole file. What is left for the transaction is two inserts.
    fn prepare_event(&self, run_id: RunId, event: RunEvent) -> Result<PreparedEvent, StoreError> {
        let redacted = redaction::redact_payload(&*self.redactor, event.payload());
        let encoded =
            serde_json::to_string(&redacted).map_err(|error| StoreError::ColumnEncoding {
                record: "run_event",
                field: "payload",
                reason: error.to_string(),
            })?;
        if !event::overflows_inline(&encoded) {
            return Ok(PreparedEvent {
                event: event.with_payload(redacted),
                payload_json: encoded,
                sealed: None,
            });
        }

        // The artifact carries the payload the caller supplied, not the
        // redacted copy: an artifact's own bytes are redacted by the stream
        // wrapper as they are written, so redacting twice would be the only way
        // to get a different answer here than every other artifact gets.
        let original =
            serde_json::to_vec(event.payload()).map_err(|error| StoreError::ColumnEncoding {
                record: "run_event",
                field: "payload",
                reason: error.to_string(),
            })?;
        let mut sink = self.create_artifact(
            run_id,
            event::OVERFLOW_PAYLOAD_NAME,
            event::OVERFLOW_PAYLOAD_MEDIA_TYPE,
            event.at(),
        )?;
        if let Some(step_id) = event.step_id() {
            sink = sink.for_step(step_id);
        }
        if let Some(tool_call_id) = event.tool_call_id() {
            sink = sink.for_tool_call(tool_call_id);
        }
        let spilled_to = sink.id();
        std::io::Write::write_all(&mut sink, &original).map_err(|source| {
            StoreError::ArtifactIo {
                operation: "spilling an oversized event payload",
                path: artifact::artifact_path(&self.data_dir, run_id, spilled_to),
                source,
            }
        })?;
        let sealed = sink.seal()?;

        let marker = event::overflow_payload(event::OverflowedPayload {
            id: sealed.id(),
            media_type: sealed.media_type().to_owned(),
            byte_size: sealed.byte_size(),
            sha256: sealed.sha256().to_owned(),
        });
        let payload_json =
            serde_json::to_string(&marker).expect("an overflow marker is representable as JSON");
        // The reference is always in the payload. The column additionally points
        // at it when the caller named no artifact of their own, so the common
        // case is queryable without parsing JSON and the uncommon one does not
        // silently lose what the caller meant.
        let event = match event.artifact_id() {
            Some(_) => event.with_payload(marker),
            None => event.with_payload(marker).for_artifact(sealed.id()),
        };
        Ok(PreparedEvent {
            event,
            payload_json,
            sealed: Some(sealed),
        })
    }

    fn mutate_run<F>(&self, id: RunId, change: F) -> Result<Run, StoreError>
    where
        F: FnOnce(&mut Run) -> Result<(), RunDomainError>,
    {
        self.in_write_transaction("transitioning a run", |connection| {
            let mut run = repository::load_run(connection, id)?;
            change(&mut run).map_err(StoreError::InvalidTransition)?;
            repository::update_run(connection, &run)?;
            Ok(run)
        })
    }

    fn mutate_step<F>(&self, id: StepId, change: F) -> Result<Step, StoreError>
    where
        F: FnOnce(&mut Step) -> Result<(), RunDomainError>,
    {
        self.in_write_transaction("transitioning a step", |connection| {
            let mut step = repository::load_step(connection, id)?;
            change(&mut step).map_err(StoreError::InvalidTransition)?;
            repository::update_step(connection, &step)?;
            Ok(step)
        })
    }

    fn mutate_tool_call<F>(&self, id: ToolCallId, change: F) -> Result<ToolCall, StoreError>
    where
        F: FnOnce(&mut ToolCall) -> Result<(), RunDomainError>,
    {
        self.in_write_transaction("transitioning a tool call", |connection| {
            let mut call = repository::load_tool_call(connection, id)?;
            change(&mut call).map_err(StoreError::InvalidTransition)?;
            repository::update_tool_call(connection, &call)?;
            Ok(call)
        })
    }

    /// Runs one read-modify-write against the single writer.
    ///
    /// `BEGIN IMMEDIATE` takes the write lock up front, so the read that
    /// decides the next state and the write that records it cannot be split by
    /// another process's commit. A rejected change returns before any statement
    /// has modified a row, and the rollback on drop makes that explicit.
    fn in_write_transaction<T, F>(
        &self,
        operation: &'static str,
        change: F,
    ) -> Result<T, StoreError>
    where
        F: FnOnce(&Connection) -> Result<T, StoreError>,
    {
        let mut writer = guard(&self.writer);
        let transaction = writer
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error::query_failed(operation, error))?;
        let value = change(&transaction)?;
        transaction
            .commit()
            .map_err(|error| error::query_failed(operation, error))?;
        Ok(value)
    }

    /// Borrows a read connection, reusing a pooled one when available.
    fn with_reader<T, F>(&self, read: F) -> Result<T, StoreError>
    where
        F: FnOnce(&Connection) -> Result<T, StoreError>,
    {
        let connection = match guard(&self.readers).pop() {
            Some(connection) => connection,
            None => {
                let connection =
                    connect(&self.path).map_err(|error| open_failed(&self.path, error))?;
                enable_wal(&connection).map_err(|error| open_failed(&self.path, error))?;
                connection
            }
        };
        let result = read(&connection);
        let mut readers = guard(&self.readers);
        if readers.len() < POOLED_READERS {
            readers.push(connection);
        }
        result
    }
}

/// An event with everything slow already done to it.
///
/// Redaction, encoding and — when the payload overflowed — writing and syncing
/// a whole artifact file happen before the transaction opens. What is left is
/// one or two inserts, which is what keeps an event ride-along inside the
/// state-change latency budget.
struct PreparedEvent {
    event: RunEvent,
    payload_json: String,
    sealed: Option<artifact::SealedArtifact>,
}

impl PreparedEvent {
    /// Records the spilled artifact, if there was one, and then the event.
    ///
    /// The order is forced: the event's `artifact_id` foreign key can only
    /// resolve once the artifact row exists. Both are in the caller's
    /// transaction, so an event referring to an artifact row that never
    /// committed is not a state the store can be found in.
    fn append(&self, connection: &Connection, run_id: RunId) -> Result<EventSeq, StoreError> {
        if let Some(sealed) = &self.sealed {
            artifact::insert_artifact(connection, sealed)?;
        }
        event::append_event(connection, run_id, &self.event, &self.payload_json)
    }
}

/// Opens a connection and applies the pragmas that do not write to the file.
///
/// WAL is requested separately by [`enable_wal`] so a database this build
/// refuses is never modified on the way to the refusal.
fn connect(path: &Path) -> Result<Connection, OpenFailure> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    Ok(connection)
}

/// Requests the write-ahead log and proves the connection entered it.
///
/// `PRAGMA journal_mode` reports the mode actually in force rather than
/// failing, so a filesystem that cannot support WAL would otherwise leave the
/// store silently running in rollback-journal mode.
///
/// # Why this retries
///
/// Moving a database into WAL takes an exclusive lock, and SQLite does not
/// route that acquisition through the busy handler on every platform: on
/// Windows a second connection opening the same new database is told the
/// database is locked rather than being made to wait, even with a busy timeout
/// set. The mode is a persistent property of the file, so the contention only
/// exists while a new database is being created and it always resolves — the
/// winner's transition makes every other connection's next read report `wal`.
/// Waiting for that is the whole of the fix.
fn enable_wal(connection: &Connection) -> Result<(), OpenFailure> {
    let deadline = Instant::now() + BUSY_TIMEOUT;
    loop {
        match request_wal(connection) {
            Ok(mode) if mode.eq_ignore_ascii_case("wal") => break,
            Ok(mode) => return Err(OpenFailure::JournalMode { mode }),
            Err(error) if error::is_busy(&error) && Instant::now() < deadline => {
                thread::sleep(WAL_RETRY_INTERVAL);
            }
            Err(error) => return Err(error.into()),
        }
    }
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

/// Reports the journal mode, asking for WAL only when it is not already set.
///
/// The read costs nothing and the write costs an exclusive lock, so checking
/// first keeps every connection after the first — including every pooled
/// reader — off the contended path entirely.
fn request_wal(connection: &Connection) -> Result<String, rusqlite::Error> {
    let current: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    if current.eq_ignore_ascii_case("wal") {
        return Ok(current);
    }
    connection.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
}

fn open_failed(path: &Path, source: OpenFailure) -> StoreError {
    StoreError::Open {
        path: path.to_path_buf(),
        source,
    }
}

/// Takes a lock, adopting the contents even if a previous holder panicked.
///
/// A panic in a caller says nothing about the database: every write is a
/// committed or rolled-back transaction, so the connection behind a poisoned
/// mutex is still consistent and refusing to use it would only turn one failure
/// into a permanently unusable store.
fn guard<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
