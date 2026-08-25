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
//! # What it holds
//!
//! One metadata row, one row per worktree, and the derived content beneath
//! them: [`INDEX_SCHEMA`] is the whole layout and the place to read before
//! changing anything persisted. Exactly one table is per-worktree — `files` —
//! and everything else is content-addressed and shared, which is why every read
//! takes a [`WorktreeKey`] and joins through that worktree's rows.
//!
//! A batch is written at a *pending* generation nothing can see and becomes
//! visible in one transaction, so a process killed part-way through a cold
//! build leaves rows no query returns rather than a half-indexed repository
//! reporting itself complete. [`IndexBatch`] is that protocol.
//!
//! # Versioning is five fields, not one
//!
//! [`index_meta`](IndexMeta) holds exactly one row. Its `schema_version`
//! describes the cache's own table layout and is the only field a mismatch
//! cannot be reconciled from: an older cache is quarantined and recreated, and
//! a *newer* one is refused read-only and left byte-identical, mirroring the
//! run store's `schema_too_new`. The four component versions — parser,
//! chunking, ranking, classify — describe what produced the rows rather than
//! where they sit, so a mismatch leaves the file alone and marks that
//! component's data stale. Rewriting the stored component version at open would
//! destroy exactly the knowledge that reconciliation needs.
//!
//! [`IndexCache::refresh`] is the reconciler, and it is the only thing that
//! moves a stored component version — after it has acted on the skew, in the
//! same transaction. What "acting" means differs by component and the
//! difference is the whole of the invalidation matrix:
//!
//! | Skew | What happens | Why |
//! | --- | --- | --- |
//! | `chunking` | `chunks` emptied, `file_versions.chunking_version` nulled | a chunk's identity was derived under rules this build does not use, so the row names something nothing can re-derive |
//! | `parser` | `symbols` emptied, `file_versions.parser_version` nulled | the same, for symbol identity |
//! | `ranking` | the tables registered as ranking-owned are emptied | a score is only meaningful under the formula that produced it |
//! | `classify` | nothing is deleted | a `files` row is a true record that a path existed at a size; only its *class* is suspect, and the row's own `classify_version` says so |
//!
//! # The generation is a token, not a counter
//!
//! `index_generation` is a component of the workspace snapshot digest
//! (ADR-0008), so a snapshot taken against a rebuilt index must not compare
//! equal to one taken against the index that produced it. A plain counter
//! cannot promise that, because the counter lives *in the file being deleted*:
//! wiping `<data_dir>/context/` and starting again at one would make every
//! stale snapshot verify as fresh. A new generation therefore seeds from the
//! wall clock in microseconds and keeps `previous + 1` as a floor, so a
//! recreation is strictly greater than the value it replaces and a clock that
//! steps backwards cannot reissue a number some snapshot already recorded.
//!
//! The floor needs the previous value to be *readable*, and one case leaves it
//! unreadable: a cache whose `index_meta` is corrupt, or a directory that was
//! deleted outright, tells the replacement nothing. There the clock alone
//! orders the generation, so a backwards clock step *combined* with a wipe or a
//! corruption is the one way a generation can repeat. That is stated rather
//! than argued away — closing it would mean keeping the counter somewhere the
//! whole point of this subtree is that a user may delete.
//!
//! # Locking
//!
//! The cache's connection lock is **leaf-level**: it is never held while the
//! repository lock or the catalog lock is acquired, so the workspace's
//! repository-then-catalog ordering is untouched. [`IndexCache::status`] takes
//! a different, short-held lock and never the connection's, which is what lets
//! a UI poll answer while a cold index build is running — and a batch releases
//! the connection between flushes for the same reason.
//!
//! A third lock is the advisory [`CACHE_LOCK_FILE`] in the cache's own root: an
//! open cache holds it *shared* for its whole life so an eviction sweep, which
//! takes it exclusively, cannot delete a cache out from under a live process.
//! It is taken once at open and never while any other lock is held.
//!
//! [#115]: https://github.com/fullstacktaiye/harkness/issues/115

mod budget;
mod schema;
mod store;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use harkness_git::Cancellation;
use rusqlite::{
    Connection, ErrorCode, OpenFlags, OptionalExtension, TransactionBehavior, named_params,
};
use time::format_description::BorrowedFormatItem;
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset, macros::format_description};

use crate::error::ContextEngineError;

pub use budget::{
    CACHE_LOCK_FILE, CacheUsage, EvictionReport, MAX_TOTAL_CONTEXT_BYTES, evict_to_budget, survey,
};
pub use schema::{CORE_TABLES, INDEX_SCHEMA, INDEX_SCHEMA_VERSION};
pub use store::{
    BatchReceipt, BatchScope, IndexBatch, IndexedChunk, IndexedFile, IndexedSymbol, MAX_READ_ROWS,
    SymbolRecord, WorktreeKey, cache_root,
};

use budget::CacheLock;

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

/// How large one repository's cache may grow before a batch is refused.
///
/// Half a gibibyte of derived rows for one repository, which the medium
/// profile is not expected to come near. Reaching it fails the batch with
/// [`ContextEngineError::IndexBudgetExhausted`] rather than storing what fits:
/// an index that silently stopped recording answers "no match" for content it
/// never held, and a caller cannot tell that from a repository that does not
/// contain it.
pub const MAX_INDEX_DB_BYTES: u64 = 512 * 1024 * 1024;

/// Version of the language grammars and symbol extraction that filled the cache.
///
/// `0` means this build has none, which is the honest answer until [#117]
/// lands: no row in any cache was produced by a parser, so nothing can be
/// stale against one.
///
/// [#117]: https://github.com/fullstacktaiye/harkness/issues/117
pub const PARSER_VERSION: &str = "0";

/// Version of the chunk-boundary rules that filled the cache.
pub use crate::chunk::CHUNKING_VERSION;

/// Version of the scoring formula whose results the cache holds ([#121]).
///
/// [#121]: https://github.com/fullstacktaiye/harkness/issues/121
pub const RANKING_VERSION: &str = "0";

/// Version of the classification rules that decided each file row's class.
pub use crate::classify::CLASSIFY_VERSION;

/// How long a connection waits for another process's writer before giving up.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the write-ahead-log transition re-checks a contended database.
const WAL_RETRY_INTERVAL: Duration = Duration::from_millis(20);

/// How often a wait re-checks the caller's cancellation token.
///
/// The workspace's cadence. SQLite's own busy wait cannot be interrupted, so
/// every contended read here is given this much patience at a time and the loop
/// around it is what answers a cancelled caller inside the 250 ms target.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How long a database with no metadata is given to turn out to be a cache
/// somebody else is still creating.
///
/// Short, because the window it covers is the microseconds between another
/// process opening the file and its one creation transaction taking the write
/// lock — everything after that is contention, which the busy wait already
/// answers. It is not zero, because a cache being built and a cache that is
/// broken are indistinguishable in a single read, and quarantining the first is
/// far more expensive than waiting out the second.
const CREATION_GRACE: Duration = Duration::from_millis(200);

