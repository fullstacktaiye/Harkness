//! The context engine's service boundary.
//!
//! [`ContextEngine`] is the one way anything reaches context features. Both
//! front ends go through it, so "what the index says" cannot differ between the
//! command line and the application, and every later retrieval issue plugs into
//! a signature that already exists rather than inventing an entry point, a
//! threading rule, and a storage location of its own.
//!
//! # Threading and cancellation
//!
//! **Every method here is blocking and runs on the caller's thread.** There is
//! no async runtime in the workspace (ADR-0003) and this crate starts none. A
//! caller on a UI thread must move the call to a worker itself — the engine
//! cannot do it for them, and pretending otherwise is how a cold index build
//! freezes a window.
//!
//! Every method that can take time takes a [`Cancellation`] and observes it
//! within the workspace's 250 ms visibility target. An already-cancelled token
//! launches nothing at all.
//!
//! An engine is safe to share: `&self` methods may run concurrently from any
//! number of threads. Indexing is serialized *inside* the cache, so concurrent
//! readers never see a half-written index and never have to coordinate.
//!
//! # What is here and what is not
//!
//! The eight facade methods are the whole retrieval surface.
//! [`snapshot`](ContextEngine::snapshot), [`inventory`](ContextEngine::inventory)
//! and [`search`](ContextEngine::search) are implemented, because [#109], [#112]
//! and [#116] landed what they need; the other five return
//! [`ContextEngineError::NotYetAvailable`] naming the missing feature. That is a
//! real, tested refusal rather than a `todo!()`, so a caller written against the
//! seam now gets a typed answer and the issue that implements a method deletes a
//! branch rather than a panic.
//!
//! [`search`](ContextEngine::search) has a second spelling,
//! [`search_under`](ContextEngine::search_under), and the difference is not a
//! convenience: the first captures a workspace snapshot and the second takes one
//! the caller already holds. A run records a snapshot before it starts work, and
//! stamping everything it retrieves with that one is what makes its evidence
//! describe a single moment rather than one per query — a capture also costs
//! several times what a scan does on a repository of any size.
//!
//! Beside them is the cache: [`reindex`](ContextEngine::reindex) fills it,
//! [`index_status`](ContextEngine::index_status) polls it without waiting on the
//! writer, and [`dispose_index`](ContextEngine::dispose_index) throws it away.
//! Those are not retrieval features and never fabricate one — they are the
//! machinery every retrieval feature will read.
//!
//! The engine returns typed values and persists no *evidence*. Turning a
//! snapshot or a pack into evidence is the caller's job in `harkness-runtime`
//! ([#122], [#123]); what the engine writes is the disposable cache, which is a
//! different store by ADR-0004 and which ADR-0001's dependency direction keeps
//! separate structurally rather than by intention, since this crate cannot name
//! the runtime.
//!
//! [#109]: https://github.com/fullstacktaiye/harkness/issues/109
//! [#112]: https://github.com/fullstacktaiye/harkness/issues/112
//! [#116]: https://github.com/fullstacktaiye/harkness/issues/116
//! [#122]: https://github.com/fullstacktaiye/harkness/issues/122
//! [#123]: https://github.com/fullstacktaiye/harkness/issues/123

use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use harkness_core::ProjectId;
use harkness_git::{Cancellation, GitService, HeadState};

use crate::chunk::{FileVersion, chunk_file};
use crate::digest::{Sha256Hex, empty_path_set_digest};
use crate::error::{ContextDomainError, ContextEngineError};
use crate::ids::ChunkId;
use crate::index::{
    self, BatchReceipt, BatchScope, CacheRecreation, ExpectedVersions, ForgetReport,
    IndexAvailability, IndexCache, IndexCounts, IndexReport, IndexStatus, IndexedChunk,
    IndexedFile, IndexedPage, RecreationReason, WorktreeKey,
};
use crate::inventory::{
    FileInventory, GLOBAL_IGNORE_FILE, InventoryBuilder, InventoryEntry, InventoryPolicy,
};
use crate::path::RepoPath;
use crate::probe::FilesystemProbe;
use crate::reconcile::{ReconcileReport, ReconcileScope, Reconciler};
use crate::search::{Scan, SearchQuery, SearchResponse};
use crate::snapshot::{Capture, CaptureRequest, WorkspaceSnapshot};
use crate::watch::{WatchOptions, WatchService};

/// A group of engine settings a repository may narrow.
///
/// Repository content is untrusted (ADR-0006): it may make the engine see
/// *less*, never more. The groups are named so provenance can be recorded per
/// group rather than as one flag over a whole configuration, because a
/// repository that tightened its ignore rules has said nothing about how
/// retrieval is scored.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum SettingGroup {
    /// Which files are eligible at all ([#112]).
    ///
    /// [#112]: https://github.com/fullstacktaiye/harkness/issues/112
    Ignore,
    /// How retrieval selects and bounds what it returns.
    Retrieval,
    /// Which instruction files are discovered and honored ([#120]).
    ///
    /// [#120]: https://github.com/fullstacktaiye/harkness/issues/120
    Instructions,
}

impl SettingGroup {
    /// Every group, in declaration order.
    pub const ALL: &'static [Self] = &[Self::Ignore, Self::Retrieval, Self::Instructions];

    /// Stable spelling for diagnostics and payloads.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ignore => "ignore",
            Self::Retrieval => "retrieval",
            Self::Instructions => "instructions",
        }
    }
}

impl std::fmt::Display for SettingGroup {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Where one group's effective settings came from.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum SettingOrigin {
    /// User and global settings alone decided the group.
    #[default]
    Global,
    /// A repository setting narrowed the group, and nothing widened it.
    RepositoryTightened,
    /// A repository setting tried to widen the group and was discarded.
    ///
    /// The effective value is still the tightened one — the attempt failed
    /// closed — but a caller reading content selected under this group should
    /// know the repository asked for more than it was given.
    RepositoryWideningRefused,
}

/// Per-group provenance of an engine configuration.
///
/// [#120] is what enforces the tightening-only rule while merging; this type is
/// what carries the answer to a caller that never saw the merge. Recording the
/// refusal rather than dropping it is the point: a repository that tried to
/// re-include a denied path is a fact worth surfacing, and a silently discarded
/// pattern looks identical to one that was never written.
///
/// [#120]: https://github.com/fullstacktaiye/harkness/issues/120
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SettingOrigins {
    ignore: SettingOrigin,
    retrieval: SettingOrigin,
    instructions: SettingOrigin,
}

