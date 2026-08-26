//! Keeping the index current without rebuilding it, and deciding what is true.
//!
//! [`ContextEngine::reindex`](crate::ContextEngine::reindex) walks a whole
//! worktree and writes everything it finds. That is right once. Every time
//! afterwards, one file has changed and the other hundred thousand have not, so
//! the question is not "what does this repository contain" but "what has moved
//! since the index was written" — and answering it by walking, reading and
//! chunking the whole tree makes every edit cost what the first one did.
//!
//! # Hints are not truth
//!
//! A filesystem watcher ([`watch`](crate::watch)) says where to look first.
//! It is never what decides. Every backend drops events under load, coalesces
//! distinct changes into one, reports paths that did not change, and sees
//! nothing at all while Harkness is not running — so an index built on the
//! belief that events are complete is stale in ways that only appear after a
//! restart or under a build. The split this module draws is therefore total:
//!
//! - A **hint** says *this path is worth examining*. It can be wrong in both
//!   directions and nothing depends on it being right.
//! - **Truth** is the filesystem compared against the stored rows. A path whose
//!   size and modification time match its row is unchanged; a path whose row is
//!   gone is new; a row whose path the walk did not record is removed. That
//!   comparison is what writes rows, and it produces the same answer whether it
//!   was reached through a watcher, through a startup sweep, or through a
//!   caller asking for one.
//!
//! The asymmetry between the two is deliberate. A **hinted** path is hashed
//! even when its metadata matches its row, because a hint is cheap suspicion
//! and a coarse modification-time granularity is a real filesystem — a
//! one-second `mtime` on a file rewritten twice in that second matches and
//! lies. An **unhinted** path met during a sweep is metadata-compared only,
//! because hashing every file of a repository to discover that nothing changed
//! is the full rebuild this module exists to avoid.
//!
//! # A reconcile always commits as targeted
//!
//! Even a [`Full`](ReconcileScope::Full) one, and this is the rule to read
//! before changing anything here. A [`BatchScope::Full`] batch deletes every row
//! it did not confirm, and the whole point of reconciliation is that it does not
//! confirm the rows that did not change — so committing one as full would delete
//! the entire index every time a single file was edited. Removals are decided
//! instead by a merge of two sorted sequences: the paths the scoped walk
//! recorded and the rows the cache holds in the same scope. A row with no path
//! beside it is a removal, and it is named rather than swept.
//!
//! That merge is also why a **truncated walk removes nothing**. An inventory
//! stopped by its file or time budget did not see the whole scope, so the rows
//! it did not reach are rows about files that still exist.
//!
//! # Scopes
//!
//! [`ReconcileScope`] is the primitive everything above this speaks in, and it
//! has exactly three shapes because there are exactly three things a caller can
//! know: a list of paths, one subtree, or nothing in particular.
//! [`Subtree`](ReconcileScope::Subtree) is the one [#118]'s package scoping and
//! every later per-directory operation build on; nothing here knows what a
//! package is.
//!
//! A scope only ever widens. A path list past [`MAX_PATHS_PER_RECONCILE`]
//! becomes the subtree that contains all of it, a re-created checkout ([#63])
//! becomes a full pass, and the report names what it became — narrowing would
//! mean an update that silently covered less than it was asked to.
//!
//! [#63]: https://github.com/fullstacktaiye/harkness/issues/63
//! [#118]: https://github.com/fullstacktaiye/harkness/issues/118

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

use harkness_git::Cancellation;

use crate::chunk::{ChunkError, FileVersion};
use crate::error::ContextEngineError;
use crate::ids::SnapshotId;
use crate::index::{BatchScope, IndexBatch, IndexCache, IndexedFile, WorktreeKey};
use crate::inventory::{FileInventory, InventoryBuilder, InventoryEntry, InventoryPolicy};
use crate::path::RepoPath;
use crate::symbol_pipeline::{chunk_with_symbol_outline, extract_file_symbols};
use crate::symbols::SymbolSource;

/// Most paths one reconcile is asked about before the scope widens.
///
/// A path list is carried, sorted, merged against the index and walked, so its
/// size is paid for several times over. Past this it is cheaper — and bounded,
/// which matters more — to reconcile the subtree that contains all of it and
/// let the metadata comparison decide which of its files moved.
pub const MAX_PATHS_PER_RECONCILE: usize = 10_000;

/// How many index rows one page of the merge holds.
///
/// Small enough that a whole-repository reconcile never materializes the index,
/// large enough that a hundred-thousand-file sweep is not a hundred thousand
/// queries. Nothing depends on the value: the merge is correct at any page size
/// because both sides are ordered by the same path bytes.
const MERGE_PAGE_ROWS: usize = 4_096;

/// How many times a file that moved under the reader is read again.
///
/// An editor writing a file while the reconciler reads it is ordinary and
/// usually over within microseconds; a file being appended to continuously is
/// not, and no number of retries fixes it. Past this the path is handed back in
/// [`ReconcileReport::requeued`] and its row is left exactly as it was — stale
/// beats torn, and the caller has somewhere to put it.
const MAX_READ_ATTEMPTS: usize = 3;

