//! Where to look first, and never what is true.
//!
//! This module turns filesystem events into [`ChangeHint`]s, coalesces them
//! into a bounded dirty set, and hands the result to
//! [`ContextEngine::reconcile`](crate::ContextEngine::reconcile) once the tree
//! has gone quiet. Everything it produces is a suggestion. The
//! [`ContextEngine::reconcile`](crate::ContextEngine::reconcile) is what decides
//! whether anything actually changed, by comparing the filesystem against the
//! stored rows — and it produces the same answer whether it was reached through
//! a watcher, a startup sweep, or a caller asking directly.
//!
//! That split is the whole design, and it is not defensive engineering. Every
//! watcher backend drops events under load, coalesces distinct changes into
//! one, reports paths that did not change, races the reader that follows them,
//! and — the one no backend can help with — sees nothing at all while Harkness
//! is not running. An index that believed its events would be quietly, subtly
//! stale, and only after a restart or under a build, which is the worst shape a
//! bug can have.
//!
//! # The pipeline
//!
//! ```text
//! notify event ──▶ normalize ──▶ dirty set ──▶ quiescence ──▶ reconcile
//!                  (layer 1,     (coalesce,     (500 ms of      (truth)
//!                   .git, tmp)    bounded)       no arrivals)
//! ```
//!
//! **Normalize** is where a denied path stops existing.
//! [`BUILT_IN_DENIALS`](crate::BUILT_IN_DENIALS)
//! are applied before anything is queued, so a `.env` being written produces no
//! hint, no queue entry, no row, and no event payload carrying its name — the
//! same rule the walk enforces, compiled from the same list rather than from a
//! second copy of it. The repository's own administrative directory is dropped
//! too, with one exception: `.git/HEAD` is a *whole-worktree* hint, because that
//! is what a branch switch rewrites and because ten thousand individual file
//! events are exactly the storm the dirty set would collapse anyway.
//!
//! **Coalesce** is [`DirtySet`], which is bounded by construction rather than by
//! hope. Hints are absorbed by any subtree marker that already covers them,
//! subtree markers swallow the paths beneath them, and passing
//! [`WATCH_QUEUE_CAPACITY`] collapses the whole set into "everything". A
//! checkout touching ten thousand files therefore costs one reconcile and a
//! bounded amount of memory, not ten thousand of either.
//!
//! **Quiescence** is [`QUIESCENCE_WINDOW`]: a scope is drained only once no hint
//! has arrived for that long. An editor writing a file five times while a
//! formatter runs over it is one reconcile, and the window is also what bounds
//! how often this module can emit anything at all.
//!
//! # Hints have two strengths
//!
//! A hint naming a *file* is force-hashed by the reconciler even when its size
//! and modification time match its row, because a one-second modification-time
//! granularity is a real filesystem and a file rewritten twice in one second
//! matches and lies. A hint naming a *directory* is not: everything beneath it
//! is metadata-compared, because a branch switch that touched ten thousand
//! files moved ten thousand modification times and rehashing all of them to
//! discover that is the rebuild this exists to avoid.
//!
//! # Threading
//!
//! One worker thread per service, and it is the only thing that writes. Hints
//! arrive on `notify`'s own thread and on any caller's; both do nothing but
//! take a mutex, insert into the dirty set, and signal. There is no async
//! runtime here and this module starts none (ADR-0003) — a thread that blocks
//! on a condition variable and a reconcile is exactly what the engine's
//! "blocking and cancellation-polled" contract is written for.
//!
//! Shutdown drains or abandons within [`SHUTDOWN_DEADLINE`]. Abandoning is safe
//! and needs no bookkeeping: a batch that never committed is invisible, and
//! reconciliation is idempotent, so the scope is re-derived by the next startup
//! sweep from the filesystem rather than from anything remembered.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use harkness_git::Cancellation;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind, RenameMode};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use thiserror::Error;

use crate::engine::ContextEngine;
use crate::error::ContextEngineError;
use crate::path::RepoPath;
use crate::reconcile::{ReconcileReport, ReconcileScope};

/// How long the filesystem must be quiet before a scope is reconciled.
///
/// Long enough to absorb an editor's save, a formatter's rewrite and the
/// `rename` that lands between them; short enough that the result is available
/// about a second after the last keystroke. It is also the emission bound: at
/// most one pass starts and one finishes per window, which is what keeps the
/// event rate inside the four-per-second budget without a throttle that could
/// silently drop the one event a surface was waiting for.
pub const QUIESCENCE_WINDOW: Duration = Duration::from_millis(500);