impl SettingOrigins {
    /// Where `group`'s effective settings came from.
    #[must_use]
    pub const fn origin(&self, group: SettingGroup) -> SettingOrigin {
        match group {
            SettingGroup::Ignore => self.ignore,
            SettingGroup::Retrieval => self.retrieval,
            SettingGroup::Instructions => self.instructions,
        }
    }

    /// Records where `group` came from.
    #[must_use]
    pub const fn recording(mut self, group: SettingGroup, origin: SettingOrigin) -> Self {
        match group {
            SettingGroup::Ignore => self.ignore = origin,
            SettingGroup::Retrieval => self.retrieval = origin,
            SettingGroup::Instructions => self.instructions = origin,
        }
        self
    }

    /// Whether every repository contribution to `group` was a tightening.
    #[must_use]
    pub const fn tightened_only(&self, group: SettingGroup) -> bool {
        !matches!(self.origin(group), SettingOrigin::RepositoryWideningRefused)
    }
}

/// Everything an engine needs that it cannot read from the workspace.
///
/// Built by the caller from user and global settings plus repository settings
/// that may only tighten; the engine never reads a setting out of the
/// repository itself, because repository content cannot be allowed to decide
/// how much of the repository is visible.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextEngineConfig {
    project_id: ProjectId,
    worktree_root: PathBuf,
    data_dir: PathBuf,
    expected_versions: ExpectedVersions,
    origins: SettingOrigins,
    config_generation: u64,
    instructions_digest: Sha256Hex,
}

impl ContextEngineConfig {
    /// Addresses the worktree at `worktree_root` within `project_id`.
    ///
    /// `data_dir` is the Harkness data directory; the cache lands beneath its
    /// reserved [`CONTEXT_DIRECTORY`](harkness_core::CONTEXT_DIRECTORY) child
    /// and nowhere else. The path is
    /// derived from the repository rather than supplied, so there is no
    /// traversal surface here for a caller to get wrong.
    #[must_use]
    pub fn new(
        project_id: ProjectId,
        worktree_root: impl Into<PathBuf>,
        data_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            project_id,
            worktree_root: worktree_root.into(),
            data_dir: data_dir.into(),
            expected_versions: ExpectedVersions::current(),
            origins: SettingOrigins::default(),
            config_generation: 0,
            instructions_digest: empty_path_set_digest(),
        }
    }

    /// Opens caches written under versions other than this build's.
    ///
    /// Production always wants [`ExpectedVersions::current`]; this exists so a
    /// test can stand where a future build will.
    #[must_use]
    pub fn with_expected_versions(mut self, versions: ExpectedVersions) -> Self {
        self.expected_versions = versions;
        self
    }

    /// Records where one group of settings came from.
    #[must_use]
    pub const fn with_setting_origin(mut self, group: SettingGroup, origin: SettingOrigin) -> Self {
        self.origins = self.origins.recording(group, origin);
        self
    }

    /// Records the configuration generation snapshots are taken under.
    ///
    /// Bumped by whoever changes a setting that alters which files are
    /// eligible. It is part of workspace identity because a different view of
    /// one unchanged tree is a different workspace as far as retrieval is
    /// concerned.
    #[must_use]
    pub const fn with_config_generation(mut self, generation: u64) -> Self {
        self.config_generation = generation;
        self
    }

    /// Records the digest of the discovered instruction set ([#120]).
    ///
    /// [#120]: https://github.com/fullstacktaiye/harkness/issues/120
    #[must_use]
    pub fn with_instructions_digest(mut self, digest: Sha256Hex) -> Self {
        self.instructions_digest = digest;
        self
    }

    /// Catalog project this workspace belongs to.
    #[must_use]
    pub const fn project_id(&self) -> ProjectId {
        self.project_id
    }

    /// Worktree the engine reads.
    #[must_use]
    pub fn worktree_root(&self) -> &Path {
        &self.worktree_root
    }

    /// Harkness data directory the cache lives beneath.
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Versions a cache must have been written under to be usable.
    #[must_use]
    pub const fn expected_versions(&self) -> &ExpectedVersions {
        &self.expected_versions
    }

    /// Per-group provenance of these settings.
    #[must_use]
    pub const fn origins(&self) -> SettingOrigins {
        self.origins
    }

    /// Configuration generation snapshots are taken under.
    #[must_use]
    pub const fn config_generation(&self) -> u64 {
        self.config_generation
    }

    /// Digest of the discovered instruction set.
    #[must_use]
    pub const fn instructions_digest(&self) -> &Sha256Hex {
        &self.instructions_digest
    }
}

/// The state of the engine's cache handle.
///
/// It is behind a lock rather than a plain field because
/// [`Unavailable`](Self::Unavailable) has to be recoverable. The commonest way
/// to reach it is another front end holding the cache past the busy timeout at
/// exactly the wrong moment, and a transient five seconds must not disable
/// retrieval — and report every snapshot as taken against no index — for the
/// whole life of the engine. [`ContextEngine::refresh_index`] and
/// [`ContextEngine::dispose_index`] retry the open, which is what makes the
/// second of those the "fix a weird index" action its documentation claims.
#[derive(Debug)]
enum Cache {
    /// Open and usable.
    Ready(Box<IndexCache>),
    /// Could not be prepared; every cache-backed call reports this.
    ///
    /// The engine still opens. A missing, read-only, or too-new cache must not
    /// take the workspace snapshot away with it: snapshot identity is read from
    /// Git and the filesystem, and it is the one thing a run cannot proceed
    /// without.
    Unavailable(ContextEngineError),
}

/// The one entry point to every context feature.
///
/// Read the module documentation for the threading and cancellation contract
/// before calling anything here.
///
/// # Examples
///
/// An engine stands entirely on its own: no run store, no agent, no model, and
/// no network.
///
/// ```
/// use harkness_context::{ContextEngine, ContextEngineConfig};
/// use harkness_core::ProjectId;
/// use harkness_git::Cancellation;
/// use harkness_test_fixtures::{Fixture, initialize_repository};
///
/// let fixture = Fixture::new();
/// let worktree = fixture.directory("workspace");
/// initialize_repository(&worktree);
///
/// let cancellation = Cancellation::default();
/// let engine = ContextEngine::open(
///     ContextEngineConfig::new(ProjectId::new(), &worktree, &fixture.data_dir),
///     &cancellation,
/// )?;
///
/// // The cache was created beneath the data directory, keyed by the
/// // repository rather than by this worktree's path.
/// assert!(engine.cache_root().starts_with(&fixture.data_dir));
/// assert!(engine.index_generation() > 0);
///
/// // Workspace identity needs nothing else in the process.
/// let snapshot = engine.snapshot(&cancellation)?;
/// assert_eq!(snapshot.worktree_root(), std::fs::canonicalize(&worktree)?);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct ContextEngine {
    config: ContextEngineConfig,
    repository_key: String,
    cache_root: PathBuf,
    git: GitService,
    cache: RwLock<Cache>,
}

