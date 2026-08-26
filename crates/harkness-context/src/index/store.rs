//! What the cache holds, how it is written, and how it is read back.
//!
//! # The generation commit protocol
//!
//! A batch never writes rows a reader can see until it says so. Opening one
//! allocates a *pending* generation from the worktree's own high-water counter,
//! every row it writes carries that number, and a reader only sees rows whose
//! generation is at or below [`worktrees.last_generation`](super::schema).
//! [`IndexBatch::commit`] moves that watermark in one transaction, together with
//! the sweep — so the whole batch becomes visible at once, and a process killed
//! part-way leaves rows that no query returns and the next batch clears.
//!
//! Rows are flushed *during* the batch rather than held until the end, which is
//! what makes a cold build of a large repository possible at all: one
//! transaction over a hundred thousand files would hold the write lock for the
//! whole walk and buffer every chunk of it in memory. The visibility rule is
//! what buys that — an interleaved read sees the pre-batch view whether the
//! batch is a tenth done or a commit away.
//!
//! The counter is allocated up front and never reused. A crashed batch's
//! generation is therefore dead rather than handed to the next batch, which
//! matters for a [`Targeted`](BatchScope::Targeted) one: it sweeps nothing, so
//! adopting a dead generation's leftovers would make another batch's abandoned
//! rows visible under its own commit.
//!
//! # Every read names a worktree
//!
//! There is no public query here that does not take a [`WorktreeKey`] and join
//! through that worktree's `files` rows. That is the isolation contract [#115]
//! rests on, expressed as an API shape rather than as query discipline: the
//! content tables are shared by every worktree of the repository, so a query
//! that reached them directly would answer one checkout's question with
//! another's rows, and the only way to write one is to add a method that does
//! not take the key.
//!
//! # Bounds
//!
//! Every read is bounded by [`MAX_READ_ROWS`] and every write by
//! [`MAX_INDEX_DB_BYTES`](super::MAX_INDEX_DB_BYTES). A cache that would grow
//! past its cap fails the batch with `index_budget_exhausted` rather than
//! truncating silently, because a partially written index that reports success
//! is one that answers "no match" for content it simply never stored.
//!
//! [#115]: https://github.com/fullstacktaiye/harkness/issues/115

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use harkness_git::Cancellation;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, named_params};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::chunk::{Anchor, ChunkRecord, ChunkSet, FileVersion, Language};
use crate::classify::FileClass;
use crate::digest::Sha256Hex;
use crate::error::ContextEngineError;
use crate::ids::{ChunkId, FileVersionId, SymbolId};
use crate::inventory::{Boundary, InventoryEntry};
use crate::path::RepoPath;
use crate::provenance::ByteRange;
use crate::symbols::{
    ExtractionSkipReason, FileSymbols, MAX_REFERENCES_PER_FILE, MAX_SYMBOLS_PER_FILE, ParseHealth,
    Symbol, SymbolKind, SymbolReference,
};

use super::{IndexCache, IndexCounts, database_bytes, lock, sqlite_failure};

/// Namespace the per-worktree key is derived under.
///
/// Fixed, like `harkness-git`'s repository-lock namespace, so the same checkout
/// resolves to the same row on every machine and every build.
const CONTEXT_WORKTREE_NAMESPACE: Uuid = Uuid::from_u128(0x1d4c_8f27_6a3b_5c9e_bf10_47d8_2e6a_9315);

/// How many derived rows one flush buffers before it commits and releases the lock.
///
/// Small enough that a reader never waits long behind a cold build, large
/// enough that a hundred thousand rows are not a hundred thousand
/// transactions. Nothing depends on the exact value: the generation gate is
/// what makes a flush safe, not its size.
const FLUSH_ROWS: usize = 512;

/// Approximate dynamic bytes one batch buffers between flushes.
const FLUSH_BYTES: usize = 4 * 1024 * 1024;

/// Most rows any single read returns.
///
/// A cap rather than a stream, because every caller of these methods is
/// assembling something bounded anyway and an unbounded read of a
/// hundred-thousand-file repository is a memory failure dressed as a query.
pub const MAX_READ_ROWS: usize = 50_000;

/// Identifies one worktree inside one repository's cache.
///
/// Derived from the *canonical worktree root* rather than from a
/// [`ProjectId`](harkness_core::ProjectId), and the difference matters. A
/// project id names a catalog entry, and two entries can name one checkout —
/// keying by project would then build two full copies of one tree's `files`
/// rows and let each sweep the other's away. The row describes a checkout, so
/// the checkout is what names it.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorktreeKey(String);

impl WorktreeKey {
    /// Derives the key for the worktree at `canonical_root`.
    ///
    /// The caller owes the canonical form. `WorkspaceSnapshot::capture` and
    /// `ContextEngine::open` both canonicalize, so every production path
    /// already holds one; taking a raw path here and canonicalizing it would
    /// make the key depend on whether the checkout happened to exist at the
    /// moment it was derived.
    #[must_use]
    pub fn for_root(canonical_root: &Path) -> Self {
        let bytes = RepoPath::from_path(canonical_root);
        Self(Uuid::new_v5(&CONTEXT_WORKTREE_NAMESPACE, bytes.as_bytes()).to_string())
    }

    /// The stable spelling stored in the `worktree_id` column.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WorktreeKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// How much of a worktree a batch claims to be presenting.
///
/// The distinction decides one thing: whether rows the batch did not confirm
/// are deleted. It is not a performance switch — a full batch that swept
/// nothing would leave deleted files in the index forever, and a targeted batch
/// that swept would delete the whole repository because one file changed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum BatchScope {
    /// Every path of the worktree is being presented; unconfirmed rows are swept.
    Full,
    /// Only the named paths are being presented; every other row is left alone.
    Targeted,
}

impl BatchScope {
    /// Stable spelling carried in receipts and event payloads.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Targeted => "targeted",
        }
    }
}

impl std::fmt::Display for BatchScope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One symbol declaration, as [#117] will present it for storage.
///
/// Defined here rather than waiting for its producer because the store has to
/// accept the rows before anything can write them, and a table with no typed
/// way in is a table the next issue redesigns. Nothing in this build produces
/// one.
///
/// [#117]: https://github.com/fullstacktaiye/harkness/issues/117
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SymbolRecord {
    /// Content-derived symbol identity.
    pub id: SymbolId,
    /// Bare declaration name, which is what a lookup matches on.
    pub name: String,
    /// Qualified structural path from the file root to this declaration.
    pub qualified_path: String,
    /// Parser-owned kind such as `function` or `type`.
    pub kind: String,
    /// Deterministic byte-order ordinal for duplicate qualified declarations.
    pub ordinal: u32,
    /// Half-open bytes of the declaration in the original file.
    pub byte_range: ByteRange,
    /// Enclosing declaration, when one was extracted.
    pub parent: Option<SymbolId>,
    /// Whether test attributes or an enclosing test module apply.
    pub is_test: bool,
    /// Whether invalid UTF-8 was replaced in the stored name.
    pub name_is_lossy: bool,
}

/// One unresolved name mention stored beside its file version.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SymbolReferenceRecord {
    /// Mentioned spelling; no target is implied.
    pub name: String,
    /// Exact mention range.
    pub byte_range: ByteRange,
    /// Whether invalid UTF-8 was replaced in `name`.
    pub name_is_lossy: bool,
}

impl From<&Symbol> for SymbolRecord {
    fn from(symbol: &Symbol) -> Self {
        Self {
            id: symbol.id.clone(),
            name: symbol.name.clone(),
            qualified_path: symbol.qualified_name.clone(),
            kind: symbol.kind.as_str().to_owned(),
            ordinal: symbol.ordinal,
            byte_range: symbol.byte_range,
            parent: symbol.parent.clone(),
            is_test: symbol.is_test,
            name_is_lossy: symbol.name_is_lossy,
        }
    }
}

impl From<&SymbolReference> for SymbolReferenceRecord {
    fn from(reference: &SymbolReference) -> Self {
        Self {
            name: reference.name.clone(),
            byte_range: reference.byte_range,
            name_is_lossy: reference.name_is_lossy,
        }
    }
}

/// What one committed batch did.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct BatchReceipt {
    /// Worktree the batch reconciled.
    pub worktree: WorktreeKey,
    /// Whether unconfirmed rows were swept.
    pub scope: BatchScope,
    /// Generation the batch's rows carry, now the worktree's watermark.
    pub generation: u64,
    /// File rows written or refreshed.
    pub files_recorded: u64,
    /// File rows the batch removed by name.
    pub files_removed: u64,
    /// Chunk rows written.
    pub chunks_recorded: u64,
    /// Symbol rows written.
    pub symbols_recorded: u64,
    /// Files for which a registered adapter attempted parsing.
    pub files_parsed: u64,
    /// Parsed files that retained bounded syntax-error ranges.
    pub partial_files: u64,
    /// Files whose adapter failed or panicked.
    pub failed_files: u64,
    /// Files deliberately skipped after detection.
    pub skipped_files: u64,
    /// File rows a full batch's sweep deleted because nothing confirmed them.
    pub rows_swept: u64,
    /// Content-addressed rows dropped because no file row referenced them.
    pub rows_collected: u64,
    /// Wall-clock time from [`IndexCache::begin`] to the committed watermark.
    pub duration: Duration,
}

/// What forgetting one checkout removed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct ForgetReport {
    /// File rows the removed worktree held.
    pub files_removed: u64,
    /// Content-addressed rows dropped because no worktree still named them.
    pub rows_collected: u64,
}

/// A bounded answer, and whether the bound is why it ended.
///
/// Every unbounded read here returns one. A bare `Vec` of exactly the limit is
/// indistinguishable from a repository that happens to hold exactly that many
/// rows, and the module already refuses that shape on the write side for the
/// same reason: an answer that stops short without saying so is read as
/// complete, and "no match" then means "no match in the part I looked at".
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct IndexedPage<T> {
    /// The rows, at most the limit that was asked for.
    pub rows: Vec<T>,
    /// Whether the store holds more than this page returned.
    pub more: bool,
}

/// Written by hand: the derived one would demand `T: Default`, and an empty
/// page is empty whatever it would have held.
impl<T> Default for IndexedPage<T> {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            more: false,
        }
    }
}

/// Reads like the `Vec` it wraps, so the flag is the only thing a caller has to
/// learn — and it is the thing a caller must not be able to overlook when it
/// matters, which is why it is a field rather than a convention.
impl<T> std::ops::Deref for IndexedPage<T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        &self.rows
    }
}

impl<T> IntoIterator for IndexedPage<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.rows.into_iter()
    }
}

/// One file row, rebuilt from the cache and re-validated.
///
/// `eligible` is derived rather than stored, exactly as it is on
/// [`InventoryEntry`]: a column would go stale against the class, the symlink
/// flag, the boundary and the unreadable flag that decide it, and a stale
/// column is how a `secret_sensitive` file becomes retrievable.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct IndexedFile {
    /// Repository-relative path, byte-exact.
    pub path: RepoPath,
    /// The exact bytes at this path, when they were read.
    pub file_version: Option<FileVersionId>,
    /// Digest of those bytes.
    pub content_sha256: Option<Sha256Hex>,
    /// Whether the chunk set for those bytes stops short of the whole file.
    ///
    /// Recorded rather than derived, because nothing short of re-chunking the
    /// file could tell. A caller assembling context from a truncated file is
    /// looking at part of it, and saying so is the difference between "there is
    /// no match here" and "there is no match in the part that was indexed".
    pub truncated: bool,
    /// Size as the walk reported it.
    pub byte_size: u64,
    /// Modification time in nanoseconds, when the platform reported one.
    pub mtime_ns: Option<i64>,
    /// The one class this file holds.
    pub class: FileClass,
    /// Whether the path is a symbolic link, recorded and never followed.
    pub symlink: bool,
    /// Set when the path is a directory the walk refused to descend into.
    pub boundary: Option<Boundary>,
    /// Whether the walk could not read the path.
    pub unreadable: bool,
    /// Classification rules that decided [`class`](Self::class).
    ///
    /// The per-row staleness marker. A cache whose `classify_version` skews
    /// keeps its file rows — they are true records of what the walk saw — and
    /// this is what tells a reconciler which of them to re-classify.
    pub classify_version: u32,
    /// Chunk-boundary rules the derived rows were produced under.
    ///
    /// The other per-row staleness marker, and the one a chunking skew moves:
    /// [`IndexCache::refresh`](super::IndexCache::refresh) empties `chunks` and
    /// nulls this, so a row whose file still exists reads as derived-under-
    /// nothing rather than as derived-under-a-version-nobody-uses. A reconciler
    /// that compared only sizes and modification times would then skip exactly
    /// the files the invalidation created work for.
    ///
    /// `None` also means a path whose content is never read — a binary, a
    /// symlink, a repository boundary — which is why the eligibility of the
    /// entry is asked first.
    pub chunking_version: Option<String>,
    /// Detected language stored for the exact file version.
    pub language: Option<Language>,
    /// Grammar marker that produced this file's symbol rows.
    pub parser_version: Option<String>,
    /// Batch generation that last confirmed this row.
    pub generation: u64,
}

impl IndexedFile {
    /// Whether this file's content may be indexed and retrieved.
    #[must_use]
    pub const fn eligible(&self) -> bool {
        self.class.is_eligible() && !self.symlink && self.boundary.is_none() && !self.unreadable
    }

    /// Whether the class was decided by rules this build no longer uses.
    #[must_use]
    pub const fn stale_classification(&self, current: u32) -> bool {
        self.classify_version != current
    }
}

