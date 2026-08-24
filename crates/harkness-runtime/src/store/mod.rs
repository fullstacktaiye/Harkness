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
//! # Redaction
//!
//! Every caller value that becomes durable here passes through a [`Redactor`]
//! first, and [`Store::open`] installs [`StandardRedactor`] rather than
//! [`PassThrough`], so the rules arrive by opening a store instead of by
//! remembering to ask for them. Event payload values, an artifact's label and
//! media type, an approval's summary and decision reason, a task's title, a
//! tool's result, and every failure message go through
//! [`redact_text`](Redactor::redact_text); artifact content goes through
//! [`wrap_stream`](Redactor::wrap_stream).
//!
//! Two durable caller documents deliberately do not, and both are load-bearing
//! bytes rather than prose. `tool_calls.input_json` is what
//! [`ToolExecutor`](crate::tool::ToolExecutor) reads back and *runs*, and what
//! an approval's hash was taken over — rewriting it would run a different
//! command than the one that was approved. `workspace_snapshots.payload_json` is
//! bound by a digest `harkness-context` re-derives on load, for the reason the
//! `redaction` submodule states at length. Anything new that persists caller
//! content comes through the redactor.
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

mod agent_registry;
mod approval;
mod artifact;
mod column;
mod error;
mod event;
mod lease;
mod listing;
mod migration;
mod recovery;
mod redaction;
mod repository;
mod snapshot;
mod trust_record;
mod workspace_trust;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use harkness_context::SnapshotId;
use rusqlite::{Connection, TransactionBehavior};
use serde_json::Value;
use time::OffsetDateTime;

use crate::agent_registry::{AgentId, AgentObservations};
use crate::approval::{
    ApprovalDecision, ApprovalGrant, ApprovalId, ApprovalRequest, ApprovalState, InputHash,
    canonical_input_hash,
};
use crate::domain::{
    ArtifactId, Failure, LeaseId, Run, RunDomainError, RunId, Step, StepId, Task, TaskId, ToolCall,
    ToolCallId,
};
use crate::integration::{SubjectKind, TrustRecord, TrustRecordId};
use crate::observe::StandardRedactor;
use crate::tool::ToolIdentity;
use crate::trust::{TrustState, WorkspaceTrust};