impl ContextEngine {
    /// Opens the engine for one worktree, creating its cache when needed.
    ///
    /// The cache root is `<data_dir>/context/<repository-key>`, where the key
    /// is the v5 UUID of the canonical Git common directory — the same
    /// derivation the repository lock uses — so every linked worktree of one
    /// repository resolves to one cache.
    ///
    /// Blocking, and cancellation-polled: preparing a cache another front end
    /// is writing waits out that contention, so a caller that gave up is not
    /// made to wait it out too.
    ///
    /// A cache that cannot be prepared does **not** fail this call. The failure
    /// is remembered and reported by every cache-backed method and by
    /// [`index_status`](Self::index_status); a read-only data directory or a
    /// cache written by a newer build costs retrieval, not workspace identity.
    /// It is not permanent either — [`refresh_index`](Self::refresh_index) and
    /// [`dispose_index`](Self::dispose_index) retry the open.
    ///
    /// # Errors
    ///
    /// Returns [`ContextDomainError::WorktreeRootMissing`] when the root is not
    /// a directory and [`ContextDomainError::RepositoryUnavailable`] when it is
    /// not a Git worktree — which is also the answer for a `ProjectSource::Local`
    /// folder that was never a repository.
    pub fn open(
        config: ContextEngineConfig,
        cancellation: &Cancellation,
    ) -> Result<Self, ContextEngineError> {
        if !config.worktree_root.is_dir() {
            return Err(ContextDomainError::WorktreeRootMissing {
                path: config.worktree_root.clone(),
            }
            .into());
        }
        // Canonicalized once, here, for the reason `WorkspaceKey` is never
        // built from a lexical path: `/w/foo`, `/w/foo/` and a path through a
        // symlink are one checkout, and an engine registry comparing the raw
        // spellings would hold two engines for it and evict each with the
        // other. `WorkspaceSnapshot::capture` canonicalizes too, so the
        // recorded root already reads this way in every snapshot.
        let mut config = config;
        config.worktree_root = std::fs::canonicalize(&config.worktree_root).map_err(|error| {
            ContextDomainError::RepositoryUnavailable {
                path: config.worktree_root.clone(),
                reason: error.to_string(),
            }
        })?;
        let repository_key =
            harkness_git::repository_identity(&config.worktree_root).map_err(|error| {
                ContextDomainError::RepositoryUnavailable {
                    path: config.worktree_root.clone(),
                    reason: error.to_string(),
                }
            })?;
        // Composed by `index` rather than here, so the eviction sweep and the
        // engine cannot disagree about which directory a repository's cache is.
        let cache_root = index::cache_root(&config.data_dir, &repository_key);
        let cache = open_cache(
            &cache_root,
            &config.expected_versions,
            &repository_key,
            cancellation,
        );
        let git = GitService::new(&config.worktree_root, &config.data_dir);
        Ok(Self {
            config,
            repository_key,
            cache_root,
            git,
            cache: RwLock::new(cache),
        })
    }

    /// The configuration this engine was opened with.
    #[must_use]
    pub const fn config(&self) -> &ContextEngineConfig {
        &self.config
    }

    /// Catalog project this engine serves.
    #[must_use]
    pub const fn project_id(&self) -> ProjectId {
        self.config.project_id
    }

    /// Worktree this engine reads.
    #[must_use]
    pub fn worktree_root(&self) -> &Path {
        &self.config.worktree_root
    }

    /// The repository key the cache is filed under.
    ///
    /// Equal for every linked worktree of one repository, which is what makes
    /// the expensive content-addressed work shared rather than duplicated.
    #[must_use]
    pub fn repository_key(&self) -> &str {
        &self.repository_key
    }

    /// Directory holding this repository's cache.
    ///
    /// Deleting it is always safe (ADR-0004): it costs warm-up time and loses
    /// no run history, provenance, or approval record.
    #[must_use]
    pub fn cache_root(&self) -> &Path {
        &self.cache_root
    }

    /// Generation of the cache snapshots are taken against; `0` when there is
    /// none.
    #[must_use]
    pub fn index_generation(&self) -> u64 {
        match &*read(&self.cache) {
            Cache::Ready(cache) => cache.generation(),
            Cache::Unavailable(_) => 0,
        }
    }

    // -- the facade ---------------------------------------------------------

    /// Reads this workspace's identity.
    ///
    /// Blocking, and the token is polled between status entries, while an
    /// untracked directory is walked, and between the blocks of one file, so a
    /// cancelled capture returns promptly whatever the size of the workspace.
    ///
    /// **This persists nothing.** A snapshot becomes evidence only when a
    /// caller in `harkness-runtime` records it, which is what emits the
    /// `snapshot_captured` event; the engine has no way to reach the run store
    /// and must not grow one.
    ///
    /// # Errors
    ///
    /// The capture failures of [`WorkspaceSnapshot::capture`], carried whole.
    pub fn snapshot(
        &self,
        cancellation: &Cancellation,
    ) -> Result<WorkspaceSnapshot, ContextEngineError> {
        self.snapshot_with_diagnostics(cancellation)
            .map(|capture| capture.snapshot)
    }

    /// Captures, and reports what reading the workspace involved.
    ///
    /// The diagnostics are what [#133] renders when a capture is slow or could
    /// not read part of a tree, without the surface re-walking the workspace to
    /// find out.
    ///
    /// # Errors
    ///
    /// The capture failures of [`WorkspaceSnapshot::capture`], carried whole.
    ///
    /// [#133]: https://github.com/fullstacktaiye/harkness/issues/133
    pub fn snapshot_with_diagnostics(
        &self,
        cancellation: &Cancellation,
    ) -> Result<Capture, ContextEngineError> {
        if cancellation.is_cancelled() {
            return Err(ContextEngineError::Cancelled);
        }
        let request = CaptureRequest::new(self.config.project_id)
            .with_instructions_digest(self.config.instructions_digest.clone())
            .with_config_generation(self.config.config_generation)
            .with_index_generation(self.index_generation());
        let probe = FilesystemProbe::new(&self.config.worktree_root);
        Ok(WorkspaceSnapshot::capture_with_diagnostics(
            &request,
            &self.git,
            &probe,
            cancellation,
        )?)
    }