/// Filename-safe, fixed-width, lexicographically chronological stamp.
///
/// Fixed width is what lets quarantine rotation sort by name instead of asking
/// the filesystem for modification times it is not obliged to keep.
const QUARANTINE_STAMP: &[BorrowedFormatItem<'_>] =
    format_description!("[year][month][day]T[hour][minute][second][subsecond digits:9]Z");

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
    /// Denial list and classification rules.
    pub classify_version: String,
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
            chunking_version: CHUNKING_VERSION.to_string(),
            ranking_version: RANKING_VERSION.to_owned(),
            classify_version: CLASSIFY_VERSION.to_string(),
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
            (
                IndexComponent::Classify,
                &stored.classify_version,
                &self.classify_version,
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
    /// Denial list and classification rules.
    Classify,
}

impl IndexComponent {
    /// Every component, in the order a skew is reported and acted on.
    pub const ALL: &'static [Self] = &[Self::Parser, Self::Chunking, Self::Ranking, Self::Classify];

    /// Stable spelling used in status reports and event payloads.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Parser => "parser",
            Self::Chunking => "chunking",
            Self::Ranking => "ranking",
            Self::Classify => "classify",
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
    /// Classification rules that decided its file rows' classes.
    pub classify_version: String,
    /// Monotonic token naming this build of the cache.
    pub index_generation: u64,
    /// Repository the cache was built for, in `harkness-git`'s spelling.
    pub repository_identity: String,
    /// When the cache was created, RFC 3339 UTC.
    pub created_at: OffsetDateTime,
    /// When a build last adopted this cache, RFC 3339 UTC.
    ///
    /// Stamped after the decision to adopt, never before it, so a cache this
    /// build refuses is still left byte-identical. It is what orders eviction:
    /// `atime` is unusable under `relatime` and absent under `noatime`, so the
    /// cache records its own recency rather than asking the filesystem for one
    /// it is not obliged to keep.
    pub last_opened_at: OffsetDateTime,
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

/// How much the cache holds.
///
/// Reported as [`None`] rather than a row of zeroes when the cache holds no
/// connection, because "nothing is indexed" and "nobody can say" are different
/// things to render.
///
/// `files` counts *visible* rows across every worktree — rows a batch has
/// committed — so a cold build in progress does not report the repository as
/// half indexed. The content-addressed counts are the file's own totals, which
/// is what a disk question wants.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct IndexCounts {
    /// Worktrees this repository's cache holds rows for.
    pub worktrees: u64,
    /// Visible file rows, across every worktree.
    pub files: u64,
    /// Distinct file contents held.
    pub contents: u64,
    /// Distinct `(path, content)` versions held.
    pub file_versions: u64,
    /// Chunks held.
    pub chunks: u64,
    /// Symbols held.
    pub symbols: u64,
    /// Bytes the database and its write-ahead log occupy.
    pub database_bytes: u64,
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
    ///
    /// Empty after a refresh that could act on every skew it found, which is
    /// the ordinary case: acting is what a refresh is for. A component whose
    /// invalidation could not be applied stays here.
    pub stale_components: Vec<VersionSkew>,
    /// What each acted-on skew deleted, in the order it was applied.
    pub invalidated: Vec<ComponentInvalidation>,
    /// Content rows dropped because a component version no longer matched.
    pub entries_reconciled: u64,
    /// How long the refresh took.
    pub duration: Duration,
}

/// One component's rows, dropped because this build did not produce them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComponentInvalidation {
    /// Which component's version had moved.
    pub component: IndexComponent,
    /// Version the cache recorded.
    pub stored: String,
    /// Version this build produces.
    pub expected: String,
    /// Rows deleted from the tables that component owns.
    pub rows_deleted: u64,
}

