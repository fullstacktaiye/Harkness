//! The disposable per-repository index cache.
//!
//! One SQLite database per repository at
//! `<data_dir>/context/<repository-key>/index.db`, where `<repository-key>` is
//! the v5 UUID `harkness-git` already keys the repository lock by. Every linked
//! worktree of one repository therefore maps to one cache root, and per-worktree
//! state is isolated *inside* it ([#115]).
//!
//! # This holds derivation, never evidence
//!
//! ADR-0004 splits the two stores by that question alone. `runtime.db` and
//! `artifacts/` hold what Harkness actually did — runs, provenance, approvals,
//! the snapshot rows this cache's generation feeds. Everything here is
//! recomputable from repository content, so deleting `<data_dir>/context/` at
//! any moment costs warm-up time and nothing else. Nothing in this module may
//! ever write the other side, and the crate boundary makes that structural
//! rather than intended: `harkness-context` cannot name `harkness-runtime`.
//!
//! # Versioning is four fields, not one
//!
//! [`index_meta`](IndexMeta) holds exactly one row. Its `schema_version`
//! describes the cache's own table layout and is the only field a mismatch
//! cannot be reconciled from: an older cache is quarantined and recreated, and
//! a *newer* one is refused read-only and left byte-identical, mirroring the
//! run store's `schema_too_new`. The three component versions — parser,
//! chunking, ranking — describe what produced the rows rather than where they
//! sit, so a mismatch leaves the file alone and marks that component's data
//! stale for [#114] to reconcile incrementally. Rewriting the stored component
//! version at open would destroy exactly the knowledge that reconciliation
//! needs.
//!
//! # The generation is a token, not a counter
//!
//! `index_generation` is a component of the workspace snapshot digest
//! (ADR-0008), so a snapshot taken against a rebuilt index must not compare
//! equal to one taken against the index that produced it. A plain counter
//! cannot promise that, because the counter lives *in the file being deleted*:
//! wiping `<data_dir>/context/` and starting again at one would make every
//! stale snapshot verify as fresh. A new generation therefore seeds from the
//! wall clock in nanoseconds and keeps `previous + 1` as a floor, so a
//! recreation is strictly greater than what came before it whether or not the
//! previous value survived, and a clock that steps backwards cannot reissue a
//! number some snapshot already recorded.
//!
//! # Locking
//!
//! The cache's connection lock is **leaf-level**: it is never held while the
//! repository lock or the catalog lock is acquired, so the workspace's
//! repository-then-catalog ordering is untouched. [`IndexCache::status`] takes
//! a different, short-held lock and never the connection's, which is what lets
//! a UI poll answer while a cold index build is running.
//!
//! [#114]: https://github.com/fullstacktaiye/harkness/issues/114
//! [#115]: https://github.com/fullstacktaiye/harkness/issues/115

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use harkness_git::Cancellation;
use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, named_params};
use time::format_description::BorrowedFormatItem;
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset, macros::format_description};

use crate::error::ContextEngineError;

/// Name of the cache database inside one repository's cache root.
pub const INDEX_DATABASE_FILE: &str = "index.db";

/// Prefix every quarantined cache file carries.
pub const QUARANTINE_PREFIX: &str = "index.db.corrupt-";

/// How many quarantined caches are kept before the oldest is deleted.
///
/// Two is enough to compare "the corruption that just happened" with "the one
/// before it" and small enough that a repeatedly failing cache cannot fill a
/// disk with copies of itself.
pub const MAX_QUARANTINED_CACHES: usize = 2;

/// Newest cache table layout this build understands.
pub const INDEX_SCHEMA_VERSION: u32 = 1;

/// Version of the language grammars and symbol extraction that filled the cache.
///
/// `0` means this build has none, which is the honest answer until [#117]
/// lands: no row in any cache was produced by a parser, so nothing can be
/// stale against one.
///
/// [#117]: https://github.com/fullstacktaiye/harkness/issues/117
pub const PARSER_VERSION: &str = "0";

/// Version of the chunk-boundary rules that filled the cache ([#113]).
///
/// [#113]: https://github.com/fullstacktaiye/harkness/issues/113
pub const CHUNKING_VERSION: &str = "0";

/// Version of the scoring formula whose results the cache holds ([#121]).
///
/// [#121]: https://github.com/fullstacktaiye/harkness/issues/121
pub const RANKING_VERSION: &str = "0";

/// How long a connection waits for another process's writer before giving up.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the write-ahead-log transition re-checks a contended database.
const WAL_RETRY_INTERVAL: Duration = Duration::from_millis(20);