    /// The classified, bounded set of files eligible for indexing.
    ///
    /// Captures a snapshot and walks it: an inventory names the capture it was
    /// built for, and the two must come from one call rather than from a
    /// caller pairing them. The walk's own bounds and layers are documented on
    /// [`InventoryBuilder`].
    ///
    /// The policy comes from the engine's configuration and never from the
    /// request, which is why [`InventoryRequest`] carries no ignore settings:
    /// the global layer is `<data_dir>/`[`GLOBAL_IGNORE_FILE`] — the one place
    /// that path is composed — and the repository's own layer is found inside
    /// the worktree, where it may only tighten.
    ///
    /// # Errors
    ///
    /// [`ContextEngineError::Cancelled`] if the token is observed, and
    /// [`ContextEngineError::Inventory`] for a root that cannot be walked or a
    /// rule file that cannot be applied.
    pub fn inventory(
        &self,
        request: &InventoryRequest,
        cancellation: &Cancellation,
    ) -> Result<FileInventory, ContextEngineError> {
        // `InventoryRequest` is empty today and taken by reference anyway, so
        // that the field a later issue adds is not a breaking change here.
        let _ = request;
        let snapshot = self.snapshot(cancellation)?;
        Ok(InventoryBuilder::build(
            &snapshot,
            &self.inventory_policy(),
            cancellation,
        )?)
    }

    /// The walk policy this engine's configuration implies.
    fn inventory_policy(&self) -> InventoryPolicy {
        InventoryPolicy::new().with_global_ignore(self.config.data_dir.join(GLOBAL_IGNORE_FILE))
    }

    /// Deterministic filename and lexical search over the index.
    ///
    /// The universe is the index rather than the filesystem, so what a query
    /// can reach was decided by the inventory's exclusion layers and not by
    /// this call: a denied path, a secret-classified file and an ignored one
    /// are not rows to be filtered but rows that were never written. A worktree
    /// the cache has never seen is therefore [`SearchError::IndexUnavailable`]
    /// rather than an empty answer — build the index with
    /// [`reindex`](Self::reindex) first.
    ///
    /// This one captures a snapshot and stamps the matches with its id, which
    /// is the convenient shape and the expensive one: a capture reads the whole
    /// workspace and, on a repository of any size, costs several times what the
    /// scan does. A caller that already holds a capture — a run, which recorded
    /// one before it started work — should use
    /// [`search_under`](Self::search_under) instead, and not only to save the
    /// time: a run that searched five times would otherwise stamp its evidence
    /// with five workspace states for one moment.
    ///
    /// Blocking, and cancellation-polled between files. A cancelled search
    /// yields no partial page: a caller that stopped one did not ask for the
    /// prefix of an answer.
    ///
    /// The whole contract — the ordering, the cursor, the budgets and the
    /// omissions — is on the [`search`](crate::search) module, and
    /// `docs/context-search.md` is the reference.
    ///
    /// # Errors
    ///
    /// [`ContextEngineError::Search`] for a pattern, filter, capability or
    /// cursor a query cannot be run with, [`ContextEngineError::Cancelled`],
    /// the capture failures of [`snapshot`](Self::snapshot), and the cache's
    /// own read failures.
    ///
    /// [`SearchError::IndexUnavailable`]: crate::SearchError::IndexUnavailable
    pub fn search(
        &self,
        query: &SearchQuery,
        cancellation: &Cancellation,
    ) -> Result<SearchResponse, ContextEngineError> {
        let worktree = self.worktree_key();
        // Refused before the capture rather than after it. A capture reads the
        // whole workspace, and a query that cannot run — an empty pattern, a
        // regular expression without the capability, a cursor from a rebuilt
        // index — must not cost one. Left the other way round it is also an
        // amplification lever: repeating a refusable query would drive an
        // unbounded number of full workspace reads for an answer that was never
        // going to change.
        let plan = self.with_cache(|cache| self.scan(cache, &worktree).prepare(query))?;
        let snapshot = self.snapshot(cancellation)?;
        self.with_cache(|cache| {
            self.scan(cache, &worktree)
                .run(query, &plan, snapshot.id(), cancellation)
        })
    }

    /// The same search, stamped with a capture the caller already holds.
    ///
    /// The pairing is exactly [`InventoryBuilder::build`]'s: a walk and a search
    /// are both readings of a workspace, and the workspace they read is named
    /// by a capture rather than by a path a caller wrote. Passing one in is what
    /// lets everything a run retrieves carry the one snapshot the run recorded,
    /// so its evidence describes a single moment instead of one per query.
    ///
    /// The capture must be of *this* engine's worktree. A foreign one is refused
    /// rather than used, because provenance built from it would be well formed
    /// and false.
    ///
    /// # Errors
    ///
    /// [`ContextEngineError::ForeignSnapshot`] when the capture describes
    /// another checkout, and otherwise the failures of
    /// [`search`](Self::search) apart from the capture's own.
    pub fn search_under(
        &self,
        snapshot: &WorkspaceSnapshot,
        query: &SearchQuery,
        cancellation: &Cancellation,
    ) -> Result<SearchResponse, ContextEngineError> {
        if snapshot.worktree_root() != self.config.worktree_root {
            return Err(ContextEngineError::ForeignSnapshot {
                expected: self.config.worktree_root.clone(),
                found: snapshot.worktree_root().to_path_buf(),
            });
        }
        let worktree = self.worktree_key();
        self.with_cache(|cache| {
            let scan = self.scan(cache, &worktree);
            let plan = scan.prepare(query)?;
            scan.run(query, &plan, snapshot.id(), cancellation)
        })
    }