/// Mutable state published to [`IndexCache::status`].
#[derive(Debug)]
struct CacheState {
    /// What [`IndexCache::status`] reports, which is *not* always `Ready`.
    ///
    /// A recreation closes the connection before it unlinks the file, so a
    /// removal or a create that fails leaves the cache holding no handle at
    /// all. Reporting `Ready` there — and handing out the generation of a
    /// database that has just been deleted — would make a surface render a
    /// healthy index and a capture record an identity against an index that is
    /// gone. The state says so instead, and the next [`IndexCache::refresh`]
    /// reopens.
    availability: IndexAvailability,
    meta: IndexMeta,
    stale_components: Vec<VersionSkew>,
    last_recreation: Option<CacheRecreation>,
    last_refreshed_at: Option<OffsetDateTime>,
    /// What the cache held when something last counted it.
    ///
    /// Cached rather than counted on demand, because [`IndexCache::status`]
    /// promises never to wait on the writer and counting takes the connection a
    /// cold build is holding between flushes. Every path that changes what the
    /// cache holds republishes this, so the number a UI polls is the one the
    /// last committed batch left behind rather than a guess.
    counts: Option<IndexCounts>,
    /// The operations running right now, innermost last.
    ///
    /// A stack rather than a flag, and it names every long operation rather
    /// than refreshes alone. A bare `Option` cleared by whichever call finishes
    /// first reports the cache idle while another is still running, and a
    /// refresh-only flag renders a disposal — the slowest thing the cache does,
    /// since it unlinks a database and builds a replacement — as idle.
    in_flight: Vec<&'static str>,
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
    /// Held for this cache's whole life so an eviction sweep skips it.
    ///
    /// `None` when the advisory lock could not be taken at all — a read-only
    /// data directory, a filesystem without locking, an exhausted descriptor
    /// table. That costs protection from eviction and nothing else, and taking
    /// retrieval away over a bookkeeping file would be the worse trade.
    _lock: Option<CacheLock>,
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
        cancellation: &Cancellation,
    ) -> Result<Self, ContextEngineError> {
        let database = cache_root.join(INDEX_DATABASE_FILE);
        fs::create_dir_all(cache_root).map_err(|error| ContextEngineError::CacheOpenFailed {
            path: database.clone(),
            reason: format!("the cache directory could not be created: {error}"),
        })?;

        // Taken before the probe, so a cache is protected from eviction from
        // the moment this build starts deciding about it rather than from the
        // moment it succeeds.
        let held = CacheLock::shared(cache_root);

        let probed = probe_existing(&database, expected, repository_identity, cancellation)?;
        // A cache this call created holds nothing, which is a count rather than
        // an absence of one. A cache it *adopted* is not counted here at all:
        // `COUNT(*)` over a `WITHOUT ROWID` table is a scan, and paying six of
        // them on the path a user reached by opening a project would spend the
        // whole open budget on a number nothing has asked for yet.
        // `refresh` and every committed batch publish it; `counts` takes it on
        // demand.
        let mut counts = Some(IndexCounts::default());
        let (connection, meta, stale_components, last_recreation) = match probed {
            Probe::Usable(meta) => {
                let mut connection = open_writable(&database, cancellation)?;
                // Stamped only now, once the cache is one this build may adopt.
                // A refusal returns above this line with the file untouched,
                // which is the promise `cache_version_conflict` makes.
                let meta = stamp_opened(&mut connection, meta);
                let stale = expected.skew(&meta);
                counts = None;
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
                    next_generation(None)?,
                    cancellation,
                )?;
                // Computed from the stored row rather than assumed empty: a
                // racing process's row may have won, and it may have been
                // written by another build. Publishing no skew for a cache
                // whose rows this build did not produce would silently skip the
                // reconciliation that skew exists to trigger.
                let stale = expected.skew(&meta);
                (connection, meta, stale, None)
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
                    next_generation(previous_generation)?,
                    cancellation,
                )?;
                let recreation = CacheRecreation {
                    reason,
                    detail,
                    previous_generation,
                    generation: meta.index_generation,
                    quarantined_to,
                };
                let stale = expected.skew(&meta);
                (connection, meta, stale, Some(recreation))
            }
        };

        let counts = counts.map(|counts| IndexCounts {
            database_bytes: database_bytes(&database),
            ..counts
        });
        let cache = Self {
            root: cache_root.to_path_buf(),
            database,
            expected: expected.clone(),
            repository_identity: repository_identity.to_owned(),
            connection: Mutex::new(Some(connection)),
            state: Mutex::new(CacheState {
                availability: IndexAvailability::Ready,
                meta,
                stale_components,
                last_recreation,
                last_refreshed_at: None,
                in_flight: Vec::new(),
                counts,
            }),
            _lock: held,
        };
        tracing::debug!(
            repository = cache.repository_identity.as_str(),
            generation = cache.generation(),
            // Adopted rather than built, which is what "warm" means here — the
            // row counts are deliberately not read on this path.
            warm = cache.status().counts.is_none(),
            "context index opened"
        );
        Ok(cache)
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

    /// The generation the open cache carries, or `0` when it holds none.
    ///
    /// This is what a [`WorkspaceSnapshot`](crate::WorkspaceSnapshot) absorbs
    /// into its identity, so a snapshot taken against a rebuilt cache never
    /// compares equal to one taken against the cache that produced it. A cache
    /// left closed by a recreation that did not finish answers `0` — "against
    /// no index" — rather than the generation of the database that was removed,
    /// which is a workspace identity nothing on disk supports.
    #[must_use]
    pub fn generation(&self) -> u64 {
        let state = lock(&self.state);
        match state.availability {
            IndexAvailability::Ready => state.meta.index_generation,
            IndexAvailability::Unavailable { .. } => 0,
        }
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
        let ready = matches!(state.availability, IndexAvailability::Ready);
        IndexStatus {
            generation: if ready {
                state.meta.index_generation
            } else {
                0
            },
            availability: state.availability.clone(),
            repository_identity: state.meta.repository_identity.clone(),
            last_recreation: state.last_recreation.clone(),
            stale_components: state.stale_components.clone(),
            last_refreshed_at: state.last_refreshed_at,
            in_progress: state.in_flight.first().map(|name| IndexOperation {
                name,
                percent_complete: None,
            }),
            counts: if ready { state.counts } else { None },
        }
    }

    /// Recounts what the cache holds and publishes it to [`Self::status`].
    ///
    /// Called by every path that changes the answer. A failure leaves the
    /// previous counts in place rather than blanking them: a count that could
    /// not be taken is not the same as a cache that holds nothing, and a
    /// surface rendering zero rows for a warm index is the more misleading of
    /// the two.
    pub(super) fn publish_counts(&self) {
        let counted = {
            let connection = lock(&self.connection);
            connection
                .as_ref()
                .and_then(|open| counts_of(open, &self.database))
        };
        if let Some(counts) = counted {
            lock(&self.state).counts = Some(counts);
        }
    }

    /// Throws the cache away and starts an empty one.
    ///
    /// The generation moves, so every snapshot taken against the old cache
    /// stops verifying as fresh. Nothing is quarantined: a caller asking for
    /// disposal is not reporting a fault, and keeping a copy of a cache
    /// somebody asked to be rid of would defeat "delete this to reclaim disk".
    ///
    /// # Emptied, not unlinked
    ///
    /// The content is dropped and the metadata reissued inside the file rather
    /// than the file being removed and rebuilt. Windows refuses to unlink a
    /// database *any* handle still has open, and the second front end sharing
    /// this cache is precisely the situation "reclaim disk" has to work in; on
    /// every platform an unlink also strands a live reader on an inode nothing
    /// can reach again. Emptying says the same thing to that reader — the cache
    /// was rebuilt — and its next [`refresh`](Self::refresh) adopts the new
    /// generation. The pages go back to the filesystem through `VACUUM`, which
    /// is best effort: a reader can hold it off, and returning the disk a
    /// moment later is worth less than the disposal itself.
    ///
    /// # Errors
    ///
    /// Returns [`ContextEngineError::CacheOpenFailed`] when the cache could not
    /// be reopened or emptied. The file is left as it was in that case — a
    /// refused transaction rolls back — so a failed disposal costs nothing but
    /// the disposal.
    pub fn dispose(
        &self,
        cancellation: &Cancellation,
    ) -> Result<CacheRecreation, ContextEngineError> {
        let _operation = self.begin_operation("dispose");
        let mut connection = lock(&self.connection);
        // Read from the state rather than through `generation()`, which answers
        // `0` for a cache that is already closed. A retried disposal would
        // otherwise take `0` as its floor — losing the whole `previous + 1`
        // protection against a backwards clock — and report a previous
        // generation no cache ever held.
        let previous = lock(&self.state).meta.index_generation;
        let generation = next_generation(Some(previous))?;
        if connection.is_none() {
            *connection = Some(open_writable(&self.database, cancellation)?);
        }
        let open = connection
            .as_mut()
            .expect("a connection was opened a line above");
        let meta = empty_in_place(open, &self.expected, &self.repository_identity, generation)?;

        let recreation = CacheRecreation {
            reason: RecreationReason::Disposed,
            detail: "a caller discarded the cache".to_owned(),
            previous_generation: Some(previous),
            generation: meta.index_generation,
            quarantined_to: None,
        };
        let mut state = lock(&self.state);
        state.availability = IndexAvailability::Ready;
        state.stale_components = self.expected.skew(&meta);
        state.meta = meta;
        // A refresh time that predates the cache it is reported beside would
        // tell a surface this index was brought up to date before it existed.
        state.last_refreshed_at = None;
        state.last_recreation = Some(recreation.clone());
        // Written here rather than through `publish_counts`, which would take
        // the connection lock this call is still holding.
        state.counts = Some(IndexCounts {
            database_bytes: database_bytes(&self.database),
            ..IndexCounts::default()
        });
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
        let _operation = self.begin_operation("refresh");
        self.refresh_locked(cancellation, started)
    }

    fn refresh_locked(
        &self,
        cancellation: &Cancellation,
        started: Instant,
    ) -> Result<IndexReport, ContextEngineError> {
        let mut connection = lock(&self.connection);
        let probed = probe_existing(
            &self.database,
            &self.expected,
            &self.repository_identity,
            cancellation,
        )?;
        let meta = match probed {
            Probe::Usable(meta) => meta,
            Probe::Absent => {
                return Err(self.replace_after_fault(
                    &mut connection,
                    RecreationReason::Corrupt,
                    "the cache file is gone".to_owned(),
                    None,
                    cancellation,
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
                    cancellation,
                ));
            }
        };
        if cancellation.is_cancelled() {
            return Err(ContextEngineError::Cancelled);
        }

        // Reopened when the handle is gone *or* points at a replaced inode. The
        // first is a recreation that failed part-way and is what makes refresh
        // the repair; the second is another process having rebuilt the file
        // underneath this one. Comparing generations alone would miss the
        // first, because a cache closed by a failed disposal still has the file
        // it was about to remove sitting on disk at the very generation this
        // process last saw.
        if connection.is_none() || meta.index_generation != lock(&self.state).meta.index_generation
        {
            drop(connection.take());
            // Marked before the open, not after it. The handle is already gone
            // by this line, so an open that fails — a momentary descriptor
            // exhaustion, a contended write-ahead-log transition — would
            // otherwise return through `?` leaving the cache reporting itself
            // ready and handing out the generation of a database it cannot
            // reach. Every path that closes the connection owes this marking.
            self.mark_closed("a refresh could not reopen the cache");
            *connection = Some(open_writable(&self.database, cancellation)?);
        }
        let open = connection
            .as_mut()
            .expect("a connection is open by this line");

        // The one place a stored component version moves, and it moves only
        // after the rows it described are gone — in the same transaction, so a
        // failure leaves both the rows and the version they were produced
        // under. Doing this at open instead would erase the knowledge
        // reconciliation needs before anything reconciled.
        let skew = self.expected.skew(&meta);
        let (invalidated, meta) = invalidate(open, &self.database, &self.expected, &meta, &skew)?;
        let entries_reconciled = invalidated
            .iter()
            .map(|applied| applied.rows_deleted)
            .sum::<u64>();
        let stale_components = self.expected.skew(&meta);
        let counts = counts_of(open, &self.database);

        let mut state = lock(&self.state);
        state.availability = IndexAvailability::Ready;
        state.meta = meta;
        state.stale_components.clone_from(&stale_components);
        state.last_refreshed_at = Some(OffsetDateTime::now_utc());
        if counts.is_some() {
            state.counts = counts;
        }
        Ok(IndexReport {
            generation: state.meta.index_generation,
            stale_components,
            invalidated,
            entries_reconciled,
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
        cancellation: &Cancellation,
    ) -> ContextEngineError {
        let previous = lock(&self.state).meta.index_generation;
        drop(connection.take());
        self.mark_closed("a recreation did not finish");
        let quarantined_to = match quarantine(&self.database) {
            Ok(quarantined_to) => quarantined_to,
            Err(error) => return error,
        };
        let (fresh, meta) = match create(
            &self.database,
            &self.expected,
            &self.repository_identity,
            match next_generation(Some(previous.max(previous_generation.unwrap_or(0)))) {
                Ok(generation) => generation,
                Err(error) => return error,
            },
            cancellation,
        ) {
            Ok(created) => created,
            Err(error) => return error,
        };
        *connection = Some(fresh);
        let mut state = lock(&self.state);
        state.availability = IndexAvailability::Ready;
        // Exact and free: the replacement was created a few lines above and
        // holds nothing.
        state.counts = Some(IndexCounts {
            database_bytes: database_bytes(&self.database),
            ..IndexCounts::default()
        });
        state.last_recreation = Some(CacheRecreation {
            reason,
            detail: detail.clone(),
            previous_generation: Some(previous),
            generation: meta.index_generation,
            quarantined_to: quarantined_to.clone(),
        });
        state.stale_components = self.expected.skew(&meta);
        state.meta = meta;
        state.last_refreshed_at = None;
        ContextEngineError::CacheCorruptQuarantined {
            path: self.database.clone(),
            quarantined_to,
            reason: detail,
        }
    }

    /// Records that the cache holds no connection, so nothing reports otherwise.
    fn mark_closed(&self, reason: &str) {
        lock(&self.state).availability = IndexAvailability::Unavailable {
            kind: "cache_open_failed",
            detail: format!("{reason}; the cache is closed until it is refreshed"),
        };
    }

    /// Publishes `name` as in flight until the returned guard is dropped.
    pub(super) fn begin_operation(&self, name: &'static str) -> Operation<'_> {
        lock(&self.state).in_flight.push(name);
        Operation { cache: self }
    }
}

/// Keeps [`IndexStatus::in_progress`] true only while work really is running.
///
/// A `Drop` implementation rather than a call at the end of the method, for the
/// reason the scheduler's worker guard exists: an unwind past the clearing
/// statement would leave a UI rendering a permanently spinning index with
/// nothing behind it, which is the same lie the counter was introduced to
/// prevent, in the other direction and for good.
pub(super) struct Operation<'a> {
    cache: &'a IndexCache,
}

impl Drop for Operation<'_> {
    fn drop(&mut self) {
        let mut state = lock(&self.cache.state);
        state.in_flight.pop();
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
    cancellation: &Cancellation,
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
    // One poll interval rather than the whole budget: SQLite's own busy wait is
    // uninterruptible, so a five-second timeout set here would be five seconds
    // in which a cancelled caller is not answered. The wait is this loop's
    // instead, and it polls the token every time round.
    let _ = connection.busy_timeout(POLL_INTERVAL);

    // The version is read *before* the rest of the row, and that ordering is
    // what keeps a version mismatch from being reported as corruption. Only two
    // columns have existed in every layout this build can meet, so a full read
    // of an older cache fails on a column that is simply not there yet — which
    // reads identically to a damaged file and would quarantine one with the
    // wrong reason attached, after paying the creation grace to be sure.
    let stamp = match read_waiting(&connection, cancellation, STAMP_SELECT, read_stamp)? {
        Ok(Some(stamp)) => stamp,
        // A cache somebody else is writing is a cache to come back to, not one
        // to throw away. Reading a locked file as corruption would let one
        // front end destroy the other's index simply by being slow.
        Err(error) if is_environmental(&error) => {
            return Err(sqlite_failure(database, &error));
        }
        // A database with no metadata in it is either corrupt or *being built*
        // by another process that has opened the file and not yet committed its
        // one transaction. Those look identical in a single read and lead to
        // opposite actions, so the answer is re-read for a bounded grace before
        // a cache somebody is still writing gets quarantined out from under
        // them. A build that has not finished within the grace is treated as
        // corruption, which is the honest end of a distinction that cannot be
        // made perfectly from outside the other process.
        outcome => {
            let Some(stamp) = await_creation(&connection, cancellation)? else {
                return Ok(Probe::Replace {
                    reason: RecreationReason::Corrupt,
                    previous_generation: None,
                    detail: match outcome {
                        Ok(_) => "index_meta holds no row".to_owned(),
                        Err(error) => format!("index_meta is missing or unreadable: {error}"),
                    },
                });
            };
            stamp
        }
    };

    if stamp.schema_version > expected.schema_version {
        return Err(ContextEngineError::CacheVersionConflict {
            path: database.to_path_buf(),
            found: stamp.schema_version,
            maximum: expected.schema_version,
        });
    }
    if stamp.schema_version < expected.schema_version {
        return Ok(Probe::Replace {
            reason: RecreationReason::Version,
            previous_generation: Some(stamp.index_generation),
            detail: format!(
                "the cache was written at schema version {}",
                stamp.schema_version
            ),
        });
    }

    // The layout is this build's, so the rest of the row is addressable. A
    // failure here really is a damaged file.
    let meta = match read_waiting(&connection, cancellation, META_SELECT, read_meta_row)? {
        Ok(Some(meta)) => meta,
        Err(error) if is_environmental(&error) => {
            return Err(sqlite_failure(database, &error));
        }
        outcome => {
            return Ok(Probe::Replace {
                reason: RecreationReason::Corrupt,
                previous_generation: Some(stamp.index_generation),
                detail: match outcome {
                    Ok(_) => "index_meta holds no row".to_owned(),
                    Err(error) => format!("index_meta is unreadable: {error}"),
                },
            });
        }
    };

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

/// The two columns every cache layout has carried, read before any other.
///
/// `schema_version` decides whether the rest of the row is even addressable,
/// and `index_generation` is what a replacement floors itself above — so both
/// have to be readable from a cache this build cannot otherwise understand.
/// Adding a column here is a promise that every future layout keeps it.
#[derive(Clone, Copy, Debug)]
struct MetaStamp {
    schema_version: u32,
    index_generation: u64,
}

/// Reads one projection of the metadata, waiting out another writer.
///
/// The outer `Result` is this side's decision — cancelled — and the inner one is
/// SQLite's answer, which the caller classifies. Collapsing them would make a
/// cancelled probe indistinguishable from an unreadable cache, and the two lead
/// to opposite actions: one returns, the other destroys a file.
fn read_waiting<T>(
    connection: &Connection,
    cancellation: &Cancellation,
    sql: &str,
    read: fn(&rusqlite::Row<'_>) -> Result<T, rusqlite::Error>,
) -> Result<Result<Option<T>, rusqlite::Error>, ContextEngineError> {
    let deadline = Instant::now() + BUSY_TIMEOUT;
    loop {
        let outcome = connection.query_row(sql, [], read).optional();
        let contended = matches!(
            &outcome,
            Err(error)
                if matches!(
                    error.sqlite_error_code(),
                    Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
                )
        );
        if !contended {
            return Ok(outcome);
        }
        if cancellation.is_cancelled() {
            return Err(ContextEngineError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Ok(outcome);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Re-reads metadata that is absent, in case a cache is mid-creation.
///
/// `Ok(None)` means the grace expired with still no row, which is the answer
/// that licences a quarantine.
fn await_creation(
    connection: &Connection,
    cancellation: &Cancellation,
) -> Result<Option<MetaStamp>, ContextEngineError> {
    let deadline = Instant::now() + CREATION_GRACE;
    while Instant::now() < deadline {
        if cancellation.is_cancelled() {
            return Err(ContextEngineError::Cancelled);
        }
        std::thread::sleep(POLL_INTERVAL);
        if let Ok(Some(stamp)) = read_waiting(connection, cancellation, STAMP_SELECT, read_stamp)? {
            return Ok(Some(stamp));
        }
    }
    Ok(None)
}

/// Whether a `rusqlite` failure is about the environment rather than the file's
/// contents.
///
/// A permission bit, an exhausted file-descriptor table, memory pressure, and
/// another process holding the write lock all say nothing about what the cache
/// holds, so none of them may be answered by throwing it away. Contention is the
/// sharp one: reading a busy cache as a corrupt one would let a front end
/// destroy the other front end's index by being slow. Adding a code here is
/// always safe — the cost of misclassifying an environmental failure as content
/// is a destroyed cache, and the cost of the reverse is one refusal a retry
/// clears.
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
                | ErrorCode::OutOfMemory
                | ErrorCode::OperationInterrupted
        )
    )
}

/// The narrow projection every layout can answer.
const STAMP_SELECT: &str = "SELECT schema_version, index_generation FROM index_meta WHERE id = 1";

/// The whole row, addressable only once the layout is known to be this build's.
const META_SELECT: &str = "\
SELECT schema_version, parser_version, chunking_version, ranking_version, classify_version, \
       index_generation, repository_identity, created_at, last_opened_at \
FROM index_meta WHERE id = 1";

fn read_stamp(row: &rusqlite::Row<'_>) -> Result<MetaStamp, rusqlite::Error> {
    let schema_version: i64 = row.get(0)?;
    let generation: i64 = row.get(1)?;
    Ok(MetaStamp {
        // Refused, never clamped. A negative or oversized version saturated to
        // `u32::MAX` would read as "written by a build newer than this one" and
        // be refused *permanently* with `cache_version_conflict`, leaving
        // retrieval dead for that repository; a row nobody could have written is
        // corruption, and the quarantine path exists for exactly that.
        schema_version: u32::try_from(schema_version)
            .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, schema_version))?,
        index_generation: u64::try_from(generation)
            .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(1, generation))?,
    })
}

fn read_meta_row(row: &rusqlite::Row<'_>) -> Result<IndexMeta, rusqlite::Error> {
    let schema_version: i64 = row.get(0)?;
    let generation: i64 = row.get(5)?;
    let created_at: String = row.get(7)?;
    let last_opened_at: String = row.get(8)?;
    Ok(IndexMeta {
        schema_version: u32::try_from(schema_version)
            .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, schema_version))?,
        parser_version: row.get(1)?,
        chunking_version: row.get(2)?,
        ranking_version: row.get(3)?,
        classify_version: row.get(4)?,
        index_generation: u64::try_from(generation)
            .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(5, generation))?,
        repository_identity: row.get(6)?,
        created_at: OffsetDateTime::parse(&created_at, &Rfc3339)
            .map_err(|_| rusqlite::Error::InvalidQuery)?
            .to_offset(UtcOffset::UTC),
        last_opened_at: OffsetDateTime::parse(&last_opened_at, &Rfc3339)
            .map_err(|_| rusqlite::Error::InvalidQuery)?
            .to_offset(UtcOffset::UTC),
    })
}