/// How much of a worktree one reconcile is about.
///
/// The scope decides three things at once and they must not drift apart: which
/// paths the walk records, which rows the merge reads, and therefore which rows
/// a removal may name. A scope that walked less than it read would delete rows
/// for files nobody looked at.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ReconcileScope {
    /// Every path of the worktree.
    Full,
    /// One directory and everything beneath it.
    ///
    /// The path is the directory itself, which is also examined: a repository
    /// boundary or a symlink standing where a directory used to be is a change
    /// in the scope it names.
    Subtree(RepoPath),
    /// Exactly these paths, plus anything beneath one that is now a directory.
    ///
    /// The second half is not a convenience. A path that was a file and is now
    /// a directory is a removal *and* an unknown number of additions, and a
    /// scope that named only the path would record the removal and leave the
    /// tree beneath it invisible until something swept.
    Paths(Vec<RepoPath>),
}

impl ReconcileScope {
    /// The whole worktree.
    #[must_use]
    pub const fn full() -> Self {
        Self::Full
    }

    /// One directory and everything beneath it.
    ///
    /// The worktree root is [`Full`](Self::Full) rather than a subtree of
    /// itself, so two spellings of "everything" cannot behave differently.
    #[must_use]
    pub fn subtree(directory: RepoPath) -> Self {
        if directory.is_empty() {
            return Self::Full;
        }
        Self::Subtree(directory)
    }

    /// Exactly these paths, normalized.
    ///
    /// Sorted by their bytes, deduplicated, and stripped of any path another
    /// entry already contains — so the list is disjoint and ordered, which is
    /// what lets the merge read the index in one forward pass. A list holding
    /// the worktree root is [`Full`](Self::Full); an empty list stays empty and
    /// reconciles nothing at all, because "no paths" is a real answer and
    /// promoting it to "every path" would turn a quiet watcher into a rebuild.
    ///
    /// **A path is stripped against every entry already kept, not against the
    /// one before it.** Sorted order is not containment order: `src/watch`,
    /// `src/watch.rs` and `src/watch/tests.rs` sort in that order — this
    /// repository's own module layout — and the entry before the last is the
    /// one that contains it. Comparing only with the previous entry keeps all
    /// three, and the read ranges the merge is built from then overlap — which
    /// hands it one row twice: consumed once as an existing row and once as a
    /// row with no path beside it, which stages a removal for a path the same
    /// pass just recorded.
    ///
    /// A container is always an ancestor and an ancestor always sorts first, so
    /// asking about the candidate's own ancestors is the whole question.
    #[must_use]
    pub fn paths(paths: impl IntoIterator<Item = RepoPath>) -> Self {
        let sorted = paths.into_iter().collect::<BTreeSet<_>>();
        if sorted.iter().any(RepoPath::is_empty) {
            return Self::Full;
        }
        let mut kept: BTreeSet<Vec<u8>> = BTreeSet::new();
        let mut disjoint: Vec<RepoPath> = Vec::with_capacity(sorted.len());
        for path in sorted {
            if ancestor_in(&kept, &path) {
                continue;
            }
            kept.insert(path.as_bytes().to_vec());
            disjoint.push(path);
        }
        Self::Paths(disjoint)
    }

    /// This scope with its invariants re-established.
    ///
    /// Free for the two variants that carry none. For a path list it re-runs
    /// [`paths`](Self::paths), because the variant's field is public and
    /// `#[non_exhaustive]` on an enum stops exhaustive *matching* rather than
    /// construction — so a caller outside this crate can hand over an unsorted
    /// or overlapping list, and every method below assumes neither.
    /// [`names_exactly`](Self::names_exactly) would silently answer "no" for a
    /// named path on an unsorted list, downgrading a force-hash hint to a
    /// metadata comparison, and the merge's read ranges would overlap exactly
    /// as [`paths`](Self::paths) describes.
    #[must_use]
    pub fn normalized(&self) -> Self {
        match self {
            Self::Full => Self::Full,
            Self::Subtree(directory) => Self::subtree(directory.clone()),
            Self::Paths(paths) => Self::paths(paths.iter().cloned()),
        }
    }