/// Filename-safe, fixed-width, lexicographically chronological stamp.
///
/// Fixed width is what lets quarantine rotation sort by name instead of asking
/// the filesystem for modification times it is not obliged to keep.
const QUARANTINE_STAMP: &[BorrowedFormatItem<'_>] =
    format_description!("[year][month][day]T[hour][minute][second][subsecond digits:9]Z");

/// The single-row schema every cache carries from the day it is created.
const INDEX_META_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS index_meta (
    id                  INTEGER PRIMARY KEY CHECK (id = 1),
    schema_version      INTEGER NOT NULL,
    parser_version      TEXT    NOT NULL,
    chunking_version    TEXT    NOT NULL,
    ranking_version     TEXT    NOT NULL,
    index_generation    INTEGER NOT NULL,
    repository_identity TEXT    NOT NULL,
    created_at          TEXT    NOT NULL
) STRICT;";

/// The versions a build expects the caches it opens to have been written under.
///
/// Compiled into the binary rather than read from anywhere: a cache is only
/// usable by the code that produced its rows, and asking the cache what it
/// should be would make every mismatch invisible.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExpectedVersions {
    /// Cache table layout this build can read.
    pub schema_version: u32,
    /// Language grammars and symbol extraction.
    pub parser_version: String,
    /// Chunk-boundary rules.
    pub chunking_version: String,
    /// Scoring formula.
    pub ranking_version: String,
}

impl Default for ExpectedVersions {
    fn default() -> Self {
        Self::current()
    }
}

impl ExpectedVersions {
    /// The versions this build was compiled with.
    #[must_use]
    pub fn current() -> Self {
        Self {
            schema_version: INDEX_SCHEMA_VERSION,
            parser_version: PARSER_VERSION.to_owned(),
            chunking_version: CHUNKING_VERSION.to_owned(),
            ranking_version: RANKING_VERSION.to_owned(),
        }
    }

    /// Every component of `stored` whose version this build disagrees with.
    ///
    /// The schema version is deliberately absent: a schema mismatch is not a
    /// stale component, it is a cache this build cannot address at all, and it
    /// is decided before a component is ever compared.
    #[must_use]
    pub fn skew(&self, stored: &IndexMeta) -> Vec<VersionSkew> {
        [
            (
                IndexComponent::Parser,
                &stored.parser_version,
                &self.parser_version,
            ),
            (
                IndexComponent::Chunking,
                &stored.chunking_version,
                &self.chunking_version,
            ),
            (
                IndexComponent::Ranking,
                &stored.ranking_version,
                &self.ranking_version,
            ),
        ]
        .into_iter()
        .filter(|(_, found, expected)| found != expected)
        .map(|(component, found, expected)| VersionSkew {
            component,
            stored: (*found).clone(),
            expected: (*expected).clone(),
        })
        .collect()
    }
}

/// One part of the cache whose producer is versioned independently.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum IndexComponent {
    /// Language grammars and symbol extraction.
    Parser,
    /// Chunk-boundary rules.
    Chunking,
    /// Scoring formula.
    Ranking,
}

impl IndexComponent {
    /// Stable spelling used in status reports and event payloads.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Parser => "parser",
            Self::Chunking => "chunking",
            Self::Ranking => "ranking",
        }
    }
}

impl std::fmt::Display for IndexComponent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A component whose cached rows were produced by a version this build is not.
///
/// The rows are kept rather than dropped: [#114] reconciles them incrementally,
/// and throwing away a whole repository's chunks because a ranking formula
/// moved would make every retrieval improvement a cold rebuild.
///
/// [#114]: https://github.com/fullstacktaiye/harkness/issues/114
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VersionSkew {
    /// Which component disagreed.
    pub component: IndexComponent,
    /// Version recorded in `index_meta`.
    pub stored: String,
    /// Version this build expects.
    pub expected: String,
}

/// Why a cache was thrown away and rebuilt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RecreationReason {
    /// The database could not be read, or its metadata was not usable.
    Corrupt,
    /// The recorded schema version was one this build cannot address.
    Version,
    /// A caller asked for the cache to be discarded.
    Disposed,
}

impl RecreationReason {
    /// Stable spelling carried in the `context_cache_recreated` event payload.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Corrupt => "corrupt",
            Self::Version => "version",
            Self::Disposed => "disposed",
        }
    }
}

impl std::fmt::Display for RecreationReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One recreation of a cache, as a front end would report it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CacheRecreation {
    /// Why the previous cache was not kept.
    pub reason: RecreationReason,
    /// What was actually wrong, beyond the class of fault.
    ///
    /// A [`reason`](Self::reason) is what a payload branches on and this is what
    /// a person reads. "corrupt" is not enough to act on; "index_meta is
    /// missing or unreadable" is, and it costs one string on a path that runs
    /// once per fault.
    pub detail: String,
    /// Generation the discarded cache carried, when it could still be read.
    pub previous_generation: Option<u64>,
    /// Generation the replacement carries.
    pub generation: u64,
    /// Where the unusable bytes were moved, when any were kept.
    pub quarantined_to: Option<PathBuf>,
}