fn read_meta(connection: &Connection) -> Result<Option<IndexMeta>, rusqlite::Error> {
    connection
        .query_row(META_SELECT, [], read_meta_row)
        .optional()
}

/// Records that this build adopted the cache, best effort.
///
/// Best effort because the stamp orders *eviction* and nothing else. A cache
/// that could not be stamped — a read-only mount, a moment of contention —
/// sorts as though it had not been opened, which makes it a candidate for
/// eviction sooner than it deserves and costs a rebuild. Failing the open over
/// it would cost retrieval instead, which is the worse of the two.
fn stamp_opened(connection: &mut Connection, meta: IndexMeta) -> IndexMeta {
    let now = OffsetDateTime::now_utc();
    let Ok(formatted) = now.format(&Rfc3339) else {
        return meta;
    };
    let updated = connection.execute(
        "UPDATE index_meta SET last_opened_at = :at WHERE id = 1",
        named_params! { ":at": formatted },
    );
    if updated.is_ok() {
        return IndexMeta {
            last_opened_at: now,
            ..meta
        };
    }
    meta
}

/// Bytes the cache database and its write-ahead log occupy.
///
/// The log is counted because it is the cache: pages committed and not yet
/// checkpointed live there, and a budget that ignored it would let a cache
/// double its cap between checkpoints.
fn database_bytes(database: &Path) -> u64 {
    let mut total = fs::metadata(database)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let mut log = database.as_os_str().to_os_string();
    log.push("-wal");
    total += fs::metadata(PathBuf::from(log))
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    total
}