/// Most entries the dirty set holds before it collapses into "everything".
///
/// A bound on memory that is also a bound on work: past this, reconciling the
/// whole worktree is cheaper than carrying the list, and a metadata sweep of an
/// unchanged repository hashes nothing. The set never grows past it, because
/// reaching it replaces the contents rather than adding to them.
pub const WATCH_QUEUE_CAPACITY: usize = 4_096;

/// How long a stopping service waits for the pass in flight.
///
/// Past it the cancellation token is set and the pass is abandoned, which costs
/// nothing: a batch that never committed is invisible, so the next startup
/// sweep re-derives the same work from the filesystem.
pub const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(2);

/// How often the worker re-checks for a stop while waiting out quiescence.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Names an editor writes and then renames away.
///
/// Dropped before they reach the queue. This is a cost rule rather than a
/// safety one — the walk would classify and record `notes.md.tmp` like any
/// other file, and a sweep still would — but an atomic save produces a create
/// and a remove on a path that no longer exists by the time anything could look
/// at it, and reconciling a path to discover it is gone is the purest waste
/// this pipeline can do.
const TEMPORARY_PATTERNS: &[&str] = &[
    "**/*.tmp",
    "**/*.temp",
    "**/*~",
    "**/.#*",
    "**/#*#",
    "**/*.swp",
    "**/*.swx",
    "**/*.swpx",
    // Vim's write test file, and GLib's atomic-replace staging name.
    "**/4913",
    "**/.goutputstream-*",
];

/// One suggestion about where the index may have fallen behind.
///
/// Never a statement that anything changed. A hint can name a path nothing
/// touched, miss a path that changed, and arrive after the change it describes
/// has been undone; the reconciler is written so that each of those costs work
/// and never correctness.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ChangeHint {
    /// One path is worth examining, and worth hashing whatever its metadata
    /// says.
    Path(RepoPath),
    /// One directory and everything beneath it is worth examining.
    ///
    /// The paths inside are metadata-compared rather than hashed, which is what
    /// makes a ten-thousand-file checkout affordable. An empty path is the
    /// worktree root and therefore means the whole tree.
    Subtree(RepoPath),
    /// The hint source gave up, and nothing about the worktree can be assumed.
    ///
    /// A backend-reported rescan, or a dirty set past its bound. It collapses
    /// everything queued into a full pass rather than being carried, which is
    /// what makes the queue's memory a constant.
    Overflow,
}

/// Failures raised while establishing or running a watch.
///
/// Carried into [`ContextEngineError::Watch`] rather than re-spelled, exactly as
/// the walk's failures are, so a caller that needs to tell an exhausted inotify
/// table from a missing root gets the discriminant this module gave it.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum WatchError {
    /// No watcher backend could be established for this worktree.
    ///
    /// An exhausted inotify table, a filesystem with no notification support, a
    /// container that does not implement the syscall. **Never fatal**: the
    /// engine degrades to reconciling on demand and at startup, which is slower
    /// and exactly as correct, because events were never the source of truth.
    #[error("no filesystem watcher is available for '{}': {reason}", path.display())]
    WatcherUnavailable {
        /// Worktree root the watch was for.
        path: PathBuf,
        /// Stable human-readable explanation from the backend.
        reason: String,
    },

    /// The worktree root is not there to watch.
    #[error("the watch root '{}' is missing", path.display())]
    WatchRootMissing {
        /// Worktree root the watch was for.
        path: PathBuf,
    },

    /// The dirty set reached its bound and collapsed into a full pass.
    ///
    /// Internal and self-correcting: it is how the queue stays bounded, and the
    /// pass that follows is more work rather than less coverage. It reaches a
    /// caller only as a diagnostic, which is why it carries a count rather than
    /// a path list.
    #[error("{dropped} queued hint(s) were collapsed into a full reconcile")]
    QueueOverflow {
        /// Entries the collapse replaced.
        dropped: usize,
    },

    /// The watch observed its cancellation token.
    #[error("the watch was cancelled")]
    Cancelled,
}

impl WatchError {
    /// Every stable discriminant this namespace defines.
    pub const KINDS: &'static [&'static str] = &[
        "watcher_unavailable",
        "watch_root_missing",
        "queue_overflow",
        "cancelled",
    ];

    /// Stable machine-readable discriminant for caller-facing error handling.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::WatcherUnavailable { .. } => "watcher_unavailable",
            Self::WatchRootMissing { .. } => "watch_root_missing",
            Self::QueueOverflow { .. } => "queue_overflow",
            Self::Cancelled => "cancelled",
        }
    }
}