/// The single `index_meta` row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexMeta {
    /// Cache table layout the file was written under.
    pub schema_version: u32,
    /// Language grammars and symbol extraction that filled it.
    pub parser_version: String,
    /// Chunk-boundary rules that filled it.
    pub chunking_version: String,
    /// Scoring formula whose results it holds.
    pub ranking_version: String,
    /// Monotonic token naming this build of the cache.
    pub index_generation: u64,
    /// Repository the cache was built for, in `harkness-git`'s spelling.
    pub repository_identity: String,
    /// When the cache was created, RFC 3339 UTC.
    pub created_at: OffsetDateTime,
}

/// Whether the cache is usable, and why not when it is not.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum IndexAvailability {
    /// The cache is open and can be read and written.
    Ready,
    /// The cache could not be prepared; cache-backed calls fail with this.
    Unavailable {
        /// Stable discriminant of the failure that kept it closed.
        kind: &'static str,
        /// Human-readable explanation, already formatted.
        detail: String,
    },
}

/// An operation running against the cache right now.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexOperation {
    /// Stable name of the operation.
    pub name: &'static str,
    /// Progress where the operation can bound its own work.
    pub percent_complete: Option<u8>,
}

/// How much the cache holds, once there is anything in it to count.
///
/// [#114] introduces the content tables and populates this; until then the
/// answer is [`None`] rather than a row of zeroes, because "nothing is indexed"
/// and "nobody can say yet" are different things to render.
///
/// [#114]: https://github.com/fullstacktaiye/harkness/issues/114
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct IndexCounts {
    /// Files with at least one indexed chunk.
    pub files: u64,
    /// Chunks held.
    pub chunks: u64,
    /// Symbols held.
    pub symbols: u64,
}

/// A non-blocking view of the cache, for a UI to poll.
///
/// Every field is read from a short-held lock that no long operation takes, so
/// asking for status during a cold index build answers immediately instead of
/// queueing behind the writer.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct IndexStatus {
    /// Generation the open cache carries; `0` when there is none.
    pub generation: u64,
    /// Whether cache-backed calls can be served.
    pub availability: IndexAvailability,
    /// Repository this cache belongs to.
    pub repository_identity: String,
    /// The most recent recreation, when this process performed one.
    pub last_recreation: Option<CacheRecreation>,
    /// Components whose rows were produced by another version.
    pub stale_components: Vec<VersionSkew>,
    /// When [`IndexCache::refresh`] last completed.
    pub last_refreshed_at: Option<OffsetDateTime>,
    /// What the cache is doing right now.
    pub in_progress: Option<IndexOperation>,
    /// What the cache holds, once anything counts it.
    pub counts: Option<IndexCounts>,
}

/// What one refresh of the cache did.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct IndexReport {
    /// Generation the cache carried when the refresh finished.
    pub generation: u64,
    /// Components still holding rows this build did not produce.
    pub stale_components: Vec<VersionSkew>,
    /// Content entries reconciled.
    ///
    /// Zero until [#114] adds the content tables there is anything to
    /// reconcile *in*; the field is here so the report's shape does not change
    /// when it does.
    ///
    /// [#114]: https://github.com/fullstacktaiye/harkness/issues/114
    pub entries_reconciled: u64,
    /// How long the refresh took.
    pub duration: Duration,
}

/// Mutable state published to [`IndexCache::status`].
#[derive(Debug)]
struct CacheState {
    meta: IndexMeta,
    stale_components: Vec<VersionSkew>,
    last_recreation: Option<CacheRecreation>,
    last_refreshed_at: Option<OffsetDateTime>,
    in_progress: Option<IndexOperation>,
}

/// One repository's disposable index cache.
///
/// # One connection, deliberately
///
/// The cache holds a single connection behind a mutex while `index_meta` is the
/// only table in it: there is nothing to read concurrently, and one handle
/// makes disposal exact rather than best effort — a caller can only be inside
/// the cache while it holds the lock, so a recreation never races a read it
/// cannot see. [#114] adds the content tables and the pooled readers they
/// justify, and owes this the same treatment the run store gives its own pool.
///
/// # Lock order
///
/// `connection` then `state`, never the other way round, and neither is held
/// while any lock outside this crate is acquired. `status` takes `state`
/// alone, which is what keeps it off the writer's path.
///
/// [#114]: https://github.com/fullstacktaiye/harkness/issues/114
#[derive(Debug)]
pub struct IndexCache {
    root: PathBuf,
    database: PathBuf,
    expected: ExpectedVersions,
    repository_identity: String,
    /// `None` only between a failed recreation and the next successful open.
    connection: Mutex<Option<Connection>>,
    state: Mutex<CacheState>,
}

