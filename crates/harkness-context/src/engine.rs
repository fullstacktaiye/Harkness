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
//! The eight facade methods are the whole retrieval surface. Only
//! [`snapshot`](ContextEngine::snapshot) is implemented today, because [#109]
//! landed what it needs; the other seven return
//! [`ContextEngineError::NotYetAvailable`] naming the missing feature. That is a
//! real, tested refusal rather than a `todo!()`, so a caller written against the
//! seam now gets a typed answer and the issue that implements a method deletes a
//! branch rather than a panic.
//!
//! The engine returns typed values and persists nothing. Turning a snapshot or
//! a pack into evidence is the caller's job in `harkness-runtime` ([#122],
//! [#123]), which is what keeps the cache disposable — and ADR-0001's
//! dependency direction makes it structural rather than merely intended, since
//! this crate cannot name the runtime.
//!
//! [#109]: https://github.com/fullstacktaiye/harkness/issues/109
//! [#122]: https://github.com/fullstacktaiye/harkness/issues/122
//! [#123]: https://github.com/fullstacktaiye/harkness/issues/123

use std::path::{Path, PathBuf};
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use harkness_core::{CONTEXT_DIRECTORY, ProjectId};
use harkness_git::{Cancellation, GitService};

use crate::digest::{Sha256Hex, empty_path_set_digest};
use crate::error::{ContextDomainError, ContextEngineError};
use crate::ids::ChunkId;
use crate::index::{
    self, CacheRecreation, ExpectedVersions, IndexAvailability, IndexCache, IndexReport,
    IndexStatus, RecreationReason,
};
use crate::inventory::FileInventory;
use crate::probe::FilesystemProbe;
use crate::snapshot::{Capture, CaptureRequest, WorkspaceSnapshot};

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
    /// reserved [`CONTEXT_DIRECTORY`] child and nowhere else. The path is
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
        let cache_root = config
            .data_dir
            .join(CONTEXT_DIRECTORY)
            .join(&repository_key);
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

    /// The classified, bounded set of files eligible for indexing ([#112]).
    ///
    /// # Errors
    ///
    /// [`ContextEngineError::NotYetAvailable`] until [#112] lands.
    ///
    /// [#112]: https://github.com/fullstacktaiye/harkness/issues/112
    pub fn inventory(
        &self,
        request: &InventoryRequest,
        cancellation: &Cancellation,
    ) -> Result<FileInventory, ContextEngineError> {
        let _ = (request, cancellation);
        Err(unavailable("file inventory"))
    }

    /// Lexical and filename search over the index ([#116]).
    ///
    /// # Errors
    ///
    /// [`ContextEngineError::NotYetAvailable`] until [#116] lands.
    ///
    /// [#116]: https://github.com/fullstacktaiye/harkness/issues/116
    pub fn search(
        &self,
        query: &SearchQuery,
        cancellation: &Cancellation,
    ) -> Result<SearchResults, ContextEngineError> {
        let _ = (query, cancellation);
        Err(unavailable("search"))
    }

    /// The content of one indexed chunk ([#113]).
    ///
    /// # Errors
    ///
    /// [`ContextEngineError::NotYetAvailable`] until [#113] lands.
    ///
    /// [#113]: https://github.com/fullstacktaiye/harkness/issues/113
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

/// What to search for.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct SearchQuery {
    /// The text to look for.
    pub pattern: String,
}

impl SearchQuery {
    /// Searches for `pattern`.
    #[must_use]
    pub fn new(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
        }
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

/// What a search found ([#116]).
///
/// Empty for the reason [`FileInventory`] is.
///
/// [#116]: https://github.com/fullstacktaiye/harkness/issues/116
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct SearchResults {}

/// The content of one indexed chunk ([#113]).
///
/// Empty for the reason [`FileInventory`] is.
///
/// [#113]: https://github.com/fullstacktaiye/harkness/issues/113
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