/// One chunk row, rebuilt from the cache.
///
/// It holds no text. The index stores paths, digests, ranges and names, so a
/// leaked `index.db` exposes structure and never source — and content is
/// re-read from the working tree when retrieval asks for it.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct IndexedChunk {
    /// Content-derived chunk identity.
    pub id: ChunkId,
    /// The file version this chunk belongs to.
    pub file_version: FileVersionId,
    /// Repository-relative path of that file version.
    pub path: RepoPath,
    /// Structural identity, independent of position.
    pub anchor: Anchor,
    /// Continuation number beneath the anchor.
    pub ordinal: u32,
    /// Original-file half-open bytes and one-based line hints.
    pub byte_range: ByteRange,
    /// SHA-256 of the UTF-8 text the chunk represents.
    pub chunk_sha256: Sha256Hex,
    /// Symbol an outline associated with the chunk.
    pub symbol: Option<SymbolId>,
    /// Detected language of the file version.
    pub language: Option<Language>,
    /// Whether the represented text was transcoded from UTF-16.
    pub transcoded: bool,
    /// Chunk-boundary rules that produced the row.
    pub chunking_version: String,
}

/// One symbol row, rebuilt from the cache.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct IndexedSymbol {
    /// Content-derived symbol identity.
    pub id: SymbolId,
    /// The file version this symbol was declared in.
    pub file_version: FileVersionId,
    /// Repository-relative path of that file version.
    pub path: RepoPath,
    /// Bare declaration name.
    pub name: String,
    /// Qualified structural path to the declaration.
    pub qualified_path: String,
    /// Parser-owned kind.
    pub kind: SymbolKind,
    /// Deterministic byte-order ordinal for duplicate declarations.
    pub ordinal: u32,
    /// Half-open bytes of the declaration.
    pub byte_range: ByteRange,
    /// Enclosing declaration, when one was extracted.
    pub parent: Option<SymbolId>,
    /// Whether test attributes or an enclosing test module apply.
    pub is_test: bool,
    /// Whether invalid UTF-8 was replaced in `name`.
    pub name_is_lossy: bool,
    /// Grammar version that produced the row.
    pub parser_version: String,
}

/// Parse health stored for one exact file version.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct IndexedParseHealth {
    /// Exact file version the answer describes.
    pub file_version: FileVersionId,
    /// Repository-relative path.
    pub path: RepoPath,
    /// Detected language, absent when detection found none.
    pub language: Option<Language>,
    /// Adapter marker that produced the answer.
    pub grammar_version: String,
    /// Complete, partial, failed, or intentionally skipped result.
    pub health: ParseHealth,
}

/// One stored unresolved name mention.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct IndexedSymbolReference {
    /// Exact file version the mention came from.
    pub file_version: FileVersionId,
    /// Repository-relative path.
    pub path: RepoPath,
    /// Mentioned spelling; no target is implied.
    pub name: String,
    /// Exact original-file mention range.
    pub byte_range: ByteRange,
    /// Whether invalid UTF-8 was replaced in `name`.
    pub name_is_lossy: bool,
}

/// One file's rows, buffered until the next flush.
#[derive(Debug)]
struct PendingFile {
    path: RepoPath,
    byte_size: u64,
    mtime_ns: Option<i64>,
    class: FileClass,
    symlink: bool,
    boundary: Option<Boundary>,
    unreadable: bool,
    classify_version: u32,
    content: Derivation,
}

/// What a batch knows about one file's *content*, which is not the same
/// question as what it knows about the file.
#[derive(Debug)]
enum Derivation {
    /// The bytes were read and chunked; this is what they produced.
    Read(Box<PendingContent>),
    /// The file is one whose content is never read — a binary, a symlink, a
    /// repository boundary. Its content columns are cleared.
    None,
    /// The file should have been read and could not be: it changed under the
    /// walk, or the open failed.
    ///
    /// Distinct from [`None`](Self::None) because clearing the columns would
    /// unlink the file version the last successful pass stored, and the commit's
    /// collection would then delete its chunks. One unreadable moment would
    /// cost the file its whole entry in the index until something walked again.
    /// Keeping the previous derivation is stale, and stale beats absent.
    Unavailable,
    /// The bytes were read, hashed, and turned out to be the ones the stored
    /// row already names.
    ///
    /// Its metadata is refreshed and its derivation is left exactly as it is —
    /// the same column treatment [`Unavailable`](Self::Unavailable) gets, for
    /// the opposite reason. There is nothing to re-derive, so re-deriving would
    /// be work whose only product is a row identical to the one already there,
    /// and clearing the link would delete a chunk set that is still correct.
    Kept,
}

/// The derived half of one file, present only when its bytes were read.
#[derive(Debug)]
struct PendingContent {
    file_version: FileVersionId,
    content_sha256: Sha256Hex,
    language: Option<String>,
    transcoded: bool,
    truncated: bool,
    chunking_version: Option<String>,
    chunks: Vec<PendingChunk>,
}

#[derive(Debug)]
struct PendingChunk {
    id: ChunkId,
    anchor: String,
    ordinal: u32,
    range: ByteRange,
    chunk_sha256: Sha256Hex,
    symbol: Option<SymbolId>,
}

/// Symbols attached to one file version, buffered until the next flush.
#[derive(Debug)]
struct PendingSymbols {
    file_version: FileVersionId,
    parser_version: String,
    symbols: Vec<SymbolRecord>,
    references: Vec<SymbolReferenceRecord>,
    health: ParseHealth,
}

/// One reconciliation of one worktree, in flight.
///
/// Dropping a batch without committing it abandons everything it wrote: the
/// rows are there, no query returns them, and the next
/// [`IndexCache::begin`] for the same worktree deletes them. That is the same
/// outcome a killed process produces, deliberately — a batch has exactly one
/// way to end well and every other way is the same way.
#[derive(Debug)]
pub struct IndexBatch<'cache> {
    cache: &'cache IndexCache,
    worktree: WorktreeKey,
    scope: BatchScope,
    generation: u64,
    started: Instant,
    head_marker: Option<Option<String>>,
    files: Vec<PendingFile>,
    symbols: Vec<PendingSymbols>,
    removals: Vec<RepoPath>,
    /// File versions this batch has written, so symbols can be told apart from
    /// symbols whose parent has not arrived yet.
    recorded_versions: BTreeSet<String>,
    displaced: BTreeSet<String>,
    files_recorded: u64,
    files_removed: u64,
    chunks_recorded: u64,
    symbols_recorded: u64,
    files_parsed: u64,
    partial_files: u64,
    failed_files: u64,
    skipped_files: u64,
}

impl IndexCache {
    /// Invalidates only symbol rows produced by a changed language adapter.
    ///
    /// Grammar versions live one row per language rather than in the global
    /// parser component marker. A Rust grammar bump therefore nulls and
    /// deletes Rust derivations while TOML and Markdown rows remain byte-for-
    /// byte untouched. Removed adapters are treated as changes too so the next
    /// reconcile records an honest `unsupported_language` health row.
    pub fn refresh_grammar_versions(
        &self,
        versions: &[(String, String)],
    ) -> Result<u64, ContextEngineError> {
        let invalidated = self.with_write(|transaction| {
            let current = versions.iter().cloned().collect::<BTreeMap<_, _>>();
            let mut statement = transaction.prepare(
                "SELECT language, grammar_version FROM parser_versions ORDER BY language",
            )?;
            let stored = statement
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
                .collect::<Result<BTreeMap<_, _>, _>>()?;
            drop(statement);
            let mut invalidated = 0_u64;
            let languages = stored
                .keys()
                .chain(current.keys())
                .cloned()
                .collect::<BTreeSet<_>>();
            for language in languages {
                if stored.get(&language) == current.get(&language) {
                    continue;
                }
                for table in ["symbol_references", "parse_health", "symbols"] {
                    invalidated = invalidated.saturating_add(
                        u64::try_from(transaction.execute(
                            &format!(
                                "DELETE FROM {table} WHERE file_version_id IN \
                                 (SELECT file_version_id FROM file_versions WHERE language = :language)"
                            ),
                            named_params! { ":language": &language },
                        )?)
                        .unwrap_or(u64::MAX),
                    );
                }
                transaction.execute(
                    "UPDATE file_versions SET parser_version = NULL WHERE language = :language",
                    named_params! { ":language": &language },
                )?;
                match current.get(&language) {
                    Some(version) => {
                        transaction.execute(
                            "INSERT INTO parser_versions (language, grammar_version) \
                             VALUES (:language, :version) \
                             ON CONFLICT(language) DO UPDATE SET grammar_version = excluded.grammar_version",
                            named_params! { ":language": &language, ":version": version },
                        )?;
                    }
                    None => {
                        transaction.execute(
                            "DELETE FROM parser_versions WHERE language = :language",
                            named_params! { ":language": &language },
                        )?;
                    }
                }
            }
            Ok(invalidated)
        })?;
        self.publish_counts();
        Ok(invalidated)
    }

    /// Opens a batch that reconciles `worktree`, rooted at `root`.
    ///
    /// Allocates the pending generation and clears any rows left above the
    /// visible watermark by a batch that never committed. Nothing this batch
    /// writes is visible until [`IndexBatch::commit`] returns.
    ///
    /// # Errors
    ///
    /// [`ContextEngineError::IndexBusy`] when another process holds the write
    /// lock past the busy timeout, [`ContextEngineError::Cancelled`] when the
    /// token is observed, and [`ContextEngineError::CacheOpenFailed`] when the
    /// cache holds no connection.
    pub fn begin(
        &self,
        worktree: &WorktreeKey,
        root: &Path,
        scope: BatchScope,
        cancellation: &Cancellation,
    ) -> Result<IndexBatch<'_>, ContextEngineError> {
        if cancellation.is_cancelled() {
            return Err(ContextEngineError::Cancelled);
        }
        let span = tracing::debug_span!(
            "context.index.begin",
            worktree = worktree.as_str(),
            scope = scope.as_str()
        );
        let _entered = span.enter();

        let root_bytes = RepoPath::from_path(root);
        let (generation, abandoned) = self.with_write(|transaction| {
            transaction.execute(
                "INSERT INTO worktrees (worktree_id, root_path, next_generation, last_generation) \
                 VALUES (:worktree, :root, 0, 0) \
                 ON CONFLICT(worktree_id) DO UPDATE SET root_path = excluded.root_path",
                named_params! { ":worktree": worktree.as_str(), ":root": root_bytes.as_bytes() },
            )?;
            transaction.execute(
                "UPDATE worktrees SET next_generation = next_generation + 1 \
                 WHERE worktree_id = :worktree",
                named_params! { ":worktree": worktree.as_str() },
            )?;
            let (pending, visible): (i64, i64) = transaction.query_row(
                "SELECT next_generation, last_generation FROM worktrees WHERE worktree_id = :worktree",
                named_params! { ":worktree": worktree.as_str() },
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            // Staged rows at or below the watermark are provably dead, and only
            // those. A batch holding generation `g` can publish only by moving
            // the watermark *to* `g`, and the commit refuses to move it
            // backwards — so once the watermark has reached `g`, the batch that
            // holds it either already committed and cleared its own rows, or is
            // going to be refused. A batch above the watermark may still be
            // running in another process, and its rows are left alone.
            //
            // Their file versions are carried into this batch's displaced set,
            // because a targeted batch collects only what it displaced: without
            // this, a repository only ever updated incrementally would
            // accumulate the derived rows of every batch that was interrupted.
            let abandoned = {
                let mut statement = transaction.prepare(
                    "SELECT DISTINCT file_version_id FROM pending_files \
                     WHERE worktree_id = :worktree AND generation <= :visible \
                     AND file_version_id IS NOT NULL",
                )?;
                statement
                    .query_map(
                        named_params! { ":worktree": worktree.as_str(), ":visible": visible },
                        |row| row.get::<_, String>(0),
                    )?
                    .collect::<Result<BTreeSet<_>, _>>()?
            };
            transaction.execute(
                "DELETE FROM pending_files \
                 WHERE worktree_id = :worktree AND generation <= :visible",
                named_params! { ":worktree": worktree.as_str(), ":visible": visible },
            )?;
            Ok((u64::try_from(pending).unwrap_or(0), abandoned))
        })?;

        Ok(IndexBatch {
            cache: self,
            worktree: worktree.clone(),
            scope,
            generation,
            started: Instant::now(),
            head_marker: None,
            files: Vec::new(),
            symbols: Vec::new(),
            removals: Vec::new(),
            recorded_versions: BTreeSet::new(),
            displaced: abandoned,
            files_recorded: 0,
            files_removed: 0,
            chunks_recorded: 0,
            symbols_recorded: 0,
            files_parsed: 0,
            partial_files: 0,
            failed_files: 0,
            skipped_files: 0,
        })
    }

    /// The file row `worktree` holds for `path`, when one is visible.
    ///
    /// # Errors
    ///
    /// The read failures of [`IndexCache::files`].
    pub fn file(
        &self,
        worktree: &WorktreeKey,
        path: &RepoPath,
    ) -> Result<Option<IndexedFile>, ContextEngineError> {
        self.with_read(|connection| {
            connection
                .query_row(
                    &format!("{FILE_SELECT} AND f.path = :path"),
                    named_params! {
                        ":worktree": worktree.as_str(),
                        ":path": path.as_bytes(),
                    },
                    read_file,
                )
                .optional()
        })
    }