pub use artifact::{ARTIFACTS_DIRECTORY, Artifact, ArtifactSink, Availability, StoreArtifacts};
pub use error::{OpenFailure, StoreError};
pub use event::{
    DEFAULT_EVENT_PAGE_LIMIT, EventKind, EventListing, EventOrder, EventPage, EventSeq,
    MAX_EVENT_PAGE_LIMIT, OVERFLOW_PAYLOAD_FIELD, OVERFLOW_PAYLOAD_MEDIA_TYPE,
    OVERFLOW_PAYLOAD_NAME, OverflowedPayload, RunEvent, StoredEvent,
};
pub use lease::LeaseRecord;
pub use listing::{DEFAULT_RUN_PAGE_LIMIT, MAX_RUN_PAGE_LIMIT, RunCursor, RunListing, RunPage};
pub use migration::SCHEMA_VERSION;
pub use recovery::{InterruptionReason, RunInterruption};
pub(crate) use redaction::redact_payload;
pub use redaction::{PassThrough, Redactor};
pub use snapshot::StoredSnapshot;
pub use trust_record::StoredTrustRecord;

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
            // Fail-closed: the rules are installed by opening a store rather
            // than by remembering to ask for them. A front end that forgot to
            // opt in would be a front end that persisted credentials, and there
            // is no version of that which is a reasonable default.
            redactor: Arc::new(StandardRedactor::standard()),
        })
    }

    /// Opens `<data_dir>/runtime.db` only if it is already there.
    ///
    /// [`Store::open`] creates the directory, the database, its WAL sidecars and
    /// the whole migration ladder as a side effect of being called. That is right
    /// for anything that is about to record something, and wrong for a read: a
    /// caller projecting recorded state should be able to ask a data directory
    /// what it holds without writing to it, and a read that reports "nothing
    /// recorded" should leave no trace behind.
    ///
    /// `Ok(None)` means nothing has ever been recorded in this data directory.
    /// An existing database is opened exactly as [`Store::open`] opens it,
    /// migrations included — a schema older than this build still has to be
    /// climbed before its rows can be read.
    ///
    /// # Errors
    ///
    /// The same failures [`Store::open`] reports for a database that does exist.
    pub fn open_existing(data_dir: &Path) -> Result<Option<Self>, StoreError> {
        if !data_dir.join(DATABASE_FILE).is_file() {
            return Ok(None);
        }
        Self::open(data_dir).map(Some)
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

    // -- workspace trust ---------------------------------------------------

    /// Stores the latest explicit trust decision for one project identity.
    ///
    /// The decision already carries a canonical root. Replacing an earlier row
    /// is one SQLite statement, so another process sees either complete record.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NonUtf8Path`] when the platform path cannot be
    /// represented in the current SQLite schema.
    pub fn put_workspace_trust(&self, trust: &WorkspaceTrust) -> Result<(), StoreError> {
        workspace_trust::put(&guard(&self.writer), trust)
    }

    /// Revokes a project's positive trust without consulting its workspace.
    ///
    /// This remains available after the checkout is moved, deleted, or made
    /// unreadable. A decision older than the stored row is ignored, and an
    /// absent row is already untrusted.
    pub fn revoke_workspace_trust(
        &self,
        project_id: harkness_core::ProjectId,
        decided_at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        workspace_trust::revoke(&guard(&self.writer), project_id, decided_at)
    }

    /// Loads the explicit trust record for `project_id`, if one exists.
    pub fn workspace_trust(
        &self,
        project_id: harkness_core::ProjectId,
    ) -> Result<Option<WorkspaceTrust>, StoreError> {
        self.with_reader(|connection| workspace_trust::load(connection, project_id))
    }

    /// Resolves trust for the exact project identity and current workspace path.
    ///
    /// No row, an unavailable root, a moved checkout, and a path reused by a
    /// different project all resolve to [`TrustState::Untrusted`]. This read
    /// never repairs or rewrites the stored decision.
    pub fn resolve_workspace_trust(
        &self,
        project_id: harkness_core::ProjectId,
        root: impl AsRef<Path>,
    ) -> Result<TrustState, StoreError> {
        Ok(self
            .workspace_trust(project_id)?
            .as_ref()
            .map_or(TrustState::Untrusted, |trust| {
                trust.resolve(project_id, root)
            }))
    }

    // -- workspace snapshots ------------------------------------------------

    /// Records a capture that belongs to no run.
    ///
    /// The shape a front end asking "what does this workspace look like right
    /// now" produces. There is no event, because there is no timeline to put
    /// one on; a snapshot that belongs to a run is recorded with
    /// [`record_workspace_snapshot_for_run`](Self::record_workspace_snapshot_for_run)
    /// instead, and that is the only thing that emits `snapshot_captured`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::AlreadyExists`] when the capture identity is taken
    /// and [`StoreError::PayloadTooLarge`] when the encoded snapshot exceeds
    /// [`MAX_INLINE_PAYLOAD_BYTES`].
    pub fn record_workspace_snapshot(
        &self,
        snapshot: &harkness_context::WorkspaceSnapshot,
    ) -> Result<(), StoreError> {
        snapshot::insert(&guard(&self.writer), None, snapshot)
    }

    /// Records a capture as evidence for one run, with the event that says so.
    ///
    /// The row and its `snapshot_captured` event share one transaction, so
    /// "this run read this workspace" and "the timeline says so" become true
    /// together. There is deliberately no event-free variant for a run's
    /// capture: the context engine reads the workspace and persists nothing, so
    /// this is the only place a snapshot becomes evidence, and a snapshot no
    /// timeline mentions is a run whose context arrived from nowhere.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::MissingParent`] when the run is not stored,
    /// [`StoreError::AlreadyExists`] when the capture identity is taken, and
    /// [`StoreError::PayloadTooLarge`] when the encoded snapshot exceeds
    /// [`MAX_INLINE_PAYLOAD_BYTES`].
    pub fn record_workspace_snapshot_for_run(
        &self,
        run_id: RunId,
        snapshot: &harkness_context::WorkspaceSnapshot,
    ) -> Result<EventSeq, StoreError> {
        let prepared = self.prepare_event(run_id, snapshot::captured_event(snapshot))?;
        self.commit_event(
            "recording a workspace snapshot",
            prepared,
            |connection, prepared| {
                snapshot::insert(connection, Some(run_id), snapshot)?;
                prepared.append(connection, run_id)
            },
        )
    }

    /// Loads one recorded capture, or `None` when nothing is stored under `id`.
    ///
    /// The payload is re-validated by `harkness-context` and every
    /// denormalized column is compared against it, so a hand-edited row fails to
    /// load rather than entering the process claiming an identity its own
    /// contents do not support.
    pub fn workspace_snapshot(&self, id: SnapshotId) -> Result<Option<StoredSnapshot>, StoreError> {
        self.with_reader(|connection| snapshot::load(connection, id))
    }

    /// Every capture recorded for one run, oldest first.
    pub fn run_workspace_snapshots(
        &self,
        run_id: RunId,
    ) -> Result<Vec<StoredSnapshot>, StoreError> {
        self.with_reader(|connection| snapshot::for_run(connection, run_id))
    }

    // -- external-integration trust and agent state -------------------------

    /// Records a new trust grant about one external subject.
    ///
    /// A *new* row, never an upsert. `TrustRecord::check` ignores the display
    /// name and the executable path and accepts a compatible upgrade, so there
    /// is no structural key to deduplicate on — and the one an upsert would
    /// invent would let a fresh grant overwrite the revocation that preceded it.
    /// Transitions of an existing grant go through
    /// [`update_trust_record`](Self::update_trust_record) instead.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::AlreadyExists`] when the row identity is taken and
    /// [`StoreError::PayloadTooLarge`] when the subject reference or the encoded
    /// record exceeds [`MAX_INLINE_PAYLOAD_BYTES`].
    pub fn insert_trust_record(
        &self,
        id: TrustRecordId,
        subject_kind: SubjectKind,
        subject_ref: &str,
        record: &TrustRecord,
        recorded_at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        trust_record::insert(
            &guard(&self.writer),
            id,
            subject_kind,
            subject_ref,
            record,
            recorded_at,
        )
    }

    /// Rewrites the grant one row holds, leaving its identity and subject alone.
    ///
    /// A revocation, an invalidation, and a re-grant are all transitions of the
    /// decision a user already made, so all three land here. Writing a new row
    /// for each would make "the most recent record about this subject" a
    /// question with more than one answer.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] when no row carries `id`.
    pub fn update_trust_record(
        &self,
        id: TrustRecordId,
        record: &TrustRecord,
    ) -> Result<(), StoreError> {
        trust_record::update(&guard(&self.writer), id, record)
    }

    /// Every grant recorded about one subject, oldest first.
    ///
    /// The whole history rather than the current answer, because a revocation
    /// followed by a fresh grant is two records and an audit that showed only
    /// the second would omit the decision the model exists to preserve.
    pub fn trust_records(
        &self,
        subject_kind: SubjectKind,
        subject_ref: &str,
    ) -> Result<Vec<StoredTrustRecord>, StoreError> {
        self.with_reader(|connection| {
            trust_record::for_subject(connection, subject_kind, subject_ref)
        })
    }

    /// The most recently recorded grant about one subject, if there is one.
    ///
    /// One indexed seek rather than the history with everything but its last
    /// row thrown away — this is the query every launch and every health check
    /// runs, and a subject with a long decision history must not make it slower.
    pub fn latest_trust_record(
        &self,
        subject_kind: SubjectKind,
        subject_ref: &str,
    ) -> Result<Option<StoredTrustRecord>, StoreError> {
        self.with_reader(|connection| {
            trust_record::latest_for_subject(connection, subject_kind, subject_ref)
        })
    }

    /// Replaces everything observed about one registered agent.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::PayloadTooLarge`] when an encoded observation
    /// exceeds [`MAX_INLINE_PAYLOAD_BYTES`].
    pub fn put_agent_observations(
        &self,
        id: &AgentId,
        observations: &AgentObservations,
    ) -> Result<(), StoreError> {
        agent_registry::put(&guard(&self.writer), id, observations)
    }

    /// Everything observed about one registered agent, if anything was.
    pub fn agent_observations(
        &self,
        id: &AgentId,
    ) -> Result<Option<AgentObservations>, StoreError> {
        self.with_reader(|connection| agent_registry::load(connection, id))
    }

    /// Forgets one agent's observations and every grant made about it.
    ///
    /// One transaction, because the two halves are one decision: state left
    /// behind for an identifier a user later reuses would answer for a program
    /// nobody checked, and a grant left behind would make the reused identifier
    /// trusted on arrival.
    pub fn forget_agent(&self, id: &AgentId) -> Result<(), StoreError> {
        self.in_write_transaction("removing an agent registration", |connection| {
            agent_registry::delete(connection, id)?;
            trust_record::delete_for_subject(
                connection,
                SubjectKind::AgentExecutable,
                id.as_str(),
            )?;
            Ok(())
        })
    }

    // -- approvals ----------------------------------------------------------

    /// Records a pending approval and the event that announces it.
    ///
    /// The row and its `approval_requested` event share one transaction, so
    /// "the question is durable" and "the surfaces have been told" become true
    /// together. There is deliberately no event-free variant: a request nobody
    /// is told about is a run that has stopped for no visible reason, and a
    /// timeline entry with no row behind it is a question nobody can answer.
    ///
    /// The event is derived from the record rather than supplied, so its payload
    /// carries the summary and the binding facts and never the raw input. The
    /// input stays in `tool_calls.input_json` for a surface to expand on demand.
    ///
    /// The wait that follows this call holds nothing. The transaction commits
    /// before the caller parks on its
    /// [`ApprovalTicket`](crate::approval::ApprovalTicket).
    ///
    /// Returns the request as it was stored, whose summary is the redacted one.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Approval`] carrying
    /// [`AlreadyResolved`](crate::approval::ApprovalError::AlreadyResolved) when
    /// the request was answered before it was ever recorded,
    /// [`StoreError::MissingParent`] when the tool call is not stored or belongs
    /// to a different run, [`StoreError::AlreadyExists`] when the identity is
    /// taken, and [`StoreError::PayloadTooLarge`] when the summary exceeds
    /// [`MAX_INLINE_PAYLOAD_BYTES`].
    pub fn open_approval(
        &self,
        request: ApprovalRequest,
    ) -> Result<(ApprovalRequest, EventSeq), StoreError> {
        // A record decided in memory and only then handed here would land as a
        // live grant whose timeline says a question was asked and never
        // answered. Approval-before-execution is a claim about what the store
        // witnessed, so the only thing that may be opened is a question.
        if request.state().is_terminal() {
            return Err(StoreError::Approval(
                crate::approval::ApprovalError::AlreadyResolved {
                    id: request.id(),
                    state: request.state(),
                },
            ));
        }
        // Redaction happens before the transaction opens, exactly as it does for
        // an event payload: the write lock holds two inserts and nothing else.
        let request = approval::redact(&**self.redactor(), request);
        let run_id = request.run_id();
        let prepared = self.prepare_event(run_id, approval::requested_event(&request)?)?;
        let seq = self.commit_event(
            "recording an approval request",
            prepared,
            |connection, prepared| {
                approval::insert(connection, &request)?;
                prepared.append(connection, run_id)
            },
        )?;
        Ok((request, seq))
    }

    /// Records a human decision, resolving the request it answers.
    ///
    /// The decision, the state change, and the `approval_decided` event share
    /// one short transaction taken with `BEGIN IMMEDIATE`, so two surfaces
    /// answering the same question cannot both win: the loser reads the
    /// already-resolved record under the write lock and is refused.
    ///
    /// Waking the waiter is the caller's next step, not this method's — the
    /// store owns durability and the [`ApprovalGate`](crate::approval::ApprovalGate)
    /// owns the rendezvous, and a store that also signalled would be signalling
    /// for decisions another process committed as well as its own.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Approval`] carrying
    /// [`AlreadyResolved`](crate::approval::ApprovalError::AlreadyResolved) when
    /// the request was already answered,
    /// [`ScopeExceedsRequest`](crate::approval::ApprovalError::ScopeExceedsRequest)
    /// when the grant is broader than the stored request allows, and
    /// [`StoreError::NotFound`] when no approval has that identity.
    pub fn decide_approval(
        &self,
        id: ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<(ApprovalRequest, EventSeq), StoreError> {
        let decision = approval::redact_decision(&**self.redactor(), decision);
        // One clone so the event can be described before the transaction opens
        // while the decision itself is still moved into the record it resolves.
        let announced = decision.clone();
        self.change_approval(
            "deciding an approval request",
            id,
            move |request| approval::decided_event(request, &announced),
            move |request| request.decide(decision),
        )
    }

    /// Resolves a pending request that nobody answered.
    ///
    /// The route to [`Expired`](crate::approval::ApprovalState::Expired),
    /// [`Cancelled`](crate::approval::ApprovalState::Cancelled), and
    /// [`Superseded`](crate::approval::ApprovalState::Superseded), each of which
    /// records that the question ended without a decision rather than
    /// manufacturing a refusal nobody made.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Approval`] carrying
    /// [`InvalidTransition`](crate::approval::ApprovalError::InvalidTransition)
    /// when the request is already resolved or the state is a decided one, and
    /// [`StoreError::NotFound`] when no approval has that identity.
    pub fn resolve_approval(
        &self,
        id: ApprovalId,
        to: ApprovalState,
        at: OffsetDateTime,
    ) -> Result<(ApprovalRequest, EventSeq), StoreError> {
        self.change_approval(
            "resolving an approval request",
            id,
            |request| approval::unanswered_event(request, to, at),
            |request| request.resolve(to, at),
        )
    }

    /// Loads one approval request.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] when no approval has that identity.
    pub fn approval(&self, id: ApprovalId) -> Result<ApprovalRequest, StoreError> {
        self.with_reader(|connection| approval::load(connection, id))
    }

    /// Lists every unanswered request across every run, oldest first.
    ///
    /// This is what a front end reads on start-up. A run interrupted mid-question
    /// left its row exactly as it was, so the question survives the restart with
    /// every binding field intact — which is what makes answering it afterwards
    /// safe rather than a guess about what was being asked.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Query`] when the listing statement fails.
    pub fn pending_approvals(&self) -> Result<Vec<ApprovalRequest>, StoreError> {
        self.with_reader(approval::list_pending)
    }

    /// Lists every request of one run, oldest first, whatever its state.
    ///
    /// # Errors
    ///
    /// As [`Store::pending_approvals`].
    pub fn run_approvals(&self, run_id: RunId) -> Result<Vec<ApprovalRequest>, StoreError> {
        self.with_reader(|connection| approval::list_for_run(connection, run_id))
    }

    /// Loads the grants of one run, for the approval matcher to evaluate.
    ///
    /// Every grant this returns is live by construction — only a granted
    /// request becomes one, and `granted` is terminal. Whether one *covers* a
    /// particular call is decided by
    /// [`grant_applies`](crate::approval::grant_applies), which reads no clock
    /// and touches no database, so a listing is grants and not verdicts.
    ///
    /// # Errors
    ///
    /// As [`Store::pending_approvals`].
    pub fn run_grants(&self, run_id: RunId) -> Result<Vec<ApprovalGrant>, StoreError> {
        self.with_reader(|connection| approval::list_grants(connection, run_id))
    }

    /// Applies one approval change and appends its event in a single
    /// transaction.
    ///
    /// The approval sibling of
    /// [`change_tool_call_with_event`](Self::change_tool_call_with_event), and
    /// for the same reason: keeping the pairing in one place is what stops a
    /// later outcome being added with its event outside the transaction, which
    /// would leave a question that was answered and a timeline that does not
    /// say so.
    ///
    /// `describe` builds the event from the record as it stands *plus* the
    /// outcome being applied, before the transaction opens, because redaction
    /// and encoding must not happen under the write lock. Describing an outcome
    /// in advance is safe here precisely because both halves are pure functions
    /// of the same inputs: if the record moved underneath, `change` refuses and
    /// the event rolls back with it.
    fn change_approval<E, F>(
        &self,
        operation: &'static str,
        id: ApprovalId,
        describe: E,
        change: F,
    ) -> Result<(ApprovalRequest, EventSeq), StoreError>
    where
        E: FnOnce(&ApprovalRequest) -> RunEvent,
        F: FnOnce(&mut ApprovalRequest) -> Result<(), crate::approval::ApprovalError>,
    {
        // The record is read before the transaction so an oversized payload can
        // be spilled outside it: no transaction is held across filesystem work.
        let current = self.approval(id)?;
        let run_id = current.run_id();
        let prepared = self.prepare_event(run_id, describe(&current))?;
        self.commit_event(operation, prepared, |connection, prepared| {
            let mut request = approval::load(connection, id)?;
            change(&mut request)?;
            approval::update_resolution(connection, &request)?;
            let seq = prepared.append(connection, request.run_id())?;
            Ok((request, seq))
        })
    }

    // -- tasks --------------------------------------------------------------

    /// Stores a task.
    ///
    /// The title is caller text that becomes a durable column, so it goes
    /// through the redactor like every other one. The workspace root does not:
    /// it is a filesystem identity this store compares, canonicalizes and hands
    /// to a scheduler, and a rewritten path would name a directory that does not
    /// exist. A path is also not a shape any rule here matches.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::AlreadyExists`] when the identity is taken and
    /// [`StoreError::NonUtf8Path`] when the workspace path cannot be stored.
    pub fn insert_task(&self, task: &Task) -> Result<(), StoreError> {
        match self.redactor.redact_text(task.title()) {
            std::borrow::Cow::Borrowed(_) => repository::insert_task(&guard(&self.writer), task),
            std::borrow::Cow::Owned(title) => {
                let redacted = task.clone().with_redacted_title(title);
                repository::insert_task(&guard(&self.writer), &redacted)
            }
        }
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

    /// Stores a run against an already-stored task, owned by nothing.
    ///
    /// A run with no lease is a run no coordinator is driving, so a later
    /// start's recovery sweep is right to interrupt it if it is not already
    /// terminal. Anything a coordinator is about to drive goes through
    /// [`insert_run_with_event`](Self::insert_run_with_event) with its lease.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::MissingParent`] when the task — or, for a retry,
    /// the run it follows — is not stored, and [`StoreError::AlreadyExists`]
    /// when the identity is taken.
    pub fn insert_run(&self, run: &Run) -> Result<(), StoreError> {
        repository::insert_run(&guard(&self.writer), run, None)
    }

    /// Stores a run, the lease claiming it, and its first event in one
    /// transaction.
    ///
    /// A refused event leaves no inert queued run behind, and the claim lands
    /// with the row rather than after it: a queued run that existed for even an
    /// instant with no lease is indistinguishable from one whose owner died,
    /// and a sweep running in another process would be right to interrupt it.
    ///
    /// The lease row is written here too, on first use, so a coordinator that
    /// never records anything leaves nothing behind to collect.
    pub fn insert_run_with_event(
        &self,
        run: &Run,
        owner: Option<&LeaseRecord>,
        event: RunEvent,
    ) -> Result<EventSeq, StoreError> {
        let prepared = self.prepare_event(run.id(), event)?;
        self.commit_event(
            "inserting a run with its first event",
            prepared,
            |connection, prepared| {
                if let Some(owner) = owner {
                    lease::ensure(connection, owner)?;
                }
                repository::insert_run(connection, run, owner.map(LeaseRecord::id))?;
                prepared.append(connection, run.id())
            },
        )
    }

    /// Stores a retry, its own first event, and the line it adds to the attempt
    /// it follows — all in one transaction.
    ///
    /// The pairing is the point. A retry exists exactly when the run it follows
    /// says it was re-attempted: recording the new row and then appending to
    /// the original as a second step would, on a refused append, leave a queued
    /// run owned by a live claim that no sweep will touch and no worker will
    /// ever drive, while its caller has been told the retry failed. Provenance
    /// that reads in both directions has to commit in one.
    ///
    /// # Errors
    ///
    /// As [`Store::insert_run_with_event`], plus [`StoreError::NotFound`] when
    /// the run being retried is not stored.
    pub fn insert_retry_with_events(
        &self,
        run: &Run,
        owner: Option<&LeaseRecord>,
        event: RunEvent,
        original: RunId,
        retried: RunEvent,
    ) -> Result<EventSeq, StoreError> {
        // Both payloads are redacted, encoded, and — in the impossible case —
        // spilled before the transaction opens, exactly as every other paired
        // write in this module prepares its own.
        let prepared = self.prepare_event(run.id(), event)?;
        let announced = match self.prepare_event(original, retried) {
            Ok(announced) => announced,
            Err(error) => {
                prepared.discard_spill(self);
                return Err(error);
            }
        };
        let result = self.in_write_transaction("inserting a retry with its events", |connection| {
            if let Some(owner) = owner {
                lease::ensure(connection, owner)?;
            }
            repository::insert_run(connection, run, owner.map(LeaseRecord::id))?;
            let seq = prepared.append(connection, run.id())?;
            announced.append(connection, original)?;
            Ok(seq)
        });
        if result.is_err() {
            prepared.discard_spill(self);
            announced.discard_spill(self);
        }
        result
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
        let failure = self.redact_failure(failure);
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
        let failure = self.redact_failure(failure);
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

    // -- leases and recovery -------------------------------------------------

    /// Refreshes a lease's "still here" timestamp.
    ///
    /// Reports whether a row moved: a lease that has taken no run has no row
    /// yet, and a released one is never resurrected. Neither is an error, and
    /// neither says anything about the holder being alive — the advisory lock
    /// file is what answers that, and this store never opens one.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Query`] when the statement fails.
    pub(crate) fn renew_lease(&self, id: LeaseId, at: OffsetDateTime) -> Result<bool, StoreError> {
        lease::renew(&guard(&self.writer), id, at).map(|updated| updated > 0)
    }

    /// Records that a lease is over.
    ///
    /// Idempotent, and deliberately unconditional on who is calling: a
    /// coordinator releases its own on the way out, and a recovery sweep
    /// releases one it proved dead.
    ///
    /// # Errors
    ///
    /// As [`Store::renew_lease`].
    pub(crate) fn release_lease(&self, id: LeaseId, at: OffsetDateTime) -> Result<(), StoreError> {
        lease::release(&guard(&self.writer), id, at)
    }

    /// Loads one lease record, reporting absence rather than failing.
    ///
    /// The record says what a claim *was*. Whether anybody still holds it is a
    /// question about a lock file, which this store never opens.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Query`] when the statement fails.
    pub fn lease(&self, id: LeaseId) -> Result<Option<LeaseRecord>, StoreError> {
        self.with_reader(|connection| lease::load(connection, id))
    }

    /// Lists every claim that has not been released, oldest first.
    ///
    /// "Not released" is what the rows say, not what any process is doing: a
    /// claim whose holder was killed stays here until a sweep writes it off.
    ///
    /// # Errors
    ///
    /// As [`Store::lease`].
    pub fn live_leases(&self) -> Result<Vec<LeaseRecord>, StoreError> {
        self.with_reader(lease::list_live)
    }

    /// Lists every run that has not reached a terminal state, with its claim.
    ///
    /// This is the recovery sweep's candidate query and it is deliberately not
    /// a record load: the answer is a state spelling and a lease identity per
    /// run, so a store holding a hundred abandoned runs costs one indexed scan
    /// rather than a hundred timelines read into memory.
    ///
    /// # Errors
    ///
    /// As [`Store::lease`].
    pub fn unfinished_runs(&self) -> Result<Vec<(RunId, Option<LeaseId>)>, StoreError> {
        self.with_reader(repository::unfinished_runs)
    }

    /// Lists the runs recorded as retries of `run`, oldest first.
    ///
    /// # Errors
    ///
    /// As [`Store::lease`].
    pub fn retries_of(&self, run: RunId) -> Result<Vec<RunId>, StoreError> {
        self.with_reader(|connection| repository::retries_of(connection, run))
    }

    /// Marks one abandoned run, everything under it, and its open questions.
    ///
    /// In one transaction: the run reaches `interrupted`, every unfinished step
    /// and in-flight tool call reaches `interrupted`, every pending approval is
    /// `superseded`, and each of those carries its own appended event beside a
    /// `run_interrupted` entry naming what was detected. Nothing already
    /// recorded is rewritten, so the timeline stays intact up to the moment the
    /// owning process stopped.
    ///
    /// `Ok(None)` means the run was already terminal — another sweeper, or the
    /// run's own process, got there first. That is what "exactly one set of
    /// markings" looks like from the loser's side, and it is not a failure.
    ///
    /// Deciding *that* a run was abandoned is not this method's job and cannot
    /// be: the store opens no lock file and probes no process. The caller
    /// supplies both the claim it read from the candidate scan and the reason
    /// it already proved — which is why this is reachable
    /// only from inside the crate. `interrupted` means the owning process
    /// stopped, and a front end that could write it would be able to put a
    /// claim about a dead process into the history of a live one.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] when no run has that identity, and
    /// [`StoreError::InvalidRecord`] when a row of the run cannot be rebuilt.
    pub(crate) fn interrupt_run(
        &self,
        run: RunId,
        owner: Option<LeaseId>,
        reason: InterruptionReason,
        at: OffsetDateTime,
    ) -> Result<Option<RunInterruption>, StoreError> {
        recovery::interrupt(self, run, owner, reason, at)
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

    /// Returns the newest `check.run` call for each requested configured id.
    ///
    /// The result is bounded by `check_ids`, whose catalog limit is 32, rather
    /// than by an arbitrary page of unrelated newer runs.
    pub fn project_latest_check_call_ids(
        &self,
        project_id: harkness_core::ProjectId,
        check_ids: &[String],
    ) -> Result<Vec<ToolCallId>, StoreError> {
        self.with_reader(|connection| {
            listing::project_latest_tool_call_ids_by_check(connection, project_id, check_ids)
        })
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
        let failure = self.redact_failure(failure);
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
        let failure = self.redact_failure(failure);
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
        let output = self.redact_output(output);
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
        let failure = self.redact_failure(failure);
        self.mutate_tool_call(id, |call| call.fail(failure, at))
    }

    /// Persists policy and applies its immediate lifecycle consequence.
    ///
    /// This is the only route from `pending` to `denied`. There is deliberately
    /// no decision-free denial: a call that was stopped by policy always
    /// carries the verdict, reason, and source that stopped it. A refusal at
    /// approval time goes through [`Store::reject_tool_call_approval`] instead
    /// and carries its own audit entry.
    ///
    /// The full decision and the `awaiting_approval` or `denied` transition it
    /// produces share one `BEGIN IMMEDIATE` transaction. `Allow` is persisted
    /// while the call remains pending, before a later dispatch may start it.
    ///
    /// # Errors
    ///
    /// As [`Store::transition_run`], for tool calls.
    pub fn apply_tool_call_policy_decision(
        &self,
        id: ToolCallId,
        decision: crate::policy::PolicyDecision,
        at: OffsetDateTime,
    ) -> Result<ToolCall, StoreError> {
        self.mutate_tool_call(id, |call| call.apply_policy_decision(decision, at))
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
        let failure = self.redact_failure(failure);
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
        self.commit_event("appending a run event", prepared, |connection, prepared| {
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
        self.commit_event(
            "transitioning a run with its event",
            prepared,
            |connection, prepared| {
                let mut run = repository::load_run(connection, id)?;
                run.transition(to, at)
                    .map_err(StoreError::InvalidTransition)?;
                repository::update_run(connection, &run)?;
                let seq = prepared.append(connection, id)?;
                Ok((run, seq))
            },
        )
    }

    /// Fails a run and appends its diagnostic state event in one transaction.
    pub fn fail_run_with_event(
        &self,
        id: RunId,
        failure: Failure,
        at: OffsetDateTime,
        event: RunEvent,
    ) -> Result<(Run, EventSeq), StoreError> {
        let failure = self.redact_failure(failure);
        self.change_run_with_event("failing a run with its event", id, event, move |run| {
            run.fail(failure, at)
        })
    }

    /// Records an approval and resumes a run with one atomic event.
    pub fn approve_run_with_event(
        &self,
        id: RunId,
        decided_by: &str,
        at: OffsetDateTime,
        event: RunEvent,
    ) -> Result<(Run, EventSeq), StoreError> {
        self.change_run_with_event("approving a run with its event", id, event, |run| {
            run.approve(decided_by, at)
        })
    }

    /// Records a denied tool-level approval and resumes agent orchestration.
    pub fn resume_run_after_denial_with_event(
        &self,
        id: RunId,
        decided_by: &str,
        at: OffsetDateTime,
        event: RunEvent,
    ) -> Result<(Run, EventSeq), StoreError> {
        self.change_run_with_event("resuming a run after a denied approval", id, event, |run| {
            run.resume_after_denial(decided_by, at)
        })
    }

    fn change_run_with_event<F>(
        &self,
        operation: &'static str,
        id: RunId,
        event: RunEvent,
        change: F,
    ) -> Result<(Run, EventSeq), StoreError>
    where
        F: FnOnce(&mut Run) -> Result<(), RunDomainError>,
    {
        let prepared = self.prepare_event(id, event)?;
        self.commit_event(operation, prepared, |connection, prepared| {
            let mut run = repository::load_run(connection, id)?;
            change(&mut run).map_err(StoreError::InvalidTransition)?;
            repository::update_run(connection, &run)?;
            let seq = prepared.append(connection, id)?;
            Ok((run, seq))
        })
    }

    /// Applies a step transition and appends its event in one transaction.
    pub fn transition_step_with_event(
        &self,
        id: StepId,
        to: crate::domain::ExecutionState,
        at: OffsetDateTime,
        event: RunEvent,
    ) -> Result<(Step, EventSeq), StoreError> {
        self.change_step_with_event("transitioning a step with its event", id, event, |step| {
            step.transition(to, at)
        })
    }

    /// Fails a step and appends its event in one transaction.
    pub fn fail_step_with_event(
        &self,
        id: StepId,
        failure: Failure,
        at: OffsetDateTime,
        event: RunEvent,
    ) -> Result<(Step, EventSeq), StoreError> {
        let failure = self.redact_failure(failure);
        self.change_step_with_event("failing a step with its event", id, event, move |step| {
            step.fail(failure, at)
        })
    }

    fn change_step_with_event<F>(
        &self,
        operation: &'static str,
        id: StepId,
        event: RunEvent,
        change: F,
    ) -> Result<(Step, EventSeq), StoreError>
    where
        F: FnOnce(&mut Step) -> Result<(), RunDomainError>,
    {
        let run_id = self.load_step(id)?.run_id();
        let prepared = self.prepare_event(run_id, event)?;
        self.commit_event(operation, prepared, |connection, prepared| {
            let mut step = repository::load_step(connection, id)?;
            change(&mut step).map_err(StoreError::InvalidTransition)?;
            repository::update_step(connection, &step)?;
            let seq = prepared.append(connection, run_id)?;
            Ok((step, seq))
        })
    }

    /// Persists policy and its immediate call-state consequence with one event.
    pub fn apply_tool_call_policy_decision_with_event(
        &self,
        id: ToolCallId,
        decision: crate::policy::PolicyDecision,
        at: OffsetDateTime,
        event: RunEvent,
    ) -> Result<(ToolCall, EventSeq), StoreError> {
        self.change_tool_call_with_event(
            "applying tool-call policy with its event",
            id,
            event,
            move |call| call.apply_policy_decision(decision, at),
        )
    }

    /// Records a denied approval and the call's terminal event atomically.
    pub fn reject_tool_call_approval_with_event(
        &self,
        id: ToolCallId,
        decided_by: &str,
        failure: Failure,
        at: OffsetDateTime,
        event: RunEvent,
    ) -> Result<(ToolCall, EventSeq), StoreError> {
        let failure = self.redact_failure(failure);
        self.change_tool_call_with_event(
            "rejecting a tool-call approval with its event",
            id,
            event,
            move |call| call.reject_approval(decided_by, failure, at),
        )
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
        self.change_tool_call_with_event(
            "transitioning a tool call with its event",
            id,
            event,
            |call| call.transition(to, at),
        )
    }

    /// Appends several events to one run's log in a single transaction.
    ///
    /// Sequence numbers are still allocated one at a time and inside the
    /// transaction, so the log is exactly what an equivalent series of
    /// [`append_event`](Self::append_event) calls would produce — and gaps
    /// remain permitted while monotonicity remains guaranteed. What changes is
    /// the cost: a running tool reports progress in bursts, and a transaction
    /// per event turns a chatty child into a benchmark of commit latency rather
    /// than of the work it is doing.
    ///
    /// Either every event is visible or none is. That is stronger than a series
    /// of appends would give and is the right way round: a partially recorded
    /// burst is a timeline that stops mid-phase for no reason a reader can see.
    ///
    /// # Errors
    ///
    /// As [`Store::append_event`]. One refused event refuses the batch, and any
    /// payload spilled on the way is cleaned up.
    pub fn append_events(
        &self,
        run_id: RunId,
        events: impl IntoIterator<Item = RunEvent>,
    ) -> Result<Vec<EventSeq>, StoreError> {
        // Every payload is redacted, encoded, and — if oversized — spilled to
        // disk before the transaction opens, exactly as the single-event path
        // does. What the transaction then holds is inserts.
        let mut prepared = Vec::new();
        for event in events {
            match self.prepare_event(run_id, event) {
                Ok(event) => prepared.push(event),
                Err(error) => {
                    // Whatever earlier events of this batch already spilled is
                    // removed: none of them will be recorded, and a caller
                    // retrying must not leave a file per attempt behind.
                    for spilled in &prepared {
                        spilled.discard_spill(self);
                    }
                    return Err(error);
                }
            }
        }
        if prepared.is_empty() {
            return Ok(Vec::new());
        }

        let result = self.in_write_transaction("appending run events", |connection| {
            prepared
                .iter()
                .map(|event| event.append(connection, run_id))
                .collect::<Result<Vec<_>, _>>()
        });
        if result.is_err() {
            for event in &prepared {
                event.discard_spill(self);
            }
        }
        result
    }

    /// Begins a tool call, pinning the resolved version, and appends its event.
    ///
    /// The one transition that also *writes* a column, because the version that
    /// ran is only known at the moment execution starts; see
    /// [`ToolCall::dispatch`](crate::domain::ToolCall::dispatch).
    ///
    /// # Errors
    ///
    /// As [`Store::transition_tool_call_with_event`], plus
    /// [`StoreError::InvalidTransition`] when the record already names a
    /// different version.
    pub fn dispatch_tool_call_with_event(
        &self,
        id: ToolCallId,
        tool_version: &str,
        at: OffsetDateTime,
        event: RunEvent,
    ) -> Result<(ToolCall, EventSeq), StoreError> {
        self.begin_tool_call(id, event, "dispatching a tool call", |call| {
            call.dispatch(tool_version, at)
        })
    }

    /// Applies one start-of-execution change, which also pins the version.
    ///
    /// Separate from [`change_tool_call_with_event`](Self::change_tool_call_with_event)
    /// for one reason: these are the only transitions that rewrite part of the
    /// recorded *request*. See
    /// [`repository::pin_tool_call_version`] for why that column is the single
    /// exception and why `update_tool_call` still names none of them.
    fn begin_tool_call<F>(
        &self,
        id: ToolCallId,
        event: RunEvent,
        operation: &'static str,
        start: F,
    ) -> Result<(ToolCall, EventSeq), StoreError>
    where
        F: FnOnce(&mut ToolCall) -> Result<(), RunDomainError>,
    {
        let run_id = self.load_tool_call(id)?.run_id();
        let prepared = self.prepare_event(run_id, event)?;
        self.commit_event(operation, prepared, |connection, prepared| {
            let mut call = repository::load_tool_call(connection, id)?;
            start(&mut call).map_err(StoreError::InvalidTransition)?;
            repository::update_tool_call(connection, &call)?;
            repository::pin_tool_call_version(connection, &call)?;
            let seq = prepared.append(connection, call.run_id())?;
            Ok((call, seq))
        })
    }

    /// Records an approval, pins the resolved version, and appends its event.
    ///
    /// The approval-gated sibling of
    /// [`dispatch_tool_call_with_event`](Self::dispatch_tool_call_with_event).
    /// The decision, the version it authorized, the transition, and the event
    /// share one transaction, so an audit can never read an approval whose call
    /// names a version the approver did not see.
    ///
    /// # Errors
    ///
    /// As [`Store::dispatch_tool_call_with_event`], and
    /// [`StoreError::InvalidTransition`] when the call is not
    /// `awaiting_approval` or the identity is blank.
    pub fn dispatch_approved_tool_call_with_event(
        &self,
        id: ToolCallId,
        decided_by: &str,
        tool_version: &str,
        at: OffsetDateTime,
        event: RunEvent,
    ) -> Result<(ToolCall, EventSeq), StoreError> {
        let call = self.load_tool_call(id)?;
        let expected_tool = ToolIdentity::parse(call.tool_id(), tool_version).map_err(|error| {
            StoreError::ColumnEncoding {
                record: "tool_call",
                field: "tool_id",
                reason: error.to_string(),
            }
        })?;
        let expected_input_hash = canonical_input_hash(call.input())?;
        self.dispatch_bound_approved_tool_call_with_event(
            id,
            decided_by,
            &expected_tool,
            expected_input_hash,
            at,
            event,
        )
    }

    /// Dispatches an approved call only while its durable request still
    /// matches the identity and input digest authorized by the coordinator.
    pub fn dispatch_bound_approved_tool_call_with_event(
        &self,
        id: ToolCallId,
        decided_by: &str,
        expected_tool: &ToolIdentity,
        expected_input_hash: InputHash,
        at: OffsetDateTime,
        event: RunEvent,
    ) -> Result<(ToolCall, EventSeq), StoreError> {
        let run_id = self.load_tool_call(id)?.run_id();
        let prepared = self.prepare_event(run_id, event)?;
        self.commit_event(
            "dispatching an approved tool call",
            prepared,
            |connection, prepared| {
                let mut call = repository::load_tool_call(connection, id)?;
                if call.tool_id() != expected_tool.id.as_str()
                    || (!call.tool_version().is_empty()
                        && call.tool_version() != expected_tool.version.to_string())
                {
                    return Err(StoreError::ApprovalBindingMismatch {
                        call: id.to_string(),
                        reason: "tool identity changed",
                    });
                }
                let actual_hash = canonical_input_hash(call.input())?;
                if actual_hash != expected_input_hash {
                    return Err(StoreError::ApprovalBindingMismatch {
                        call: id.to_string(),
                        reason: "input changed",
                    });
                }
                call.dispatch_approved(decided_by, expected_tool.version.to_string(), at)
                    .map_err(StoreError::InvalidTransition)?;
                repository::update_tool_call(connection, &call)?;
                repository::pin_tool_call_version(connection, &call)?;
                let seq = prepared.append(connection, call.run_id())?;
                Ok((call, seq))
            },
        )
    }

    /// Succeeds a tool call with its output and appends its event.
    ///
    /// # Errors
    ///
    /// As [`Store::transition_tool_call_with_event`], plus
    /// [`StoreError::PayloadTooLarge`] when the output exceeds
    /// [`MAX_INLINE_PAYLOAD_BYTES`].
    pub fn succeed_tool_call_with_event(
        &self,
        id: ToolCallId,
        output: Value,
        at: OffsetDateTime,
        event: RunEvent,
    ) -> Result<(ToolCall, EventSeq), StoreError> {
        let output = self.redact_output(output);
        self.change_tool_call_with_event(
            "succeeding a tool call with its event",
            id,
            event,
            move |call| call.succeed(output, at),
        )
    }

    /// Fails a tool call with structured detail and appends its event.
    ///
    /// # Errors
    ///
    /// As [`Store::transition_tool_call_with_event`].
    pub fn fail_tool_call_with_event(
        &self,
        id: ToolCallId,
        failure: Failure,
        at: OffsetDateTime,
        event: RunEvent,
    ) -> Result<(ToolCall, EventSeq), StoreError> {
        let failure = self.redact_failure(failure);
        self.change_tool_call_with_event(
            "failing a tool call with its event",
            id,
            event,
            move |call| call.fail(failure, at),
        )
    }

    /// Applies one tool-call change and appends its event in a single
    /// transaction.
    ///
    /// The shape every outcome-specific pairing shares. Keeping it in one place
    /// is what stops a later outcome from being added with the event outside the
    /// transaction, which would leave a call whose state moved and whose history
    /// does not say so.
    ///
    /// `FnOnce` rather than `FnMut`, so a change may simply *own* the value it
    /// applies. A repeatable bound would force each caller to hand its output or
    /// its failure over through an `Option` and invent something to store on a
    /// second call — and the day a busy-retry is wrapped around the transaction
    /// below, those inventions would quietly persist a null result or a
    /// placeholder failure instead of failing loudly. The type makes running
    /// twice impossible instead of merely unlikely.
    fn change_tool_call_with_event<F>(
        &self,
        operation: &'static str,
        id: ToolCallId,
        event: RunEvent,
        change: F,
    ) -> Result<(ToolCall, EventSeq), StoreError>
    where
        F: FnOnce(&mut ToolCall) -> Result<(), RunDomainError>,
    {
        // The run is read before the transaction so the payload can be spilled
        // and redacted outside it: no transaction is held across work that
        // touches the filesystem.
        let run_id = self.load_tool_call(id)?.run_id();
        let prepared = self.prepare_event(run_id, event)?;
        self.commit_event(operation, prepared, |connection, prepared| {
            let mut call = repository::load_tool_call(connection, id)?;
            change(&mut call).map_err(StoreError::InvalidTransition)?;
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

    /// Returns one page of a run's event log in either direction.
    ///
    /// The sibling of [`Store::events`] for a surface rather than a subscriber:
    /// it can open at the newest end, and its
    /// [`next_cursor`](EventListing::next_cursor) distinguishes a full page with
    /// more behind it from a full page that is simply last. A page addresses a
    /// position in the log, so events appended between two requests neither
    /// repeat nor hide an entry the caller has yet to see.
    ///
    /// An unknown run is an empty page rather than an error: the log of a run
    /// this store never held and the log of a run that recorded nothing are the
    /// same statement here. A caller that must tell them apart loads the run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidPageLimit`] when the page size is zero or
    /// above [`MAX_EVENT_PAGE_LIMIT`].
    pub fn event_page(&self, run_id: RunId, page: EventPage) -> Result<EventListing, StoreError> {
        self.with_reader(|connection| event::event_page(connection, run_id, page))
    }

    /// Returns the highest durable event sequence for `run_id`, if any.
    pub fn latest_event_seq(&self, run_id: RunId) -> Result<Option<EventSeq>, StoreError> {
        self.with_reader(|connection| event::latest_sequence(connection, run_id))
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
        ArtifactSink::create(
            self,
            run_id,
            name,
            media_type,
            at,
            artifact::Redaction::Pending,
        )
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
    /// Harkness data directory containing this store and its artifacts.
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The redactor every recorded byte passes through.
    ///
    /// Public because a front end has to be able to project a record through the
    /// *same* rules the coordinator will, and a second, separately chosen
    /// redactor would silently disagree with it: a workspace reference derived
    /// with [`PassThrough`] no longer equals the one
    /// [`RunCoordinator`](crate::coordinator::RunCoordinator) rebuilds from the
    /// stored task, and the run is refused for a mismatch nobody introduced.
    /// Reading the store's own answer is the only way to be sure they agree.
    ///
    /// It hands back a shared reference and no way to replace it, so publishing
    /// it does not make the redactor swappable — see [`Store::redacting`] for
    /// why that stays a construction-time decision.
    #[must_use]
    pub fn redactor(&self) -> &Arc<dyn Redactor> {
        &self.redactor
    }

    /// Rewrites a failure's detail before it becomes a durable column.
    ///
    /// Applied where the value *arrives* rather than deep in the repository
    /// layer, so it happens once and outside every write transaction — the same
    /// discipline `prepare_event` follows, and for the same reason: redaction is
    /// the expensive part and a write lock is the last place to spend it.
    fn redact_failure(&self, failure: Failure) -> Failure {
        let message = self.redactor.redact_text(failure.message());
        match message {
            std::borrow::Cow::Borrowed(_) => failure,
            std::borrow::Cow::Owned(redacted) => failure.with_redacted_message(redacted),
        }
    }

    /// Rewrites a tool's result before it becomes `tool_calls.output_json`.
    ///
    /// A result is a document whose *values* a tool chose, so it is walked
    /// exactly as an event payload is: strings rewritten, keys and structure
    /// untouched. Its sibling column `input_json` deliberately is not — see the
    /// coverage table in [`observe`](crate::observe), and
    /// [`ToolExecutor`](crate::tool::ToolExecutor), which runs the bytes it
    /// reads back out of it.
    ///
    /// # Redaction happens before the inline bound, and can move it
    ///
    /// A replacement marker is longer than much of what it replaces, so a result
    /// that fitted [`MAX_INLINE_PAYLOAD_BYTES`] before scrubbing can exceed it
    /// after — and is then refused with [`StoreError::PayloadTooLarge`] exactly
    /// as an oversized one is. That ordering is the deliberate one: checking
    /// first would admit a row whose *stored* form breaks the bound every other
    /// column keeps, and the artifact store already checks a label after
    /// redaction for the same reason.
    ///
    /// The consequence is the one the inline threshold already documents — the
    /// caller summarizes and retries — and it is worth knowing that redaction is
    /// among the reasons a result can cross the line. A tool returning bulk
    /// content should be writing an artifact rather than a payload either way.
    fn redact_output(&self, output: Value) -> Value {
        redaction::redact_payload(&*self.redactor, &output)
    }

    /// The write connection, for a test that has to author a row this store
    /// would never write — a hand edit, in other words, which is exactly the
    /// input the load-time validation exists to refuse.
    #[cfg(test)]
    pub(crate) fn writer_for_test(&self) -> MutexGuard<'_, Connection> {
        guard(&self.writer)
    }

    /// Runs a prepared event's write, cleaning up its spill if the write fails.
    ///
    /// The crash matrix in [`artifact`](self::artifact) accounts for the orphan
    /// file a *crash* between the rename and the insert leaves behind. An
    /// ordinary rejection is not a crash: an invalid transition or an event
    /// naming an unstored step returns `Err` on a perfectly healthy store, and a
    /// caller retrying one must not accumulate a file per attempt in a store
    /// with no collector. Removal is best effort — the write's own failure is
    /// what the caller needs to hear about.
    fn commit_event<T, F>(
        &self,
        operation: &'static str,
        prepared: PreparedEvent,
        change: F,
    ) -> Result<T, StoreError>
    where
        F: FnOnce(&Connection, &PreparedEvent) -> Result<T, StoreError>,
    {
        let result =
            self.in_write_transaction(operation, |connection| change(connection, &prepared));
        if result.is_err() {
            prepared.discard_spill(self);
        }
        result
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

        // The artifact carries the *redacted* encoding — the exact bytes the row
        // would have held had they fit — so it is created in `Redaction::Applied`
        // mode and the stream wrapper does not scrub them a second time.
        //
        // Spilling the caller's original and leaving the wrapper to do the work
        // would make redaction depend on payload size: a rule implemented in
        // `redact_text` alone, which the trait allows, would scrub a payload
        // under the threshold and persist the same secret in the clear above it.
        // It would also rewrite object keys, so the recovered payload would not
        // be the one the inline path produces.
        let spilled = encoded.into_bytes();
        let mut sink = artifact::ArtifactSink::create(
            self,
            run_id,
            event::OVERFLOW_PAYLOAD_NAME,
            event::OVERFLOW_PAYLOAD_MEDIA_TYPE,
            event.at(),
            artifact::Redaction::Applied,
        )?;
        if let Some(step_id) = event.step_id() {
            sink = sink.for_step(step_id);
        }
        if let Some(tool_call_id) = event.tool_call_id() {
            sink = sink.for_tool_call(tool_call_id);
        }
        let staged = sink.temporary().to_path_buf();
        std::io::Write::write_all(&mut sink, &spilled).map_err(|source| {
            StoreError::ArtifactIo {
                operation: "spilling an oversized event payload",
                path: staged,
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

    /// Removes the spilled artifact's bytes after a rejected write.
    ///
    /// Best effort: the caller is about to be told why the write failed, and a
    /// file that outlives its row is the harmless half of the crash matrix.
    fn discard_spill(&self, store: &Store) {
        if let Some(sealed) = &self.sealed {
            let _ = fs::remove_file(artifact::artifact_path(
                store.data_dir(),
                sealed.run_id(),
                sealed.id(),
            ));
        }
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
/// # The context cache has a twin of this
///
/// `harkness_context::index` runs the same three routines against its own
/// database. They are deliberately not shared — the only crate beneath both is
/// `harkness-git`, which has no business gaining a SQLite dependency, and
/// ADR-0004 already accepts "two databases, two connection disciplines" — but
/// **a change to the contention handling below has to be made in both places.**
/// The retry exists for a Windows-only failure, so a divergence is invisible on
/// two of the three matrix legs.
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