    /// Stable spelling carried in reports, event payloads, and diagnostics.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Subtree(_) => "subtree",
            Self::Paths(_) => "paths",
        }
    }

    /// Whether this scope covers the whole worktree.
    #[must_use]
    pub const fn is_full(&self) -> bool {
        matches!(self, Self::Full)
    }

    /// Whether this scope names nothing, and so has nothing to reconcile.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Paths(paths) if paths.is_empty())
    }

    /// How many paths a [`Paths`](Self::Paths) scope names; zero otherwise.
    ///
    /// Deliberately not called `len`: a [`Full`](Self::Full) scope names no
    /// paths and covers every one of them, so a length beside
    /// [`is_empty`](Self::is_empty) would read as a contradiction on the one
    /// value where both are true.
    #[must_use]
    pub fn named_paths(&self) -> usize {
        match self {
            Self::Full | Self::Subtree(_) => 0,
            Self::Paths(paths) => paths.len(),
        }
    }

    /// Whether this scope names `path` itself, rather than merely covering it.
    ///
    /// This is the difference between the two strengths of hint, and it is the
    /// whole cost model. A scope that names `src/main.rs` is saying *something
    /// touched this file*, so it is hashed however unchanged its metadata looks
    /// — a one-second modification-time granularity is an ordinary filesystem
    /// and a file rewritten twice inside one tick matches its row and lies. A
    /// scope that names the directory `src` is saying *something happened in
    /// here*, and the files beneath it are metadata-compared, because a
    /// checkout touching ten thousand files moved ten thousand modification
    /// times and rehashing all of them is the rebuild this module exists to
    /// avoid.
    ///
    /// A coalesced watcher scope holds both kinds in one list, which is exactly
    /// why the question is asked per path rather than per scope.
    #[must_use]
    pub fn names_exactly(&self, path: &RepoPath) -> bool {
        match self {
            Self::Full | Self::Subtree(_) => false,
            // Sorted by construction, so this is a search rather than a scan
            // over ten thousand entries per examined file.
            Self::Paths(paths) => paths.binary_search(path).is_ok(),
        }
    }

    /// Whether this scope covers `path`.
    #[must_use]
    pub fn covers(&self, path: &RepoPath) -> bool {
        match self {
            Self::Full => true,
            Self::Subtree(directory) => directory.contains(path),
            Self::Paths(paths) => paths.iter().any(|scoped| scoped.contains(path)),
        }
    }

    /// The wider scope this one becomes when it names too many paths.
    ///
    /// The subtree containing every path, or the whole worktree when they have
    /// no common directory. `None` when the scope is already within its bound,
    /// which is what a caller reports as "not escalated".
    #[must_use]
    pub fn overflowed(&self) -> Option<Self> {
        let Self::Paths(paths) = self else {
            return None;
        };
        if paths.len() <= MAX_PATHS_PER_RECONCILE {
            return None;
        }
        Some(Self::subtree(RepoPath::common_ancestor(paths.iter())))
    }

    /// The index ranges this scope's rows live in, ordered so that reading them
    /// one after another is a single sorted sequence.
    ///
    /// Two units per named path — the path's own row, and everything beneath it
    /// — rather than one, and the reason is a case that looks impossible until
    /// it is written down. A scope may name both `src` and `src.rs`, and the
    /// range holding `src`'s descendants *begins* at `src/`, which sorts after
    /// `src.rs`. Reading one whole path and then the next would therefore hand
    /// the merge a stream that goes backwards, and a stream that goes backwards
    /// makes it stage a removal and a record for one path in the same batch.
    ///
    /// Split into a point and an interval, every unit across the whole scope is
    /// disjoint from every other — the point of one path cannot be inside
    /// another's subtree, because a path list drops anything an earlier entry
    /// contains — and disjoint intervals over a total order sort by their low
    /// bound. `src`, `src.rs`, `src.rs/…`, `src/…` is that order, and it is the
    /// order the inventory's own entries are already in.
    fn read_units(&self) -> Vec<ReadUnit> {
        let mut units = match self {
            // The worktree root has no row of its own, so the whole cache is
            // one interval.
            Self::Full => vec![ReadUnit::descendants(&RepoPath::from_bytes(Vec::new()))],
            Self::Subtree(directory) => {
                vec![ReadUnit::exact(directory), ReadUnit::descendants(directory)]
            }
            Self::Paths(paths) => paths
                .iter()
                .flat_map(|path| [ReadUnit::exact(path), ReadUnit::descendants(path)])
                .collect(),
        };
        units.sort_by(|left, right| left.low.cmp(&right.low));
        units
    }

    /// The precomputed form the walk asks its two questions of.
    #[must_use]
    pub(crate) fn filter(&self) -> ScopeFilter {
        match self {
            Self::Full => ScopeFilter::Everything,
            Self::Subtree(directory) => ScopeFilter::Subtree(directory.clone()),
            Self::Paths(paths) => {
                let mut open = BTreeSet::new();
                for path in paths {
                    open.extend(
                        path.ancestors()
                            .iter()
                            .map(|ancestor| ancestor.as_bytes().to_vec()),
                    );
                }
                ScopeFilter::Paths {
                    exact: paths.iter().map(|path| path.as_bytes().to_vec()).collect(),
                    open,
                }
            }
        }
    }
}

impl std::fmt::Display for ReconcileScope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => formatter.write_str("full"),
            Self::Subtree(directory) => write!(formatter, "subtree '{}'", directory.display()),
            Self::Paths(paths) => write!(formatter, "{} path(s)", paths.len()),
        }
    }
}

/// A scope arranged for the two questions a walk asks of every entry.
///
/// Built once per walk rather than answered from the path list each time: a
/// scope may name ten thousand paths and a walk may meet a hundred thousand
/// entries, and asking "is any of these an ancestor" per entry is the product
/// of the two.
#[derive(Clone, Debug)]
pub(crate) enum ScopeFilter {
    /// Every directory is descended into and every path recorded.
    Everything,
    /// One directory, its ancestors on the way down, and everything beneath it.
    Subtree(RepoPath),
    /// A path list: its ancestors are descended into, its members recorded.
    ///
    /// Held as raw bytes rather than as paths so the containment questions the
    /// walk asks of every entry can be answered without allocating.
    Paths {
        exact: BTreeSet<Vec<u8>>,
        open: BTreeSet<Vec<u8>>,
    },
}