    /// Every visible file row of `worktree`, in path order, bounded by `limit`.
    ///
    /// # Errors
    ///
    /// [`ContextEngineError::IndexBusy`] when the connection is contended past
    /// the busy timeout, [`ContextEngineError::CacheOpenFailed`] when the cache
    /// holds no connection or a row cannot be rebuilt.
    pub fn files(
        &self,
        worktree: &WorktreeKey,
        limit: usize,
    ) -> Result<IndexedPage<IndexedFile>, ContextEngineError> {
        let Some(probe) = probe_limit(limit) else {
            return Ok(IndexedPage::default());
        };
        self.with_read(|connection| {
            let mut statement =
                connection.prepare(&format!("{FILE_SELECT} ORDER BY f.path LIMIT :limit"))?;
            let rows = statement.query_map(
                named_params! { ":worktree": worktree.as_str(), ":limit": probe },
                read_file,
            )?;
            Ok(page(rows.collect::<Result<Vec<_>, _>>()?, limit))
        })
    }

    /// Every visible file row of `worktree` at or beneath `prefix`.
    ///
    /// In path order, starting after `after` and bounded by `limit`, so a
    /// caller reconciling a subtree pages through it rather than holding a
    /// repository's worth of rows. An empty `prefix` is the worktree root and
    /// means every row, which is what makes this one method serve a whole-tree
    /// sweep and a one-directory one.
    ///
    /// Containment requires the separator: `src` returns `src` and
    /// `src/main.rs` and never `src-generated.rs`. That is the same rule
    /// [`RepoPath::contains`] states, expressed as a range over the stored
    /// bytes — SQLite compares blobs by `memcmp`, which is the ordering
    /// [`RepoPath`] already digests and sorts under, so the range and the
    /// in-memory predicate cannot disagree.
    ///
    /// # Errors
    ///
    /// The read failures of [`IndexCache::files`].
    pub fn files_under(
        &self,
        worktree: &WorktreeKey,
        prefix: &RepoPath,
        after: Option<&RepoPath>,
        limit: usize,
    ) -> Result<IndexedPage<IndexedFile>, ContextEngineError> {
        let Some(probe) = probe_limit(limit) else {
            return Ok(IndexedPage::default());
        };
        let scoped = !prefix.is_empty();
        let mut sql = String::from(FILE_SELECT);
        if scoped {
            sql.push_str(" AND (f.path = :prefix OR (f.path > :low AND f.path < :high))");
        }
        if after.is_some() {
            sql.push_str(" AND f.path > :after");
        }
        sql.push_str(" ORDER BY f.path LIMIT :limit");

        let key = worktree.as_str();
        let (low, high) = subtree_bounds(prefix);
        let prefix_bytes = prefix.as_bytes().to_vec();
        let after_bytes = after.map(|path| path.as_bytes().to_vec());
        self.with_read(|connection| {
            let mut statement = connection.prepare(&sql)?;
            let mut params: Vec<(&str, &dyn rusqlite::ToSql)> =
                vec![(":worktree", &key), (":limit", &probe)];
            if scoped {
                params.push((":prefix", &prefix_bytes));
                params.push((":low", &low));
                params.push((":high", &high));
            }
            if let Some(after) = after_bytes.as_ref() {
                params.push((":after", after));
            }
            let rows = statement.query_map(params.as_slice(), read_file)?;
            Ok(page(rows.collect::<Result<Vec<_>, _>>()?, limit))
        })
    }

    /// The committed base a whole-worktree reconcile last verified `worktree`
    /// against, when one has.
    ///
    /// `None` means no full pass has ever recorded one — a cache that has only
    /// seen targeted updates, or one that has never seen this checkout at all.
    /// A caller must read that as "cannot be told" rather than as "unchanged":
    /// the marker exists to make a re-created worktree ([#63]) distrust its own
    /// metadata, and an absent marker is exactly the case where metadata is all
    /// there is.
    ///
    /// # Errors
    ///
    /// The read failures of [`IndexCache::files`].
    ///
    /// [#63]: https://github.com/fullstacktaiye/harkness/issues/63
    pub fn worktree_marker(
        &self,
        worktree: &WorktreeKey,
    ) -> Result<Option<String>, ContextEngineError> {
        self.with_read(|connection| {
            connection
                .query_row(
                    "SELECT head_marker FROM worktrees WHERE worktree_id = :worktree",
                    named_params! { ":worktree": worktree.as_str() },
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()
                .map(Option::flatten)
        })
    }

    /// Forgets one checkout entirely, keeping every row a sibling still uses.
    ///
    /// The answer to a worktree that has been removed. Its `worktrees` row and
    /// every `files` row beneath it go; the content-addressed tables are then
    /// collected, so a blob two checkouts shared survives exactly as long as the
    /// other one still names it. Nothing about the *repository's* cache is
    /// disturbed — this is not a disposal, and a sibling worktree keeps
    /// answering from the same file throughout.
    ///
    /// Removing a checkout is a decision nothing here makes. A worktree whose
    /// root has gone is reported as unavailable and keeps its rows, because a
    /// mount that has not come back and a checkout that was deleted look
    /// identical from inside this process and only one of them licenses
    /// throwing the rows away.
    ///
    /// # Errors
    ///
    /// [`ContextEngineError::IndexBusy`] under sustained contention,
    /// [`ContextEngineError::Cancelled`] when the token is observed, and
    /// [`ContextEngineError::CacheOpenFailed`] when the cache holds no
    /// connection.
    pub fn forget_worktree(
        &self,
        worktree: &WorktreeKey,
        cancellation: &Cancellation,
    ) -> Result<ForgetReport, ContextEngineError> {
        if cancellation.is_cancelled() {
            return Err(ContextEngineError::Cancelled);
        }
        let key = worktree.as_str().to_owned();
        let report = self.with_write(|transaction| {
            // Deleted rather than left to the foreign key, even though
            // `foreign_keys` is on and the cascade would do it. A collection
            // that ran before the cascade had been applied would find rows
            // still pointing at the versions it was deciding about, and the
            // order of two statements is a cheaper thing to be sure of than the
            // order of a statement and a trigger.
            let files = transaction.execute(
                "DELETE FROM files WHERE worktree_id = :worktree",
                named_params! { ":worktree": &key },
            )?;
            transaction.execute(
                "DELETE FROM pending_files WHERE worktree_id = :worktree",
                named_params! { ":worktree": &key },
            )?;
            transaction.execute(
                "DELETE FROM worktrees WHERE worktree_id = :worktree",
                named_params! { ":worktree": &key },
            )?;
            let collected = collect_all(transaction)?;
            Ok(ForgetReport {
                files_removed: files as u64,
                rows_collected: collected,
            })
        })?;
        self.publish_counts();
        tracing::debug!(
            worktree = worktree.as_str(),
            files = report.files_removed,
            collected = report.rows_collected,
            "context index forgot a worktree"
        );
        Ok(report)
    }

    /// Every chunk of the file `worktree` holds at `path`, in ordinal order.
    ///
    /// # Errors
    ///
    /// The read failures of [`IndexCache::files`].
    pub fn chunks(
        &self,
        worktree: &WorktreeKey,
        path: &RepoPath,
    ) -> Result<Vec<IndexedChunk>, ContextEngineError> {
        self.with_read(|connection| {
            let mut statement = connection.prepare(&format!(
                "{CHUNK_SELECT} AND f.path = :path ORDER BY c.ordinal, c.chunk_id LIMIT :limit"
            ))?;
            let rows = statement.query_map(
                named_params! {
                    ":worktree": worktree.as_str(),
                    ":path": path.as_bytes(),
                    ":limit": clamp_limit(MAX_READ_ROWS),
                },
                read_chunk,
            )?;
            rows.collect()
        })
    }

    /// The chunk `id` names, when `worktree` holds the file it belongs to.
    ///
    /// A chunk id is derived from a path and content, so the *same* id can be
    /// reached from two worktrees holding that path. Answering `None` for a
    /// worktree that does not hold it is the isolation contract doing its job
    /// rather than a missing row.
    ///
    /// # Errors
    ///
    /// The read failures of [`IndexCache::files`].
    pub fn chunk(
        &self,
        worktree: &WorktreeKey,
        id: &ChunkId,
    ) -> Result<Option<IndexedChunk>, ContextEngineError> {
        self.with_read(|connection| {
            connection
                .query_row(
                    &format!("{CHUNK_SELECT} AND c.chunk_id = :chunk"),
                    named_params! {
                        ":worktree": worktree.as_str(),
                        ":chunk": id.to_string(),
                    },
                    read_chunk,
                )
                .optional()
        })
    }

    /// Every symbol named `name` in a file `worktree` holds.
    ///
    /// # Errors
    ///
    /// The read failures of [`IndexCache::files`].
    pub fn symbols_named(
        &self,
        worktree: &WorktreeKey,
        name: &str,
        limit: usize,
    ) -> Result<IndexedPage<IndexedSymbol>, ContextEngineError> {
        let Some(probe) = probe_limit(limit) else {
            return Ok(IndexedPage::default());
        };
        self.with_read(|connection| {
            let mut statement = connection.prepare(&format!(
                "{SYMBOL_SELECT} AND s.name = :name \
                 ORDER BY s.qualified_path, f.path, s.start_byte LIMIT :limit"
            ))?;
            let rows = statement.query_map(
                named_params! {
                    ":worktree": worktree.as_str(),
                    ":name": name,
                    ":limit": probe,
                },
                read_symbol,
            )?;
            Ok(page(rows.collect::<Result<Vec<_>, _>>()?, limit))
        })
    }

    /// Every symbol whose qualified path ends in `suffix`.
    pub fn symbols_qualified_suffix(
        &self,
        worktree: &WorktreeKey,
        suffix: &str,
        limit: usize,
    ) -> Result<IndexedPage<IndexedSymbol>, ContextEngineError> {
        let Some(probe) = probe_limit(limit) else {
            return Ok(IndexedPage::default());
        };
        let pattern = format!("%::{}", escape_like(suffix));
        self.with_read(|connection| {
            let mut statement = connection.prepare(&format!(
                "{SYMBOL_SELECT} AND (s.qualified_path = :suffix OR \
                 s.qualified_path LIKE :pattern ESCAPE '\\') \
                 ORDER BY s.qualified_path, f.path, s.start_byte LIMIT :limit"
            ))?;
            let rows = statement.query_map(
                named_params! {
                    ":worktree": worktree.as_str(),
                    ":suffix": suffix,
                    ":pattern": pattern,
                    ":limit": probe,
                },
                read_symbol,
            )?;
            Ok(page(rows.collect::<Result<Vec<_>, _>>()?, limit))
        })
    }

    /// Every declaration in `path`, in byte order.
    pub fn symbols_in_file(
        &self,
        worktree: &WorktreeKey,
        path: &RepoPath,
        limit: usize,
    ) -> Result<IndexedPage<IndexedSymbol>, ContextEngineError> {
        let Some(probe) = probe_limit(limit) else {
            return Ok(IndexedPage::default());
        };
        self.with_read(|connection| {
            let mut statement = connection.prepare(&format!(
                "{SYMBOL_SELECT} AND f.path = :path \
                 ORDER BY s.start_byte, s.qualified_path LIMIT :limit"
            ))?;
            let rows = statement.query_map(
                named_params! {
                    ":worktree": worktree.as_str(),
                    ":path": path.as_bytes(),
                    ":limit": probe,
                },
                read_symbol,
            )?;
            Ok(page(rows.collect::<Result<Vec<_>, _>>()?, limit))
        })
    }

    /// Parse health for `path`, when the worktree has a derived file version.
    pub fn parse_health(
        &self,
        worktree: &WorktreeKey,
        path: &RepoPath,
    ) -> Result<Option<IndexedParseHealth>, ContextEngineError> {
        self.with_read(|connection| {
            connection
                .query_row(
                    "SELECT h.file_version_id, f.path, v.language, v.parser_version, h.status, \
                            h.reason, h.error_ranges_json, c.byte_size \
                     FROM parse_health h \
                     JOIN file_versions v ON v.file_version_id = h.file_version_id \
                     JOIN contents c ON c.content_sha256 = v.content_sha256 \
                     JOIN files f ON f.file_version_id = h.file_version_id \
                     JOIN worktrees w ON w.worktree_id = f.worktree_id \
                     WHERE f.worktree_id = :worktree AND f.generation <= w.last_generation \
                       AND f.path = :path",
                    named_params! {
                        ":worktree": worktree.as_str(),
                        ":path": path.as_bytes(),
                    },
                    read_parse_health,
                )
                .optional()
        })
    }

    /// Best-effort unresolved mentions in `path`, in byte order.
    pub fn symbol_references_in_file(
        &self,
        worktree: &WorktreeKey,
        path: &RepoPath,
        limit: usize,
    ) -> Result<IndexedPage<IndexedSymbolReference>, ContextEngineError> {
        let Some(probe) = probe_limit(limit) else {
            return Ok(IndexedPage::default());
        };
        self.with_read(|connection| {
            let mut statement = connection.prepare(
                "SELECT r.file_version_id, f.path, r.name, r.start_byte, r.end_byte, \
                        r.start_line, r.end_line, r.name_is_lossy, c.byte_size \
                 FROM symbol_references r \
                 JOIN file_versions v ON v.file_version_id = r.file_version_id \
                 JOIN contents c ON c.content_sha256 = v.content_sha256 \
                 JOIN files f ON f.file_version_id = r.file_version_id \
                 JOIN worktrees w ON w.worktree_id = f.worktree_id \
                 WHERE f.worktree_id = :worktree AND f.generation <= w.last_generation \
                   AND f.path = :path ORDER BY r.start_byte, r.ordinal LIMIT :limit",
            )?;
            let rows = statement.query_map(
                named_params! {
                    ":worktree": worktree.as_str(),
                    ":path": path.as_bytes(),
                    ":limit": probe,
                },
                read_symbol_reference,
            )?;
            Ok(page(rows.collect::<Result<Vec<_>, _>>()?, limit))
        })
    }

    /// Visible files whose symbol coverage stopped at a per-file budget.
    ///
    /// Worktree-scoped for the same reason every other derived read is: one
    /// repository cache is shared by all linked checkouts, while visibility is
    /// decided by each checkout's committed `files` rows.
    pub fn incomplete_symbol_files(
        &self,
        worktree: &WorktreeKey,
    ) -> Result<u64, ContextEngineError> {
        self.with_read(|connection| {
            connection.query_row(
                "SELECT COUNT(*) FROM parse_health h \
                 JOIN files f ON f.file_version_id = h.file_version_id \
                 JOIN worktrees w ON w.worktree_id = f.worktree_id \
                 WHERE f.worktree_id = :worktree AND f.generation <= w.last_generation \
                   AND h.status = 'failed' \
                   AND h.reason IN ('symbol_budget_exhausted', 'reference_budget_exhausted')",
                named_params! { ":worktree": worktree.as_str() },
                |row| {
                    row.get::<_, i64>(0)
                        .map(|count| u64::try_from(count).unwrap_or(0))
                },
            )
        })
    }

    /// How many visible file rows of `worktree` were classified by other rules.
    ///
    /// What a reconciler asks after a `classify_version` skew: the rows are
    /// kept, and this is how many of them it owes a fresh classification.
    ///
    /// # Errors
    ///
    /// The read failures of [`IndexCache::files`].
    pub fn stale_classifications(
        &self,
        worktree: &WorktreeKey,
        current: u32,
    ) -> Result<u64, ContextEngineError> {
        self.with_read(|connection| {
            connection.query_row(
                "SELECT COUNT(*) FROM files f JOIN worktrees w ON w.worktree_id = f.worktree_id \
                 WHERE f.worktree_id = :worktree AND f.generation <= w.last_generation \
                 AND f.classify_version <> :current",
                named_params! { ":worktree": worktree.as_str(), ":current": current },
                |row| {
                    row.get::<_, i64>(0)
                        .map(|count| u64::try_from(count).unwrap_or(0))
                },
            )
        })
    }

    /// The generation `worktree`'s visible rows carry; `0` when it has none.
    ///
    /// # Errors
    ///
    /// The read failures of [`IndexCache::files`].
    pub fn worktree_generation(&self, worktree: &WorktreeKey) -> Result<u64, ContextEngineError> {
        self.with_read(|connection| {
            let stored: Option<i64> = connection
                .query_row(
                    "SELECT last_generation FROM worktrees WHERE worktree_id = :worktree",
                    named_params! { ":worktree": worktree.as_str() },
                    |row| row.get(0),
                )
                .optional()?;
            Ok(stored.map_or(0, |value| u64::try_from(value).unwrap_or(0)))
        })
    }

    /// What the cache holds, across every worktree.
    ///
    /// An aggregate rather than a query: it answers "how big is this cache",
    /// which is a maintenance question about the file, not a retrieval question
    /// about a checkout. No row leaves it, so it needs no worktree to be honest.
    ///
    /// # Errors
    ///
    /// The read failures of [`IndexCache::files`].
    pub fn counts(&self) -> Result<IndexCounts, ContextEngineError> {
        // Delegated rather than written out again. The visible-files join is
        // the visibility rule, and the module spells that exactly once on
        // purpose — two copies of it would let `status().counts` and this
        // disagree about one database.
        self.with_read(|connection| {
            super::counts_of(connection, self.path()).ok_or(rusqlite::Error::QueryReturnedNoRows)
        })
    }

    /// Runs `call` against the cache's connection, mapping SQLite's failures.
    pub(super) fn with_read<T>(
        &self,
        call: impl FnOnce(&Connection) -> Result<T, rusqlite::Error>,
    ) -> Result<T, ContextEngineError> {
        let connection = lock(&self.connection);
        let open = connection
            .as_ref()
            .ok_or_else(|| ContextEngineError::CacheOpenFailed {
                path: self.database.clone(),
                reason: "the cache is closed until it is refreshed".to_owned(),
            })?;
        call(open).map_err(|error| sqlite_failure(&self.database, &error))
    }

    /// Runs `call` inside one immediate transaction, committing on success.
    pub(super) fn with_write<T>(
        &self,
        call: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T, rusqlite::Error>,
    ) -> Result<T, ContextEngineError> {
        let mut connection = lock(&self.connection);
        let open = connection
            .as_mut()
            .ok_or_else(|| ContextEngineError::CacheOpenFailed {
                path: self.database.clone(),
                reason: "the cache is closed until it is refreshed".to_owned(),
            })?;
        let transaction = open
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sqlite_failure(&self.database, &error))?;
        let value = call(&transaction).map_err(|error| sqlite_failure(&self.database, &error))?;
        transaction
            .commit()
            .map_err(|error| sqlite_failure(&self.database, &error))?;
        Ok(value)
    }
}