/// What one filesystem event says happened, stripped of its backend.
///
/// Defined here rather than taken from `notify` so that every rule below can be
/// exercised without an operating system in the loop: the default test suite
/// injects these directly, and the mapping from a real event to one of them is
/// the only part that needs a watcher to test. It is also the seam a future
/// hint source — an editor plugin, a language server — plugs into without
/// touching the reconciler.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ChangeClass {
    /// A path appeared.
    Created,
    /// A path's content or metadata changed.
    Modified,
    /// A path went away.
    Removed,
    /// A path arrived at, or left, this name.
    Renamed,
    /// The backend lost track and asked for a rescan.
    Rescan,
}

/// One normalized filesystem event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilesystemChange {
    /// Absolute paths the event names.
    pub paths: Vec<PathBuf>,
    /// What the backend said happened.
    pub class: ChangeClass,
}

/// Turns filesystem events into hints, and denied paths into nothing.
///
/// Stateless past its compiled rules, so it can run on the watcher's own
/// delivery thread: one gitignore match, one cheap `lstat` for paths that still
/// exist, and a push into the queue.
#[derive(Debug)]
pub struct Normalizer {
    root: PathBuf,
    denials: Gitignore,
    temporary: Gitignore,
}

impl Normalizer {
    /// Compiles the rules a hint must survive, rooted at `worktree_root`.
    ///
    /// # Errors
    ///
    /// [`WatchError::WatcherUnavailable`] when the built-in rules do not
    /// compile, which is a build fault rather than an environment one and is
    /// reported rather than panicked on.
    pub fn new(worktree_root: &Path) -> Result<Self, WatchError> {
        let denials = crate::inventory::compile_denials(worktree_root).map_err(|error| {
            WatchError::WatcherUnavailable {
                path: worktree_root.to_path_buf(),
                reason: error.to_string(),
            }
        })?;
        let mut builder = GitignoreBuilder::new(worktree_root);
        for pattern in TEMPORARY_PATTERNS {
            builder
                .add_line(None, pattern)
                .map_err(|error| WatchError::WatcherUnavailable {
                    path: worktree_root.to_path_buf(),
                    reason: error.to_string(),
                })?;
        }
        let temporary = builder
            .build()
            .map_err(|error| WatchError::WatcherUnavailable {
                path: worktree_root.to_path_buf(),
                reason: error.to_string(),
            })?;
        Ok(Self {
            root: worktree_root.to_path_buf(),
            denials,
            temporary,
        })
    }

    /// The hints one event is worth, which is very often none at all.
    #[must_use]
    pub fn normalize(&self, change: &FilesystemChange) -> Vec<ChangeHint> {
        if change.class == ChangeClass::Rescan {
            return vec![ChangeHint::Overflow];
        }
        let mut hints = Vec::new();
        for path in &change.paths {
            if let Some(hint) = self.hint(path, &change.class) {
                hints.push(hint);
            }
        }
        hints
    }

    /// The hint one path is worth.
    fn hint(&self, absolute: &Path, class: &ChangeClass) -> Option<ChangeHint> {
        let relative = absolute.strip_prefix(&self.root).ok()?;
        // The root itself changing says nothing a path inside it does not.
        if relative.as_os_str().is_empty() {
            return None;
        }
        let path = RepoPath::from_path(relative);

        // The administrative directory is not content. `HEAD` is the one thing
        // in it this module reads, because rewriting it is what a branch switch
        // does and the working-tree events that follow are exactly the storm
        // the dirty set would collapse into this hint anyway. A linked
        // worktree's `.git` is a *file* pointing elsewhere, so its `HEAD` is
        // outside the watched tree and never arrives — which costs nothing,
        // because the checkout still rewrites the files.
        let bytes = path.as_bytes();
        if bytes == b".git" || bytes.starts_with(b".git/") {
            return (bytes == b".git/HEAD")
                .then(|| ChangeHint::Subtree(RepoPath::from_bytes(Vec::new())));
        }

        // Layer 1, before anything is queued. A denied path produces no hint,
        // so nothing downstream ever holds its name.
        if self
            .denials
            .matched_path_or_any_parents(absolute, false)
            .is_ignore()
            || self
                .denials
                .matched_path_or_any_parents(absolute, true)
                .is_ignore()
        {
            return None;
        }
        if self.temporary.matched(absolute, false).is_ignore() {
            return None;
        }

        match class {
            // A removal cannot be stat-ed to find out what it was, so it is
            // always a subtree hint: on a file that is the file's own row, and
            // on a directory it is every row beneath it. One shape covers both
            // and neither is guessed.
            ChangeClass::Removed => Some(ChangeHint::Subtree(path)),
            ChangeClass::Created | ChangeClass::Modified | ChangeClass::Renamed => {
                match std::fs::symlink_metadata(absolute) {
                    Ok(metadata) if metadata.is_dir() => Some(ChangeHint::Subtree(path)),
                    // Gone between the event and this stat: the rename's source
                    // half, and every atomic save's temporary file that was not
                    // caught by name.
                    Err(_) => Some(ChangeHint::Subtree(path)),
                    Ok(_) => Some(ChangeHint::Path(path)),
                }
            }
            ChangeClass::Rescan => Some(ChangeHint::Overflow),
        }
    }
}