/// Counts what the cache holds, answering `None` when it cannot be counted.
fn counts_of(connection: &Connection, database: &Path) -> Option<IndexCounts> {
    let count = |sql: &str| -> Option<u64> {
        connection
            .query_row(sql, [], |row| row.get::<_, i64>(0))
            .ok()
            .map(|value| u64::try_from(value).unwrap_or(0))
    };
    Some(IndexCounts {
        worktrees: count("SELECT COUNT(*) FROM worktrees")?,
        files: count(
            "SELECT COUNT(*) FROM files f JOIN worktrees w ON w.worktree_id = f.worktree_id \
             WHERE f.generation <= w.last_generation",
        )?,
        contents: count("SELECT COUNT(*) FROM contents")?,
        file_versions: count("SELECT COUNT(*) FROM file_versions")?,
        chunks: count("SELECT COUNT(*) FROM chunks")?,
        symbols: count("SELECT COUNT(*) FROM symbols")?,
        database_bytes: database_bytes(database),
    })
}

/// Empties each skewed component's tables and records the version that replaced
/// them, in one transaction per component.
///
/// One transaction *per component* rather than one for all of them, because a
/// component whose invalidation fails must not undo one that succeeded: each
/// pair of "the rows are gone" and "the version says so" is what has to be
/// atomic, and there is nothing to gain from coupling `chunks` to `symbols`.
///
/// The returned metadata is re-read from the file, so a caller reports what the
/// cache says rather than what this build asked for.
fn invalidate(
    connection: &mut Connection,
    database: &Path,
    expected: &ExpectedVersions,
    meta: &IndexMeta,
    skew: &[VersionSkew],
) -> Result<(Vec<ComponentInvalidation>, IndexMeta), ContextEngineError> {
    if skew.is_empty() {
        return Ok((Vec::new(), meta.clone()));
    }
    let span = tracing::debug_span!("context.index.invalidate", components = skew.len());
    let _entered = span.enter();

    let mut applied = Vec::new();
    for entry in skew {
        let expectation = match entry.component {
            IndexComponent::Parser => &expected.parser_version,
            IndexComponent::Chunking => &expected.chunking_version,
            IndexComponent::Ranking => &expected.ranking_version,
            IndexComponent::Classify => &expected.classify_version,
        };
        let rows_deleted =
            invalidate_component(connection, database, entry.component, expectation)?;
        applied.push(ComponentInvalidation {
            component: entry.component,
            stored: entry.stored.clone(),
            expected: entry.expected.clone(),
            rows_deleted,
        });
        tracing::debug!(
            component = entry.component.as_str(),
            stored = entry.stored.as_str(),
            expected = entry.expected.as_str(),
            rows_deleted,
            "context index component invalidated"
        );
    }

    let refreshed = read_meta(connection)
        .map_err(|error| sqlite_failure(database, &error))?
        .unwrap_or_else(|| meta.clone());
    Ok((applied, refreshed))
}