impl ScopeFilter {
    /// Whether the walk needs to look inside `directory` at all.
    ///
    /// True for a directory *on the way to* the scope as well as one inside it,
    /// because the `.gitignore` chain is read on the way down and a walk that
    /// jumped straight to its scope would apply a different set of rules than a
    /// full one — which is the one thing a scoped walk must never do.
    pub(crate) fn descends_into(&self, directory: &RepoPath) -> bool {
        match self {
            Self::Everything => true,
            Self::Subtree(scoped) => scoped.contains(directory) || directory.contains(scoped),
            Self::Paths { exact, open } => {
                open.contains(directory.as_bytes()) || covered(exact, directory)
            }
        }
    }

    /// Whether an entry at `path` belongs in this walk's inventory.
    pub(crate) fn records(&self, path: &RepoPath) -> bool {
        match self {
            Self::Everything => true,
            Self::Subtree(scoped) => scoped.contains(path),
            Self::Paths { exact, .. } => covered(exact, path),
        }
    }
}

/// Whether `paths` holds a strict ancestor of `path`.
///
/// Written against the raw bytes and scanning separator positions in place,
/// because the walk asks this of *every* entry it meets: building the ancestor
/// chain first would allocate a vector and a path per component per file, which
/// is the per-entry cost the precomputed filter exists to avoid.
fn ancestor_in(paths: &BTreeSet<Vec<u8>>, path: &RepoPath) -> bool {
    let bytes = path.as_bytes();
    bytes
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'/')
        .any(|(index, _)| paths.contains(&bytes[..index]))
}

/// Whether any scoped path contains `path`, itself included.
fn covered(exact: &BTreeSet<Vec<u8>>, path: &RepoPath) -> bool {
    exact.contains(path.as_bytes()) || ancestor_in(exact, path)
}

/// What one reconcile did.
///
/// Every count is about paths rather than rows written, which is the honest
/// unit: a caller wants to know how much of the worktree was looked at and how
/// much of it moved, and those are the two numbers the incremental promise is
/// made in.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ReconcileReport {
    /// Checkout that was reconciled.
    pub worktree: WorktreeKey,
    /// Scope the caller asked for.
    pub scope: ReconcileScope,
    /// Scope it actually became, when widening was necessary.
    pub escalated: Option<ReconcileScope>,
    /// Whether the checkout's committed base differs from the one the last full
    /// pass recorded.
    ///
    /// When it does, every row is treated as a suspect and hashed rather than
    /// metadata-compared. That is the answer to [#63]: a worktree's identity is
    /// its path, so a checkout deleted and re-created at the same path is the
    /// same key holding another branch's rows, and a filesystem that preserved
    /// sizes and modification times would have every one of them verify.
    ///
    /// [#63]: https://github.com/fullstacktaiye/harkness/issues/63
    pub head_changed: bool,
    /// Paths the walk recorded and the merge considered.
    pub examined: u64,
    /// Paths whose bytes were read and digested.
    ///
    /// The number the incremental promise lives on: a sweep over an unchanged
    /// repository hashes nothing, and one over a repository with ten changed
    /// files hashes ten.
    pub hashed: u64,
    /// Paths that had no row and now have one.
    pub added: u64,
    /// Paths whose stored derivation or classification was replaced.
    pub changed: u64,
    /// Rows removed because the walk recorded no path for them.
    pub removed: u64,
    /// Whether the walk stopped on its own budget, in which case nothing was
    /// removed.
    pub truncated: bool,
    /// Paths that kept moving while they were being read.
    ///
    /// Their rows were left exactly as they were, and the caller owes them
    /// another reconcile. Handed back as paths rather than counted because the
    /// caller is the only thing that can re-queue them.
    pub requeued: Vec<RepoPath>,
    /// Generation this reconcile published, or the standing watermark when it
    /// wrote nothing.
    pub generation: u64,
    /// Wall-clock time the whole pass took, walk included.
    pub duration: Duration,
}

impl ReconcileReport {
    /// Whether anything about the index changed.
    #[must_use]
    pub const fn is_quiet(&self) -> bool {
        self.added == 0 && self.changed == 0 && self.removed == 0
    }

    /// The scope that was actually walked.
    #[must_use]
    pub const fn effective_scope(&self) -> &ReconcileScope {
        match &self.escalated {
            Some(escalated) => escalated,
            None => &self.scope,
        }
    }
}

/// One worktree's reconciler, holding everything a pass needs and nothing else.
///
/// Constructed per call rather than kept, because every field is either
/// borrowed from the engine or read from the repository at the moment the pass
/// begins — a reconciler that remembered a head marker would compare the
/// filesystem against a base it read at start-up.
pub(crate) struct Reconciler<'engine> {
    pub(crate) cache: &'engine IndexCache,
    pub(crate) worktree: WorktreeKey,
    pub(crate) root: &'engine Path,
    pub(crate) policy: &'engine InventoryPolicy,
    /// Language adapters and their per-language version markers.
    pub(crate) symbols: &'engine dyn SymbolSource,
    /// The committed base this checkout is on right now, or `None` when it
    /// cannot be read.
    pub(crate) head_marker: Option<String>,
}