impl IndexBatch<'_> {
    /// The generation every row this batch writes carries.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// The worktree this batch reconciles.
    #[must_use]
    pub const fn worktree(&self) -> &WorktreeKey {
        &self.worktree
    }

    /// Records a path the walk saw and did not read.
    ///
    /// Every path the inventory recorded belongs in the index, eligible or not:
    /// a reconciler has to know a binary exists to know it has not changed, and
    /// a repository map that skipped them would describe a tree nobody has. A
    /// *denied* path is different and never reaches here — the walk counts it
    /// and records no name, so there is nothing to store.
    ///
    /// # Errors
    ///
    /// The flush failures of [`IndexBatch::commit`].
    pub fn record_entry(
        &mut self,
        entry: &InventoryEntry,
        classify_version: u32,
    ) -> Result<(), ContextEngineError> {
        self.files.push(PendingFile {
            path: entry.path.clone(),
            byte_size: entry.byte_size,
            mtime_ns: entry.mtime_ns,
            class: entry.class,
            symlink: entry.symlink,
            boundary: entry.boundary,
            unreadable: entry.unreadable,
            classify_version,
            content: Derivation::None,
        });
        self.flush_if_full()
    }

    /// Records a path whose bytes should have been read and could not be.
    ///
    /// The file's own metadata is refreshed and whatever derivation the last
    /// successful pass stored is left in place. Recording it as
    /// [`record_entry`](Self::record_entry) instead would clear the link to that
    /// derivation, and the commit's collection would then delete its chunks —
    /// so a file that was being written at the moment the walk reached it would
    /// disappear from retrieval until something walked again.
    ///
    /// # Errors
    ///
    /// The flush failures of [`IndexBatch::commit`].
    pub fn record_unreadable(
        &mut self,
        entry: &InventoryEntry,
        classify_version: u32,
    ) -> Result<(), ContextEngineError> {
        self.files.push(PendingFile {
            path: entry.path.clone(),
            byte_size: entry.byte_size,
            mtime_ns: entry.mtime_ns,
            class: entry.class,
            symlink: entry.symlink,
            boundary: entry.boundary,
            unreadable: true,
            classify_version,
            content: Derivation::Unavailable,
        });
        self.flush_if_full()
    }

    /// Records a path whose bytes were read and chunked.
    ///
    /// The entry and the version must describe one file: the version's path is
    /// what the derived rows are keyed by, and pairing an entry with another
    /// file's bytes would file one path's chunks under another's identity.
    ///
    /// # Errors
    ///
    /// [`ContextEngineError::IndexBatchInvalid`] when the entry and the version
    /// disagree about which path they describe, plus the flush failures of
    /// [`IndexBatch::commit`].
    pub fn record_chunked(
        &mut self,
        entry: &InventoryEntry,
        version: &FileVersion,
        chunks: &ChunkSet,
        classify_version: u32,
    ) -> Result<(), ContextEngineError> {
        if entry.path != *version.path() {
            return Err(ContextEngineError::IndexBatchInvalid {
                reason: format!(
                    "the inventory entry names '{}' and the file version names '{}'",
                    entry.path.display(),
                    version.path().display()
                ),
            });
        }
        let chunking_version = chunks.chunks.first().map_or_else(
            || crate::CHUNKING_VERSION.to_string(),
            |chunk| chunk.chunking_version.to_string(),
        );
        self.chunks_recorded += chunks.chunks.len() as u64;
        self.files.push(PendingFile {
            path: entry.path.clone(),
            byte_size: entry.byte_size,
            mtime_ns: entry.mtime_ns,
            class: entry.class,
            symlink: entry.symlink,
            boundary: entry.boundary,
            unreadable: entry.unreadable,
            classify_version,
            content: Derivation::Read(Box::new(PendingContent {
                file_version: version.id().clone(),
                content_sha256: version.content_sha256().clone(),
                language: version
                    .language()
                    .map(|language| language.as_str().to_owned()),
                transcoded: version.encoding().is_transcoded(),
                truncated: chunks.truncation.is_some(),
                chunking_version: Some(chunking_version),
                chunks: chunks.chunks.iter().map(pending_chunk).collect(),
            })),
        });
        self.flush_if_full()
    }

    /// Attaches symbols to a file version this batch is also recording.
    ///
    /// Separate from [`record_chunked`](Self::record_chunked) because the
    /// producers are: [#117] extracts symbols from an outline the chunker has
    /// already used, and forcing one call would make the store's API change
    /// when that lands.
    ///
    /// **Order does not matter.** Symbols named for a file version this batch
    /// has not written yet are carried forward rather than written into a
    /// foreign key that does not exist — a flush boundary falling between the
    /// two calls would otherwise fail the batch, which the caller cannot see
    /// coming and cannot control.
    ///
    /// # Errors
    ///
    /// The flush failures of [`IndexBatch::commit`], and
    /// [`ContextEngineError::IndexBatchInvalid`] at commit when the file version
    /// never arrived.
    ///
    /// [#117]: https://github.com/fullstacktaiye/harkness/issues/117
    pub fn record_symbols(
        &mut self,
        file_version: &FileVersionId,
        parser_version: &str,
        symbols: &[SymbolRecord],
    ) -> Result<(), ContextEngineError> {
        self.record_symbol_rows(
            file_version,
            parser_version,
            ParseHealth::Complete,
            symbols.to_vec(),
            Vec::new(),
        )
    }

    /// Attaches a complete extraction, including health and unresolved mentions.
    ///
    /// The ordering, buffering, and error contract are identical to
    /// [`record_symbols`](Self::record_symbols).
    pub fn record_extraction(
        &mut self,
        file_version: &FileVersionId,
        extracted: &FileSymbols,
    ) -> Result<(), ContextEngineError> {
        let symbols = extracted
            .symbols
            .iter()
            .map(SymbolRecord::from)
            .collect::<Vec<_>>();
        let references = extracted
            .references
            .iter()
            .map(SymbolReferenceRecord::from)
            .collect::<Vec<_>>();
        self.record_symbol_rows(
            file_version,
            &extracted.grammar_version,
            extracted.health.clone(),
            symbols,
            references,
        )
    }

    fn record_symbol_rows(
        &mut self,
        file_version: &FileVersionId,
        parser_version: &str,
        health: ParseHealth,
        symbols: Vec<SymbolRecord>,
        references: Vec<SymbolReferenceRecord>,
    ) -> Result<(), ContextEngineError> {
        if symbols.len() > MAX_SYMBOLS_PER_FILE {
            return Err(ContextEngineError::IndexBatchInvalid {
                reason: format!(
                    "file version {file_version} supplied {} symbols, above the per-file limit {MAX_SYMBOLS_PER_FILE}",
                    symbols.len()
                ),
            });
        }
        if references.len() > MAX_REFERENCES_PER_FILE {
            return Err(ContextEngineError::IndexBatchInvalid {
                reason: format!(
                    "file version {file_version} supplied {} references, above the per-file limit {MAX_REFERENCES_PER_FILE}",
                    references.len()
                ),
            });
        }
        match &health {
            ParseHealth::Complete => self.files_parsed = self.files_parsed.saturating_add(1),
            ParseHealth::Partial { .. } => {
                self.files_parsed = self.files_parsed.saturating_add(1);
                self.partial_files = self.partial_files.saturating_add(1);
            }
            ParseHealth::Failed { .. } => {
                self.files_parsed = self.files_parsed.saturating_add(1);
                self.failed_files = self.failed_files.saturating_add(1);
            }
            ParseHealth::Skipped { .. } => {
                self.skipped_files = self.skipped_files.saturating_add(1);
            }
        }
        self.symbols_recorded += symbols.len() as u64;
        self.symbols.push(PendingSymbols {
            file_version: file_version.clone(),
            parser_version: parser_version.to_owned(),
            symbols,
            references,
            health,
        });
        self.flush_if_full()
    }

    /// Records a path whose bytes are the ones its stored row already names.
    ///
    /// The third answer, beside [`record_entry`](Self::record_entry) and
    /// [`record_unreadable`](Self::record_unreadable), and the one an
    /// incremental update spends most of its calls on. A hint said a file
    /// changed, the file was read and hashed, and the digest matched: its size
    /// and modification time are refreshed so the next sweep has no reason to
    /// hash it again, and its derivation is left alone because re-deriving it
    /// would produce the rows that are already there.
    ///
    /// Recording it with [`record_entry`](Self::record_entry) instead would
    /// clear `files.file_version_id`, and the commit's collection would then
    /// delete a chunk set nothing was wrong with — the same failure
    /// [`record_unreadable`](Self::record_unreadable) exists to avoid, reached
    /// from the opposite direction.
    ///
    /// # Errors
    ///
    /// The flush failures of [`IndexBatch::commit`].
    pub fn record_refreshed(
        &mut self,
        entry: &InventoryEntry,
        classify_version: u32,
    ) -> Result<(), ContextEngineError> {
        self.files.push(PendingFile {
            path: entry.path.clone(),
            byte_size: entry.byte_size,
            mtime_ns: entry.mtime_ns,
            class: entry.class,
            symlink: entry.symlink,
            boundary: entry.boundary,
            unreadable: entry.unreadable,
            classify_version,
            content: Derivation::Kept,
        });
        self.flush_if_full()
    }

    /// Records the committed base this batch verified the whole worktree
    /// against.
    ///
    /// Written by the commit beside the watermark, so a batch that never
    /// published never claims a base. **Only a batch that examined every path
    /// of the worktree may call this.** A targeted update that recorded a
    /// marker would say the checkout as a whole had been verified against a
    /// base one file was compared to, and the next full pass would then trust
    /// metadata it had never checked — which is the exact case the marker
    /// exists to catch.
    ///
    /// `None` clears it, which is what a worktree whose head cannot be read
    /// deserves: "no base was verified" is a true statement and "the base is
    /// whatever it was last time" is not.
    pub fn record_head_marker(&mut self, marker: Option<&str>) {
        self.head_marker = Some(marker.map(ToOwned::to_owned));
    }

    /// Removes one path's row from the worktree this batch reconciles.
    ///
    /// The targeted answer to a deleted file. A full batch does not need it —
    /// its sweep removes whatever it did not confirm — but calling it there is
    /// harmless and says the same thing.
    ///
    /// # Errors
    ///
    /// The flush failures of [`IndexBatch::commit`].
    pub fn remove(&mut self, path: &RepoPath) -> Result<(), ContextEngineError> {
        self.removals.push(path.clone());
        self.flush_if_full()
    }

    /// Writes everything buffered, sweeps, and moves the visible watermark.
    ///
    /// The watermark and the sweep are one transaction, so the batch becomes
    /// visible all at once or not at all.
    ///
    /// # Errors
    ///
    /// [`ContextEngineError::IndexBudgetExhausted`] when the cache would grow
    /// past its per-repository cap, [`ContextEngineError::IndexBusy`] under
    /// sustained contention, [`ContextEngineError::Cancelled`] when the token is
    /// observed, and [`ContextEngineError::CacheOpenFailed`] when the cache
    /// holds no connection.
    pub fn commit(
        mut self,
        cancellation: &Cancellation,
    ) -> Result<BatchReceipt, ContextEngineError> {
        let span = tracing::debug_span!(
            "context.index.commit",
            worktree = self.worktree.as_str(),
            generation = self.generation
        );
        let _entered = span.enter();
        if cancellation.is_cancelled() {
            return Err(ContextEngineError::Cancelled);
        }
        self.flush()?;
        if cancellation.is_cancelled() {
            return Err(ContextEngineError::Cancelled);
        }

        if let Some(orphan) = self.symbols.first() {
            return Err(ContextEngineError::IndexBatchInvalid {
                reason: format!(
                    "symbols were attached to file version {}, which this batch never recorded",
                    orphan.file_version
                ),
            });
        }

        let worktree = self.worktree.as_str().to_owned();
        let generation = i64::try_from(self.generation).unwrap_or(i64::MAX);
        let scope = self.scope;
        let head_marker = self.head_marker.take();
        let displaced = std::mem::take(&mut self.displaced);
        let database = self.cache.path().to_path_buf();
        let pending = self.generation;
        let committed_at = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| String::new());

        let (rows_swept, rows_collected, files_removed) = self
            .cache
            .with_write(|transaction| {
                // Forward only, and checked before anything else. A batch that lost
                // a race would otherwise drag the watermark back below the winner's
                // generation and hide every row the winner published — a success
                // that makes the index smaller. The `WHERE` is the whole guard: zero
                // rows changed means somebody else got there.
                let claimed = match &head_marker {
                    // A batch that examined the whole worktree publishes the
                    // base it verified against in the same statement that moves
                    // the watermark, so the two can never disagree about which
                    // pass a marker belongs to.
                    Some(marker) => transaction.execute(
                        "UPDATE worktrees \
                         SET last_generation = :generation, last_reconciled_at = :at, \
                             head_marker = :marker \
                         WHERE worktree_id = :worktree AND last_generation < :generation",
                        named_params! {
                            ":worktree": &worktree,
                            ":generation": generation,
                            ":at": &committed_at,
                            ":marker": marker.as_deref(),
                        },
                    )?,
                    None => transaction.execute(
                        "UPDATE worktrees SET last_generation = :generation, last_reconciled_at = :at \
                         WHERE worktree_id = :worktree AND last_generation < :generation",
                        named_params! {
                            ":worktree": &worktree,
                            ":generation": generation,
                            ":at": &committed_at,
                        },
                    )?,
                };
                if claimed == 0 {
                    let watermark: i64 = transaction
                        .query_row(
                            "SELECT last_generation FROM worktrees WHERE worktree_id = :worktree",
                            named_params! { ":worktree": &worktree },
                            |row| row.get(0),
                        )
                        .unwrap_or_default();
                    return Err(rusqlite::Error::SqliteFailure(
                        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
                        Some(format!("superseded:{watermark}")),
                    ));
                }

                // Everything this batch is about to displace, read before it is
                // displaced. Doing it here rather than per file during the flush
                // turns a query per file into one query per batch.
                let mut displaced =
                    displaced_by_batch(transaction, &worktree, generation, displaced)?;
                // Staged rows below this generation belong to batches the guard
                // above will now refuse, so they are dead and their derived rows are
                // this batch's to collect. Sweeping them here rather than waiting
                // for a later `begin` is what keeps an interrupted incremental
                // update from paying for itself twice.
                collect_staged_below(transaction, &worktree, generation, &mut displaced)?;

                // The staged rows become the visible ones. Two statements rather
                // than one, because a file whose content could not be read leaves
                // `files.file_version_id` alone instead of clearing it.
                apply_staged(transaction, &worktree, generation, false)?;
                apply_staged(transaction, &worktree, generation, true)?;
                let removed = transaction.execute(
                    "DELETE FROM files WHERE worktree_id = :worktree AND path IN \
                    (SELECT path FROM pending_files \
                     WHERE worktree_id = :worktree AND generation = :generation AND removed = 1)",
                    named_params! { ":worktree": &worktree, ":generation": generation },
                )?;

                // Before the collection, not after it: a staged row *references* a
                // file version, so a version this batch is about to collect is one
                // its own staging is still holding down.
                transaction.execute(
                    "DELETE FROM pending_files \
                 WHERE worktree_id = :worktree AND generation <= :generation",
                    named_params! { ":worktree": &worktree, ":generation": generation },
                )?;

                let swept = match scope {
                // Anything this batch did not confirm is a path that is no
                // longer in the worktree.
                BatchScope::Full => transaction.execute(
                    "DELETE FROM files WHERE worktree_id = :worktree AND generation <> :generation",
                    named_params! { ":worktree": &worktree, ":generation": generation },
                )?,
                BatchScope::Targeted => 0,
            };
                let collected = match scope {
                    BatchScope::Full => collect_all(transaction)?,
                    // Exactly the versions this batch displaced, so a one-file
                    // update reads and deletes rows about one file rather than
                    // scanning every content-addressed row in the repository.
                    BatchScope::Targeted => collect_displaced(transaction, &displaced)?,
                };
                Ok((swept as u64, collected, removed as u64))
            })
            .map_err(|error| supersession(&database, pending, &error))?;
        self.files_removed = files_removed;

        let receipt = BatchReceipt {
            worktree: self.worktree.clone(),
            scope: self.scope,
            generation: self.generation,
            files_recorded: self.files_recorded,
            files_removed: self.files_removed,
            chunks_recorded: self.chunks_recorded,
            symbols_recorded: self.symbols_recorded,
            files_parsed: self.files_parsed,
            partial_files: self.partial_files,
            failed_files: self.failed_files,
            skipped_files: self.skipped_files,
            rows_swept,
            rows_collected,
            duration: self.started.elapsed(),
        };
        self.cache.publish_counts();
        tracing::debug!(
            worktree = receipt.worktree.as_str(),
            generation = receipt.generation,
            files = receipt.files_recorded,
            chunks = receipt.chunks_recorded,
            symbols = receipt.symbols_recorded,
            parsed_files = receipt.files_parsed,
            partial_files = receipt.partial_files,
            failed_files = receipt.failed_files,
            skipped_files = receipt.skipped_files,
            swept = receipt.rows_swept,
            duration_ms = receipt.duration.as_millis(),
            "context index batch committed"
        );
        Ok(receipt)
    }

    fn flush_if_full(&mut self) -> Result<(), ContextEngineError> {
        if self.buffered_rows() >= FLUSH_ROWS || self.buffered_bytes() >= FLUSH_BYTES {
            return self.flush();
        }
        Ok(())
    }

    fn buffered_rows(&self) -> usize {
        let files = self.files.iter().fold(0_usize, |total, file| {
            let chunks = match &file.content {
                Derivation::Read(content) => content.chunks.len(),
                Derivation::None | Derivation::Unavailable | Derivation::Kept => 0,
            };
            total.saturating_add(1).saturating_add(chunks)
        });
        let symbols = self.symbols.iter().fold(0_usize, |total, attached| {
            total
                .saturating_add(1)
                .saturating_add(attached.symbols.len())
                .saturating_add(attached.references.len())
        });
        files
            .saturating_add(symbols)
            .saturating_add(self.removals.len())
    }

    fn buffered_bytes(&self) -> usize {
        let files = self.files.iter().fold(0_usize, |total, file| {
            let content = match &file.content {
                Derivation::Read(content) => content.chunks.iter().fold(
                    content
                        .language
                        .as_deref()
                        .map_or(0, str::len)
                        .saturating_add(content.chunking_version.as_deref().map_or(0, str::len)),
                    |bytes, chunk| bytes.saturating_add(chunk.anchor.len()),
                ),
                Derivation::None | Derivation::Unavailable | Derivation::Kept => 0,
            };
            total
                .saturating_add(file.path.as_bytes().len())
                .saturating_add(content)
        });
        let symbols = self.symbols.iter().fold(0_usize, |total, attached| {
            let declaration_bytes = attached.symbols.iter().fold(0_usize, |bytes, symbol| {
                bytes
                    .saturating_add(symbol.name.len())
                    .saturating_add(symbol.qualified_path.len())
                    .saturating_add(symbol.kind.len())
            });
            let reference_bytes = attached
                .references
                .iter()
                .fold(0_usize, |bytes, reference| {
                    bytes.saturating_add(reference.name.len())
                });
            let health_bytes = match &attached.health {
                ParseHealth::Failed { reason } => reason.len(),
                ParseHealth::Partial { error_ranges } => error_ranges
                    .len()
                    .saturating_mul(std::mem::size_of::<ByteRange>()),
                ParseHealth::Complete | ParseHealth::Skipped { .. } => 0,
            };
            total
                .saturating_add(attached.parser_version.len())
                .saturating_add(declaration_bytes)
                .saturating_add(reference_bytes)
                .saturating_add(health_bytes)
        });
        files.saturating_add(symbols)
    }

    /// Writes the buffer at the pending generation, where nothing can see it.
    fn flush(&mut self) -> Result<(), ContextEngineError> {
        if self.files.is_empty() && self.symbols.is_empty() && self.removals.is_empty() {
            return Ok(());
        }
        // Checked before the write rather than after it: a cap enforced on the
        // way out would report the failure only once the bytes were already on
        // disk, which is the truncation this is meant to prevent.
        let bytes = database_bytes(self.cache.path());
        if bytes > super::MAX_INDEX_DB_BYTES {
            return Err(ContextEngineError::IndexBudgetExhausted {
                path: self.cache.path().to_path_buf(),
                bytes,
                limit: super::MAX_INDEX_DB_BYTES,
            });
        }

        // Borrowed rather than taken. A flush that fails is one the caller is
        // told to retry — `index_busy` above all — and moving the buffers out
        // first would destroy up to a flush's worth of rows on the way past,
        // leaving a batch that cannot be resumed and, if the caller carries on,
        // a commit that silently omits the paths it swallowed.
        let files = &self.files;
        let symbols = &self.symbols;
        let removals = &self.removals;
        let worktree = self.worktree.as_str();
        let generation = i64::try_from(self.generation).unwrap_or(i64::MAX);
        let known = &self.recorded_versions;

        let write = self.cache.with_write(|transaction| {
            let mut recorded = 0_u64;
            let mut written = BTreeSet::new();

            for path in removals {
                stage_removal(transaction, worktree, generation, path)?;
            }

            for file in files {
                if let Derivation::Read(content) = &file.content {
                    write_content(transaction, file, content)?;
                    written.insert(content.file_version.to_string());
                }
                stage_file(transaction, worktree, generation, file)?;
                recorded += 1;
            }

            // A file version this batch has not written yet is not a caller
            // mistake: `record_symbols` documents that order does not matter,
            // and a flush boundary falling between the two calls is not
            // something a caller can see coming. Anything still unattached at
            // commit is.
            let mut deferred = Vec::new();
            for attached in symbols {
                let parent = attached.file_version.to_string();
                if known.contains(&parent)
                    || written.contains(&parent)
                    || version_exists(transaction, &parent)?
                {
                    write_symbols(transaction, attached)?;
                } else {
                    deferred.push(attached.file_version.clone());
                }
            }
            let logical_bytes = logical_database_bytes(transaction)?;
            if logical_bytes > super::MAX_INDEX_DB_BYTES {
                return Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
                    Some(format!("index_budget_exhausted:{logical_bytes}")),
                ));
            }
            Ok((recorded, written, deferred))
        });
        let (recorded, written, deferred) =
            write.map_err(|error| index_budget(&self.cache.database, &error))?;

        // Only now that the write committed.
        let carried = std::mem::take(&mut self.symbols)
            .into_iter()
            .filter(|attached| deferred.contains(&attached.file_version))
            .collect();
        self.files.clear();
        self.removals.clear();
        self.symbols = carried;
        self.recorded_versions.extend(written);
        self.files_recorded += recorded;
        Ok(())
    }
}