/// The transaction one component's skew resolves to.
fn invalidate_component(
    connection: &mut Connection,
    database: &Path,
    component: IndexComponent,
    expectation: &str,
) -> Result<u64, ContextEngineError> {
    let failed = |error: &rusqlite::Error| sqlite_failure(database, error);
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| failed(&error))?;
    let mut rows_deleted = 0_u64;
    // The table names come from this build's own ownership list rather than
    // from the database, so nothing a file records can decide what is dropped.
    for table in component.owned_tables() {
        rows_deleted += transaction
            .execute(&format!("DELETE FROM \"{table}\""), [])
            .map_err(|error| failed(&error))? as u64;
    }
    for (table, column) in component.cleared_columns() {
        transaction
            .execute(
                &format!(
                    "UPDATE \"{table}\" SET \"{column}\" = NULL WHERE \"{column}\" IS NOT NULL"
                ),
                [],
            )
            .map_err(|error| failed(&error))?;
    }
    transaction
        .execute(
            &format!(
                "UPDATE index_meta SET \"{}_version\" = :expected WHERE id = 1",
                component.as_str()
            ),
            named_params! { ":expected": expectation },
        )
        .map_err(|error| failed(&error))?;
    transaction.commit().map_err(|error| failed(&error))?;
    Ok(rows_deleted)
}

/// Maps a SQLite failure onto the namespace a caller branches on.
///
/// Contention is the one that has to be told apart. A caller met by
/// `index_busy` degrades to reading the workspace live; one met by
/// `cache_open_failed` has a cache that is not going to start working on a
/// retry. Answering both with the same discriminant would make the first
/// indistinguishable from the second and the fallback impossible to write.
pub(super) fn sqlite_failure(database: &Path, error: &rusqlite::Error) -> ContextEngineError {
    if matches!(
        error.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    ) {
        return ContextEngineError::IndexBusy {
            path: database.to_path_buf(),
            reason: error.to_string(),
        };
    }
    ContextEngineError::CacheOpenFailed {
        path: database.to_path_buf(),
        reason: error.to_string(),
    }
}