    /// The scan this engine's configuration implies, over `cache`.
    fn scan<'engine>(
        &'engine self,
        cache: &'engine IndexCache,
        worktree: &'engine WorktreeKey,
    ) -> Scan<'engine> {
        Scan {
            cache,
            worktree,
            root: &self.config.worktree_root,
        }
    }

    /// The content of one indexed chunk.
    ///
    /// Distinct from [`indexed_chunks`](Self::indexed_chunks), which answers
    /// *where* a chunk is: the index holds paths, digests and ranges and never
    /// text, so returning content means re-reading the working tree and
    /// deciding what to do when it has moved since the chunk was recorded.
    /// That decision is retrieval's, and it arrives with [#123].
    ///
    /// # Errors
    ///
    /// [`ContextEngineError::NotYetAvailable`] until [#123] lands.
    ///
    /// [#123]: https://github.com/fullstacktaiye/harkness/issues/123
    pub fn read_chunk(
        &self,
        id: &ChunkId,
        cancellation: &Cancellation,
    ) -> Result<ChunkContent, ContextEngineError> {
        let _ = (id, cancellation);
        Err(unavailable("chunk reads"))
    }

    /// Symbol lookup over the index ([#117]).
    ///
    /// # Errors
    ///
    /// [`ContextEngineError::NotYetAvailable`] until [#117] lands.
    ///
    /// [#117]: https://github.com/fullstacktaiye/harkness/issues/117
    pub fn symbols(
        &self,
        query: &SymbolQuery,
        cancellation: &Cancellation,
    ) -> Result<SymbolResults, ContextEngineError> {
        let _ = (query, cancellation);
        Err(unavailable("symbol lookup"))
    }

    /// The structural map of the repository ([#118]).
    ///
    /// # Errors
    ///
    /// [`ContextEngineError::NotYetAvailable`] until [#118] lands.
    ///
    /// [#118]: https://github.com/fullstacktaiye/harkness/issues/118
    pub fn repository_map(
        &self,
        request: &MapRequest,
        cancellation: &Cancellation,
    ) -> Result<RepositoryMap, ContextEngineError> {
        let _ = (request, cancellation);
        Err(unavailable("the repository map"))
    }

    /// The discovered, scoped instruction set ([#120]).
    ///
    /// # Errors
    ///
    /// [`ContextEngineError::NotYetAvailable`] until [#120] lands.
    ///
    /// [#120]: https://github.com/fullstacktaiye/harkness/issues/120
    pub fn instructions(
        &self,
        cancellation: &Cancellation,
    ) -> Result<InstructionSet, ContextEngineError> {
        let _ = cancellation;
        Err(unavailable("instruction discovery"))
    }

    /// Assembles a budgeted context pack ([#122]).
    ///
    /// # Errors
    ///
    /// [`ContextEngineError::NotYetAvailable`] until [#122] lands.
    ///
    /// [#122]: https://github.com/fullstacktaiye/harkness/issues/122
    pub fn build_pack(
        &self,
        request: &PackRequest,
        cancellation: &Cancellation,
    ) -> Result<ContextPack, ContextEngineError> {
        let _ = (request, cancellation);
        Err(unavailable("pack assembly"))
    }

    // -- the cache ----------------------------------------------------------

    /// A non-blocking view of the cache, for a UI to poll.
    ///
    /// Never waits on the index writer, so asking during a cold build answers
    /// immediately. A cache that could not be prepared reports
    /// [`IndexAvailability::Unavailable`] carrying the discriminant of the
    /// failure rather than looking like an empty index.
    #[must_use]
    pub fn index_status(&self) -> IndexStatus {
        match &*read(&self.cache) {
            Cache::Ready(cache) => cache.status(),
            Cache::Unavailable(error) => IndexStatus {
                generation: 0,
                availability: IndexAvailability::Unavailable {
                    kind: error.kind(),
                    detail: error.to_string(),
                },
                repository_identity: self.repository_key.clone(),
                last_recreation: None,
                stale_components: Vec::new(),
                last_refreshed_at: None,
                in_progress: None,
                counts: None,
            },
        }
    }

    /// The key this worktree's rows are filed under inside the cache.
    ///
    /// Derived from the canonical worktree root, so two catalog entries naming
    /// one checkout share its rows instead of each building a copy.
    #[must_use]
    pub fn worktree_key(&self) -> WorktreeKey {
        WorktreeKey::for_root(&self.config.worktree_root)
    }

    /// Walks the worktree and writes what it finds into the cache.
    ///
    /// The cold build: capture, walk, read each eligible file, chunk it, and
    /// commit the whole thing at one generation. Nothing it writes is visible
    /// until the commit, so a build stopped half-way leaves the previous
    /// generation answering rather than a repository that reports itself
    /// half-indexed.
    ///
    /// [`reconcile`](Self::reconcile) is the incremental half, and it is what
    /// every later pass should use: this one reads and chunks every file
    /// whatever its metadata says, which is right exactly once. A full
    /// reconcile of a worktree the cache has never seen does the same work and
    /// leaves the same rows, so the two differ only in what they are willing to
    /// assume ([#115]).
    ///
    /// # A truncated walk never sweeps
    ///
    /// A [`Full`](BatchScope::Full) batch deletes every row it did not confirm,
    /// which is right when the walk saw the whole worktree and catastrophic
    /// when it did not: an inventory stopped by its file or time budget would
    /// have the index delete rows for files that exist. A truncated inventory
    /// therefore commits as [`Targeted`](BatchScope::Targeted) — everything it
    /// did see is updated, nothing else is touched — and the receipt's scope
    /// says which happened.
    ///
    /// Blocking, and cancellation-polled between files.
    ///
    /// # Errors
    ///
    /// The failures of [`inventory`](Self::inventory), plus
    /// [`ContextEngineError::IndexBudgetExhausted`] when the cache reaches its
    /// per-repository cap, [`ContextEngineError::IndexBusy`] under sustained
    /// contention, and the cache's own open failures.
    ///
    /// [#115]: https://github.com/fullstacktaiye/harkness/issues/115
    pub fn reindex(&self, cancellation: &Cancellation) -> Result<BatchReceipt, ContextEngineError> {
        if cancellation.is_cancelled() {
            return Err(ContextEngineError::Cancelled);
        }
        // Reconciled before anything is written, so a build under a bumped
        // chunking version does not add rows beside the ones it invalidates.
        self.refresh_index(cancellation)?;

        let snapshot = self.snapshot(cancellation)?;
        let inventory = InventoryBuilder::build(&snapshot, &self.inventory_policy(), cancellation)?;
        let scope = batch_scope(&inventory);
        let key = self.worktree_key();
        let classify_version = inventory.classify_version();
        let marker = self.head_marker();

        self.with_cache(|cache| {
            let _operation = cache.begin_operation("reindex");
            let mut batch = cache.begin(&key, inventory.worktree_root(), scope, cancellation)?;
            // A cold build examined every path, so it is entitled to say which
            // committed base this checkout was verified against — which is what
            // lets the *next* pass tell a re-created worktree from an unchanged
            // one. A truncated walk is not entitled to, and does not.
            if scope == BatchScope::Full {
                batch.record_head_marker(marker.as_deref());
            }
            for entry in inventory.entries() {
                if cancellation.is_cancelled() {
                    return Err(ContextEngineError::Cancelled);
                }
                match self.derive(entry, snapshot.id(), cancellation)? {
                    Derived::Content(derived) => {
                        let (version, chunks) = derived.as_ref();
                        batch.record_chunked(entry, version, chunks, classify_version)?;
                    }
                    // A path whose content is never read — a binary, a symlink,
                    // a repository boundary. Recording it without content is the
                    // honest answer; dropping it would make a full batch sweep a
                    // path that exists.
                    Derived::Ineligible => batch.record_entry(entry, classify_version)?,
                    // A path that *should* have been read and could not be,
                    // because it changed under the walk or would not open. Its
                    // metadata is refreshed and whatever the last successful
                    // pass derived is left alone — clearing it would delete the
                    // file's chunks over one unreadable moment.
                    Derived::Unreadable => batch.record_unreadable(entry, classify_version)?,
                }
            }
            batch.commit(cancellation)
        })
    }

    /// Brings the index back into agreement with the worktree, within `scope`.
    ///
    /// The incremental half of the pair [`reindex`](Self::reindex) is the cold
    /// half of. Where a reindex reads and chunks every file, this compares the
    /// filesystem against the rows already stored and writes only the
    /// difference: a path whose size and modification time match its row is
    /// left alone, a path the scope *named* is hashed whatever its metadata
    /// says, and a row the walk found no path for is removed by name.
    ///
    /// **Events are not truth and this is why.** A watcher decides only what
    /// goes into `scope`; every answer comes from this comparison, so a
    /// reconcile reached through a dropped event, a startup sweep, or a caller
    /// asking directly produces the same rows. `docs/context-index.md` states
    /// the model and `reconcile`'s module documentation states the rules.
    ///
    /// A [`ReconcileScope::Full`] pass over a worktree the cache has never seen
    /// is a cold build by another route: every path is an addition, and the
    /// commit publishes them at one generation exactly as
    /// [`reindex`](Self::reindex) does.
    ///
    /// Blocking, and cancellation-polled between paths. A cancelled pass leaves
    /// the previous generation answering, because nothing it staged was ever
    /// visible.
    ///
    /// # Errors
    ///
    /// The failures of [`inventory`](Self::inventory), plus
    /// [`ContextEngineError::IndexBudgetExhausted`],
    /// [`ContextEngineError::IndexBusy`],
    /// [`ContextEngineError::IndexBatchSuperseded`] when another process
    /// published this worktree while the pass was open, and the cache's own
    /// open failures.
    pub fn reconcile(
        &self,
        scope: &ReconcileScope,
        cancellation: &Cancellation,
    ) -> Result<ReconcileReport, ContextEngineError> {
        if cancellation.is_cancelled() {
            return Err(ContextEngineError::Cancelled);
        }
        // Acted on before anything is compared, so a pass running under a
        // bumped chunking version sees the rows the invalidation cleared and
        // treats them as the suspects they are. It is also the repair: a cache
        // whose handle is gone, or whose file another process replaced, is
        // reopened here rather than written into as a ghost.
        //
        // Paid on every pass rather than only on the first, which is a real
        // cost — the refresh re-counts the cache's rows — and is kept anyway.
        // A pass runs at most twice a second by construction, the count is
        // small beside the walk and the hashing it precedes, and the
        // alternative is deciding from remembered state that nothing needs
        // repairing, which is exactly the class of answer this whole module
        // refuses to give.
        self.refresh_index(cancellation)?;

        let policy = self.inventory_policy();
        let marker = self.head_marker();
        let worktree = self.worktree_key();
        self.with_cache(|cache| {
            Reconciler {
                cache,
                worktree: worktree.clone(),
                root: &self.config.worktree_root,
                policy: &policy,
                head_marker: marker.clone(),
            }
            .run(scope, cancellation)
        })
    }

    /// Watches the worktree and keeps the index current as it changes.
    ///
    /// Starts a [`WatchService`]: a filesystem watcher whose events are treated
    /// as hints, a bounded dirty set that coalesces them, and one worker thread
    /// that sweeps at startup and then reconciles whatever the tree has settled
    /// into. Everything it publishes goes through
    /// [`reconcile`](Self::reconcile), so the watcher decides latency and never
    /// truth.
    ///
    /// **A watcher that cannot be established is not a failure.** The service
    /// starts degraded and reports the reason, still sweeps, and still accepts
    /// hints from a caller — an exhausted inotify table costs a second of
    /// freshness rather than a correct index. The only refusal is a worktree
    /// root that is not there, because that leaves nothing to watch and nothing
    /// to sweep.
    ///
    /// Takes an [`Arc`] receiver because the worker outlives the call: the
    /// service holds the engine for as long as it runs, which is what makes
    /// dropping the service the whole of stopping it.
    ///
    /// # Errors
    ///
    /// [`ContextEngineError::Watch`] carrying
    /// [`WatchError::WatchRootMissing`](crate::watch::WatchError::WatchRootMissing).
    pub fn watch(
        self: &Arc<Self>,
        options: WatchOptions,
    ) -> Result<WatchService, ContextEngineError> {
        WatchService::start(Arc::clone(self), options).map_err(ContextEngineError::from)
    }

    /// Forgets one checkout's rows, keeping everything a sibling still uses.
    ///
    /// The answer to a worktree that has been removed. Reached through any
    /// engine of the same repository, because the cache is keyed by repository
    /// and the checkout being forgotten no longer has an engine of its own —
    /// which is also why it takes a key rather than acting on this engine's own
    /// worktree.
    ///
    /// Nothing here decides that a checkout is gone. A worktree whose root has
    /// disappeared keeps its rows and reports the failure, because an
    /// unmounted filesystem and a deleted checkout are indistinguishable from
    /// inside this process and only one of them licenses throwing rows away.
    ///
    /// # Errors
    ///
    /// The cache's write failures.
    pub fn forget_worktree(
        &self,
        worktree: &WorktreeKey,
        cancellation: &Cancellation,
    ) -> Result<ForgetReport, ContextEngineError> {
        self.with_cache(|cache| cache.forget_worktree(worktree, cancellation))
    }

    /// What this checkout is *on*, as one comparable string.
    ///
    /// The branch, and the commit only when there is no branch. That asymmetry
    /// is the whole design of the marker, and it is a cost decision rather than
    /// a correctness one:
    ///
    /// - Including the commit would make **every ordinary commit** a divergence,
    ///   and a divergence makes every row a suspect. A commit does not touch the
    ///   working tree at all, so that would be a whole-repository rehash to
    ///   discover that nothing moved — on the single most frequent operation
    ///   there is.
    /// - Leaving the branch out would miss the case the marker exists for: a
    ///   checkout deleted and re-created at the same path holding **another
    ///   branch**, which is [#63]'s, and which path-derived identity cannot
    ///   otherwise tell from the one the rows describe.
    ///
    /// The residual is stated rather than argued away. A worktree re-created at
    /// the same path on the *same* branch at a different commit does not move
    /// the marker, and is caught by metadata instead — a fresh checkout writes
    /// its files now, so their modification times move. The marker is what
    /// covers the case where that inference is unavailable.
    ///
    /// A detached checkout carries its commit, because there is no branch to
    /// carry and moving one is a real change of content. The three states are
    /// spelled apart so a detached checkout never compares equal to the branch
    /// sitting at the same commit.
    ///
    /// `None` means the root is not a repository this build can read, which is
    /// a true statement and never an "unchanged". Read in process through
    /// libgit2, so a reconcile that runs every time an editor goes quiet does
    /// not spawn a Git process to find out.
    ///
    /// [#63]: https://github.com/fullstacktaiye/harkness/issues/63
    fn head_marker(&self) -> Option<String> {
        Some(match self.git.head_state().ok().flatten()? {
            HeadState::Unborn { branch } => {
                format!("unborn:{}", branch.unwrap_or_default())
            }
            HeadState::Branch { name } => format!("branch:{name}"),
            HeadState::Detached { commit } => format!("detached:{commit}"),
        })
    }

    /// Reads and chunks one entry, saying which of three things happened.
    ///
    /// The distinction between "never read" and "could not be read" is the
    /// whole reason this returns three answers rather than an [`Option`]: the
    /// two lead to different rows, and collapsing them costs a file its chunks
    /// the first time it is written to while a build is walking past.
    fn derive(
        &self,
        entry: &InventoryEntry,
        snapshot: crate::ids::SnapshotId,
        cancellation: &Cancellation,
    ) -> Result<Derived, ContextEngineError> {
        if !entry.eligible() {
            return Ok(Derived::Ineligible);
        }
        let Ok(bytes) = std::fs::read(self.config.worktree_root.join(entry.path.to_path_buf()))
        else {
            return Ok(Derived::Unreadable);
        };
        let version = match FileVersion::new(entry, snapshot, bytes.into(), cancellation) {
            Ok(version) => version,
            // A cancelled token is the caller's answer wherever it is observed.
            // Reading it as "this file could not be read" would let a stopped
            // build commit a batch describing a repository nobody walked.
            Err(crate::chunk::ChunkError::Cancelled) => return Err(ContextEngineError::Cancelled),
            Err(_) => return Ok(Derived::Unreadable),
        };
        match chunk_file(&version, None, cancellation) {
            Ok(chunks) => Ok(Derived::Content(Box::new((version, chunks)))),
            Err(crate::chunk::ChunkError::Cancelled) => Err(ContextEngineError::Cancelled),
            Err(_) => Ok(Derived::Unreadable),
        }
    }

    /// What the cache holds right now, counted rather than remembered.
    ///
    /// [`index_status`](Self::index_status) reports the counts the last write
    /// published and never waits; this one takes the connection and answers
    /// from the file, which is what a test or a `context status` command wants.
    ///
    /// # Errors
    ///
    /// The cache's read failures.
    pub fn index_counts(&self) -> Result<IndexCounts, ContextEngineError> {
        self.with_cache(IndexCache::counts)
    }

    /// Every file row this worktree holds, bounded by `limit`.
    ///
    /// The page says whether the bound is why it ended, because a full `Vec` and
    /// a repository that happens to hold exactly that many files are otherwise
    /// the same answer.
    ///
    /// # Errors
    ///
    /// The cache's read failures.
    pub fn indexed_files(
        &self,
        limit: usize,
    ) -> Result<IndexedPage<IndexedFile>, ContextEngineError> {
        let key = self.worktree_key();
        self.with_cache(|cache| cache.files(&key, limit))
    }

    /// The file row this worktree holds for `path`, when there is one.
    ///
    /// # Errors
    ///
    /// The cache's read failures.
    pub fn indexed_file(&self, path: &RepoPath) -> Result<Option<IndexedFile>, ContextEngineError> {
        let key = self.worktree_key();
        self.with_cache(|cache| cache.file(&key, path))
    }

    /// Every chunk this worktree's copy of `path` was recorded with.
    ///
    /// # Errors
    ///
    /// The cache's read failures.
    pub fn indexed_chunks(&self, path: &RepoPath) -> Result<Vec<IndexedChunk>, ContextEngineError> {
        let key = self.worktree_key();
        self.with_cache(|cache| cache.chunks(&key, path))
    }

    /// Re-checks the cache and reconciles what this build can.
    ///
    /// A cache that could not be prepared at [`open`](Self::open) is retried
    /// here first, so contention that has since cleared is recovered from
    /// rather than remembered forever.
    ///
    /// # Errors
    ///
    /// The cache's own failures, and the open failure again when the cache
    /// still cannot be prepared.
    pub fn refresh_index(
        &self,
        cancellation: &Cancellation,
    ) -> Result<IndexReport, ContextEngineError> {
        self.reopen_if_unavailable(cancellation)?;
        self.with_cache(|cache| cache.refresh(cancellation))
    }

    /// Throws the cache away and starts an empty one.
    ///
    /// This is the supported "reclaim disk" and "fix a weird index" action, and
    /// it is the same action: nothing here is evidence, so there is nothing to
    /// lose but warm-up time. A cache that could not be prepared at all is the
    /// weirdest index there is, so it is retried here before anything else —
    /// an action documented as the fix has to be able to fix that case.
    ///
    /// # Errors
    ///
    /// The cache's own failures, and the open failure again when the cache
    /// still cannot be prepared.
    pub fn dispose_index(
        &self,
        cancellation: &Cancellation,
    ) -> Result<CacheRecreation, ContextEngineError> {
        if self.reopen_if_unavailable(cancellation).is_err() {
            // A cache this build cannot even *read* — one written by a newer
            // build, refused before a handle exists — is the weirdest index
            // there is, and `IndexCache::dispose` cannot serve it because it
            // needs the cache open to dispose of it. Deleting the file outright
            // is the whole of the fix, and refusing to do it here would leave a
            // user editing the data directory by hand for the one case the
            // action is documented to cover.
            return self.discard_and_reopen(cancellation);
        }
        self.with_cache(|cache| cache.dispose(cancellation))
    }

    /// Deletes an unreadable cache and opens a replacement in its place.
    fn discard_and_reopen(
        &self,
        cancellation: &Cancellation,
    ) -> Result<CacheRecreation, ContextEngineError> {
        let mut cache = write(&self.cache);
        index::discard(&self.cache_root)?;
        *cache = open_cache(
            &self.cache_root,
            &self.config.expected_versions,
            &self.repository_key,
            cancellation,
        );
        match &*cache {
            Cache::Ready(fresh) => Ok(CacheRecreation {
                reason: RecreationReason::Disposed,
                detail: "a caller discarded a cache this build could not read".to_owned(),
                // The discarded cache's own generation was never readable —
                // that is why it had to be deleted rather than disposed — so
                // there is nothing honest to report for it.
                previous_generation: None,
                generation: fresh.generation(),
                quarantined_to: None,
            }),
            Cache::Unavailable(error) => Err(error.clone()),
        }
    }

    /// Retries the cache open when the engine is holding a failure.
    ///
    /// **The open runs with no lock held.** Preparing a contended cache waits
    /// out the busy timeout, and holding the write lock across it would block
    /// [`index_status`](Self::index_status) — documented as never waiting on
    /// the index writer — for the whole of it, in exactly the scenario recovery
    /// exists for. The lock is taken afterwards, the state re-checked
    /// underneath it, and a caller that lost the race drops its own engine
    /// rather than replacing the winner's.
    fn reopen_if_unavailable(&self, cancellation: &Cancellation) -> Result<(), ContextEngineError> {
        if let Cache::Ready(_) = &*read(&self.cache) {
            return Ok(());
        }
        let opened = open_cache(
            &self.cache_root,
            &self.config.expected_versions,
            &self.repository_key,
            cancellation,
        );
        let mut cache = write(&self.cache);
        if let Cache::Ready(_) = &*cache {
            return Ok(());
        }
        *cache = opened;
        match &*cache {
            Cache::Ready(_) => Ok(()),
            Cache::Unavailable(error) => Err(error.clone()),
        }
    }

    fn with_cache<T>(
        &self,
        call: impl FnOnce(&IndexCache) -> Result<T, ContextEngineError>,
    ) -> Result<T, ContextEngineError> {
        match &*read(&self.cache) {
            Cache::Ready(cache) => call(cache),
            Cache::Unavailable(error) => Err(error.clone()),
        }
    }
}