/// The coalesced set of hints waiting to become one scope.
///
/// Bounded by construction: every insertion either lands in a set already
/// smaller than [`WATCH_QUEUE_CAPACITY`], is absorbed by a marker that covers
/// it, or collapses the whole set into a single "everything" flag.
#[derive(Clone, Debug, Default)]
pub struct DirtySet {
    paths: std::collections::BTreeSet<RepoPath>,
    subtrees: std::collections::BTreeSet<RepoPath>,
    everything: bool,
    overflows: u64,
}

impl DirtySet {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Absorbs one hint.
    pub fn insert(&mut self, hint: ChangeHint) {
        if self.everything {
            return;
        }
        match hint {
            ChangeHint::Overflow => self.collapse(),
            ChangeHint::Subtree(directory) => {
                if directory.is_empty() {
                    self.collapse();
                    return;
                }
                if self.covered(&directory) {
                    return;
                }
                // A marker swallows everything beneath it, so the set can only
                // ever shrink when a wider hint arrives.
                self.paths.retain(|path| !directory.contains(path));
                self.subtrees.retain(|scoped| !directory.contains(scoped));
                self.subtrees.insert(directory);
                self.bound();
            }
            ChangeHint::Path(path) => {
                if self.covered(&path) {
                    return;
                }
                self.paths.insert(path);
                self.bound();
            }
        }
    }

    /// How many entries the set is carrying.
    #[must_use]
    pub fn len(&self) -> usize {
        self.paths.len() + self.subtrees.len()
    }

    /// Whether there is nothing to reconcile.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.everything && self.paths.is_empty() && self.subtrees.is_empty()
    }

    /// Whether the set has collapsed into a full pass.
    #[must_use]
    pub const fn is_collapsed(&self) -> bool {
        self.everything
    }

    /// How many times this set has collapsed since it was created.
    ///
    /// The honesty metric for hint quality: a watcher that overflows constantly
    /// is one whose events are not worth much, and the number says so without
    /// anybody having to read a log.
    #[must_use]
    pub const fn overflows(&self) -> u64 {
        self.overflows
    }

    /// Empties the set into the scope it describes.
    ///
    /// A collapsed set is [`ReconcileScope::Full`]. Everything else is a path
    /// list, subtree markers included: a scope entry that is a directory covers
    /// everything beneath it and is *not* force-hashed, which is exactly the
    /// difference between "this file was edited" and "this tree was checked
    /// out".
    #[must_use]
    pub fn take(&mut self) -> ReconcileScope {
        if self.everything {
            self.everything = false;
            self.paths.clear();
            self.subtrees.clear();
            return ReconcileScope::Full;
        }
        let entries = std::mem::take(&mut self.paths)
            .into_iter()
            .chain(std::mem::take(&mut self.subtrees));
        ReconcileScope::paths(entries)
    }

    /// Whether a marker already covers this path.
    fn covered(&self, path: &RepoPath) -> bool {
        self.subtrees.iter().any(|scoped| scoped.contains(path))
    }

    /// Collapses everything queued into a single full pass.
    fn collapse(&mut self) {
        self.paths.clear();
        self.subtrees.clear();
        self.everything = true;
        self.overflows = self.overflows.saturating_add(1);
    }

    /// Enforces the capacity, which is the only thing that can grow the set.
    fn bound(&mut self) {
        if self.len() > WATCH_QUEUE_CAPACITY {
            self.collapse();
        }
    }
}

/// What a watch is currently able to do.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum WatchState {
    /// A backend is delivering events.
    Watching,
    /// No backend could be established; hints arrive only from callers.
    ///
    /// Not a failure of the index. Everything still works through the startup
    /// sweep and on-demand reconciles; what is lost is latency, not
    /// correctness.
    Degraded {
        /// Stable discriminant of the failure that caused the degradation.
        kind: &'static str,
        /// Human-readable explanation.
        detail: String,
    },
    /// The service has been stopped.
    Stopped,
}