/// What reading one path produced.
enum Read {
    /// The bytes were read and are consistent with the metadata the walk saw.
    Version(Box<FileVersion>),
    /// The path could not be opened, or its bytes could not be decoded.
    Unreadable,
    /// The path changed under the reader often enough to give up on it.
    Moving,
}

/// What happened to one examined path.
enum Applied {
    /// Nothing was written: the row already describes the file.
    Skipped,
    /// Only the row's own metadata was refreshed; its derivation is unchanged.
    Refreshed,
    /// A path that had no row now has one.
    Added,
    /// A row's derivation or classification was replaced.
    Changed,
    /// A path that should have been read could not be.
    Unreadable,
    /// The path kept moving and was left alone.
    Moving,
}

impl Reconciler<'_> {
    /// Compares the worktree against the index within `requested` and publishes
    /// the difference.
    pub(crate) fn run(
        &self,
        requested: &ReconcileScope,
        cancellation: &Cancellation,
    ) -> Result<ReconcileReport, ContextEngineError> {
        let started = Instant::now();
        if cancellation.is_cancelled() {
            return Err(ContextEngineError::Cancelled);
        }

        // Re-established rather than assumed. The variant's field is public and
        // `#[non_exhaustive]` stops exhaustive matching rather than
        // construction, so a caller outside this crate can hand over an
        // unsorted or overlapping list — and every method below assumes
        // neither. Free for a scope that already came from `paths`.
        let requested = &requested.normalized();

        // Asked before anything else, and about the *requested* scope. A caller
        // that named nothing gets nothing; widening it — which the head check
        // below would otherwise do, because a scope naming no paths is not a
        // full one — would turn "the queue was empty" into a whole-repository
        // pass.
        if requested.is_empty() {
            return Ok(self.nothing_to_do(requested, started));
        }

        // A recorded base and a different one now. An *absent* marker is not a
        // change: it means no full pass has ever recorded one, which is the
        // case where metadata is all there is, and reading it as divergence
        // would make every reconcile of such a cache a full rehash forever.
        let stored = self.cache.worktree_marker(&self.worktree)?;
        let head_changed = stored.is_some() && stored.as_deref() != self.head_marker.as_deref();

        // Nothing published for this checkout means nothing to compare against,
        // so an incremental pass over it would index the paths it was handed
        // and leave the rest of the tree invisible. It is reached two ways and
        // both matter: a worktree the cache has never seen, and one whose cache
        // was quarantined and recreated underneath a running watch — where the
        // pass that met the fault reported it, the scope it had drained was
        // small, and every pass after it would otherwise be small too.
        let unindexed = self.cache.worktree_generation(&self.worktree)? == 0;

        let mut escalated = requested.overflowed();
        if (head_changed || unindexed) && !escalated.as_ref().unwrap_or(requested).is_full() {
            escalated = Some(ReconcileScope::Full);
        }
        let effective = escalated.clone();
        let effective = effective.as_ref().unwrap_or(requested);

        let span = tracing::debug_span!(
            "context.reconcile",
            worktree = self.worktree.as_str(),
            scope = effective.kind(),
        );
        let _entered = span.enter();

        // A reading of the tree rather than a workspace capture. The identifier
        // names *this* read, which is what a `FileVersion` carries so a chunk
        // can say where its bytes came from; nothing the batch writes records
        // it, and reconciling is deliberately not a snapshot — capturing one
        // reads Git's status and hashes every dirty file, which is the whole
        // repository's cost paid to update one row.
        let reading = SnapshotId::new();
        let inventory = InventoryBuilder::build_scoped(
            reading,
            self.root,
            self.policy,
            effective,
            cancellation,
        )?;
        let classify = inventory.classify_version();
        let chunking = crate::CHUNKING_VERSION.to_string();

        let _operation = self.cache.begin_operation("reconcile");
        let mut batch = self.cache.begin(
            &self.worktree,
            self.root,
            BatchScope::Targeted,
            cancellation,
        )?;
        // Only a pass that examined every path may say which base the checkout
        // as a whole was verified against.
        if effective.is_full() && !inventory.is_truncated() {
            batch.record_head_marker(self.head_marker.as_deref());
        }

        let mut pass = Pass {
            scope: effective,
            distrust_metadata: head_changed,
            classify,
            chunking,
            reading,
            totals: Totals::default(),
        };
        self.merge(&mut batch, &inventory, &mut pass, cancellation)?;
        let receipt = batch.commit(cancellation)?;
        let totals = pass.totals;

        let report = ReconcileReport {
            worktree: self.worktree.clone(),
            scope: requested.clone(),
            escalated,
            head_changed,
            examined: totals.examined,
            hashed: totals.hashed,
            added: totals.added,
            changed: totals.changed,
            removed: receipt.files_removed,
            truncated: inventory.is_truncated(),
            requeued: totals.requeued,
            generation: receipt.generation,
            duration: started.elapsed(),
        };
        tracing::debug!(
            worktree = report.worktree.as_str(),
            scope = report.effective_scope().kind(),
            examined = report.examined,
            hashed = report.hashed,
            added = report.added,
            changed = report.changed,
            removed = report.removed,
            generation = report.generation,
            duration_ms = report.duration.as_millis(),
            "context index reconciled"
        );
        Ok(report)
    }

    /// The report a scope naming nothing deserves: the standing watermark, and
    /// no batch opened at all.
    fn nothing_to_do(&self, requested: &ReconcileScope, started: Instant) -> ReconcileReport {
        ReconcileReport {
            worktree: self.worktree.clone(),
            scope: requested.clone(),
            escalated: None,
            head_changed: false,
            examined: 0,
            hashed: 0,
            added: 0,
            changed: 0,
            removed: 0,
            truncated: false,
            requeued: Vec::new(),
            // Read rather than reported as zero: a caller comparing generations
            // must not see a quiet pass as a rebuilt index.
            generation: self
                .cache
                .worktree_generation(&self.worktree)
                .unwrap_or_default(),
            duration: started.elapsed(),
        }
    }

    /// Walks the inventory and the index rows together, in path order.
    ///
    /// Both sides are sorted by the same bytes — the walk sorts every listing
    /// and the store orders every page — so one forward pass over each decides
    /// additions, changes and removals without either side being held in memory
    /// whole.
    fn merge(
        &self,
        batch: &mut IndexBatch<'_>,
        inventory: &FileInventory,
        pass: &mut Pass<'_>,
        cancellation: &Cancellation,
    ) -> Result<(), ContextEngineError> {
        let mut rows = RowCursor::new(self.cache, &self.worktree, pass.scope.read_units());
        let mut pending = rows.next()?;
        // A walk stopped by its own budget did not see the whole scope, so a
        // row it has no path for is a row about a file nobody looked at.
        let sweep = !inventory.is_truncated();

        for entry in inventory.entries() {
            if cancellation.is_cancelled() {
                return Err(ContextEngineError::Cancelled);
            }
            while let Some(row) = pending.take() {
                if row.path >= entry.path {
                    pending = Some(row);
                    break;
                }
                if sweep {
                    batch.remove(&row.path)?;
                }
                pending = rows.next()?;
            }
            let existing = match pending.take() {
                Some(row) if row.path == entry.path => {
                    pending = rows.next()?;
                    Some(row)
                }
                carried => {
                    pending = carried;
                    None
                }
            };

            pass.totals.examined += 1;
            let hinted = pass.distrust_metadata || pass.scope.names_exactly(&entry.path);
            let applied =
                self.apply(batch, entry, existing.as_ref(), hinted, pass, cancellation)?;
            match applied {
                Applied::Added => pass.totals.added += 1,
                Applied::Changed => pass.totals.changed += 1,
                Applied::Moving => pass.totals.requeued.push(entry.path.clone()),
                Applied::Skipped | Applied::Refreshed | Applied::Unreadable => {}
            }
        }

        while let Some(row) = pending {
            if cancellation.is_cancelled() {
                return Err(ContextEngineError::Cancelled);
            }
            if sweep {
                batch.remove(&row.path)?;
            }
            pending = rows.next()?;
        }
        Ok(())
    }

    /// Decides what one path's row should become, and stages it.
    fn apply(
        &self,
        batch: &mut IndexBatch<'_>,
        entry: &InventoryEntry,
        existing: Option<&IndexedFile>,
        hinted: bool,
        pass: &mut Pass<'_>,
        cancellation: &Cancellation,
    ) -> Result<Applied, ContextEngineError> {
        let classify = pass.classify;
        let chunking = pass.chunking.as_str();
        // A path whose content is never read — a binary, a symlink, a
        // repository boundary. There is nothing to hash, so its metadata and
        // its classification are the whole comparison.
        if !entry.eligible() {
            if existing.is_some_and(|row| ineligible_row_is_current(row, entry, classify)) {
                return Ok(Applied::Skipped);
            }
            batch.record_entry(entry, classify)?;
            return Ok(if existing.is_some() {
                Applied::Changed
            } else {
                Applied::Added
            });
        }

        if !hinted
            && existing.is_some_and(|row| {
                row_is_current(
                    row,
                    entry,
                    classify,
                    chunking,
                    self.symbols.expected_version(row.language.as_ref()),
                )
            })
        {
            return Ok(Applied::Skipped);
        }

        let version = match self.read(entry, pass.reading, cancellation)? {
            Read::Version(version) => {
                // Counted here rather than before the read, because the number
                // is published as "paths whose bytes were read and digested"
                // and a caller judges from it whether an update was
                // incremental. A tree holding a handful of unreadable files
                // would otherwise inflate it on every sweep, since a row marked
                // unreadable is a suspect every time.
                pass.totals.hashed += 1;
                version
            }
            Read::Unreadable => {
                batch.record_unreadable(entry, classify)?;
                return Ok(Applied::Unreadable);
            }
            Read::Moving => return Ok(Applied::Moving),
        };

        // The hash short-circuit, and the reason a duplicate event costs
        // nothing: the bytes are the ones the row already names, so its chunk
        // set is still correct and re-deriving it would produce the rows that
        // are already there.
        if existing.is_some_and(|row| {
            derivation_survives(
                row,
                entry,
                classify,
                chunking,
                self.symbols.expected_version(row.language.as_ref()),
                version.content_sha256(),
            )
        }) {
            let row = existing.expect("checked above");
            if row.byte_size == entry.byte_size && row.mtime_ns == entry.mtime_ns {
                return Ok(Applied::Skipped);
            }
            batch.record_refreshed(entry, classify)?;
            return Ok(Applied::Refreshed);
        }

        let extracted = extract_file_symbols(self.symbols, &entry.path, &version, cancellation);
        let version = match extracted.detection.language.clone() {
            Some(language) => (*version).with_language(language),
            None => *version,
        };
        let chunks = match chunk_with_symbol_outline(&version, &extracted, cancellation) {
            Ok(chunks) => chunks,
            Err(ChunkError::Cancelled) => return Err(ContextEngineError::Cancelled),
            Err(_) => {
                batch.record_unreadable(entry, classify)?;
                return Ok(Applied::Unreadable);
            }
        };
        batch.record_chunked(entry, &version, &chunks, classify)?;
        batch.record_extraction(version.id(), &extracted)?;
        Ok(if existing.is_some() {
            Applied::Changed
        } else {
            Applied::Added
        })
    }

    /// Reads one eligible path, refusing bytes that do not match the metadata
    /// the walk recorded.
    ///
    /// The stat *after* the read is what decides, exactly as it is in the walk:
    /// bytes read from a file that was rewritten half-way through describe
    /// neither version, and a chunk set derived from them would be indexed
    /// under a digest no file on disk has.
    fn read(
        &self,
        entry: &InventoryEntry,
        reading: SnapshotId,
        cancellation: &Cancellation,
    ) -> Result<Read, ContextEngineError> {
        let absolute = self.root.join(entry.path.to_path_buf());
        for _ in 0..MAX_READ_ATTEMPTS {
            if cancellation.is_cancelled() {
                return Err(ContextEngineError::Cancelled);
            }
            // Stat-ed *before* the read as well as after it. The walk's size is
            // what bounds this file — it was taken under the inventory's own
            // limits — and a path that is a hundred bytes in the index and ten
            // gigabytes on disk is exactly the case a watched directory
            // produces. Reading first and rejecting afterwards would buffer the
            // whole of it, three times over.
            let Ok(before) = std::fs::symlink_metadata(&absolute) else {
                return Ok(Read::Unreadable);
            };
            if !before.is_file() {
                return Ok(Read::Moving);
            }
            if before.len() != entry.byte_size {
                return Ok(Read::Moving);
            }
            let Ok(bytes) = std::fs::read(&absolute) else {
                return Ok(Read::Unreadable);
            };
            let Ok(metadata) = std::fs::symlink_metadata(&absolute) else {
                return Ok(Read::Unreadable);
            };
            // A regular file that became a link or a directory while it was
            // being read is not a file that moved, it is a different entry —
            // and the next walk is what records it as one.
            if !metadata.is_file() {
                return Ok(Read::Moving);
            }
            // The stat after the read is what decides, exactly as the walk's
            // does: bytes read from a file that was rewritten half-way through
            // describe neither version.
            if metadata.len() != entry.byte_size
                || crate::inventory::modified_nanos(&metadata) != entry.mtime_ns
            {
                continue;
            }
            return match FileVersion::new(entry, reading, bytes.into(), cancellation) {
                Ok(version) => Ok(Read::Version(Box::new(version))),
                Err(ChunkError::Cancelled) => Err(ContextEngineError::Cancelled),
                Err(_) => Ok(Read::Unreadable),
            };
        }
        Ok(Read::Moving)
    }
}