/// The scope a walk implies, which is the sharpest edge in `reindex`.
///
/// A pure function rather than an inline `if`, because it is the rule and not
/// an implementation detail: a full batch deletes every row it did not confirm,
/// and a walk that did not see the whole worktree — because its own file or
/// time budget stopped it, or because it was only ever asked about part of the
/// tree — would have the index delete rows for files that exist. Both questions
/// are asked of the inventory rather than of the caller, so the answer travels
/// with the value it is about. Reaching the truncated branch through the engine
/// means building a repository past `MAX_INVENTORY_FILES`, so the rule is held
/// to directly instead of by a fixture nobody can afford to write.
fn batch_scope(inventory: &FileInventory) -> BatchScope {
    if inventory.is_truncated() || !inventory.scope().is_full() {
        BatchScope::Targeted
    } else {
        BatchScope::Full
    }
}

/// What reading one inventory entry produced.
enum Derived {
    /// The bytes were read and chunked.
    ///
    /// Boxed because the other two carry nothing, and one value per file of a
    /// hundred-thousand-file walk is a size worth not paying on every entry.
    Content(Box<(FileVersion, crate::chunk::ChunkSet)>),
    /// The entry is one whose content is never read.
    Ineligible,
    /// The entry should have been read and could not be.
    Unreadable,
}