impl IndexCache {
    /// Opens the cache under `cache_root`, creating or replacing it as needed.
    ///
    /// `repository_identity` is what the cache records it was built for, and a
    /// file recording a different one is not this repository's cache however it
    /// came to be at this path — it is quarantined rather than read, because
    /// serving one checkout's chunks for another is the cross-repository bleed
    /// the derived path exists to prevent.
    ///
    /// The sequence is: read the metadata *without writing*, decide, and only
    /// then open for writing. That order is what lets a cache written by a
    /// newer build be refused with its bytes untouched.
    ///
    /// # Errors
    ///
    /// Returns [`ContextEngineError::CacheVersionConflict`] for a cache written
    /// by a newer build, and [`ContextEngineError::CacheOpenFailed`] when the
    /// directory or the connection could not be prepared. A cache that is
    /// merely unreadable is not an error: it is quarantined and replaced, and
    /// the replacement is reported through [`IndexCache::status`].
    pub fn open_or_create(
        cache_root: &Path,
        expected: &ExpectedVersions,
        repository_identity: &str,
    ) -> Result<Self, ContextEngineError> {
        let database = cache_root.join(INDEX_DATABASE_FILE);
        fs::create_dir_all(cache_root).map_err(|error| ContextEngineError::CacheOpenFailed {
            path: database.clone(),
            reason: format!("the cache directory could not be created: {error}"),
        })?;

        let probed = probe_existing(&database, expected, repository_identity)?;
        let (connection, meta, stale_components, last_recreation) = match probed {
            Probe::Usable(meta) => {
                let connection = open_writable(&database)?;
                let stale = expected.skew(&meta);
                (connection, meta, stale, None)
            }
            // A cache that was never there is a first build, not a recreation.
            // Reporting one would make an ordinary cold start look like a fault
            // in every surface that renders the reason.
            Probe::Absent => {
                let (connection, meta) = create(
                    &database,
                    expected,
                    repository_identity,
                    next_generation(None),
                )?;
                (connection, meta, Vec::new(), None)
            }
            Probe::Replace {
                reason,
                previous_generation,
                detail,
            } => {
                let quarantined_to = quarantine(&database)?;
                let (connection, meta) = create(
                    &database,
                    expected,
                    repository_identity,
                    next_generation(previous_generation),
                )?;
                let recreation = CacheRecreation {
                    reason,
                    detail,
                    previous_generation,
                    generation: meta.index_generation,
                    quarantined_to,
                };
                (connection, meta, Vec::new(), Some(recreation))
            }
        };

        Ok(Self {
            root: cache_root.to_path_buf(),
            database,
            expected: expected.clone(),
            repository_identity: repository_identity.to_owned(),
            connection: Mutex::new(Some(connection)),
            state: Mutex::new(CacheState {
                meta,
                stale_components,
                last_recreation,
                last_refreshed_at: None,
                in_progress: None,
            }),
        })
    }