/// Everything one pass compares against, fixed before the first path is
/// examined and carried rather than threaded.
///
/// The versions in particular are read once: `CHUNKING_VERSION` is rendered to
/// a string here rather than at every file, and the classification version
/// comes from the inventory that was actually walked rather than from the
/// constant, so a walk and the comparison of its results cannot disagree about
/// which rules ran.
struct Pass<'scope> {
    scope: &'scope ReconcileScope,
    /// Whether the recorded base diverged, making every row a suspect.
    distrust_metadata: bool,
    classify: u32,
    chunking: String,
    reading: SnapshotId,
    totals: Totals,
}

/// The running counts of one pass.
#[derive(Default)]
struct Totals {
    examined: u64,
    hashed: u64,
    added: u64,
    changed: u64,
    requeued: Vec<RepoPath>,
}

/// Whether an eligible path's row already describes the file the walk saw.
///
/// Every field a later read would produce, compared against what the walk
/// reported. A `None` modification time on either side fails the comparison:
/// a platform that reports none has nothing to compare, and answering
/// "unchanged" from an absence is how a file stops being re-indexed forever.
fn row_is_current(
    row: &IndexedFile,
    entry: &InventoryEntry,
    classify: u32,
    chunking: &str,
    parser: &str,
) -> bool {
    row.file_version.is_some()
        && row.chunking_version.as_deref() == Some(chunking)
        && row.parser_version.as_deref() == Some(parser)
        && row.classify_version == classify
        && !row.unreadable
        && row.class == entry.class
        && row.symlink == entry.symlink
        && row.boundary == entry.boundary
        && row.byte_size == entry.byte_size
        && row.mtime_ns.is_some()
        && row.mtime_ns == entry.mtime_ns
}