/// Prepares the cache, keeping a failure rather than propagating it.
fn open_cache(
    cache_root: &Path,
    expected: &ExpectedVersions,
    repository_key: &str,
    cancellation: &Cancellation,
) -> Cache {
    match IndexCache::open_or_create(cache_root, expected, repository_key, cancellation) {
        Ok(cache) => Cache::Ready(Box::new(cache)),
        Err(error) => Cache::Unavailable(error),
    }
}

/// Takes a read lock, adopting the contents even if a previous holder panicked.
///
/// A panic in a caller says nothing about which cache this engine holds, and
/// refusing to use it afterwards would turn one failure into an engine that can
/// never serve retrieval again.
fn read(cache: &RwLock<Cache>) -> RwLockReadGuard<'_, Cache> {
    cache.read().unwrap_or_else(PoisonError::into_inner)
}

fn write(cache: &RwLock<Cache>) -> RwLockWriteGuard<'_, Cache> {
    cache.write().unwrap_or_else(PoisonError::into_inner)
}

fn unavailable(feature: &'static str) -> ContextEngineError {
    ContextEngineError::NotYetAvailable { feature }
}

/// Which files an inventory should cover.
///
/// The *policy* an inventory is walked under — ignore layers, bounds — is
/// derived by the engine from its configuration and never supplied by a caller,
/// because a request that could widen the walk would be repository-controllable
/// input deciding what the engine may read.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct InventoryRequest {}