    /// Directory this cache owns.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The cache database file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.database
    }

    /// The generation the open cache carries.
    ///
    /// This is what a [`WorkspaceSnapshot`](crate::WorkspaceSnapshot) absorbs
    /// into its identity, so a snapshot taken against a rebuilt cache never
    /// compares equal to one taken against the cache that produced it.
    #[must_use]
    pub fn generation(&self) -> u64 {
        lock(&self.state).meta.index_generation
    }

    /// The single `index_meta` row as it stands.
    #[must_use]
    pub fn meta(&self) -> IndexMeta {
        lock(&self.state).meta.clone()
    }

    /// A non-blocking view for a UI to poll.
    #[must_use]
    pub fn status(&self) -> IndexStatus {
        let state = lock(&self.state);
        IndexStatus {
            generation: state.meta.index_generation,
            availability: IndexAvailability::Ready,
            repository_identity: state.meta.repository_identity.clone(),
            last_recreation: state.last_recreation.clone(),
            stale_components: state.stale_components.clone(),
            last_refreshed_at: state.last_refreshed_at,
            in_progress: state.in_progress.clone(),
            counts: None,
        }
    }

    /// Throws the cache away and starts an empty one.
    ///
    /// The generation moves, so every snapshot taken against the old cache
    /// stops verifying as fresh. Nothing is quarantined: a caller asking for
    /// disposal is not reporting a fault, and keeping a copy of a cache
    /// somebody asked to be rid of would defeat "delete this to reclaim disk".
    ///
    /// # Errors
    ///
    /// Returns [`ContextEngineError::CacheOpenFailed`] when the replacement
    /// could not be created. The cache is left closed in that case and every
    /// later call reports the same failure rather than pretending to work.
    pub fn dispose(&self) -> Result<CacheRecreation, ContextEngineError> {
        let mut connection = lock(&self.connection);
        let previous = self.generation();
        // Closed before the file is unlinked: Windows refuses to remove a file
        // that is still open, and a replacement created beside a live handle
        // would inherit the old write-ahead log.
        drop(connection.take());
        remove_database(&self.database)?;
        let (fresh, meta) = create(
            &self.database,
            &self.expected,
            &self.repository_identity,
            next_generation(Some(previous)),
        )?;
        let recreation = CacheRecreation {
            reason: RecreationReason::Disposed,
            detail: "a caller discarded the cache".to_owned(),
            previous_generation: Some(previous),
            generation: meta.index_generation,
            quarantined_to: None,
        };
        *connection = Some(fresh);
        let mut state = lock(&self.state);
        state.meta = meta;
        state.stale_components.clear();
        // A refresh time that predates the cache it is reported beside would
        // tell a surface this index was brought up to date before it existed.
        state.last_refreshed_at = None;
        state.last_recreation = Some(recreation.clone());
        Ok(recreation)
    }

    /// Re-reads the cache from disk and reconciles what this build can.
    ///
    /// The metadata is read from the *file* rather than from what this process
    /// last saw, and that is the point: two front ends share one cache, so the
    /// file behind a live handle can be disposed, rebuilt, or upgraded by
    /// somebody else. A generation that moved is adopted by reopening; a file
    /// that has stopped being a cache is quarantined and replaced, and the
    /// refresh that met it fails with
    /// [`ContextEngineError::CacheCorruptQuarantined`] — the cache the caller
    /// addressed is gone, even though the engine is healthy again.
    ///
    /// The content reconciliation [#114] and [#115] define plugs in behind this
    /// same call and this same report; `entries_reconciled` is zero until there
    /// are content tables to reconcile.
    ///
    /// # Errors
    ///
    /// [`ContextEngineError::Cancelled`] when the token is observed,
    /// [`ContextEngineError::CacheVersionConflict`] when another build upgraded
    /// the file underneath this one, [`ContextEngineError::CacheCorruptQuarantined`]
    /// when the file had to be replaced, and
    /// [`ContextEngineError::CacheOpenFailed`] when the replacement could not
    /// be prepared.
    ///
    /// [#114]: https://github.com/fullstacktaiye/harkness/issues/114
    /// [#115]: https://github.com/fullstacktaiye/harkness/issues/115
    pub fn refresh(&self, cancellation: &Cancellation) -> Result<IndexReport, ContextEngineError> {
        // An already-cancelled token launches nothing at all, exactly as every
        // other blocking seam in the workspace refuses to start work somebody
        // has already stopped.
        if cancellation.is_cancelled() {
            return Err(ContextEngineError::Cancelled);
        }
        let started = Instant::now();
        self.set_operation(Some(IndexOperation {
            name: "refresh",
            percent_complete: None,
        }));
        let outcome = self.refresh_locked(cancellation, started);
        self.set_operation(None);
        outcome
    }

    fn refresh_locked(
        &self,
        cancellation: &Cancellation,
        started: Instant,
    ) -> Result<IndexReport, ContextEngineError> {
        let mut connection = lock(&self.connection);
        let probed = probe_existing(&self.database, &self.expected, &self.repository_identity)?;
        let meta = match probed {
            Probe::Usable(meta) => meta,
            Probe::Absent => {
                return Err(self.replace_after_fault(
                    &mut connection,
                    RecreationReason::Corrupt,
                    "the cache file is gone".to_owned(),
                    None,
                ));
            }
            Probe::Replace {
                reason,
                previous_generation,
                detail,
            } => {
                return Err(self.replace_after_fault(
                    &mut connection,
                    reason,
                    detail,
                    previous_generation,
                ));
            }
        };
        if cancellation.is_cancelled() {
            return Err(ContextEngineError::Cancelled);
        }

        if meta.index_generation != self.generation() {
            // Another process rebuilt the file. The open handle points at the
            // inode that was replaced, so it is reopened rather than reused.
            drop(connection.take());
            *connection = Some(open_writable(&self.database)?);
        }
        let stale_components = self.expected.skew(&meta);
        let mut state = lock(&self.state);
        state.meta = meta;
        state.stale_components.clone_from(&stale_components);
        state.last_refreshed_at = Some(OffsetDateTime::now_utc());
        Ok(IndexReport {
            generation: state.meta.index_generation,
            stale_components,
            entries_reconciled: 0,
            duration: started.elapsed(),
        })
    }

    /// Sets a faulted cache aside, opens a replacement, and names the failure.
    ///
    /// Always an error: the cache the caller addressed no longer exists, so the
    /// call that met the fault did not do what it was asked. The engine is
    /// usable again the moment this returns.
    fn replace_after_fault(
        &self,
        connection: &mut Option<Connection>,
        reason: RecreationReason,
        detail: String,
        previous_generation: Option<u64>,
    ) -> ContextEngineError {
        let previous = self.generation();
        drop(connection.take());
        let quarantined_to = match quarantine(&self.database) {
            Ok(quarantined_to) => quarantined_to,
            Err(error) => return error,
        };
        let (fresh, meta) = match create(
            &self.database,
            &self.expected,
            &self.repository_identity,
            next_generation(Some(previous.max(previous_generation.unwrap_or(0)))),
        ) {
            Ok(created) => created,
            Err(error) => return error,
        };
        *connection = Some(fresh);
        let mut state = lock(&self.state);
        state.last_recreation = Some(CacheRecreation {
            reason,
            detail: detail.clone(),
            previous_generation: Some(previous),
            generation: meta.index_generation,
            quarantined_to: quarantined_to.clone(),
        });
        state.meta = meta;
        state.stale_components.clear();
        state.last_refreshed_at = None;
        ContextEngineError::CacheCorruptQuarantined {
            path: self.database.clone(),
            quarantined_to,
            reason: detail,
        }
    }

    fn set_operation(&self, operation: Option<IndexOperation>) {
        lock(&self.state).in_progress = operation;
    }
}