/// What a watch is doing right now, answered without waiting on the worker.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct WatchStatus {
    /// Whether events are arriving, and why not when they are not.
    pub state: WatchState,
    /// Entries the dirty set is carrying.
    pub queue_depth: usize,
    /// Whether the queue has collapsed and the next pass is a full one.
    pub collapsed: bool,
    /// How many times the queue has collapsed since the watch started.
    pub overflows: u64,
    /// Whether a reconcile is running right now.
    pub reconciling: bool,
    /// Passes that have finished, successfully or not.
    pub passes: u64,
    /// Generation the last successful pass published.
    pub generation: u64,
}

/// Something a watch did, for a surface that wants to say so.
///
/// Counts and scope kinds, never a path list — except the paths a
/// [`ReconcileReport`] carries for re-queueing, which are the watcher's own and
/// have already survived layer 1.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum WatchEvent {
    /// A pass is beginning.
    Started {
        /// What it will cover.
        scope: ReconcileScope,
    },
    /// A pass finished and published its generation.
    Finished(Box<ReconcileReport>),
    /// A pass failed. The previous generation is still answering.
    Failed {
        /// Stable discriminant of the failure.
        kind: &'static str,
        /// Human-readable explanation.
        detail: String,
    },
    /// Events stopped arriving, or never started.
    Degraded {
        /// Stable discriminant of the failure.
        kind: &'static str,
        /// Human-readable explanation.
        detail: String,
    },
}

/// Where a watch reports what it did.
///
/// Named rather than written out at its two use sites: it is one contract —
/// called on the worker thread, so an observer that blocks stops indexing — and
/// a second spelling of it is a second place for that to be forgotten.
type Observer = Arc<dyn Fn(&WatchEvent) + Send + Sync>;

/// How a watch is set up.
#[derive(Clone)]
pub struct WatchOptions {
    quiescence: Duration,
    watch_filesystem: bool,
    startup_sweep: bool,
    observer: Option<Observer>,
}

impl std::fmt::Debug for WatchOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WatchOptions")
            .field("quiescence", &self.quiescence)
            .field("watch_filesystem", &self.watch_filesystem)
            .field("startup_sweep", &self.startup_sweep)
            .field("observer", &self.observer.is_some())
            .finish()
    }
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            quiescence: QUIESCENCE_WINDOW,
            watch_filesystem: true,
            startup_sweep: true,
            observer: None,
        }
    }
}

impl WatchOptions {
    /// The published defaults: watch, sweep at startup, report nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the quiescence window.
    #[must_use]
    pub const fn with_quiescence(mut self, window: Duration) -> Self {
        self.quiescence = window;
        self
    }

    /// Establishes no filesystem watcher, so hints arrive only from callers.
    ///
    /// The supported way to run degraded, and the way a test proves that events
    /// are not truth: the same edits are found by the startup sweep and by an
    /// on-demand reconcile with no backend in the process at all.
    #[must_use]
    pub const fn without_filesystem_events(mut self) -> Self {
        self.watch_filesystem = false;
        self
    }

    /// Skips the startup sweep.
    ///
    /// Only for a caller that has just reconciled the worktree itself. Skipping
    /// it otherwise means every change made while this process was not running
    /// stays invisible until something else asks.
    #[must_use]
    pub const fn without_startup_sweep(mut self) -> Self {
        self.startup_sweep = false;
        self
    }

    /// Reports every pass to `observer`.
    ///
    /// Called on the worker thread, so an observer that blocks stops indexing.
    #[must_use]
    pub fn observed_by(mut self, observer: impl Fn(&WatchEvent) + Send + Sync + 'static) -> Self {
        self.observer = Some(Arc::new(observer));
        self
    }
}