/// Opens a cache for reading and writing, applying the store's pragma set.
///
/// # The run store has a twin of this
///
/// `harkness-runtime`'s `store::{connect, enable_wal, request_wal}` are the same
/// three routines against `runtime.db`, and they are deliberately not shared:
/// the alternative is a SQLite dependency in `harkness-git`, which is the only
/// crate beneath both, and ADR-0004 already accepts "two databases, two
/// connection disciplines". **A fix to the WAL-transition contention handling
/// has to be made in both places** — that retry loop exists for a Windows-only
/// failure, so a divergence is invisible on two of the three matrix legs.
fn open_writable(
    database: &Path,
    cancellation: &Cancellation,
) -> Result<Connection, ContextEngineError> {
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
    enable_wal(&connection, cancellation)?;
    Ok(connection)
}

/// Requests the write-ahead log and proves the connection entered it.
///
/// `PRAGMA journal_mode` reports the mode in force rather than failing, so a
/// filesystem that cannot support WAL would otherwise leave the cache silently
/// in rollback-journal mode and make two front ends serialize against each
/// other. The retry is the run store's, for the same reason: moving a database
/// into WAL takes an exclusive lock that is not routed through the busy handler
/// on every platform. Unlike the store's, this one polls the caller's token
/// between attempts, because a context call is one a user can stop.
fn enable_wal(
    connection: &Connection,
    cancellation: &Cancellation,
) -> Result<(), ContextEngineError> {
    let failed = |reason: String| ContextEngineError::CacheOpenFailed {
        path: PathBuf::from(connection.path().unwrap_or_default()),
        reason,
    };
    let deadline = Instant::now() + BUSY_TIMEOUT;
    loop {
        match request_wal(connection) {
            Ok(mode) if mode.eq_ignore_ascii_case("wal") => break,
            Ok(mode) => {
                return Err(failed(format!(
                    "the cache is in {mode} journal mode, not WAL"
                )));
            }
            Err(error)
                if matches!(
                    error.sqlite_error_code(),
                    Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
                ) && Instant::now() < deadline =>
            {
                if cancellation.is_cancelled() {
                    return Err(ContextEngineError::Cancelled);
                }
                std::thread::sleep(WAL_RETRY_INTERVAL);
            }
            Err(error) => return Err(failed(error.to_string())),
        }
    }
    connection
        .pragma_update(None, "synchronous", "NORMAL")
        .map_err(|error| failed(error.to_string()))
}

fn request_wal(connection: &Connection) -> Result<String, rusqlite::Error> {
    let current: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    if current.eq_ignore_ascii_case("wal") {
        return Ok(current);
    }
    connection.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
}

/// Creates an empty cache and its single metadata row.
///
/// The table and the row are written in **one** transaction, and that is not a
/// tidiness preference. A second process probing between them would find a
/// database with no `index_meta` in it, conclude corruption, and quarantine a
/// cache this one is still building — so the whole creation is held under the
/// write lock a concurrent reader already waits out. The residual window is
/// between the file being opened and the transaction starting, which
/// [`probe_existing`] covers with a bounded grace rather than a guess.
fn create(
    database: &Path,
    expected: &ExpectedVersions,
    repository_identity: &str,
    generation: u64,
    cancellation: &Cancellation,
) -> Result<(Connection, IndexMeta), ContextEngineError> {
    let failed = |reason: String| ContextEngineError::CacheOpenFailed {
        path: database.to_path_buf(),
        reason,
    };
    let mut connection = open_writable(database, cancellation)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| failed(error.to_string()))?;
    transaction
        .execute_batch(INDEX_SCHEMA)
        .map_err(|error| failed(error.to_string()))?;
    insert_meta(&transaction, expected, repository_identity, generation)?;
    transaction
        .commit()
        .map_err(|error| failed(error.to_string()))?;
    let stored = read_meta(&connection)
        .map_err(|error| failed(error.to_string()))?
        .ok_or_else(|| failed("the metadata row vanished after it was written".to_owned()))?;
    // The row that is actually there may be a racing process's rather than
    // this one's, and yielding to it is only safe if it describes the same
    // cache. A row naming another repository or another schema version is one
    // this build must not adopt: it would report somebody else's identity
    // through `status`, and the next `refresh` would quarantine a file that was
    // never wrong. The generation is deliberately *not* compared — a racing
    // process minting a different one is the normal outcome of yielding, and
    // whatever won is genuinely this cache's generation now.
    if stored.repository_identity != repository_identity
        || stored.schema_version != expected.schema_version
    {
        return Err(failed(format!(
            "a concurrent build wrote index_meta for repository {} at schema version {}",
            stored.repository_identity, stored.schema_version
        )));
    }
    Ok((connection, stored))
}

/// Moves an unusable cache aside, or deletes it when there is nothing to keep.
///
/// The write-ahead log and shared-memory sidecars are removed rather than moved
/// with it. They describe the file under its old name, a replacement created
/// beside them would try to recover them as its own, and a quarantined file is
/// kept to be *looked at* rather than to be reopened.
///
/// # What that costs a concurrent older build
///
/// The `schema_version` too-old branch is reachable while an older Harkness is
/// still running against this cache — an upgrade in place, or two installs. Its
/// committed-but-uncheckpointed pages live in the log this removes, so it loses
/// them. That is the trade ADR-0004 already licenses and it is bounded to
/// exactly what this subtree is for: warm-up time, never evidence. The
/// alternative — leaving the sidecars for a replacement to recover as its own —
/// corrupts the new cache, which is worse in the case that actually matters.
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

/// Writes the single metadata row, yielding to one that is already there.
///
/// A second process may have written it before this transaction took the lock,
/// and its row is as good as this one would have been — so the insert yields
/// and the caller reads whatever is actually stored rather than what it asked
/// for.
fn insert_meta(
    transaction: &rusqlite::Transaction<'_>,
    expected: &ExpectedVersions,
    repository_identity: &str,
    generation: u64,
) -> Result<IndexMeta, ContextEngineError> {
    let failed = |reason: String| ContextEngineError::CacheOpenFailed {
        path: PathBuf::from(INDEX_DATABASE_FILE),
        reason,
    };
    let now = OffsetDateTime::now_utc();
    let meta = IndexMeta {
        schema_version: expected.schema_version,
        parser_version: expected.parser_version.clone(),
        chunking_version: expected.chunking_version.clone(),
        ranking_version: expected.ranking_version.clone(),
        classify_version: expected.classify_version.clone(),
        index_generation: generation,
        repository_identity: repository_identity.to_owned(),
        created_at: now,
        last_opened_at: now,
    };
    let stored_generation = i64::try_from(meta.index_generation)
        .map_err(|_| failed(format!("generation {generation} is not representable")))?;
    let created_at = meta
        .created_at
        .format(&Rfc3339)
        .map_err(|error| failed(error.to_string()))?;
    transaction
        .execute(
            "INSERT INTO index_meta \
             (id, schema_version, parser_version, chunking_version, ranking_version, \
              classify_version, index_generation, repository_identity, created_at, last_opened_at) \
             VALUES (1, :schema_version, :parser_version, :chunking_version, :ranking_version, \
              :classify_version, :index_generation, :repository_identity, :created_at, :created_at) \
             ON CONFLICT(id) DO NOTHING",
            named_params! {
                ":schema_version": meta.schema_version,
                ":parser_version": meta.parser_version,
                ":chunking_version": meta.chunking_version,
                ":ranking_version": meta.ranking_version,
                ":classify_version": meta.classify_version,
                ":index_generation": stored_generation,
                ":repository_identity": meta.repository_identity,
                ":created_at": created_at,
            },
        )
        .map_err(|error| failed(error.to_string()))?;
    Ok(meta)
}