/// Whether an ineligible path's row already describes the entry the walk saw.
///
/// `file_version` must be absent: a file that was source and is now a symlink
/// keeps a derivation that describes bytes at a path whose content is no longer
/// read, and leaving it would keep that content retrievable.
fn ineligible_row_is_current(row: &IndexedFile, entry: &InventoryEntry, classify: u32) -> bool {
    row.file_version.is_none()
        && row.classify_version == classify
        && row.class == entry.class
        && row.symlink == entry.symlink
        && row.boundary == entry.boundary
        && row.unreadable == entry.unreadable
        && row.byte_size == entry.byte_size
        && row.mtime_ns == entry.mtime_ns
}

/// Whether the derivation a row holds is still the right one for these bytes.
///
/// The digest is the question and everything else is the reason the digest is
/// not enough on its own: a chunk set is a function of the path, the bytes and
/// the rules that produced it, so identical content under a bumped chunking
/// version is content that has to be chunked again.
fn derivation_survives(
    row: &IndexedFile,
    entry: &InventoryEntry,
    classify: u32,
    chunking: &str,
    parser: &str,
    digest: &crate::digest::Sha256Hex,
) -> bool {
    row.content_sha256.as_ref() == Some(digest)
        && row.chunking_version.as_deref() == Some(chunking)
        && row.parser_version.as_deref() == Some(parser)
        && row.classify_version == classify
        && !row.unreadable
        && row.class == entry.class
        && row.symlink == entry.symlink
        && row.boundary == entry.boundary
}