/// What reading an existing cache concluded.
enum Probe {
    /// Nothing is there yet; this is a first build rather than a replacement.
    Absent,
    /// The file describes a cache this build can address.
    Usable(IndexMeta),
    /// The file must be set aside and replaced before it can be used.
    Replace {
        reason: RecreationReason,
        previous_generation: Option<u64>,
        detail: String,
    },
}

/// Reads an existing cache's metadata without writing a byte of it.
///
/// The connection is deliberately read-only. Opening for writing would recover
/// a write-ahead log and could rewrite the file this build has not yet decided
/// it may touch, which is exactly the promise
/// [`ContextEngineError::CacheVersionConflict`] makes.
fn probe_existing(
    database: &Path,
    expected: &ExpectedVersions,
    repository_identity: &str,
) -> Result<Probe, ContextEngineError> {
    if !database.exists() {
        return Ok(Probe::Absent);
    }

    let connection = match Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(connection) => connection,
        Err(error) if is_environmental(&error) => {
            return Err(ContextEngineError::CacheOpenFailed {
                path: database.to_path_buf(),
                reason: error.to_string(),
            });
        }
        // Anything else is a statement about the *contents* — not a database,
        // a corrupt header — and is a cache to replace rather than a reason to
        // fail the engine.
        Err(error) => {
            return Ok(Probe::Replace {
                reason: RecreationReason::Corrupt,
                previous_generation: None,
                detail: error.to_string(),
            });
        }
    };
    let _ = connection.busy_timeout(BUSY_TIMEOUT);

    let meta = match read_meta(&connection) {
        Ok(Some(meta)) => meta,
        // A cache somebody else is writing is a cache to come back to, not one
        // to throw away. Reading a locked file as corruption would let one
        // front end destroy the other's index simply by being slow.
        Err(error) if is_environmental(&error) => {
            return Err(ContextEngineError::CacheOpenFailed {
                path: database.to_path_buf(),
                reason: error.to_string(),
            });
        }
        Ok(None) | Err(_) => {
            return Ok(Probe::Replace {
                reason: RecreationReason::Corrupt,
                previous_generation: None,
                detail: "index_meta is missing or unreadable".to_owned(),
            });
        }
    };

    if meta.schema_version > expected.schema_version {
        return Err(ContextEngineError::CacheVersionConflict {
            path: database.to_path_buf(),
            found: meta.schema_version,
            maximum: expected.schema_version,
        });
    }
    if meta.schema_version < expected.schema_version {
        return Ok(Probe::Replace {
            reason: RecreationReason::Version,
            previous_generation: Some(meta.index_generation),
            detail: format!(
                "the cache was written at schema version {}",
                meta.schema_version
            ),
        });
    }
    if meta.repository_identity != repository_identity {
        return Ok(Probe::Replace {
            reason: RecreationReason::Corrupt,
            previous_generation: Some(meta.index_generation),
            detail: format!(
                "the cache records repository {} and was asked for {repository_identity}",
                meta.repository_identity
            ),
        });
    }
    Ok(Probe::Usable(meta))
}