/// Drops every table and reissues the metadata, in one transaction.
///
/// The disposal an open handle can survive. `DROP TABLE` names come from this
/// database's own schema — [#114]'s content tables and nothing else — and are
/// quoted rather than interpolated raw, because a name is still a name.
///
/// [#114]: https://github.com/fullstacktaiye/harkness/issues/114
fn empty_in_place(
    connection: &mut Connection,
    expected: &ExpectedVersions,
    repository_identity: &str,
    generation: u64,
) -> Result<IndexMeta, ContextEngineError> {
    let database = PathBuf::from(connection.path().unwrap_or_default());
    let failed = move |reason: String| ContextEngineError::CacheOpenFailed {
        path: database.clone(),
        reason,
    };
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| failed(error.to_string()))?;
    // `DROP TABLE` performs an implicit delete and checks foreign keys as it
    // goes, so dropping a parent before its children fails on a cache that is
    // about to hold neither. Deferring to commit — by which point every table is
    // gone — makes the order the schema listing happens to return irrelevant.
    transaction
        .execute_batch("PRAGMA defer_foreign_keys = ON")
        .map_err(|error| failed(error.to_string()))?;
    let tables = {
        let mut statement = transaction
            .prepare(
                "SELECT name FROM sqlite_schema \
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            )
            .map_err(|error| failed(error.to_string()))?;
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| failed(error.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| failed(error.to_string()))?
    };
    for table in tables {
        let quoted = table.replace('"', "\"\"");
        transaction
            .execute_batch(&format!("DROP TABLE IF EXISTS \"{quoted}\""))
            .map_err(|error| failed(error.to_string()))?;
    }
    transaction
        .execute_batch(INDEX_SCHEMA)
        .map_err(|error| failed(error.to_string()))?;
    let meta = insert_meta(&transaction, expected, repository_identity, generation)?;
    transaction
        .commit()
        .map_err(|error| failed(error.to_string()))?;
    // Best effort: a concurrent reader can hold a vacuum off, and an emptied
    // cache that has not yet given its pages back is still an emptied cache.
    let _ = connection.execute_batch("VACUUM");
    Ok(meta)
}

/// Deletes the cache under `cache_root` without opening it.
///
/// The one way to be rid of a cache this build cannot address at all — one
/// written by a newer build, whose `index_meta` is refused before a handle
/// exists. [`IndexCache::dispose`] cannot serve that case because it needs the
/// cache open to dispose of it, and "delete this to reclaim disk" has to work
/// on the file that is hardest to read.
///
/// # Errors
///
/// Returns [`ContextEngineError::CacheOpenFailed`] when the file could not be
/// removed.
pub fn discard(cache_root: &Path) -> Result<(), ContextEngineError> {
    remove_database(&cache_root.join(INDEX_DATABASE_FILE))
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

/// Picks a quarantine name nothing is already using.
///
/// `fs::rename` overwrites its destination without complaint on every platform,
/// so a name that is already taken destroys the evidence it was chosen to keep
/// — and two processes quarantining one cache in the same clock tick is exactly
/// the case where the older copy is most worth having. The discriminator is
/// only reached on a collision, so the ordinary name stays fixed width and
/// sorts chronologically.
fn quarantine_path(database: &Path) -> Result<PathBuf, ContextEngineError> {
    let directory = database.parent().unwrap_or_else(|| Path::new("."));
    let stamp = OffsetDateTime::now_utc()
        .format(QUARANTINE_STAMP)
        .map_err(|error| ContextEngineError::CacheOpenFailed {
            path: database.to_path_buf(),
            reason: error.to_string(),
        })?;
    let plain = directory.join(format!("{QUARANTINE_PREFIX}{stamp}"));
    if !plain.exists() {
        return Ok(plain);
    }
    for attempt in 1..u32::MAX {
        let candidate = directory.join(format!("{QUARANTINE_PREFIX}{stamp}.{attempt}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(ContextEngineError::CacheOpenFailed {
        path: database.to_path_buf(),
        reason: "no quarantine name is available".to_owned(),
    })
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
/// Seeded from the wall clock in **microseconds** and floored at `previous + 1`.
/// The clock is what keeps a cache that was *deleted along with its directory*
/// from reissuing a number a stored snapshot already recorded, and the floor is
/// what keeps a clock that stepped backwards from doing the same.
///
/// # Why microseconds
///
/// Nanoseconds since the epoch is about `1.8e18`, past the `2^53` an IEEE-754
/// double represents exactly, and this value travels as a JSON number in both
/// the frozen snapshot wire form and the `context_cache_recreated` payload. A
/// consumer that parses JSON into doubles — every JavaScript front end — would
/// round two distinct generations onto one and read a stale snapshot as fresh.
/// Microseconds is about `1.8e15`, exact in a double until the twenty-third
/// century, and still finer than the file operations a recreation performs; the
/// floor covers two recreations inside one microsecond.
///
/// # Errors
///
/// Returns [`ContextEngineError::CacheOpenFailed`] when `previous` is already
/// `u64::MAX`, which a hand-edited row can produce. Saturating there would hand
/// back the generation being replaced, and a replacement that shares its
/// predecessor's token is the one failure this whole mechanism exists to
/// prevent — better to refuse the cache than to answer with a number that lies.
fn next_generation(previous: Option<u64>) -> Result<u64, ContextEngineError> {
    let clock =
        u64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000).unwrap_or(0);
    let floor = match previous {
        Some(previous) => previous
            .checked_add(1)
            .ok_or_else(|| ContextEngineError::CacheOpenFailed {
                path: PathBuf::from(INDEX_DATABASE_FILE),
                reason: format!(
                    "the cache records generation {previous}, leaving none above it to replace it with"
                ),
            })?,
        None => 0,
    };
    Ok(clock.max(floor))
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
mod store_tests;
#[cfg(test)]
pub(crate) mod tests;
