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
//! written by a newer build leaves its bytes exactly as they were found.
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
//! No column holds more than [`MAX_INLINE_PAYLOAD_BYTES`] of caller data. Tool
//! input and output above that are refused with
//! [`StoreError::PayloadTooLarge`] naming the threshold, because a row is the
//! wrong home for a large payload: it inflates every query that touches the
//! table and defeats the page cache. Large content belongs in the artifact
//! store instead.
//!
//! # Backups
//!
//! A WAL database is three files: `runtime.db`, `runtime.db-wal`, and
//! `runtime.db-shm`. Copying only `runtime.db` from a running Harkness loses
//! every commit still in the log. Either copy all three, or call
//! [`Store::checkpoint`] first and copy `runtime.db` alone.

mod column;
mod error;
mod listing;
mod migration;
mod repository;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rusqlite::{Connection, TransactionBehavior};
use serde_json::Value;
use time::OffsetDateTime;

use crate::domain::{
    Failure, Run, RunDomainError, RunId, Step, StepId, Task, TaskId, ToolCall, ToolCallId,
};

pub use error::{OpenFailure, StoreError};
pub use listing::{DEFAULT_RUN_PAGE_LIMIT, MAX_RUN_PAGE_LIMIT, RunCursor, RunListing, RunPage};
pub use migration::SCHEMA_VERSION;

/// Name of the run database inside the Harkness data directory.
pub const DATABASE_FILE: &str = "runtime.db";

/// Largest inline payload any single column will hold.
///
/// The artifact store and the redaction rules that follow it enforce the same
/// number, so it is named once here.
pub const MAX_INLINE_PAYLOAD_BYTES: usize = 64 * 1024;

/// How long a connection waits for another writer before giving up.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Read connections retained for reuse; extra readers are opened and dropped.
const POOLED_READERS: usize = 4;

/// The durable record of tasks, runs, steps, and tool calls.
///
/// A `Store` is safe to share across threads. Writes serialize through one
/// connection; reads borrow their own.
#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    writer: Mutex<Connection>,
    readers: Mutex<Vec<Connection>>,
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
            path,
            writer: Mutex::new(connection),
            readers: Mutex::new(Vec::new()),
        })
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
    /// # Errors
    ///
    /// Returns [`StoreError::Busy`] when another connection holds the database
    /// past the busy timeout.
    pub fn checkpoint(&self) -> Result<(), StoreError> {
        let writer = guard(&self.writer);
        writer
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .map_err(|error| error::query_failed("checkpointing the write-ahead log", error))
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

    // -- internals ----------------------------------------------------------

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
fn enable_wal(connection: &Connection) -> Result<(), OpenFailure> {
    let mode: String =
        connection.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(OpenFailure::JournalMode { mode });
    }
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
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