/// Whether a file version is already stored, for a symbol looking for its parent.
fn version_exists(
    transaction: &rusqlite::Transaction<'_>,
    file_version: &str,
) -> Result<bool, rusqlite::Error> {
    transaction
        .query_row(
            "SELECT 1 FROM file_versions WHERE file_version_id = :id",
            named_params! { ":id": file_version },
            |_| Ok(()),
        )
        .optional()
        .map(|found| found.is_some())
}

/// Logical database bytes after the writes in the current transaction.
///
/// Unlike a filesystem check before the transaction, this includes pages the
/// pending rows actually allocated and lets returning an error roll them all
/// back before they become part of the cache.
fn logical_database_bytes(transaction: &rusqlite::Transaction<'_>) -> Result<u64, rusqlite::Error> {
    let pages = transaction.query_row("PRAGMA page_count", [], |row| row.get::<_, i64>(0))?;
    let page_size = transaction.query_row("PRAGMA page_size", [], |row| row.get::<_, i64>(0))?;
    Ok(strict_u64(pages, 0, "page_count")?.saturating_mul(strict_u64(page_size, 0, "page_size")?))
}

/// Recovers the typed budget refusal used to roll an oversized transaction back.
fn index_budget(database: &Path, error: &ContextEngineError) -> ContextEngineError {
    let ContextEngineError::CacheOpenFailed { reason, .. } = error else {
        return error.clone();
    };
    let Some(bytes) = reason
        .split_once("index_budget_exhausted:")
        .and_then(|(_, rest)| rest.trim().parse::<u64>().ok())
    else {
        return error.clone();
    };
    ContextEngineError::IndexBudgetExhausted {
        path: database.to_path_buf(),
        bytes,
        limit: super::MAX_INDEX_DB_BYTES,
    }
}