impl InventoryRequest {
    /// Inventories the whole worktree.
    #[must_use]
    pub const fn new() -> Self {
        Self {}
    }
}

/// Which symbol to look up.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct SymbolQuery {
    /// The symbol name to look for.
    pub name: String,
}

impl SymbolQuery {
    /// Looks up `name`.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

/// What a repository map should cover.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct MapRequest {}

impl MapRequest {
    /// Maps the whole repository.
    #[must_use]
    pub const fn new() -> Self {
        Self {}
    }
}

/// What a context pack is being assembled for.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct PackRequest {
    /// What the pack is meant to help with.
    pub objective: String,
}

impl PackRequest {
    /// Assembles a pack for `objective`.
    #[must_use]
    pub fn new(objective: impl Into<String>) -> Self {
        Self {
            objective: objective.into(),
        }
    }
}

/// The content of one indexed chunk ([#123]).
///
/// Empty for the reason [`FileInventory`] is.
///
/// [#123]: https://github.com/fullstacktaiye/harkness/issues/123
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct ChunkContent {}

/// What a symbol lookup found ([#117]).
///
/// Empty for the reason [`FileInventory`] is.
///
/// [#117]: https://github.com/fullstacktaiye/harkness/issues/117
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct SymbolResults {}

/// The structural map of a repository ([#118]).
///
/// Empty for the reason [`FileInventory`] is.
///
/// [#118]: https://github.com/fullstacktaiye/harkness/issues/118
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct RepositoryMap {}

/// The discovered, scoped instruction set ([#120]).
///
/// Empty for the reason [`FileInventory`] is.
///
/// [#120]: https://github.com/fullstacktaiye/harkness/issues/120
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct InstructionSet {}

/// A budgeted context pack ([#122]).
///
/// Empty for the reason [`FileInventory`] is.
///
/// [#122]: https://github.com/fullstacktaiye/harkness/issues/122
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct ContextPack {}

#[cfg(test)]
mod tests;