/// One contiguous, disjoint range of the index a scope covers.
///
/// Either one path's own row or everything beneath it, never both, so that
/// every unit of a scope is disjoint from every other and the whole set sorts
/// by `low`. See [`ReconcileScope::read_units`] for the case that makes the
/// split necessary.
#[derive(Clone, Debug)]
struct ReadUnit {
    /// The path the range is anchored at.
    prefix: RepoPath,
    /// The lowest path the range can hold, which is what units sort by.
    low: Vec<u8>,
    /// Whether the anchor's own row is the range, or is excluded from it.
    exact: bool,
}

impl ReadUnit {
    fn exact(prefix: &RepoPath) -> Self {
        Self {
            prefix: prefix.clone(),
            low: prefix.as_bytes().to_vec(),
            exact: true,
        }
    }

    fn descendants(prefix: &RepoPath) -> Self {
        let mut low = prefix.as_bytes().to_vec();
        low.push(b'/');
        Self {
            prefix: prefix.clone(),
            low,
            exact: false,
        }
    }
}

/// One forward pass over the index rows a scope covers.
///
/// Pages rather than collects, and moves from one unit to the next in order, so
/// a whole-repository reconcile holds a page of rows instead of a repository's
/// worth. Every read names the worktree, which is the isolation contract
/// expressed where it has to be.
struct RowCursor<'cache> {
    cache: &'cache IndexCache,
    worktree: &'cache WorktreeKey,
    units: std::vec::IntoIter<ReadUnit>,
    current: Option<ReadUnit>,
    after: Option<RepoPath>,
    buffered: std::vec::IntoIter<IndexedFile>,
    more: bool,
}

impl<'cache> RowCursor<'cache> {
    fn new(cache: &'cache IndexCache, worktree: &'cache WorktreeKey, units: Vec<ReadUnit>) -> Self {
        Self {
            cache,
            worktree,
            units: units.into_iter(),
            current: None,
            after: None,
            buffered: Vec::new().into_iter(),
            more: false,
        }
    }

    fn next(&mut self) -> Result<Option<IndexedFile>, ContextEngineError> {
        loop {
            if let Some(row) = self.buffered.next() {
                self.after = Some(row.path.clone());
                return Ok(Some(row));
            }
            if let Some(unit) = self.current.as_ref().filter(|_| self.more) {
                if unit.exact {
                    // One row or none, and never paged: a path is one path.
                    let row = self.cache.file(self.worktree, &unit.prefix)?;
                    self.more = false;
                    self.buffered = row.into_iter().collect::<Vec<_>>().into_iter();
                    continue;
                }
                // Anchored past the prefix itself, which is exactly what turns
                // "this path and everything beneath it" into "everything
                // beneath it" — the other half is the unit before this one.
                let after = self.after.clone().unwrap_or_else(|| unit.prefix.clone());
                let page = self.cache.files_under(
                    self.worktree,
                    &unit.prefix,
                    Some(&after),
                    MERGE_PAGE_ROWS,
                )?;
                self.more = page.more;
                self.buffered = page.rows.into_iter();
                continue;
            }
            let Some(next) = self.units.next() else {
                return Ok(None);
            };
            self.current = Some(next);
            self.after = None;
            self.more = true;
        }
    }
}

#[cfg(test)]
mod tests;