/// Turns the constraint failure `commit` raises for a lost race back into the
/// typed refusal, and leaves every other failure as it was.
fn supersession(
    database: &Path,
    generation: u64,
    error: &ContextEngineError,
) -> ContextEngineError {
    let ContextEngineError::CacheOpenFailed { reason, .. } = error else {
        return error.clone();
    };
    let Some(watermark) = reason
        .split_once("superseded:")
        .and_then(|(_, rest)| rest.trim().parse::<u64>().ok())
    else {
        return error.clone();
    };
    ContextEngineError::IndexBatchSuperseded {
        path: database.to_path_buf(),
        generation,
        watermark,
    }
}

/// The visible-file projection every read starts from.
///
/// `f.generation <= w.last_generation` is the whole of the visibility rule, and
/// it is spelled once here rather than in each query so that a new read cannot
/// forget it and return a batch that has not committed.
const FILE_SELECT: &str = "\
SELECT f.path, f.file_version_id, v.content_sha256, f.byte_size, f.mtime_ns, f.file_class, \
       f.symlink, f.boundary, f.unreadable, f.classify_version, f.generation, v.truncated, \
       v.chunking_version, v.language, v.parser_version \
FROM files f \
JOIN worktrees w ON w.worktree_id = f.worktree_id \
LEFT JOIN file_versions v ON v.file_version_id = f.file_version_id \
WHERE f.worktree_id = :worktree AND f.generation <= w.last_generation";

const CHUNK_SELECT: &str = "\
SELECT c.chunk_id, c.file_version_id, f.path, c.anchor, c.ordinal, c.start_byte, c.end_byte, \
       c.start_line, c.end_line, c.chunk_sha256, c.symbol_id, v.language, v.transcoded, \
       v.chunking_version \
FROM chunks c \
JOIN file_versions v ON v.file_version_id = c.file_version_id \
JOIN files f ON f.file_version_id = c.file_version_id \
JOIN worktrees w ON w.worktree_id = f.worktree_id \
WHERE f.worktree_id = :worktree AND f.generation <= w.last_generation";

const SYMBOL_SELECT: &str = "\
SELECT s.symbol_id, s.file_version_id, f.path, s.name, s.qualified_path, s.kind, \
       s.ordinal, s.start_byte, s.end_byte, s.start_line, s.end_line, s.parent_symbol_id, \
       s.is_test, s.name_is_lossy, v.parser_version, v.language, c.byte_size, \
       p.qualified_path, p.kind, p.ordinal, p.start_byte, p.end_byte \
FROM symbols s \
JOIN file_versions v ON v.file_version_id = s.file_version_id \
JOIN contents c ON c.content_sha256 = v.content_sha256 \
JOIN files f ON f.file_version_id = s.file_version_id \
JOIN worktrees w ON w.worktree_id = f.worktree_id \
LEFT JOIN symbols p \
  ON p.file_version_id = s.file_version_id AND p.symbol_id = s.parent_symbol_id \
WHERE f.worktree_id = :worktree AND f.generation <= w.last_generation";

/// One more row than the caller asked for, which is how truncation is detected.
///
/// `None` for a limit of zero: a caller asking for no rows gets none. Clamping
/// zero up to one would answer a question nobody asked and make a paging loop
/// whose budget reached zero run forever.
fn probe_limit(limit: usize) -> Option<i64> {
    let bounded = limit.min(MAX_READ_ROWS);
    if bounded == 0 {
        return None;
    }
    Some(i64::try_from(bounded.saturating_add(1)).unwrap_or(i64::MAX))
}