/// Whether a `rusqlite` failure is about the environment rather than the file's
/// contents.
///
/// A permission bit, an exhausted file-descriptor table, and another process
/// holding the write lock all say nothing about what the cache holds, so none
/// of them may be answered by throwing it away. Contention is the sharp one:
/// reading a busy cache as a corrupt one would let a front end destroy the
/// other front end's index by being slow.
fn is_environmental(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(
            ErrorCode::CannotOpen
                | ErrorCode::PermissionDenied
                | ErrorCode::SystemIoFailure
                | ErrorCode::DiskFull
                | ErrorCode::ReadOnly
                | ErrorCode::DatabaseBusy
                | ErrorCode::DatabaseLocked
        )
    )
}

fn read_meta(connection: &Connection) -> Result<Option<IndexMeta>, rusqlite::Error> {
    connection
        .query_row(
            "SELECT schema_version, parser_version, chunking_version, ranking_version, \
             index_generation, repository_identity, created_at FROM index_meta WHERE id = 1",
            [],
            |row| {
                let schema_version: i64 = row.get(0)?;
                let generation: i64 = row.get(4)?;
                let created_at: String = row.get(6)?;
                Ok(IndexMeta {
                    schema_version: u32::try_from(schema_version).unwrap_or(u32::MAX),
                    parser_version: row.get(1)?,
                    chunking_version: row.get(2)?,
                    ranking_version: row.get(3)?,
                    index_generation: u64::try_from(generation).unwrap_or(0),
                    repository_identity: row.get(5)?,
                    created_at: OffsetDateTime::parse(&created_at, &Rfc3339)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?
                        .to_offset(UtcOffset::UTC),
                })
            },
        )
        .optional()
}

/// Opens a cache for reading and writing, applying the store's pragma set.
fn open_writable(database: &Path) -> Result<Connection, ContextEngineError> {
    let failed = |reason: String| ContextEngineError::CacheOpenFailed {
        path: database.to_path_buf(),
        reason,
    };
    let connection = Connection::open(database).map_err(|error| failed(error.to_string()))?;
    connection
        .busy_timeout(BUSY_TIMEOUT)
        .map_err(|error| failed(error.to_string()))?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(|error| failed(error.to_string()))?;
    enable_wal(&connection).map_err(failed)?;
    Ok(connection)
}