/// One worktree's watch: a hint source, a queue, and the thread that drains it.
///
/// Dropping one stops it. The watcher is released first so no further events
/// arrive, then the worker is asked to stop and given [`SHUTDOWN_DEADLINE`] to
/// finish the pass it is in.
pub struct WatchService {
    shared: Arc<Shared>,
    watcher: Option<RecommendedWatcher>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for WatchService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WatchService")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

/// Everything the worker and the hint sources both touch.
struct Shared {
    engine: Arc<ContextEngine>,
    queue: Mutex<Queue>,
    woken: Condvar,
    cancellation: Cancellation,
    quiescence: Duration,
    observer: Option<Observer>,
    passes: AtomicU64,
    generation: AtomicU64,
}

/// The mutable half, behind one mutex.
struct Queue {
    dirty: DirtySet,
    last_arrival: Option<Instant>,
    state: WatchState,
    reconciling: bool,
    stopping: bool,
    finished: bool,
}

impl WatchService {
    /// Starts watching the worktree `engine` serves.
    ///
    /// Runs the startup sweep before the first hint is drained, because that is
    /// the recovery for everything that changed while this process was not
    /// running — and it is incremental, comparing metadata against stored rows
    /// rather than rebuilding. The sweep runs on the worker, so this call
    /// returns as soon as the watcher is established.
    ///
    /// A backend that cannot be established is **not** an error. The service
    /// starts [`Degraded`](WatchState::Degraded), reports the reason, and still
    /// sweeps and still accepts [`hint`](Self::hint) — losing latency and
    /// nothing else.
    ///
    /// # Errors
    ///
    /// [`WatchError::WatchRootMissing`] when the worktree root is not a
    /// directory, which is the one condition under which there is nothing to
    /// watch and nothing to sweep.
    pub fn start(engine: Arc<ContextEngine>, options: WatchOptions) -> Result<Self, WatchError> {
        let root = engine.worktree_root().to_path_buf();
        if !root.is_dir() {
            return Err(WatchError::WatchRootMissing { path: root });
        }

        let shared = Arc::new(Shared {
            engine,
            queue: Mutex::new(Queue {
                dirty: DirtySet::new(),
                last_arrival: None,
                state: WatchState::Watching,
                // Set before the worker exists, so a caller that asks whether
                // the watch is quiet between `start` and the worker's first
                // instruction is told the truth. Reading it from the worker
                // alone would answer "quiet" for the whole of that window and
                // let a test — or a surface — read an index nothing has swept.
                reconciling: options.startup_sweep,
                stopping: false,
                finished: false,
            }),
            woken: Condvar::new(),
            cancellation: Cancellation::default(),
            quiescence: options.quiescence,
            observer: options.observer.clone(),
            passes: AtomicU64::new(0),
            generation: AtomicU64::new(0),
        });

        let watcher = if options.watch_filesystem {
            match establish(&root, &shared) {
                Ok(watcher) => Some(watcher),
                Err(error) => {
                    shared.degrade(&error);
                    None
                }
            }
        } else {
            shared.degrade(&WatchError::WatcherUnavailable {
                path: root.clone(),
                reason: "filesystem events were not requested".to_owned(),
            });
            None
        };

        let worker = {
            let shared = Arc::clone(&shared);
            let sweep = options.startup_sweep;
            std::thread::Builder::new()
                .name("harkness-context-index".to_owned())
                .spawn(move || shared.run(sweep))
                .map_err(|error| WatchError::WatcherUnavailable {
                    path: root,
                    reason: error.to_string(),
                })?
        };

        Ok(Self {
            shared,
            watcher,
            worker: Some(worker),
        })
    }

    /// Offers one hint from a source of the caller's own.
    ///
    /// The seam a deterministic test drives, and the one a future editor
    /// integration plugs into. It is accepted whether or not a backend is
    /// running, and it is a suggestion either way.
    pub fn hint(&self, hint: ChangeHint) {
        self.shared.offer(hint);
    }

    /// What the watch is doing, without waiting on the worker.
    #[must_use]
    pub fn status(&self) -> WatchStatus {
        self.shared.status()
    }

    /// The worktree root being watched.
    #[must_use]
    pub fn worktree_root(&self) -> &Path {
        self.shared.engine.worktree_root()
    }