/// Trims the probe row off and says whether it was there.
fn page<T>(mut rows: Vec<T>, limit: usize) -> IndexedPage<T> {
    let bounded = limit.min(MAX_READ_ROWS);
    let more = rows.len() > bounded;
    rows.truncate(bounded);
    IndexedPage { rows, more }
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// The half-open blob range holding everything strictly beneath `prefix`.
///
/// `prefix || '/'` is the low bound and the same bytes with that separator
/// incremented is the high one, which works because `/` is `0x2f` and can never
/// be the largest byte. The prefix itself sits outside the range and is matched
/// on its own, so a directory recorded as a boundary is returned beside the
/// files under it rather than instead of them.
fn subtree_bounds(prefix: &RepoPath) -> (Vec<u8>, Vec<u8>) {
    let mut low = prefix.as_bytes().to_vec();
    low.push(b'/');
    let mut high = low.clone();
    if let Some(last) = high.last_mut() {
        *last = b'/' + 1;
    }
    (low, high)
}

/// The row bound one read is given, saturated into what SQLite can bind.
fn clamp_limit(limit: usize) -> i64 {
    i64::try_from(limit.clamp(1, MAX_READ_ROWS)).unwrap_or(i64::MAX)
}

fn write_content(
    transaction: &rusqlite::Transaction<'_>,
    file: &PendingFile,
    content: &PendingContent,
) -> Result<(), rusqlite::Error> {
    transaction.execute(
        "INSERT INTO contents (content_sha256, byte_size) VALUES (:digest, :size) \
         ON CONFLICT(content_sha256) DO UPDATE SET byte_size = excluded.byte_size",
        named_params! {
            ":digest": content.content_sha256.as_str(),
            ":size": i64::try_from(file.byte_size).unwrap_or(i64::MAX),
        },
    )?;
    // `ON CONFLICT DO UPDATE` rather than `INSERT OR REPLACE`: replacing the
    // row would delete it first, and the cascade would take this version's
    // symbols with it — including ones an earlier flush of this same batch
    // wrote.
    transaction.execute(
        "INSERT INTO file_versions \
            (file_version_id, content_sha256, path, language, transcoded, truncated, \
             chunking_version, parser_version) \
         VALUES (:id, :digest, :path, :language, :transcoded, :truncated, :chunking, NULL) \
         ON CONFLICT(file_version_id) DO UPDATE SET \
            language = excluded.language, \
            transcoded = excluded.transcoded, \
            truncated = excluded.truncated, \
            chunking_version = excluded.chunking_version",
        named_params! {
            ":id": content.file_version.to_string(),
            ":digest": content.content_sha256.as_str(),
            ":path": file.path.as_bytes(),
            ":language": content.language.as_deref(),
            ":transcoded": i64::from(content.transcoded),
            ":truncated": i64::from(content.truncated),
            ":chunking": content.chunking_version.as_deref(),
        },
    )?;
    // A file version's chunk set is replaced whole. A chunk that survived an
    // edit keeps its identity, so re-inserting it is a no-op in everything but
    // the write; one that did not has to go, and there is no partial answer.
    transaction.execute(
        "DELETE FROM chunks WHERE file_version_id = :id",
        named_params! { ":id": content.file_version.to_string() },
    )?;
    for chunk in &content.chunks {
        transaction.execute(
            "INSERT INTO chunks \
                (file_version_id, chunk_id, anchor, ordinal, start_byte, end_byte, start_line, \
                 end_line, chunk_sha256, symbol_id) \
             VALUES (:version, :chunk, :anchor, :ordinal, :start, :end, :first_line, :last_line, \
                 :digest, :symbol) \
             ON CONFLICT(file_version_id, chunk_id) DO NOTHING",
            named_params! {
                ":version": content.file_version.to_string(),
                ":chunk": chunk.id.to_string(),
                ":anchor": chunk.anchor,
                ":ordinal": i64::from(chunk.ordinal),
                ":start": i64::try_from(chunk.range.start).unwrap_or(i64::MAX),
                ":end": i64::try_from(chunk.range.end).unwrap_or(i64::MAX),
                ":first_line": chunk.range.first_line.map(i64::from),
                ":last_line": chunk.range.last_line.map(i64::from),
                ":digest": chunk.chunk_sha256.as_str(),
                ":symbol": chunk.symbol.as_ref().map(ToString::to_string),
            },
        )?;
    }
    Ok(())
}

/// Stages one file row where no query can reach it.
fn stage_file(
    transaction: &rusqlite::Transaction<'_>,
    worktree: &str,
    generation: i64,
    file: &PendingFile,
) -> Result<(), rusqlite::Error> {
    let version = match &file.content {
        Derivation::Read(content) => Some(content.file_version.to_string()),
        Derivation::None | Derivation::Unavailable | Derivation::Kept => None,
    };
    transaction.execute(
        "INSERT INTO pending_files \
            (worktree_id, generation, path, file_version_id, keep_version, removed, byte_size, \
             mtime_ns, file_class, symlink, boundary, unreadable, classify_version) \
         VALUES (:worktree, :generation, :path, :version, :keep, 0, :size, :mtime, :class, \
             :symlink, :boundary, :unreadable, :classify) \
         ON CONFLICT(worktree_id, generation, path) DO UPDATE SET \
            file_version_id = excluded.file_version_id, \
            keep_version = excluded.keep_version, \
            removed = 0, \
            byte_size = excluded.byte_size, \
            mtime_ns = excluded.mtime_ns, \
            file_class = excluded.file_class, \
            symlink = excluded.symlink, \
            boundary = excluded.boundary, \
            unreadable = excluded.unreadable, \
            classify_version = excluded.classify_version",
        named_params! {
            ":worktree": worktree,
            ":generation": generation,
            ":path": file.path.as_bytes(),
            ":version": version,
            ":keep": i64::from(matches!(
                file.content,
                Derivation::Unavailable | Derivation::Kept
            )),
            ":size": i64::try_from(file.byte_size).unwrap_or(i64::MAX),
            ":mtime": file.mtime_ns,
            ":class": file.class.as_str(),
            ":symlink": i64::from(file.symlink),
            ":boundary": file.boundary.map(Boundary::as_str),
            ":unreadable": i64::from(file.unreadable),
            ":classify": i64::from(file.classify_version),
        },
    )?;
    Ok(())
}

/// Stages one path's removal, which the commit applies with everything else.
fn stage_removal(
    transaction: &rusqlite::Transaction<'_>,
    worktree: &str,
    generation: i64,
    path: &RepoPath,
) -> Result<(), rusqlite::Error> {
    transaction.execute(
        "INSERT INTO pending_files \
            (worktree_id, generation, path, file_version_id, keep_version, removed, byte_size, \
             mtime_ns, file_class, symlink, boundary, unreadable, classify_version) \
         VALUES (:worktree, :generation, :path, NULL, 0, 1, 0, NULL, 'unknown_text', 0, NULL, 0, 0) \
         ON CONFLICT(worktree_id, generation, path) DO UPDATE SET \
            removed = 1, file_version_id = NULL, keep_version = 0",
        named_params! {
            ":worktree": worktree,
            ":generation": generation,
            ":path": path.as_bytes(),
        },
    )?;
    Ok(())
}

/// Copies this batch's staged rows into `files`.
///
/// `keep` selects the half of the batch whose content could not be read: those
/// rows refresh the file's own metadata and leave `file_version_id` exactly as
/// it was, so an unreadable moment does not unlink a file from the derivation
/// the last successful pass stored.
fn apply_staged(
    transaction: &rusqlite::Transaction<'_>,
    worktree: &str,
    generation: i64,
    keep: bool,
) -> Result<(), rusqlite::Error> {
    let version_clause = if keep {
        "files.file_version_id"
    } else {
        "excluded.file_version_id"
    };
    transaction.execute(
        &format!(
            "INSERT INTO files \
                (worktree_id, path, file_version_id, byte_size, mtime_ns, file_class, symlink, \
                 boundary, unreadable, classify_version, generation) \
             SELECT worktree_id, path, file_version_id, byte_size, mtime_ns, file_class, symlink, \
                 boundary, unreadable, classify_version, :generation \
             FROM pending_files \
             WHERE worktree_id = :worktree AND generation = :generation AND removed = 0 \
               AND keep_version = :keep AND true \
             ON CONFLICT(worktree_id, path) DO UPDATE SET \
                file_version_id = {version_clause}, \
                byte_size = excluded.byte_size, \
                mtime_ns = excluded.mtime_ns, \
                file_class = excluded.file_class, \
                symlink = excluded.symlink, \
                boundary = excluded.boundary, \
                unreadable = excluded.unreadable, \
                classify_version = excluded.classify_version, \
                generation = excluded.generation"
        ),
        named_params! {
            ":worktree": worktree,
            ":generation": generation,
            ":keep": i64::from(keep),
        },
    )?;
    Ok(())
}

/// Adds the derived rows of every dead staged batch to what this one collects.
///
/// "Dead" is decided by the same forward-only rule the commit enforces: a batch
/// holding a generation below this one can no longer publish, so nothing will
/// ever point at what it staged.
fn collect_staged_below(
    transaction: &rusqlite::Transaction<'_>,
    worktree: &str,
    generation: i64,
    displaced: &mut BTreeSet<String>,
) -> Result<(), rusqlite::Error> {
    let mut statement = transaction.prepare(
        "SELECT DISTINCT file_version_id FROM pending_files \
         WHERE worktree_id = :worktree AND generation < :generation \
           AND file_version_id IS NOT NULL",
    )?;
    let rows = statement.query_map(
        named_params! { ":worktree": worktree, ":generation": generation },
        |row| row.get::<_, String>(0),
    )?;
    for row in rows {
        displaced.insert(row?);
    }
    Ok(())
}

/// The file versions this batch's staged rows are about to displace.
///
/// One query for the whole batch rather than one per file, and read *before*
/// the staged rows are applied — afterwards the answer would be the new
/// versions rather than the ones being replaced.
fn displaced_by_batch(
    transaction: &rusqlite::Transaction<'_>,
    worktree: &str,
    generation: i64,
    mut displaced: BTreeSet<String>,
) -> Result<BTreeSet<String>, rusqlite::Error> {
    let mut statement = transaction.prepare(
        "SELECT DISTINCT f.file_version_id FROM files f \
         JOIN pending_files p \
           ON p.worktree_id = f.worktree_id AND p.path = f.path \
         WHERE f.worktree_id = :worktree AND p.generation = :generation \
           AND f.file_version_id IS NOT NULL \
           AND (p.removed = 1 OR p.keep_version = 0)",
    )?;
    let rows = statement.query_map(
        named_params! { ":worktree": worktree, ":generation": generation },
        |row| row.get::<_, String>(0),
    )?;
    for row in rows {
        displaced.insert(row?);
    }
    Ok(displaced)
}

fn write_symbols(
    transaction: &rusqlite::Transaction<'_>,
    attached: &PendingSymbols,
) -> Result<(), rusqlite::Error> {
    let version = attached.file_version.to_string();
    let (path, language, byte_size): (Vec<u8>, Option<String>, i64) = transaction.query_row(
        "SELECT v.path, v.language, c.byte_size FROM file_versions v \
         JOIN contents c ON c.content_sha256 = v.content_sha256 \
         WHERE v.file_version_id = :id",
        named_params! { ":id": &version },
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let path = RepoPath::from_bytes(path);
    let language = language
        .map(Language::new)
        .transpose()
        .map_err(|_| invalid_symbol(0))?;
    let byte_size = strict_u64(byte_size, 2, "file_byte_size")?;
    validate_pending_symbols(attached, &path, language.as_ref(), byte_size)?;
    transaction.execute(
        "UPDATE file_versions SET parser_version = :parser WHERE file_version_id = :id",
        named_params! { ":parser": &attached.parser_version, ":id": &version },
    )?;
    transaction.execute(
        "DELETE FROM symbols WHERE file_version_id = :id",
        named_params! { ":id": &version },
    )?;
    transaction.execute(
        "DELETE FROM symbol_references WHERE file_version_id = :id",
        named_params! { ":id": &version },
    )?;
    transaction.execute(
        "DELETE FROM parse_health WHERE file_version_id = :id",
        named_params! { ":id": &version },
    )?;
    {
        let mut insert_symbol = transaction.prepare(
            "INSERT INTO symbols \
                (file_version_id, symbol_id, name, qualified_path, kind, ordinal, start_byte, \
                 end_byte, start_line, end_line, parent_symbol_id, is_test, \
                 name_is_lossy) \
             VALUES (:version, :symbol, :name, :qualified, :kind, :ordinal, :start, :end, \
                 :first_line, :last_line, :parent, :is_test, :lossy)",
        )?;
        for symbol in &attached.symbols {
            insert_symbol.execute(named_params! {
                ":version": &version,
                ":symbol": symbol.id.to_string(),
                ":name": symbol.name,
                ":qualified": symbol.qualified_path,
                ":kind": symbol.kind,
                ":ordinal": i64::from(symbol.ordinal),
                ":start": i64::try_from(symbol.byte_range.start)
                    .map_err(|_| invalid_integer(6, "symbol_start_byte"))?,
                ":end": i64::try_from(symbol.byte_range.end)
                    .map_err(|_| invalid_integer(7, "symbol_end_byte"))?,
                ":first_line": symbol.byte_range.first_line.map(i64::from),
                ":last_line": symbol.byte_range.last_line.map(i64::from),
                ":parent": symbol.parent.as_ref().map(ToString::to_string),
                ":is_test": i64::from(symbol.is_test),
                ":lossy": i64::from(symbol.name_is_lossy),
            })?;
        }
    }
    {
        let mut insert_reference = transaction.prepare(
            "INSERT INTO symbol_references \
                (file_version_id, ordinal, name, start_byte, end_byte, start_line, end_line, \
                 name_is_lossy) \
             VALUES (:version, :ordinal, :name, :start, :end, :first_line, :last_line, :lossy)",
        )?;
        for (ordinal, reference) in attached.references.iter().enumerate() {
            insert_reference.execute(named_params! {
                ":version": &version,
                ":ordinal": i64::try_from(ordinal)
                    .map_err(|_| invalid_integer(1, "reference_ordinal"))?,
                ":name": reference.name,
                ":start": i64::try_from(reference.byte_range.start)
                    .map_err(|_| invalid_integer(3, "reference_start_byte"))?,
                ":end": i64::try_from(reference.byte_range.end)
                    .map_err(|_| invalid_integer(4, "reference_end_byte"))?,
                ":first_line": reference.byte_range.first_line.map(i64::from),
                ":last_line": reference.byte_range.last_line.map(i64::from),
                ":lossy": i64::from(reference.name_is_lossy),
            })?;
        }
    }
    let (status, reason, error_ranges) = encode_parse_health(&attached.health)?;
    transaction.execute(
        "INSERT INTO parse_health (file_version_id, status, reason, error_ranges_json) \
         VALUES (:version, :status, :reason, :ranges)",
        named_params! {
            ":version": &version,
            ":status": status,
            ":reason": reason,
            ":ranges": error_ranges,
        },
    )?;
    Ok(())
}

fn validate_pending_symbols(
    attached: &PendingSymbols,
    path: &RepoPath,
    language: Option<&Language>,
    byte_size: u64,
) -> Result<(), rusqlite::Error> {
    let by_id = attached
        .symbols
        .iter()
        .map(|symbol| (symbol.id.clone(), symbol))
        .collect::<BTreeMap<_, _>>();
    if by_id.len() != attached.symbols.len() {
        return Err(invalid_symbol(0));
    }
    for symbol in &attached.symbols {
        let language = language.ok_or_else(|| invalid_symbol(0))?;
        validate_bounded_range(&symbol.byte_range, byte_size, 6, "symbol_range")?;
        let kind = SymbolKind::parse(&symbol.kind).ok_or_else(|| invalid_symbol(4))?;
        let identity_name = if symbol.ordinal == 0 {
            symbol.qualified_path.clone()
        } else {
            format!("{}#duplicate:{}", symbol.qualified_path, symbol.ordinal)
        };
        let expected = SymbolId::derive(path, language.as_str(), &identity_name, kind.as_str());
        if symbol.id != expected {
            return Err(invalid_symbol(0));
        }
        if let Some(parent_id) = symbol.parent.as_ref() {
            let parent = by_id.get(parent_id).ok_or_else(|| invalid_symbol(10))?;
            if parent.byte_range.start > symbol.byte_range.start
                || symbol
                    .qualified_path
                    .strip_prefix(&parent.qualified_path)
                    .and_then(|suffix| suffix.strip_prefix("::"))
                    .is_none_or(str::is_empty)
            {
                return Err(invalid_symbol(10));
            }
        }
    }
    for reference in &attached.references {
        validate_bounded_range(&reference.byte_range, byte_size, 3, "reference_range")?;
    }
    if let ParseHealth::Partial { error_ranges } = &attached.health {
        for range in error_ranges {
            validate_bounded_range(range, byte_size, 6, "parse_error_range")?;
        }
    }
    Ok(())
}

fn encode_parse_health(
    health: &ParseHealth,
) -> Result<(&'static str, Option<String>, String), rusqlite::Error> {
    let (status, reason, ranges) = match health {
        ParseHealth::Complete => ("complete", None, &[][..]),
        ParseHealth::Partial { error_ranges } => ("partial", None, error_ranges.as_slice()),
        ParseHealth::Failed { reason } => ("failed", Some(reason.clone()), &[][..]),
        ParseHealth::Skipped { reason } => ("skipped", Some(reason.as_str().to_owned()), &[][..]),
    };
    let ranges = serde_json::to_string(ranges)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    Ok((status, reason, ranges))
}

/// Drops every content-addressed row nothing still points at.
///
/// "Nothing" includes another batch's staged rows. A cold build running in
/// another process has file versions written and no `files` row pointing at
/// them yet — collecting those would delete the work it is part-way through,
/// and the foreign key would refuse the whole commit rather than let it.
fn collect_all(transaction: &rusqlite::Transaction<'_>) -> Result<u64, rusqlite::Error> {
    let versions = transaction.execute(
        "DELETE FROM file_versions WHERE file_version_id NOT IN \
            (SELECT file_version_id FROM files WHERE file_version_id IS NOT NULL) \
         AND file_version_id NOT IN \
            (SELECT file_version_id FROM pending_files WHERE file_version_id IS NOT NULL)",
        [],
    )?;
    let contents = transaction.execute(
        "DELETE FROM contents WHERE content_sha256 NOT IN \
            (SELECT content_sha256 FROM file_versions)",
        [],
    )?;
    Ok((versions + contents) as u64)
}

/// Drops exactly the versions this batch displaced, when nothing else holds them.
///
/// The reason a targeted batch does not run [`collect_all`]: that statement
/// reads every file row in the repository to decide about one file, and the
/// contract a single-file update makes is that it touches one file's rows.
fn collect_displaced(
    transaction: &rusqlite::Transaction<'_>,
    displaced: &BTreeSet<String>,
) -> Result<u64, rusqlite::Error> {
    let mut collected = 0_u64;
    let mut orphaned = BTreeSet::new();
    for version in displaced {
        let digest: Option<String> = transaction
            .query_row(
                "SELECT content_sha256 FROM file_versions WHERE file_version_id = :id",
                named_params! { ":id": version },
                |row| row.get(0),
            )
            .optional()?;
        let removed = transaction.execute(
            "DELETE FROM file_versions WHERE file_version_id = :id \
             AND NOT EXISTS (SELECT 1 FROM files WHERE file_version_id = :id) \
             AND NOT EXISTS (SELECT 1 FROM pending_files WHERE file_version_id = :id)",
            named_params! { ":id": version },
        )?;
        collected += removed as u64;
        if removed > 0
            && let Some(digest) = digest
        {
            orphaned.insert(digest);
        }
    }
    // By digest rather than by anti-join. The whole reason a targeted batch does
    // not run `collect_all` is that scanning every content-addressed row to
    // decide about one file is what it was written to avoid, and a `NOT IN` over
    // `file_versions` here would put that scan straight back.
    for digest in orphaned {
        collected += transaction.execute(
            "DELETE FROM contents WHERE content_sha256 = :digest \
             AND NOT EXISTS (SELECT 1 FROM file_versions WHERE content_sha256 = :digest)",
            named_params! { ":digest": digest },
        )? as u64;
    }
    Ok(collected)
}

fn pending_chunk(chunk: &ChunkRecord) -> PendingChunk {
    PendingChunk {
        id: chunk.id.clone(),
        // Serialized rather than re-derived on read, because the anchor
        // vocabulary is [#113]'s and this table must be able to hold one it
        // does not understand the day a variant is added.
        anchor: serde_json::to_string(&chunk.anchor).unwrap_or_else(|_| "null".to_owned()),
        ordinal: chunk.ordinal,
        range: chunk.byte_range,
        chunk_sha256: chunk.chunk_sha256.clone(),
        symbol: chunk.symbol.clone(),
    }
}

/// Rebuilds one file row, refusing a class this build does not define.
///
/// The refusal is deliberate and matches [`FileClass`]'s own deserialization: a
/// spelling this build does not know means a newer build wrote the row, and
/// coercing it to something benign is how a `secret_sensitive` file stops being
/// excluded without anyone being told.
fn read_file(row: &rusqlite::Row<'_>) -> Result<IndexedFile, rusqlite::Error> {
    let path: Vec<u8> = row.get(0)?;
    let file_version: Option<String> = row.get(1)?;
    let content: Option<String> = row.get(2)?;
    let class: String = row.get(5)?;
    let boundary: Option<String> = row.get(7)?;
    let generation: i64 = row.get(10)?;
    Ok(IndexedFile {
        path: RepoPath::from_bytes(path),
        file_version: file_version.map(|value| parse_id(1, &value)).transpose()?,
        content_sha256: content.map(|value| parse_digest(2, &value)).transpose()?,
        truncated: row.get::<_, Option<i64>>(11)?.unwrap_or(0) != 0,
        byte_size: row.get::<_, i64>(3).map(cast_u64)?,
        mtime_ns: row.get(4)?,
        class: parse_class(5, &class)?,
        symlink: row.get::<_, i64>(6)? != 0,
        boundary: boundary
            .map(|value| parse_boundary(7, &value))
            .transpose()?,
        unreadable: row.get::<_, i64>(8)? != 0,
        classify_version: u32::try_from(row.get::<_, i64>(9)?).map_err(|_| {
            rusqlite::Error::InvalidColumnType(
                9,
                "classify_version".to_owned(),
                rusqlite::types::Type::Integer,
            )
        })?,
        chunking_version: row.get(12)?,
        language: row
            .get::<_, Option<String>>(13)?
            .map(|value| {
                Language::new(value).map_err(|_| {
                    rusqlite::Error::InvalidColumnType(
                        13,
                        "language".to_owned(),
                        rusqlite::types::Type::Text,
                    )
                })
            })
            .transpose()?,
        parser_version: row.get(14)?,
        generation: cast_u64(generation),
    })
}

fn read_chunk(row: &rusqlite::Row<'_>) -> Result<IndexedChunk, rusqlite::Error> {
    let id: String = row.get(0)?;
    let version: String = row.get(1)?;
    let path: Vec<u8> = row.get(2)?;
    let anchor: String = row.get(3)?;
    let digest: String = row.get(9)?;
    let symbol: Option<String> = row.get(10)?;
    let language: Option<String> = row.get(11)?;
    let range = ByteRange {
        start: cast_u64(row.get::<_, i64>(5)?),
        end: cast_u64(row.get::<_, i64>(6)?),
        first_line: row.get::<_, Option<i64>>(7)?.map(cast_u32),
        last_line: row.get::<_, Option<i64>>(8)?.map(cast_u32),
    };
    Ok(IndexedChunk {
        id: parse_id(0, &id)?,
        file_version: parse_id(1, &version)?,
        path: RepoPath::from_bytes(path),
        anchor: serde_json::from_str(&anchor).map_err(|_| {
            rusqlite::Error::InvalidColumnType(3, "anchor".to_owned(), rusqlite::types::Type::Text)
        })?,
        ordinal: cast_u32(row.get::<_, i64>(4)?),
        byte_range: range,
        chunk_sha256: parse_digest(9, &digest)?,
        symbol: symbol.map(|value| parse_id(10, &value)).transpose()?,
        language: language
            .map(|value| {
                Language::new(value).map_err(|_| {
                    rusqlite::Error::InvalidColumnType(
                        11,
                        "language".to_owned(),
                        rusqlite::types::Type::Text,
                    )
                })
            })
            .transpose()?,
        transcoded: row.get::<_, i64>(12)? != 0,
        chunking_version: row.get::<_, Option<String>>(13)?.unwrap_or_default(),
    })
}

fn read_symbol(row: &rusqlite::Row<'_>) -> Result<IndexedSymbol, rusqlite::Error> {
    let encoded_id: String = row.get(0)?;
    let version: String = row.get(1)?;
    let path: Vec<u8> = row.get(2)?;
    let path = RepoPath::from_bytes(path);
    let name: String = row.get(3)?;
    let qualified_path: String = row.get(4)?;
    let kind_text: String = row.get(5)?;
    let kind = SymbolKind::parse(&kind_text).ok_or_else(|| invalid_symbol(5))?;
    let ordinal = strict_u32(row.get(6)?, 6, "symbol_ordinal")?;
    let byte_size = strict_u64(row.get(16)?, 16, "file_byte_size")?;
    let byte_range = read_bounded_range(row, 7, 8, 9, 10, byte_size, "symbol_range")?;
    let language_text: String = row.get(15).map_err(|_| invalid_symbol(15))?;
    let language = Language::new(language_text).map_err(|_| invalid_symbol(15))?;
    let id: SymbolId = parse_id(0, &encoded_id)?;
    let identity_name = if ordinal == 0 {
        qualified_path.clone()
    } else {
        format!("{qualified_path}#duplicate:{ordinal}")
    };
    let expected = SymbolId::derive(&path, language.as_str(), &identity_name, kind.as_str());
    if id != expected {
        return Err(invalid_symbol(0));
    }

    let parent = row
        .get::<_, Option<String>>(11)?
        .map(|value| parse_id(11, &value))
        .transpose()?;
    if let Some(parent_id) = parent.as_ref() {
        let parent_qualified: String = row.get(17).map_err(|_| invalid_symbol(11))?;
        let parent_kind_text: String = row.get(18).map_err(|_| invalid_symbol(11))?;
        let parent_kind = SymbolKind::parse(&parent_kind_text).ok_or_else(|| invalid_symbol(11))?;
        let parent_ordinal = strict_u32(
            row.get::<_, Option<i64>>(19)?
                .ok_or_else(|| invalid_symbol(11))?,
            19,
            "parent_ordinal",
        )?;
        let parent_identity = if parent_ordinal == 0 {
            parent_qualified.clone()
        } else {
            format!("{parent_qualified}#duplicate:{parent_ordinal}")
        };
        let expected_parent = SymbolId::derive(
            &path,
            language.as_str(),
            &parent_identity,
            parent_kind.as_str(),
        );
        let parent_start = strict_u64(
            row.get::<_, Option<i64>>(20)?
                .ok_or_else(|| invalid_symbol(11))?,
            20,
            "parent_start_byte",
        )?;
        let parent_end = strict_u64(
            row.get::<_, Option<i64>>(21)?
                .ok_or_else(|| invalid_symbol(11))?,
            21,
            "parent_end_byte",
        )?;
        if parent_id != &expected_parent
            || parent_start > byte_range.start
            || parent_start > parent_end
            || parent_end > byte_size
            || qualified_path
                .strip_prefix(&parent_qualified)
                .and_then(|suffix| suffix.strip_prefix("::"))
                .is_none_or(str::is_empty)
        {
            return Err(invalid_symbol(11));
        }
    }

    Ok(IndexedSymbol {
        id,
        file_version: parse_id(1, &version)?,
        path,
        name,
        qualified_path,
        kind,
        ordinal,
        byte_range,
        parent,
        is_test: strict_bool(row.get(12)?, 12, "symbol_is_test")?,
        name_is_lossy: strict_bool(row.get(13)?, 13, "symbol_name_is_lossy")?,
        parser_version: row.get::<_, Option<String>>(14)?.unwrap_or_default(),
    })
}

fn read_parse_health(row: &rusqlite::Row<'_>) -> Result<IndexedParseHealth, rusqlite::Error> {
    let version: String = row.get(0)?;
    let path: Vec<u8> = row.get(1)?;
    let language = row
        .get::<_, Option<String>>(2)?
        .map(|value| {
            Language::new(value).map_err(|_| {
                rusqlite::Error::InvalidColumnType(
                    2,
                    "language".to_owned(),
                    rusqlite::types::Type::Text,
                )
            })
        })
        .transpose()?;
    let status: String = row.get(4)?;
    let reason: Option<String> = row.get(5)?;
    let encoded_ranges: String = row.get(6)?;
    let ranges = serde_json::from_str::<Vec<ByteRange>>(&encoded_ranges).map_err(|_| {
        rusqlite::Error::InvalidColumnType(
            6,
            "error_ranges_json".to_owned(),
            rusqlite::types::Type::Text,
        )
    })?;
    let byte_size = strict_u64(row.get(7)?, 7, "file_byte_size")?;
    for range in &ranges {
        validate_bounded_range(range, byte_size, 6, "parse_error_range")?;
    }
    let health = match status.as_str() {
        "complete" if reason.is_none() && ranges.is_empty() => ParseHealth::Complete,
        "partial" if reason.is_none() && !ranges.is_empty() => ParseHealth::Partial {
            error_ranges: ranges,
        },
        "failed" if ranges.is_empty() => ParseHealth::Failed {
            reason: reason.ok_or_else(|| invalid_health(5))?,
        },
        "skipped" if ranges.is_empty() => ParseHealth::Skipped {
            reason: match reason.as_deref() {
                Some("unsupported_language") => ExtractionSkipReason::UnsupportedLanguage,
                Some("unknown_language") => ExtractionSkipReason::UnknownLanguage,
                Some("transcoded_input") => ExtractionSkipReason::TranscodedInput,
                _ => return Err(invalid_health(5)),
            },
        },
        _ => return Err(invalid_health(4)),
    };
    Ok(IndexedParseHealth {
        file_version: parse_id(0, &version)?,
        path: RepoPath::from_bytes(path),
        language,
        grammar_version: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
        health,
    })
}

fn invalid_health(column: usize) -> rusqlite::Error {
    rusqlite::Error::InvalidColumnType(
        column,
        "parse_health".to_owned(),
        rusqlite::types::Type::Text,
    )
}

fn read_symbol_reference(
    row: &rusqlite::Row<'_>,
) -> Result<IndexedSymbolReference, rusqlite::Error> {
    let version: String = row.get(0)?;
    let path: Vec<u8> = row.get(1)?;
    let byte_size = strict_u64(row.get(8)?, 8, "file_byte_size")?;
    Ok(IndexedSymbolReference {
        file_version: parse_id(0, &version)?,
        path: RepoPath::from_bytes(path),
        name: row.get(2)?,
        byte_range: read_bounded_range(row, 3, 4, 5, 6, byte_size, "reference_range")?,
        name_is_lossy: strict_bool(row.get(7)?, 7, "reference_name_is_lossy")?,
    })
}

fn read_bounded_range(
    row: &rusqlite::Row<'_>,
    start_column: usize,
    end_column: usize,
    first_line_column: usize,
    last_line_column: usize,
    byte_size: u64,
    name: &str,
) -> Result<ByteRange, rusqlite::Error> {
    let range = ByteRange {
        start: strict_u64(row.get(start_column)?, start_column, name)?,
        end: strict_u64(row.get(end_column)?, end_column, name)?,
        first_line: row
            .get::<_, Option<i64>>(first_line_column)?
            .map(|value| strict_u32(value, first_line_column, name))
            .transpose()?,
        last_line: row
            .get::<_, Option<i64>>(last_line_column)?
            .map(|value| strict_u32(value, last_line_column, name))
            .transpose()?,
    };
    validate_bounded_range(&range, byte_size, start_column, name)?;
    Ok(range)
}

fn validate_bounded_range(
    range: &ByteRange,
    byte_size: u64,
    column: usize,
    name: &str,
) -> Result<(), rusqlite::Error> {
    if range.start > range.end
        || range.end > byte_size
        || range.first_line == Some(0)
        || range.last_line == Some(0)
        || matches!((range.first_line, range.last_line), (Some(first), Some(last)) if last < first)
    {
        return Err(invalid_integer(column, name));
    }
    Ok(())
}

fn strict_u64(value: i64, column: usize, name: &str) -> Result<u64, rusqlite::Error> {
    u64::try_from(value).map_err(|_| invalid_integer(column, name))
}

fn strict_u32(value: i64, column: usize, name: &str) -> Result<u32, rusqlite::Error> {
    u32::try_from(value).map_err(|_| invalid_integer(column, name))
}

fn strict_bool(value: i64, column: usize, name: &str) -> Result<bool, rusqlite::Error> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(invalid_integer(column, name)),
    }
}