/// Requests the write-ahead log and proves the connection entered it.
///
/// `PRAGMA journal_mode` reports the mode in force rather than failing, so a
/// filesystem that cannot support WAL would otherwise leave the cache silently
/// in rollback-journal mode and make two front ends serialize against each
/// other. The retry is the run store's, for the same reason: moving a database
/// into WAL takes an exclusive lock that is not routed through the busy handler
/// on every platform.
fn enable_wal(connection: &Connection) -> Result<(), String> {
    let deadline = Instant::now() + BUSY_TIMEOUT;
    loop {
        match request_wal(connection) {
            Ok(mode) if mode.eq_ignore_ascii_case("wal") => break,
            Ok(mode) => return Err(format!("the cache is in {mode} journal mode, not WAL")),
            Err(error)
                if matches!(
                    error.sqlite_error_code(),
                    Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
                ) && Instant::now() < deadline =>
            {
                std::thread::sleep(WAL_RETRY_INTERVAL);
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    connection
        .pragma_update(None, "synchronous", "NORMAL")
        .map_err(|error| error.to_string())
}

fn request_wal(connection: &Connection) -> Result<String, rusqlite::Error> {
    let current: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    if current.eq_ignore_ascii_case("wal") {
        return Ok(current);
    }
    connection.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
}

/// Creates an empty cache and its single metadata row.
fn create(
    database: &Path,
    expected: &ExpectedVersions,
    repository_identity: &str,
    generation: u64,
) -> Result<(Connection, IndexMeta), ContextEngineError> {
    let failed = |reason: String| ContextEngineError::CacheOpenFailed {
        path: database.to_path_buf(),
        reason,
    };
    let connection = open_writable(database)?;
    connection
        .execute_batch(INDEX_META_SCHEMA)
        .map_err(|error| failed(error.to_string()))?;
    let meta = IndexMeta {
        schema_version: expected.schema_version,
        parser_version: expected.parser_version.clone(),
        chunking_version: expected.chunking_version.clone(),
        ranking_version: expected.ranking_version.clone(),
        index_generation: generation,
        repository_identity: repository_identity.to_owned(),
        created_at: OffsetDateTime::now_utc(),
    };
    let stored_generation = i64::try_from(meta.index_generation)
        .map_err(|_| failed(format!("generation {generation} is not representable")))?;
    let created_at = meta
        .created_at
        .format(&Rfc3339)
        .map_err(|error| failed(error.to_string()))?;
    // A second process may have created the row between the two statements.
    // Its row is as good as this one would have been, so the insert yields to
    // it and the caller reads whatever is actually there.
    connection
        .execute(
            "INSERT INTO index_meta \
             (id, schema_version, parser_version, chunking_version, ranking_version, \
              index_generation, repository_identity, created_at) \
             VALUES (1, :schema_version, :parser_version, :chunking_version, :ranking_version, \
              :index_generation, :repository_identity, :created_at) \
             ON CONFLICT(id) DO NOTHING",
            named_params! {
                ":schema_version": meta.schema_version,
                ":parser_version": meta.parser_version,
                ":chunking_version": meta.chunking_version,
                ":ranking_version": meta.ranking_version,
                ":index_generation": stored_generation,
                ":repository_identity": meta.repository_identity,
                ":created_at": created_at,
            },
        )
        .map_err(|error| failed(error.to_string()))?;
    let stored = read_meta(&connection)
        .map_err(|error| failed(error.to_string()))?
        .ok_or_else(|| failed("the metadata row vanished after it was written".to_owned()))?;
    Ok((connection, stored))
}

/// Moves an unusable cache aside, or deletes it when there is nothing to keep.
///
/// The write-ahead log and shared-memory sidecars are removed rather than moved
/// with it. They describe the file under its old name, a replacement created
/// beside them would try to recover them as its own, and a quarantined file is
/// kept to be *looked at* rather than to be reopened.
///
/// Returns where the bytes went, or `None` when there were none to keep.
fn quarantine(database: &Path) -> Result<Option<PathBuf>, ContextEngineError> {
    if !database.exists() {
        return Ok(None);
    }
    let destination = quarantine_path(database)?;
    fs::rename(database, &destination).map_err(|error| ContextEngineError::CacheOpenFailed {
        path: database.to_path_buf(),
        reason: format!(
            "the unusable cache could not be moved to '{}': {error}",
            destination.display()
        ),
    })?;
    remove_sidecars(database);
    prune_quarantines(database);
    Ok(Some(destination))
}

/// Removes a database and its sidecars outright.
fn remove_database(database: &Path) -> Result<(), ContextEngineError> {
    if database.exists() {
        fs::remove_file(database).map_err(|error| ContextEngineError::CacheOpenFailed {
            path: database.to_path_buf(),
            reason: format!("the cache could not be removed: {error}"),
        })?;
    }
    remove_sidecars(database);
    Ok(())
}

/// Best-effort removal of the write-ahead log and shared-memory files.
///
/// Best effort on purpose: a leftover sidecar beside a database that is gone is
/// inert, and refusing to recreate a cache because one could not be unlinked
/// would turn a cosmetic problem into an unusable engine.
fn remove_sidecars(database: &Path) {
    for suffix in ["-wal", "-shm"] {
        let mut name = database.as_os_str().to_os_string();
        name.push(suffix);
        let _ = fs::remove_file(PathBuf::from(name));
    }
}

fn quarantine_path(database: &Path) -> Result<PathBuf, ContextEngineError> {
    let directory = database.parent().unwrap_or_else(|| Path::new("."));
    let stamp = OffsetDateTime::now_utc()
        .format(QUARANTINE_STAMP)
        .map_err(|error| ContextEngineError::CacheOpenFailed {
            path: database.to_path_buf(),
            reason: error.to_string(),
        })?;
    Ok(directory.join(format!("{QUARANTINE_PREFIX}{stamp}")))
}

/// Keeps at most [`MAX_QUARANTINED_CACHES`], deleting the oldest first.
///
/// The stamp is fixed width, so name order is age order and no metadata read is
/// needed to decide which to drop.
fn prune_quarantines(database: &Path) {
    let Some(directory) = database.parent() else {
        return;
    };
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut quarantined = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .filter_map(|name| name.into_string().ok())
        .filter(|name| name.starts_with(QUARANTINE_PREFIX))
        .collect::<Vec<_>>();
    quarantined.sort_unstable();
    let excess = quarantined.len().saturating_sub(MAX_QUARANTINED_CACHES);
    for name in quarantined.into_iter().take(excess) {
        let _ = fs::remove_file(directory.join(name));
    }
}

/// The generation a newly created cache takes.
///
/// Seeded from the wall clock in nanoseconds and floored at `previous + 1`. The
/// clock is what keeps a cache that was *deleted along with its directory* from
/// reissuing a number a stored snapshot already recorded, and the floor is what
/// keeps a clock that stepped backwards from doing the same.
fn next_generation(previous: Option<u64>) -> u64 {
    let clock = u64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos()).unwrap_or(0);
    let floor = previous.map_or(0, |previous| previous.saturating_add(1));
    clock.max(floor)
}

/// Takes a lock, adopting the contents even if a previous holder panicked.
///
/// A panic in a caller says nothing about the cache: every write is a committed
/// or rolled-back statement, and refusing to use the connection behind a
/// poisoned mutex would turn one failure into a permanently unusable engine for
/// a store whose whole point is that it can be thrown away.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