    /// Blocks until the queue is empty and no pass is running.
    ///
    /// Returns whether it became quiet inside `timeout`. A caller that has just
    /// written a file and wants to read the index it produced needs this;
    /// without it the only alternative is sleeping for longer than the
    /// quiescence window and hoping, which is how a suite becomes flaky.
    pub fn wait_until_quiet(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut queue = lock(&self.shared.queue);
        loop {
            if queue.finished || (queue.dirty.is_empty() && !queue.reconciling) {
                return true;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (guard, _) = self
                .shared
                .woken
                .wait_timeout(queue, remaining.min(POLL_INTERVAL))
                .unwrap_or_else(PoisonError::into_inner);
            queue = guard;
        }
    }

    /// Stops the watch, draining or abandoning the pass in flight.
    ///
    /// Idempotent, and what [`Drop`] does.
    pub fn stop(&mut self) {
        // The backend goes first: a watcher still delivering into a queue
        // nobody drains is a slow leak for as long as the shutdown takes.
        drop(self.watcher.take());
        {
            let mut queue = lock(&self.shared.queue);
            queue.stopping = true;
            queue.state = WatchState::Stopped;
        }
        self.shared.woken.notify_all();

        let deadline = Instant::now() + SHUTDOWN_DEADLINE;
        {
            let mut queue = lock(&self.shared.queue);
            while !queue.finished {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    break;
                };
                let (guard, _) = self
                    .shared
                    .woken
                    .wait_timeout(queue, remaining.min(POLL_INTERVAL))
                    .unwrap_or_else(PoisonError::into_inner);
                queue = guard;
            }
        }
        // Past the deadline the pass is abandoned rather than waited out. Its
        // batch was never visible and reconciliation is idempotent, so the next
        // startup sweep re-derives the same work from the filesystem.
        self.shared.cancellation.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for WatchService {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Shared {
    /// Adds one hint and wakes the worker.
    fn offer(&self, hint: ChangeHint) {
        {
            let mut queue = lock(&self.queue);
            if queue.stopping {
                return;
            }
            queue.dirty.insert(hint);
            queue.last_arrival = Some(Instant::now());
        }
        self.woken.notify_all();
    }

    /// Records that no backend is delivering events.
    fn degrade(&self, error: &WatchError) {
        {
            let mut queue = lock(&self.queue);
            queue.state = WatchState::Degraded {
                kind: error.kind(),
                detail: error.to_string(),
            };
        }
        self.report(&WatchEvent::Degraded {
            kind: error.kind(),
            detail: error.to_string(),
        });
    }

    fn status(&self) -> WatchStatus {
        let queue = lock(&self.queue);
        WatchStatus {
            state: queue.state.clone(),
            queue_depth: queue.dirty.len(),
            collapsed: queue.dirty.is_collapsed(),
            overflows: queue.dirty.overflows(),
            reconciling: queue.reconciling,
            passes: self.passes.load(Ordering::Acquire),
            generation: self.generation.load(Ordering::Acquire),
        }
    }

    fn report(&self, event: &WatchEvent) {
        if let Some(observer) = self.observer.as_ref() {
            observer(event);
        }
    }

    /// The worker: sweep once, then drain quiesced scopes until stopped.
    fn run(&self, startup_sweep: bool) {
        if startup_sweep {
            self.pass(ReconcileScope::Full);
        }
        while let Some(scope) = self.wait_for_scope() {
            self.pass(scope);
        }
        {
            let mut queue = lock(&self.queue);
            queue.finished = true;
        }
        self.woken.notify_all();
    }

    /// Waits out the quiescence window and takes what is queued.
    ///
    /// `None` means the service is stopping. A scope is drained only when no
    /// hint has arrived for the whole window, so a burst of writes is one pass
    /// however many events it produced.
    fn wait_for_scope(&self) -> Option<ReconcileScope> {
        let mut queue = lock(&self.queue);
        loop {
            if queue.stopping || self.cancellation.is_cancelled() {
                return None;
            }
            let ready = queue
                .last_arrival
                .is_some_and(|arrival| arrival.elapsed() >= self.quiescence);
            if ready && !queue.dirty.is_empty() {
                queue.last_arrival = None;
                queue.reconciling = true;
                return Some(queue.dirty.take());
            }
            if ready {
                // Hints that all collapsed into nothing — every one of them
                // absorbed by a marker, or a set drained by an earlier pass.
                queue.last_arrival = None;
            }
            let wait = queue.last_arrival.map_or(POLL_INTERVAL, |arrival| {
                self.quiescence
                    .saturating_sub(arrival.elapsed())
                    .max(Duration::from_millis(1))
                    .min(POLL_INTERVAL)
            });
            let (guard, _) = self
                .woken
                .wait_timeout(queue, wait)
                .unwrap_or_else(PoisonError::into_inner);
            queue = guard;
        }
    }

    /// Runs one reconcile and reports what it did.
    fn pass(&self, scope: ReconcileScope) {
        {
            let mut queue = lock(&self.queue);
            queue.reconciling = true;
        }
        self.report(&WatchEvent::Started {
            scope: scope.clone(),
        });
        let outcome = self.engine.reconcile(&scope, &self.cancellation);
        match outcome {
            Ok(report) => {
                self.generation.store(report.generation, Ordering::Release);
                // A path that kept moving while it was read is offered back
                // rather than dropped: its row was left alone, so nothing else
                // will notice it changed.
                for path in &report.requeued {
                    self.offer(ChangeHint::Path(path.clone()));
                }
                self.report(&WatchEvent::Finished(Box::new(report)));
            }
            Err(ContextEngineError::Cancelled) => {}
            Err(error) => {
                // A pass the *cache* refused rather than the worktree is one
                // worth having again: another process held the write lock, or
                // published this worktree while this pass was open. Both clear,
                // and the scope was drained when the pass started — so dropping
                // it would leave exactly the paths something told us about
                // unexamined until the next startup sweep.
                //
                // Every other failure is dropped rather than retried. A cache
                // at its budget or a batch built wrong will refuse the same
                // scope every time, and re-offering it would spend a walk per
                // quiescence window on an answer that is not going to change.
                if matches!(error.kind(), "index_busy" | "index_batch_superseded") {
                    for hint in scope_hints(&scope) {
                        self.offer(hint);
                    }
                }
                self.report(&WatchEvent::Failed {
                    kind: error.kind(),
                    detail: error.to_string(),
                });
            }
        }
        self.passes.fetch_add(1, Ordering::AcqRel);
        {
            let mut queue = lock(&self.queue);
            queue.reconciling = false;
        }
        self.woken.notify_all();
    }
}

/// The hints that put one scope back on the queue, covering exactly what it
/// covered.
///
/// A full pass becomes an overflow, which is the only spelling that survives a
/// set already holding something else and is also the truthful one: a scope
/// that covered everything cannot be narrowed on its way back in. A path list
/// comes back **as paths**, keeping each hint's strength — a retry that
/// downgraded them to subtree markers would drop exactly the suspicion the
/// original hint carried, and the coarse-modification-time case is the one it
/// exists for.
fn scope_hints(scope: &ReconcileScope) -> Vec<ChangeHint> {
    match scope {
        ReconcileScope::Full => vec![ChangeHint::Overflow],
        ReconcileScope::Subtree(directory) => vec![ChangeHint::Subtree(directory.clone())],
        ReconcileScope::Paths(paths) => paths.iter().cloned().map(ChangeHint::Path).collect(),
    }
}

/// Establishes a recursive watcher whose events feed `shared`'s queue.
///
/// Normalization runs on the backend's own delivery thread rather than on a
/// thread of ours. It is one gitignore match and at most one `lstat`, and doing
/// it here is what keeps a denied path from existing anywhere downstream — a
/// queue that held raw events would hold `.env` in it until something drained
/// it.
fn establish(root: &Path, shared: &Arc<Shared>) -> Result<RecommendedWatcher, WatchError> {
    let normalizer = Normalizer::new(root)?;
    let sink = Arc::clone(shared);
    let mut watcher = notify::recommended_watcher(
        move |event: Result<notify::Event, notify::Error>| match event {
            Ok(event) => {
                let Some(class) = classify(&event.kind) else {
                    return;
                };
                let class = if event.need_rescan() {
                    ChangeClass::Rescan
                } else {
                    class
                };
                let change = FilesystemChange {
                    paths: event.paths,
                    class,
                };
                for hint in normalizer.normalize(&change) {
                    sink.offer(hint);
                }
            }
            // A backend error is not a reason to stop: it is a reason to stop
            // trusting what has been delivered, which is what an overflow says.
            Err(_) => sink.offer(ChangeHint::Overflow),
        },
    )
    .map_err(|error| watch_failure(root, &error))?;
    watcher
        .watch(root, RecursiveMode::Recursive)
        .map_err(|error| watch_failure(root, &error))?;
    Ok(watcher)
}

/// Maps one backend event kind onto the vocabulary the normalizer speaks.
///
/// `None` for the kinds that say nothing about content: an access, and the
/// catch-all a backend emits when it does not know. The catch-all is
/// deliberately *not* an overflow — `notify` reports a genuine loss of tracking
/// through `need_rescan`, and treating every unknown kind as a rescan would make
/// a platform with coarse events reconcile the whole repository continuously.
fn classify(kind: &EventKind) -> Option<ChangeClass> {
    match kind {
        EventKind::Create(
            CreateKind::Any | CreateKind::File | CreateKind::Folder | CreateKind::Other,
        ) => Some(ChangeClass::Created),
        EventKind::Remove(
            RemoveKind::Any | RemoveKind::File | RemoveKind::Folder | RemoveKind::Other,
        ) => Some(ChangeClass::Removed),
        EventKind::Modify(ModifyKind::Name(
            RenameMode::Any
            | RenameMode::To
            | RenameMode::From
            | RenameMode::Both
            | RenameMode::Other,
        )) => Some(ChangeClass::Renamed),
        EventKind::Modify(_) => Some(ChangeClass::Modified),
        EventKind::Access(_) | EventKind::Any | EventKind::Other => None,
    }
}

/// Separates a missing root from every other reason a backend refused.
fn watch_failure(root: &Path, error: &notify::Error) -> WatchError {
    if matches!(error.kind, notify::ErrorKind::PathNotFound) {
        return WatchError::WatchRootMissing {
            path: root.to_path_buf(),
        };
    }
    WatchError::WatcherUnavailable {
        path: root.to_path_buf(),
        reason: error.to_string(),
    }
}

/// Takes the queue lock, adopting the contents even if a holder panicked.
///
/// A panic somewhere above says nothing about which paths are dirty, and
/// refusing to use the queue afterwards would stop a watch permanently over one
/// failure it can simply carry on from.
fn lock(queue: &Mutex<Queue>) -> MutexGuard<'_, Queue> {
    queue.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