fn invalid_integer(column: usize, name: &str) -> rusqlite::Error {
    rusqlite::Error::InvalidColumnType(column, name.to_owned(), rusqlite::types::Type::Integer)
}

fn invalid_symbol(column: usize) -> rusqlite::Error {
    rusqlite::Error::InvalidColumnType(column, "symbol".to_owned(), rusqlite::types::Type::Text)
}

fn parse_id<T: std::str::FromStr>(column: usize, value: &str) -> Result<T, rusqlite::Error> {
    value.parse().map_err(|_| {
        rusqlite::Error::InvalidColumnType(
            column,
            "identifier".to_owned(),
            rusqlite::types::Type::Text,
        )
    })
}

fn parse_digest(column: usize, value: &str) -> Result<Sha256Hex, rusqlite::Error> {
    value.parse().map_err(|_| {
        rusqlite::Error::InvalidColumnType(column, "digest".to_owned(), rusqlite::types::Type::Text)
    })
}

fn parse_class(column: usize, value: &str) -> Result<FileClass, rusqlite::Error> {
    FileClass::parse(value).ok_or_else(|| {
        rusqlite::Error::InvalidColumnType(
            column,
            "file_class".to_owned(),
            rusqlite::types::Type::Text,
        )
    })
}

fn parse_boundary(column: usize, value: &str) -> Result<Boundary, rusqlite::Error> {
    Boundary::parse(value).ok_or_else(|| {
        rusqlite::Error::InvalidColumnType(
            column,
            "boundary".to_owned(),
            rusqlite::types::Type::Text,
        )
    })
}

const fn cast_u64(value: i64) -> u64 {
    if value < 0 { 0 } else { value as u64 }
}

fn cast_u32(value: i64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// The cache root a repository key resolves to beneath `data_dir`.
///
/// One composition point, so a caller cannot assemble the path a different way
/// and address a directory the eviction sweep does not know about.
#[must_use]
pub fn cache_root(data_dir: &Path, repository_key: &str) -> PathBuf {
    data_dir
        .join(harkness_core::CONTEXT_DIRECTORY)
        .join(repository_key)
}
