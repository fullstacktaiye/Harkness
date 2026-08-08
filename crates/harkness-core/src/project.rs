use std::{
    collections::HashSet,
    ffi::OsString,
    fs::{self, File},
    io,
    path::{Component, Path, PathBuf},
};

use thiserror::Error;
use time::OffsetDateTime;

use crate::{
    catalog::{
        self, Catalog,
        entry::{GitStatus, Project, ProjectId, ProjectSource},
        lock,
    },
    git::{self, Cancellation, GitError, GitService, clone},
    paths::{
        self, CATALOG_FILE, CHECKOUT_DIRECTORY, LOCKS_DIRECTORY, REPOSITORIES_DIRECTORY,
        UnreservedPath, WORKTREES_DIRECTORY, canonical_reserved_root,
    },
    remote::{normalize_remote_with_local, repository_name},
};

/// Actionable failures returned by [`ProjectService`].
#[derive(Debug, Error)]
pub enum ProjectError {
    /// The operating system did not expose a suitable user data directory.
    #[error("the platform data directory could not be determined")]
    DataDirectoryUnavailable,

    /// The import path does not identify a directory.
    #[error("invalid project directory '{}': {reason}", path.display())]
    InvalidDirectory { path: PathBuf, reason: String },

    /// The import directory exists but cannot be read or canonicalized.
    #[error("project directory '{}' is not readable", path.display())]
    UnreadableDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// The catalog file could not be read.
    #[error("failed to read project catalog '{}': {source}", path.display())]
    CatalogRead {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// The catalog lock file could not be created or locked.
    #[error("failed to lock project catalog '{}': {source}", path.display())]
    CatalogLock {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// The catalog contains invalid JSON or does not match the catalog schema.
    #[error("project catalog '{}' is malformed: {source}", path.display())]
    MalformedCatalog {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// The catalog predates the oldest schema this build can interpret.
    #[error(
        "project catalog version {found} is too old for this Harkness build \
         (minimum supported version is {minimum})"
    )]
    CatalogVersionTooOld { found: u32, minimum: u32 },

    /// The catalog requires a newer Harkness build.
    #[error(
        "project catalog version {found} requires a newer Harkness build \
         (this build supports through version {maximum})"
    )]
    CatalogVersionTooNew { found: u32, maximum: u32 },

    /// The catalog is syntactically valid but violates a schema invariant.
    #[error("project catalog '{}' contains invalid data: {reason}", path.display())]
    InvalidCatalog { path: PathBuf, reason: String },

    /// No project matches an explicit selector or the current directory.
    #[error("no project matches selector '{selector}'")]
    ProjectSelectorNotFound { selector: String },

    /// More than one project matches a selector at the same precedence.
    #[error(
        "project selector '{selector}' is ambiguous; matches: {matches}",
        matches = DisplayProjects(.candidates)
    )]
    AmbiguousProjectSelector {
        selector: String,
        candidates: Vec<Project>,
    },

    /// No project has the requested identifier.
    #[error("project {0} was not found in the catalog")]
    ProjectNotFound(ProjectId),

    /// A catalog entry exists, but its root cannot currently be opened.
    #[error("project {id} is unavailable at '{}'", path.display())]
    ProjectUnavailable { id: ProjectId, path: PathBuf },

    /// Git metadata could not be inspected for a readable project.
    #[error("failed to inspect Git metadata for '{}': {source}", path.display())]
    GitInspection {
        path: PathBuf,
        #[source]
        source: git2::Error,
    },

    /// An updated catalog could not be written atomically.
    #[error("failed to persist project catalog '{}': {source}", path.display())]
    Persistence {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// The remote cannot be normalized into a supported Git identity.
    #[error(
        "invalid Git repository remote '{remote}'; expected a GitHub HTTP(S) URL or SSH remote"
    )]
    InvalidRemote { remote: String },

    /// The system Git executable could not be launched.
    #[error("failed to start system Git: {source}")]
    GitLaunch {
        #[source]
        source: io::Error,
    },

    /// Git could not clone the repository. Its diagnostic output is retained.
    #[error("Git clone failed: {stderr}")]
    CloneFailed { stderr: String },

    /// The repository import was cancelled.
    #[error("repository clone was cancelled")]
    CloneCancelled,

    /// A project is not a safely removable Harkness-managed clone.
    #[error("refusing to delete project {id} at '{}': {reason}", path.display())]
    UnsafeManagedRemoval {
        id: ProjectId,
        path: PathBuf,
        reason: String,
    },

    /// A managed checkout could not be deleted.
    #[error("failed to delete managed checkout '{}': {source}", path.display())]
    ManagedRemoval {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// A managed-import lock could not be opened or acquired.
    #[error("failed to lock managed repository storage '{}': {source}", path.display())]
    ManagedRepositoryLock {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// Orphaned managed-repository storage could not be inspected or removed.
    #[error("failed to reconcile managed repository storage '{}': {source}", path.display())]
    ManagedRepositoryReconciliation {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// A project cannot be removed while its worktrees remain catalogued.
    #[error(
        "project {id} still has catalogued worktrees; remove them first: {paths}",
        paths = DisplayWorktrees(.worktrees)
    )]
    ParentHasWorktrees {
        id: ProjectId,
        worktrees: Vec<PathBuf>,
    },

    /// A managed worktree must be removed through Git before its row is dropped.
    #[error(
        "project {id} is a managed worktree at '{}'; use worktree removal so its Git metadata is cleaned up",
        path.display()
    )]
    WorktreeRemovalRequired { id: ProjectId, path: PathBuf },

    /// A project is not a safely removable Harkness-managed worktree.
    #[error("refusing to remove worktree {id} at '{}': {reason}", path.display())]
    UnsafeWorktreeRemoval {
        id: ProjectId,
        path: PathBuf,
        reason: String,
    },

    /// Worktrees cannot be nested beneath another worktree.
    #[error(
        "project {id} at '{}' is itself a worktree; nested worktrees are not supported",
        path.display()
    )]
    WorktreeParentUnsupported { id: ProjectId, path: PathBuf },

    /// Ordinary removal preserves uncommitted work unless force is explicit.
    #[error(
        "worktree {id} at '{}' has uncommitted changes; explicitly force removal to discard them",
        path.display()
    )]
    DirtyWorktreeRemoval { id: ProjectId, path: PathBuf },

    /// A project is not a Harkness-managed worktree that can be relocated.
    #[error("refusing to move worktree {id} at '{}': {reason}", path.display())]
    UnsafeWorktreeMove {
        id: ProjectId,
        path: PathBuf,
        reason: String,
    },

    /// Git requires a move destination that does not already exist.
    #[error("worktree move destination '{}' already exists", path.display())]
    WorktreeDestinationExists { path: PathBuf },

    /// Relative destinations have no stable meaning across front ends.
    #[error("worktree move destination '{}' must be absolute", path.display())]
    WorktreeDestinationNotAbsolute { path: PathBuf },

    /// The move destination's immediate parent could not be resolved.
    #[error(
        "worktree move destination parent '{}' cannot be resolved: {source}",
        path.display()
    )]
    WorktreeDestinationParentUnavailable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// Worktree roots cannot overlap another catalogued project.
    #[error(
        "worktree move destination '{}' is inside project {project_id} at '{}'",
        path.display(),
        project_root.display()
    )]
    WorktreeDestinationInsideProject {
        path: PathBuf,
        project_id: ProjectId,
        project_root: PathBuf,
    },

    /// A move destination cannot become an ancestor of another project root.
    #[error(
        "worktree move destination '{}' would contain project {project_id} at '{}'",
        path.display(),
        project_root.display()
    )]
    WorktreeDestinationContainsProject {
        path: PathBuf,
        project_id: ProjectId,
        project_root: PathBuf,
    },

    /// Relocated worktrees cannot overlap Harkness's own durable state.
    #[error(
        "worktree move destination '{}' is inside the Harkness data directory '{}'",
        path.display(),
        data_dir.display()
    )]
    WorktreeDestinationInsideDataDirectory { path: PathBuf, data_dir: PathBuf },

    /// Git completed the relocation but the catalog could not safely follow.
    #[error(
        "worktree {id} moved from '{}' toward '{}', but its catalog entry is stale: {reason}; run worktree reconciliation",
        stale_root.display(),
        destination.display()
    )]
    WorktreeMovedCatalogStale {
        id: ProjectId,
        stale_root: PathBuf,
        destination: PathBuf,
        reason: String,
    },

    /// A Git operation on a catalogued project failed.
    #[error(transparent)]
    Git(GitError),
}

macro_rules! impl_project_error_kinds {
    ($( $pattern:pat => $kind:literal, )+) => {
        impl ProjectError {
            /// Stable discriminants defined by the project layer itself.
            ///
            /// [`ProjectError::Git`] delegates to [`GitError::KINDS`], so callers that
            /// need the complete namespace should combine both arrays.
            pub const DIRECT_KINDS: &'static [&'static str] = &[$($kind),+];

            /// Stable machine-readable discriminant for agent-facing error handling.
            #[must_use]
            pub fn kind(&self) -> &'static str {
                match self {
                    $($pattern => $kind,)+
                    Self::Git(error) => error.kind(),
                }
            }
        }
    };
}

impl_project_error_kinds! {
    Self::DataDirectoryUnavailable => "data_directory_unavailable",
    Self::InvalidDirectory { .. } => "invalid_directory",
    Self::UnreadableDirectory { .. } => "unreadable_directory",
    Self::CatalogRead { .. } => "catalog_read",
    Self::CatalogLock { .. } => "catalog_lock",
    Self::MalformedCatalog { .. } => "malformed_catalog",
    Self::CatalogVersionTooOld { .. } => "catalog_version_too_old",
    Self::CatalogVersionTooNew { .. } => "catalog_version_too_new",
    Self::InvalidCatalog { .. } => "invalid_catalog",
    Self::ProjectSelectorNotFound { .. } => "project_selector_not_found",
    Self::AmbiguousProjectSelector { .. } => "ambiguous_project_selector",
    Self::ProjectNotFound(_) => "project_not_found",
    Self::ProjectUnavailable { .. } => "project_unavailable",
    Self::GitInspection { .. } => "git_inspection",
    Self::Persistence { .. } => "persistence",
    Self::InvalidRemote { .. } => "invalid_remote",
    Self::GitLaunch { .. } => "git_launch",
    Self::CloneFailed { .. } => "clone_failed",
    Self::CloneCancelled => "clone_cancelled",
    Self::UnsafeManagedRemoval { .. } => "unsafe_managed_removal",
    Self::ManagedRemoval { .. } => "managed_removal",
    Self::ManagedRepositoryLock { .. } => "managed_repository_lock",
    Self::ManagedRepositoryReconciliation { .. } => "managed_repository_reconciliation",
    Self::ParentHasWorktrees { .. } => "parent_has_worktrees",
    Self::WorktreeRemovalRequired { .. } => "worktree_removal_required",
    Self::UnsafeWorktreeRemoval { .. } => "unsafe_worktree_removal",
    Self::WorktreeParentUnsupported { .. } => "worktree_parent_unsupported",
    Self::DirtyWorktreeRemoval { .. } => "dirty_worktree_removal",
    Self::UnsafeWorktreeMove { .. } => "unsafe_worktree_move",
    Self::WorktreeDestinationExists { .. } => "worktree_destination_exists",
    Self::WorktreeDestinationNotAbsolute { .. } => "worktree_destination_not_absolute",
    Self::WorktreeDestinationParentUnavailable { .. } => "worktree_destination_parent_unavailable",
    Self::WorktreeDestinationInsideProject { .. } => "worktree_destination_inside_project",
    Self::WorktreeDestinationContainsProject { .. } => "worktree_destination_contains_project",
    Self::WorktreeDestinationInsideDataDirectory { .. } => "worktree_destination_inside_data_directory",
    Self::WorktreeMovedCatalogStale { .. } => "worktree_moved_catalog_stale",
}

/// A project lookup requested by a front end.
///
/// Explicit selectors are resolved as a full identifier, an identifier prefix,
/// an absolute or dot-relative path, an exact display name, and finally a
/// case-insensitive display name. Bare words are always names. Current-directory
/// selectors choose the deepest catalogued root containing the supplied
/// directory.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProjectSelector {
    /// A selector written by a caller, such as a UUID prefix or display name.
    Value(String),
    /// A process working directory used for ambient project resolution.
    CurrentDirectory(PathBuf),
}

impl ProjectSelector {
    /// Selects the project containing `directory`.
    #[must_use]
    pub fn current_directory(directory: impl Into<PathBuf>) -> Self {
        Self::CurrentDirectory(directory.into())
    }
}

impl From<String> for ProjectSelector {
    fn from(value: String) -> Self {
        Self::Value(value)
    }
}

impl From<&str> for ProjectSelector {
    fn from(value: &str) -> Self {
        Self::Value(value.to_owned())
    }
}

/// What a newly created worktree should check out.
///
/// The variants make branch creation, reuse, and detached checkout mutually
/// exclusive, so callers cannot accidentally ask Git to invent a suffixed
/// branch or attach a commit-only workspace to a made-up branch name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorktreeBase {
    /// Create `name` from `start_point`, or from the parent's HEAD when absent.
    NewBranch {
        name: String,
        start_point: Option<String>,
    },
    /// Check out an existing local branch without creating or renaming it.
    ExistingBranch { name: String },
    /// Check out one commit with a detached HEAD.
    Detached { commit: String },
}

/// One worktree associated with a catalogued parent.
///
/// `project` is `Some` only for a Harkness-owned worktree. An external row is
/// deliberately read-only: it has no project identifier that can be passed to
/// [`ProjectService::remove_worktree`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Worktree {
    /// The checkout path reported by Git.
    pub root: PathBuf,
    /// The currently checked-out branch, or `None` for detached HEAD.
    pub branch: Option<String>,
    /// Whether Git has locked this worktree against removal or pruning.
    pub locked: bool,
    /// Whether Git considers the administrative record safe to clean up.
    pub prunable: bool,
    /// The catalog entry when Harkness owns this worktree.
    pub project: Option<Project>,
}

/// The complete outcome of selective worktree reconciliation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorktreeReconciliation {
    /// Catalog entries whose missing checkout and Git record were removed.
    pub removed: Vec<Project>,
    /// Catalog entries whose root was repaired from Git's live record.
    pub repaired: Vec<Project>,
    /// Entries deliberately left unchanged, with an actionable reason.
    pub skipped: Vec<WorktreeReconciliationSkip>,
}

/// One worktree reconciliation could not safely mutate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeReconciliationSkip {
    /// The catalog entry that remains unchanged.
    pub project: Project,
    /// Why the row was kept for a later repair attempt.
    pub reason: String,
}

impl From<GitError> for ProjectError {
    fn from(error: GitError) -> Self {
        match error {
            // Inspection predates the Git module and is what `import_local`
            // reports, so it keeps its own variant and its own message.
            GitError::Inspection { path, source } => Self::GitInspection { path, source },
            other => Self::Git(other),
        }
    }
}

/// Loads and updates the durable local project catalog.
///
/// Concurrent front ends are safe. Every mutation takes an exclusive advisory
/// lock on `projects.lock`, re-reads `projects.json` inside the critical
/// section, applies its delta to that fresh state, and persists atomically, so
/// the snapshot a service cached at load is never written back over another
/// process's work. Reads take a shared lock for the same reason.
///
/// The lock covers only the read-modify-write of the catalog, never a network
/// clone, and the kernel releases it if the process dies, so neither a slow
/// import nor a crash can wedge the catalog.
pub struct ProjectService {
    data_dir: PathBuf,
    catalog: Catalog,
    allow_local_remotes: bool,
    git_executable: PathBuf,
}

impl ProjectService {
    /// Loads the catalog from the platform user data directory.
    ///
    /// `HARKNESS_DATA_DIR` replaces that location entirely when it is set, so a
    /// caller can run against an isolated catalog without an isolated build.
    pub fn load() -> Result<Self, ProjectError> {
        let data_dir = paths::data_directory().ok_or(ProjectError::DataDirectoryUnavailable)?;
        Self::load_from_data_dir(data_dir)
    }

    /// Loads a catalog rooted at an explicit Harkness data directory.
    ///
    /// This constructor is useful for isolated applications and tests. The
    /// catalog file is always named `projects.json` within `data_dir`.
    pub fn load_from_data_dir(data_dir: impl Into<PathBuf>) -> Result<Self, ProjectError> {
        let data_dir = data_dir.into();
        let catalog = catalog::read_catalog(&data_dir.join(CATALOG_FILE))?;
        Ok(Self {
            data_dir,
            catalog,
            allow_local_remotes: false,
            git_executable: PathBuf::from("git"),
        })
    }

    /// Loads an isolated service whose managed imports may clone local test
    /// repositories. Production constructors deliberately never enable this.
    #[cfg(test)]
    pub(crate) fn load_for_test(data_dir: impl Into<PathBuf>) -> Result<Self, ProjectError> {
        let mut service = Self::load_from_data_dir(data_dir)?;
        service.allow_local_remotes = true;
        Ok(service)
    }

    /// Takes the exclusive catalog lock for one read-modify-write.
    pub(crate) fn lock_exclusive(&self) -> Result<File, ProjectError> {
        lock::lock_exclusive(&self.data_dir)
    }

    /// The Harkness data directory this service is rooted at.
    #[cfg(test)]
    pub(crate) fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Reads the catalog fresh from disk. Call only while holding the lock.
    fn read_catalog(&self) -> Result<Catalog, ProjectError> {
        catalog::read_catalog(&self.data_dir.join(CATALOG_FILE))
    }

    /// Reads the catalog under a shared lock.
    fn read_catalog_shared(&self) -> Result<Catalog, ProjectError> {
        lock::read_catalog_shared(&self.data_dir)
    }

    /// Lists projects by most-recently-opened order with current availability
    /// and Git metadata.
    ///
    /// A project whose Git metadata cannot be read is reported with `git:
    /// None` rather than hiding the rest of the catalog. Catalog read or
    /// validation failures are returned so a front end never presents a stale
    /// in-memory snapshot as if it were current.
    pub fn list(&self) -> Result<Vec<Project>, ProjectError> {
        // Git and availability checks touch the filesystem once per project,
        // so they run after the shared lock is released.
        let catalog = self.read_catalog_shared()?;
        let mut projects = catalog
            .projects
            .iter()
            .cloned()
            .map(refresh_project)
            .collect::<Vec<_>>();
        sort_recents(&mut projects);
        Ok(projects)
    }

    /// Lists projects by most-recently-opened order exactly as the catalog
    /// stores them.
    ///
    /// Reads the catalog and nothing else: `available` and `git` are reported
    /// at their defaults rather than derived, so a caller that only needs the
    /// stored identity of every project does not pay one metadata call and one
    /// Git inspection per entry. Use [`list`] for current derived state.
    ///
    /// [`list`]: ProjectService::list
    pub fn list_catalog_only(&self) -> Result<Vec<Project>, ProjectError> {
        let catalog = self.read_catalog_shared()?;
        let mut projects = catalog
            .projects
            .into_iter()
            .map(|project| Project {
                available: false,
                git: None,
                ..project
            })
            .collect::<Vec<_>>();
        sort_recents(&mut projects);
        Ok(projects)
    }

    /// Resolves one catalog entry without mutating the catalog or Recents.
    ///
    /// Explicit values are considered in ergonomic precedence order: full
    /// [`ProjectId`], UUID prefix of at least eight characters, explicit
    /// absolute or dot-relative path, exact display name, then case-insensitive
    /// display name. A bare word is always a name and therefore never changes
    /// meaning with the process working directory. A current-directory selector
    /// returns the deepest catalogued root containing that directory, which
    /// makes a nested worktree win over its repository parent.
    pub fn resolve(&self, selector: &ProjectSelector) -> Result<Project, ProjectError> {
        let projects = self.read_catalog_shared()?.projects;
        let project = match selector {
            ProjectSelector::Value(value) => resolve_value(&projects, value)?,
            ProjectSelector::CurrentDirectory(directory) => {
                resolve_current_directory(&projects, directory)?
            }
        };
        Ok(refresh_project(project))
    }

    /// Imports a readable local directory, or reopens its existing canonical
    /// catalog entry when it was already imported.
    pub fn import_local(&mut self, path: impl AsRef<Path>) -> Result<Project, ProjectError> {
        let canonical_root = validate_local_directory(path.as_ref())?;
        // Inspecting Git walks the working tree, so it runs before the lock.
        let git = inspect_git(&canonical_root)?;

        let _lock = self.lock_exclusive()?;
        let mut candidate = self.read_catalog()?;

        let project = if let Some(project) = candidate
            .projects
            .iter_mut()
            .find(|project| project.root == canonical_root)
        {
            project.available = true;
            project.last_opened = OffsetDateTime::now_utc();
            project.git = git;
            project.clone()
        } else {
            let project = Project {
                id: ProjectId::new(),
                display_name: display_name(&canonical_root),
                root: canonical_root.clone(),
                source: ProjectSource::Local,
                available: true,
                last_opened: OffsetDateTime::now_utc(),
                git,
            };
            candidate.projects.push(project.clone());
            project
        };

        sort_recents(&mut candidate.projects);
        self.persist(&candidate)?;
        self.catalog = candidate;
        Ok(project)
    }

    /// Reopens a project and moves it to the top of Recents.
    pub fn open(&mut self, id: ProjectId) -> Result<Project, ProjectError> {
        let _lock = self.lock_exclusive()?;
        let mut candidate = self.read_catalog()?;
        let project = candidate
            .projects
            .iter_mut()
            .find(|project| project.id == id)
            .ok_or(ProjectError::ProjectNotFound(id))?;

        let refreshed = refresh_project(project.clone());
        if !refreshed.available {
            return Err(ProjectError::ProjectUnavailable {
                id,
                path: project.root.clone(),
            });
        }

        *project = Project {
            last_opened: OffsetDateTime::now_utc(),
            ..refreshed
        };
        let opened = project.clone();
        sort_recents(&mut candidate.projects);
        self.persist(&candidate)?;
        self.catalog = candidate;
        Ok(opened)
    }

    /// Removes a project record without touching its source directory.
    pub fn remove(&mut self, id: ProjectId) -> Result<Project, ProjectError> {
        let _lock = self.lock_exclusive()?;
        let mut candidate = self.read_catalog()?;
        let index = candidate
            .projects
            .iter()
            .position(|project| project.id == id)
            .ok_or(ProjectError::ProjectNotFound(id))?;
        if matches!(
            RemovalPolicy::from(&candidate.projects[index].source),
            RemovalPolicy::Worktree { .. }
        ) {
            return Err(worktree_removal_required(&candidate.projects[index]));
        }
        refuse_parent_with_worktrees(&candidate, id)?;
        let removed = candidate.projects.remove(index);
        self.persist(&candidate)?;
        self.catalog = candidate;
        Ok(removed)
    }

    /// Clones a GitHub repository with the system Git executable.
    ///
    /// This method waits for Git, so GUI callers should invoke it on a worker
    /// thread. Progress arrives as Git writes each update to standard error,
    /// and cancellation is cooperative through `cancellation`.
    pub fn import_repository(
        &mut self,
        remote: &str,
        cancellation: &Cancellation,
        mut on_progress: impl FnMut(String),
    ) -> Result<Project, ProjectError> {
        // Git reads its argument literally, so surrounding whitespace from a
        // pasted URL would reach it as part of the protocol name.
        let remote = remote.trim();
        let normalized = normalize_remote_with_local(remote, self.allow_local_remotes)?;

        // An existing, available checkout for this remote is reopened rather
        // than cloned again. This critical section ends before the clone
        // starts, so a slow import never holds the lock.
        {
            let _lock = self.lock_exclusive()?;
            let mut candidate = self.read_catalog()?;
            if let Some(index) = candidate.projects.iter().position(|project| {
                matches!(
                    &project.source,
                    ProjectSource::ManagedRepository { remote }
                        if remote == &normalized
                )
            }) {
                let reopened = refresh_project(candidate.projects[index].clone());
                if reopened.available {
                    let reopened = Project {
                        last_opened: OffsetDateTime::now_utc(),
                        ..reopened
                    };
                    candidate.projects[index] = reopened.clone();
                    sort_recents(&mut candidate.projects);
                    self.persist(&candidate)?;
                    self.catalog = candidate;
                    return Ok(reopened);
                }
                // Otherwise the checkout was deleted outside Harkness. Refuse
                // before cloning when replacing it would orphan worktrees.
                refuse_parent_with_worktrees(&candidate, candidate.projects[index].id)?;
                // With no children, clone a replacement and reconcile the
                // stale row after the slow operation finishes.
                // Reporting a successful import of a path that no longer
                // exists would strand the user, so the clone is repeated and
                // the stale entry replaced once the fresh checkout exists.
            }
        }

        let id = ProjectId::new();
        // Reconciliation takes the same per-identifier lock before deleting an
        // unreferenced directory. Keeping it for the whole clone prevents a
        // concurrent cleanup from mistaking this not-yet-catalogued checkout
        // for storage orphaned by a killed importer.
        let _import_lock = ManagedImportLock::acquire(&self.data_dir, id)?;
        let managed_directory = self
            .data_dir
            .join(REPOSITORIES_DIRECTORY)
            .join(id.to_string());
        let checkout = managed_directory.join(CHECKOUT_DIRECTORY);
        fs::create_dir_all(&managed_directory).map_err(|source| ProjectError::Persistence {
            path: managed_directory.clone(),
            source,
        })?;

        // Every failure past this point can leave a partial checkout behind, so
        // the rest of the import runs in one fallible block with one cleanup.
        let imported = (|| {
            // No repository lock: the clone creates a repository that does not
            // exist yet, inside a directory reserved for this identifier alone,
            // so there is nothing for it to be serialized against.
            clone::run(
                &self.git_executable,
                remote,
                &managed_directory,
                cancellation,
                &mut on_progress,
            )?;
            let canonical_root = validate_local_directory(&checkout)?;
            let project = Project {
                id,
                display_name: repository_name(&normalized),
                root: canonical_root.clone(),
                source: ProjectSource::ManagedRepository {
                    remote: normalized.clone(),
                },
                last_opened: OffsetDateTime::now_utc(),
                available: true,
                git: inspect_git(&canonical_root)?,
            };

            let reconciled = {
                let _lock = self.lock_exclusive()?;
                let mut candidate = self.read_catalog()?;
                // The reopen check above ran against a catalog that is now as
                // old as the clone. If a concurrent import won the race, use
                // its live checkout instead of replacing its entry and
                // orphaning one of the two managed directories.
                let reconciled = if let Some(index) = candidate.projects.iter().position(|entry| {
                    matches!(
                        &entry.source,
                        ProjectSource::ManagedRepository { remote }
                            if remote == &normalized
                    )
                }) {
                    let existing = refresh_project(candidate.projects[index].clone());
                    if existing.available {
                        let existing = Project {
                            last_opened: OffsetDateTime::now_utc(),
                            ..existing
                        };
                        candidate.projects[index] = existing.clone();
                        existing
                    } else {
                        refuse_parent_with_worktrees(&candidate, existing.id)?;
                        candidate.projects.remove(index);
                        candidate.projects.push(project.clone());
                        project.clone()
                    }
                } else {
                    candidate.projects.push(project.clone());
                    project.clone()
                };
                sort_recents(&mut candidate.projects);
                self.persist(&candidate)?;
                self.catalog = candidate;
                reconciled
            };

            if reconciled.id != id {
                fs::remove_dir_all(&managed_directory).map_err(|source| {
                    ProjectError::Persistence {
                        path: managed_directory.clone(),
                        source,
                    }
                })?;
            }
            Ok(reconciled)
        })();

        if imported.is_err() {
            let _ = fs::remove_dir_all(&managed_directory);
        }
        imported
    }

    /// Removes managed-repository directories that have no catalog entry.
    ///
    /// Only immediate children named as project UUIDs are eligible. A live
    /// import holds the matching per-identifier lock until its catalog row is
    /// durable, so reconciliation skips in-progress clones and can safely reap
    /// storage left behind when an importer was killed.
    pub fn reconcile_managed_repositories(&self) -> Result<Vec<PathBuf>, ProjectError> {
        let repositories_root = self.data_dir.join(REPOSITORIES_DIRECTORY);
        let entries = match fs::read_dir(&repositories_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(ProjectError::ManagedRepositoryReconciliation {
                    path: repositories_root,
                    source,
                });
            }
        };

        let mut candidates = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| ProjectError::ManagedRepositoryReconciliation {
                path: repositories_root.clone(),
                source,
            })?;
            let file_type = entry.file_type().map_err(|source| {
                ProjectError::ManagedRepositoryReconciliation {
                    path: entry.path(),
                    source,
                }
            })?;
            if !file_type.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(id) = name.parse::<ProjectId>() else {
                continue;
            };
            candidates.push((id, entry.path()));
        }
        candidates.sort_by(|left, right| left.1.cmp(&right.1));

        let mut locked = Vec::new();
        for (id, path) in candidates {
            if let Some(lock) = ManagedImportLock::try_acquire(&self.data_dir, id)? {
                locked.push((id, path, lock));
            }
        }

        // Every possible importer for one of these identifiers is excluded by
        // its lock, so a single short shared catalog read is enough. No global
        // catalog lock is held while directories are recursively removed.
        let catalogued = self
            .read_catalog_shared()?
            .projects
            .into_iter()
            .map(|project| project.id)
            .collect::<HashSet<_>>();
        let mut removed = Vec::new();
        for (id, path, _lock) in locked {
            if catalogued.contains(&id) {
                continue;
            }
            match fs::remove_dir_all(&path) {
                Ok(()) => removed.push(path),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(ProjectError::ManagedRepositoryReconciliation { path, source });
                }
            }
        }
        Ok(removed)
    }

    /// Deletes a checkout only after proving it is the managed path for `id`.
    ///
    /// Front ends must obtain explicit confirmation naming [`Project::root`]
    /// before calling this destructive operation.
    pub fn remove_managed(&mut self, id: ProjectId) -> Result<Project, ProjectError> {
        // Removing a checkout is a Git mutation, so it takes the repository
        // lock, and the repository lock is always taken before the catalog
        // lock. Learning the path therefore needs a shared read that is
        // released before the lock is taken, and everything it read is
        // re-verified under the exclusive lock below.
        let preliminary = self
            .read_catalog_shared()?
            .projects
            .into_iter()
            .find(|project| project.id == id)
            .ok_or(ProjectError::ProjectNotFound(id))?;
        // A checkout that is no longer a repository has no object store to
        // serialize against. Whether it may be deleted at all is still decided
        // by the re-verification, not here.
        let _repository_lock = match self
            .git_service(&preliminary.root)
            .lock(&Cancellation::default())
        {
            Ok(lock) => Some(lock),
            Err(GitError::NotARepository { .. }) => None,
            Err(error) => return Err(error.into()),
        };

        // Deleting the checkout stays inside the critical section: the path is
        // proven from the catalog entry, so releasing the lock first would let
        // a concurrent import reuse the directory this call is about to erase.
        let _lock = self.lock_exclusive()?;
        let mut candidate = self.read_catalog()?;
        let project = candidate
            .projects
            .iter()
            .find(|project| project.id == id)
            .cloned()
            .ok_or(ProjectError::ProjectNotFound(id))?;
        match RemovalPolicy::from(&project.source) {
            RemovalPolicy::ManagedRepository => {}
            RemovalPolicy::Worktree { .. } => return Err(worktree_removal_required(&project)),
            RemovalPolicy::CatalogOnly => {
                return Err(unsafe_removal(
                    &project,
                    "catalog entry is not a managed clone",
                ));
            }
        }
        refuse_parent_with_worktrees(&candidate, id)?;

        let repositories_root = canonical_reserved_root(
            &self.data_dir.join(REPOSITORIES_DIRECTORY),
            &PathBuf::from(id.to_string()).join(CHECKOUT_DIRECTORY),
            &project.root,
        )
        .map_err(|unreserved| {
            unsafe_removal(
                &project,
                match unreserved {
                    UnreservedPath::StorageRootUnavailable => {
                        "managed repositories root is unavailable"
                    }
                    UnreservedPath::CandidateUnavailable => "checkout is unavailable",
                    UnreservedPath::Mismatch => {
                        "checkout is not the managed path reserved for this project"
                    }
                },
            )
        })?;
        let managed_directory = repositories_root.join(id.to_string());

        fs::remove_dir_all(&managed_directory).map_err(|source| ProjectError::ManagedRemoval {
            path: managed_directory,
            source,
        })?;
        candidate.projects.retain(|entry| entry.id != id);
        self.persist(&candidate)?;
        self.catalog = candidate;
        Ok(project)
    }

    /// Creates one Harkness-owned worktree below `worktrees/<new-project-id>`.
    ///
    /// The repository lock covers Git creation and the later catalog write.
    /// The catalog lock is taken only after it, and the parent is re-checked
    /// under that lock before the new entry becomes durable.
    pub fn create_worktree(
        &mut self,
        parent_id: ProjectId,
        base: &WorktreeBase,
        cancellation: &Cancellation,
    ) -> Result<Project, ProjectError> {
        let preliminary_catalog = self.read_catalog_shared()?;
        let preliminary_parent = preliminary_catalog
            .projects
            .iter()
            .find(|project| project.id == parent_id)
            .cloned()
            .ok_or(ProjectError::ProjectNotFound(parent_id))?;
        validate_worktree_parent(&preliminary_parent)?;

        let worktrees_root = self.data_dir.join(WORKTREES_DIRECTORY);
        fs::create_dir_all(&worktrees_root).map_err(|source| ProjectError::Persistence {
            path: worktrees_root.clone(),
            source,
        })?;
        let (id, destination) = loop {
            let id = ProjectId::new();
            let destination = self.worktree_path(id);
            if !destination.exists() {
                break (id, destination);
            }
        };

        let repository_lock = self
            .git_service(&preliminary_parent.root)
            .lock(cancellation)?;
        let added = match git::worktree::add(
            &self.git_executable,
            &preliminary_parent.root,
            &repository_lock,
            &destination,
            base,
            cancellation,
        ) {
            Ok(added) => added,
            Err(error) => {
                git::worktree::cleanup_failed_add(
                    &self.git_executable,
                    &preliminary_parent.root,
                    &repository_lock,
                    &destination,
                );
                return Err(error.into());
            }
        };

        let created = (|| {
            let canonical_root = validate_local_directory(&destination)?;
            // Canonicalization and the status walk both touch the checkout and
            // therefore finish before the global catalog lock is taken.
            let git = inspect_git(&canonical_root)?;
            let _catalog_lock = self.lock_exclusive()?;
            let mut candidate = self.read_catalog()?;
            let parent = candidate
                .projects
                .iter()
                .find(|project| project.id == parent_id)
                .ok_or(ProjectError::ProjectNotFound(parent_id))?;
            validate_worktree_parent(parent)?;
            if parent.root != preliminary_parent.root {
                return Err(ProjectError::ProjectUnavailable {
                    id: parent_id,
                    path: preliminary_parent.root.clone(),
                });
            }

            let display_name = added.branch.clone().unwrap_or_else(|| {
                let oid = added.commit.to_string();
                format!("Detached at {}", &oid[..12])
            });
            let project = Project {
                id,
                display_name,
                root: canonical_root,
                source: ProjectSource::Worktree {
                    parent: parent_id,
                    worktree_branch: added.branch.clone(),
                },
                last_opened: OffsetDateTime::now_utc(),
                available: true,
                git,
            };
            candidate.projects.push(project.clone());
            sort_recents(&mut candidate.projects);
            self.persist(&candidate)?;
            self.catalog = candidate;
            Ok(project)
        })();

        if created.is_err() {
            git::worktree::cleanup_failed_add(
                &self.git_executable,
                &preliminary_parent.root,
                &repository_lock,
                &destination,
            );
        }
        created
    }

    /// Relocates a Harkness-managed worktree through Git and records its
    /// canonical destination without changing any other project metadata.
    ///
    /// Destination validation happens before Git is allowed to mutate the
    /// checkout. The repository lock is acquired before the catalog lock, and
    /// the worktree-parent relationship plus destination are re-verified under
    /// the catalog lock immediately before the move. Destinations must be
    /// absolute, and the parent must still be a Git repository. Git implements
    /// relocation with a filesystem rename, so cross-device requests return
    /// [`GitError::WorktreeMoveAcrossDevices`] without changing the checkout.
    pub fn move_worktree(
        &mut self,
        id: ProjectId,
        destination: impl AsRef<Path>,
        cancellation: &Cancellation,
    ) -> Result<Project, ProjectError> {
        let destination = destination.as_ref();
        let preliminary_catalog = self.read_catalog_shared()?;
        let preliminary = preliminary_catalog
            .projects
            .iter()
            .find(|project| project.id == id)
            .cloned()
            .ok_or(ProjectError::ProjectNotFound(id))?;
        let preliminary_parent_id = worktree_parent_for_move(&preliminary)?;
        let preliminary_parent = preliminary_catalog
            .projects
            .iter()
            .find(|project| project.id == preliminary_parent_id)
            .cloned()
            .ok_or_else(|| {
                unsafe_worktree_move(&preliminary, "catalogued parent no longer exists")
            })?;
        validate_worktree_parent(&preliminary_parent)?;
        validate_worktree_move_source(&preliminary)?;
        let canonical_destination =
            validate_worktree_destination(&preliminary_catalog, &self.data_dir, destination)?;

        let repository_lock = self
            .git_service(&preliminary_parent.root)
            .lock(cancellation)?;
        let listed =
            git::worktree::list(&self.git_executable, &preliminary_parent.root, cancellation)?;
        match git::worktree::registered_root(&listed, &id.to_string()) {
            Some(registered_root)
                if git::worktree::same_path(registered_root, &preliminary.root) => {}
            Some(registered_root) => {
                return Err(unsafe_worktree_move(
                    &preliminary,
                    format!(
                        "Git records this checkout at '{}'",
                        registered_root.display()
                    ),
                ));
            }
            None => {
                return Err(unsafe_worktree_move(
                    &preliminary,
                    "Git has no matching administrative record",
                ));
            }
        }
        git::worktree::refuse_if_locked(&listed, &preliminary.root)?;

        // No Git command runs under the global catalog lock. This phase only
        // re-verifies the values learned before the repository lock and proves
        // that the destination remains safe immediately before mutation.
        {
            let _catalog_lock = self.lock_exclusive()?;
            let candidate = self.read_catalog()?;
            let fresh = verify_worktree_move_relationship(
                &candidate,
                id,
                preliminary_parent_id,
                &preliminary.root,
                &preliminary_parent.root,
            )?;
            validate_worktree_move_source(&fresh)?;
            let rechecked_destination =
                validate_worktree_destination(&candidate, &self.data_dir, destination)?;
            if rechecked_destination != canonical_destination {
                return Err(ProjectError::WorktreeDestinationParentUnavailable {
                    path: destination.to_path_buf(),
                    source: io::Error::other(
                        "destination parent changed while locks were acquired",
                    ),
                });
            }
        }

        git::worktree::move_known_unlocked(
            &self.git_executable,
            &preliminary_parent.root,
            &repository_lock,
            &preliminary.root,
            &canonical_destination,
            cancellation,
        )?;

        // Every failure after Git succeeds is terminally distinct: callers can
        // tell that retrying the move is wrong and reconciliation is required.
        // A SIGKILL inside Git can still strand data between rename and its
        // administrative rewrite; reconciliation deliberately preserves any
        // row it cannot identify instead of guessing or deleting files.
        let post_move = (|| -> Result<(Catalog, Project), String> {
            let listed =
                git::worktree::list(&self.git_executable, &preliminary_parent.root, cancellation)
                    .map_err(|error| error.to_string())?;
            let registered_root = git::worktree::registered_root(&listed, &id.to_string())
                .ok_or_else(|| {
                    "Git has no matching administrative record after the move".to_owned()
                })?;
            if !git::worktree::same_path(registered_root, &canonical_destination) {
                return Err(format!(
                    "Git records the moved checkout at '{}', not the requested destination",
                    registered_root.display()
                ));
            }
            let moved_root =
                validate_local_directory(registered_root).map_err(|error| error.to_string())?;
            if moved_root != canonical_destination {
                return Err(format!(
                    "destination resolved to '{}' after the move",
                    moved_root.display()
                ));
            }

            let _catalog_lock = self.lock_exclusive().map_err(|error| error.to_string())?;
            let mut candidate = self.read_catalog().map_err(|error| error.to_string())?;
            verify_worktree_move_relationship(
                &candidate,
                id,
                preliminary_parent_id,
                &preliminary.root,
                &preliminary_parent.root,
            )
            .map_err(|error| error.to_string())?;
            refuse_worktree_root_location(&candidate, &self.data_dir, &moved_root, Some(id))
                .map_err(|error| error.to_string())?;
            let project = candidate
                .projects
                .iter_mut()
                .find(|project| project.id == id)
                .expect("the relationship check found this project");
            project.root = moved_root;
            let moved = project.clone();
            self.persist(&candidate)
                .map_err(|error| error.to_string())?;
            Ok((candidate, moved))
        })();
        let (candidate, moved) =
            post_move.map_err(|reason| ProjectError::WorktreeMovedCatalogStale {
                id,
                stale_root: preliminary.root,
                destination: canonical_destination,
                reason,
            })?;
        self.catalog = candidate;
        Ok(refresh_project(moved))
    }

    /// Removes a Harkness-managed worktree through Git, then drops its catalog
    /// entry. Dirty worktrees are refused with a typed error unless `force` is
    /// explicit. The checked-out branch is never deleted.
    ///
    /// The repository lock is acquired through the parent before the catalog
    /// lock. The parent and worktree relationship is then re-verified under the
    /// catalog lock so a future creation path cannot race this removal.
    pub fn remove_worktree(
        &mut self,
        id: ProjectId,
        force: bool,
        cancellation: &Cancellation,
    ) -> Result<Project, ProjectError> {
        let preliminary_catalog = self.read_catalog_shared()?;
        let preliminary = preliminary_catalog
            .projects
            .iter()
            .find(|project| project.id == id)
            .cloned()
            .ok_or(ProjectError::ProjectNotFound(id))?;
        let preliminary_parent_id = match RemovalPolicy::from(&preliminary.source) {
            RemovalPolicy::Worktree { parent } => parent,
            RemovalPolicy::CatalogOnly | RemovalPolicy::ManagedRepository => {
                return Err(unsafe_worktree_removal(
                    &preliminary,
                    "catalog entry is not a managed worktree",
                ));
            }
        };
        let preliminary_parent = preliminary_catalog
            .projects
            .iter()
            .find(|project| project.id == preliminary_parent_id)
            .cloned()
            .ok_or_else(|| {
                unsafe_worktree_removal(&preliminary, "catalogued parent no longer exists")
            })?;
        validate_worktree_parent(&preliminary_parent)?;
        let repository_lock = match self
            .git_service(&preliminary_parent.root)
            .lock(cancellation)
        {
            Ok(lock) => Some(lock),
            Err(GitError::NotARepository { .. }) => None,
            Err(error) => return Err(error.into()),
        };
        let checkout_state = recorded_worktree_state(&preliminary)?;
        let listed = if repository_lock.is_some() {
            let listed =
                git::worktree::list(&self.git_executable, &preliminary_parent.root, cancellation)?;
            match git::worktree::registered_root(&listed, &preliminary.id.to_string()) {
                Some(registered_root)
                    if git::worktree::same_path(registered_root, &preliminary.root) => {}
                Some(registered_root) => {
                    return Err(unsafe_worktree_removal(
                        &preliminary,
                        format!(
                            "Git records this checkout at '{}'; reconcile worktrees before removal",
                            registered_root.display()
                        ),
                    ));
                }
                None if checkout_state == WorktreeCheckoutState::Available => {
                    return Err(unsafe_worktree_removal(
                        &preliminary,
                        "checkout is not the Git worktree Harkness created; reconcile worktrees before removal",
                    ));
                }
                None => {}
            }
            git::worktree::refuse_if_locked(&listed, &preliminary.root)?;
            Some(listed)
        } else {
            None
        };

        // Status can walk the entire checkout. The repository lock protects it
        // from other Harkness mutations; the global catalog lock is not needed.
        if repository_lock.is_some() && checkout_state == WorktreeCheckoutState::Available {
            let status = inspect_git(&preliminary.root)?.ok_or_else(|| {
                unsafe_worktree_removal(&preliminary, "checkout is not a Git worktree")
            })?;
            if status.dirty && !force {
                return Err(ProjectError::DirtyWorktreeRemoval {
                    id,
                    path: preliminary.root.clone(),
                });
            }
        }

        // Re-verify every value learned before the repository lock. This
        // critical section performs no Git or working-tree walk.
        {
            let _catalog_lock = self.lock_exclusive()?;
            let candidate = self.read_catalog()?;
            verify_worktree_relationship(
                &candidate,
                id,
                preliminary_parent_id,
                &preliminary.root,
                &preliminary_parent.root,
            )?;
            // Worktrees cannot have children under the catalog invariant. Keep
            // the guard as defence in depth against a future relaxed reader.
            refuse_parent_with_worktrees(&candidate, id)?;
        }

        // Git removal can recursively delete a large checkout and therefore
        // runs after the catalog lock is released. A missing checkout is still
        // removed through Git when the parent survives, selectively cleaning
        // only this Harkness-owned administrative record.
        if let Some(repository_lock) = &repository_lock {
            let has_administrative_row = listed.as_ref().is_some_and(|listed| {
                listed
                    .iter()
                    .any(|worktree| git::worktree::same_path(&worktree.root, &preliminary.root))
            });
            if has_administrative_row || preliminary.root.exists() {
                git::worktree::remove_known_unlocked(
                    &self.git_executable,
                    &preliminary_parent.root,
                    repository_lock,
                    &preliminary.root,
                    force || checkout_state == WorktreeCheckoutState::Missing,
                    cancellation,
                )?;
            }
        }

        let _catalog_lock = self.lock_exclusive()?;
        let mut candidate = self.read_catalog()?;
        let project = verify_worktree_relationship(
            &candidate,
            id,
            preliminary_parent_id,
            &preliminary.root,
            &preliminary_parent.root,
        )?;
        candidate.projects.retain(|entry| entry.id != id);
        self.persist(&candidate)?;
        self.catalog = candidate;
        Ok(project)
    }

    /// Reconciles missing or externally moved Harkness worktrees without
    /// touching external ones.
    ///
    /// A live Git row with the stable administrative name created from the
    /// project identifier repairs the catalogued root. Otherwise, for each
    /// Harkness-owned checkout that is absent, this removes only that path's Git
    /// administrative record and then drops its catalog row. A locked worktree
    /// is kept because its missing path may be an intentionally unmounted
    /// volume. No repository-wide `git worktree prune` is run.
    pub fn reconcile_worktrees(
        &mut self,
        parent_id: ProjectId,
        cancellation: &Cancellation,
    ) -> Result<WorktreeReconciliation, ProjectError> {
        let preliminary_catalog = self.read_catalog_shared()?;
        let preliminary_parent = preliminary_catalog
            .projects
            .iter()
            .find(|project| project.id == parent_id)
            .cloned()
            .ok_or(ProjectError::ProjectNotFound(parent_id))?;
        validate_worktree_parent(&preliminary_parent)?;
        if !preliminary_catalog.projects.iter().any(|project| {
            matches!(
                project.source,
                ProjectSource::Worktree { parent, .. } if parent == parent_id
            )
        }) {
            return Ok(WorktreeReconciliation::default());
        }

        let preliminary_worktrees = preliminary_catalog
            .projects
            .iter()
            .filter(|project| {
                matches!(
                    project.source,
                    ProjectSource::Worktree { parent, .. } if parent == parent_id
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut outcome = WorktreeReconciliation::default();
        let mut worktree_states = Vec::new();
        for project in preliminary_worktrees {
            match recorded_worktree_state(&project) {
                Ok(state) => worktree_states.push((project, state)),
                Err(error) => outcome.skipped.push(WorktreeReconciliationSkip {
                    project,
                    reason: error.to_string(),
                }),
            }
        }
        let repository_lock = match self
            .git_service(&preliminary_parent.root)
            .lock(cancellation)
        {
            Ok(lock) => Some(lock),
            Err(GitError::NotARepository { .. }) => None,
            Err(error) => return Err(error.into()),
        };

        {
            let _catalog_lock = self.lock_exclusive()?;
            let candidate = self.read_catalog()?;
            let parent = candidate
                .projects
                .iter()
                .find(|project| project.id == parent_id)
                .ok_or(ProjectError::ProjectNotFound(parent_id))?;
            validate_worktree_parent(parent)?;
            if parent.root != preliminary_parent.root {
                return Err(ProjectError::ProjectUnavailable {
                    id: parent_id,
                    path: preliminary_parent.root.clone(),
                });
            }
        }

        let mut removable = Vec::new();
        let mut git_removals = Vec::new();
        let mut repairs = Vec::new();
        let mut terminal_error = None;
        if let Some(repository_lock) = &repository_lock {
            let live =
                git::worktree::list(&self.git_executable, &preliminary_parent.root, cancellation)?;
            for (project, state) in &worktree_states {
                let registered = git::worktree::registered_root(&live, &project.id.to_string());
                if *state == WorktreeCheckoutState::Available {
                    match registered {
                        Some(root) if git::worktree::same_path(root, &project.root) => {}
                        Some(root) => outcome.skipped.push(WorktreeReconciliationSkip {
                            project: project.clone(),
                            reason: format!(
                                "Git records this checkout at '{}' instead of its available catalogued root",
                                root.display()
                            ),
                        }),
                        None => outcome.skipped.push(WorktreeReconciliationSkip {
                            project: project.clone(),
                            reason: "available checkout is not the Git worktree Harkness created"
                                .to_owned(),
                        }),
                    }
                    continue;
                }
                let listed = live
                    .iter()
                    .find(|worktree| git::worktree::same_path(&worktree.root, &project.root));
                if let Some(worktree) = listed
                    && let Some(reason) = &worktree.locked
                {
                    outcome.skipped.push(WorktreeReconciliationSkip {
                        project: project.clone(),
                        reason: if reason.is_empty() {
                            "Git has locked the missing worktree".to_owned()
                        } else {
                            format!("Git has locked the missing worktree: {reason}")
                        },
                    });
                    continue;
                }
                if listed.is_some() {
                    git_removals.push(project.clone());
                    continue;
                }
                if let Some(moved_root) = registered {
                    let repair = validate_local_directory(moved_root).and_then(|moved_root| {
                        refuse_worktree_root_location(
                            &preliminary_catalog,
                            &self.data_dir,
                            &moved_root,
                            Some(project.id),
                        )?;
                        Ok(moved_root)
                    });
                    match repair {
                        Ok(moved_root) => {
                            repairs.push((project.clone(), moved_root));
                        }
                        Err(error) => outcome.skipped.push(WorktreeReconciliationSkip {
                            project: project.clone(),
                            reason: error.to_string(),
                        }),
                    }
                    continue;
                }
                removable.push((project.id, project.root.clone()));
            }

            for project in git_removals {
                match git::worktree::remove_known_unlocked(
                    &self.git_executable,
                    &preliminary_parent.root,
                    repository_lock,
                    &project.root,
                    true,
                    cancellation,
                ) {
                    Ok(()) => removable.push((project.id, project.root.clone())),
                    Err(GitError::Cancelled) => {
                        terminal_error = Some(ProjectError::Git(GitError::Cancelled));
                        break;
                    }
                    Err(error) => outcome.skipped.push(WorktreeReconciliationSkip {
                        project,
                        reason: error.to_string(),
                    }),
                }
            }
        } else {
            removable.extend(
                worktree_states
                    .iter()
                    .filter(|(_, state)| *state == WorktreeCheckoutState::Missing)
                    .map(|(project, _)| (project.id, project.root.clone())),
            );
            outcome.skipped.extend(
                worktree_states
                    .iter()
                    .filter(|(_, state)| *state == WorktreeCheckoutState::Available)
                    .map(|(project, _)| WorktreeReconciliationSkip {
                        project: project.clone(),
                        reason: "parent repository is unavailable; checkout ownership cannot be verified"
                            .to_owned(),
                    }),
            );
        }

        let _catalog_lock = self.lock_exclusive()?;
        let mut candidate = self.read_catalog()?;
        let parent = candidate
            .projects
            .iter()
            .find(|project| project.id == parent_id)
            .ok_or(ProjectError::ProjectNotFound(parent_id))?;
        validate_worktree_parent(parent)?;
        if parent.root != preliminary_parent.root {
            return Err(ProjectError::ProjectUnavailable {
                id: parent_id,
                path: preliminary_parent.root,
            });
        }

        let mut applicable_repairs = Vec::new();
        for (project, moved_root) in repairs {
            let verified = verify_worktree_relationship(
                &candidate,
                project.id,
                parent_id,
                &project.root,
                &preliminary_parent.root,
            )
            .and_then(|_| {
                let canonical_root = validate_local_directory(&moved_root)?;
                if canonical_root != moved_root {
                    return Err(ProjectError::ProjectUnavailable {
                        id: project.id,
                        path: moved_root.clone(),
                    });
                }
                refuse_worktree_root_location(
                    &candidate,
                    &self.data_dir,
                    &moved_root,
                    Some(project.id),
                )?;
                Ok(())
            });
            match verified {
                Ok(()) => applicable_repairs.push((project, moved_root)),
                Err(error) => outcome.skipped.push(WorktreeReconciliationSkip {
                    project,
                    reason: error.to_string(),
                }),
            }
        }
        for (project, moved_root) in applicable_repairs {
            let repaired = candidate
                .projects
                .iter_mut()
                .find(|entry| entry.id == project.id)
                .expect("the relationship check found this project");
            repaired.root = moved_root;
            outcome.repaired.push(repaired.clone());
        }

        candidate.projects.retain(|project| {
            let selected = removable
                .iter()
                .any(|(id, root)| *id == project.id && *root == project.root);
            let keep = !selected
                || !matches!(
                    project.source,
                    ProjectSource::Worktree { parent, .. } if parent == parent_id
                )
                || project.root.exists();
            if !keep {
                outcome.removed.push(project.clone());
            } else if selected && project.root.exists() {
                outcome.skipped.push(WorktreeReconciliationSkip {
                    project: project.clone(),
                    reason: "checkout reappeared before its catalog row could be removed"
                        .to_owned(),
                });
            }
            keep
        });
        if !outcome.repaired.is_empty() || !outcome.removed.is_empty() {
            self.persist(&candidate)?;
        }
        self.catalog = candidate;
        if let Some(error) = terminal_error {
            Err(error)
        } else {
            Ok(outcome)
        }
    }

    /// Compatibility name for callers that adopted the original API. This is
    /// now selective reconciliation rather than repository-wide pruning.
    pub fn prune_worktrees(
        &mut self,
        parent_id: ProjectId,
        cancellation: &Cancellation,
    ) -> Result<WorktreeReconciliation, ProjectError> {
        self.reconcile_worktrees(parent_id, cancellation)
    }

    /// Lists Harkness-owned and external linked worktrees for `parent_id`.
    ///
    /// External worktrees have `project: None` and are therefore observable
    /// but cannot be removed through Harkness's identifier-based API.
    pub fn worktrees(
        &self,
        parent_id: ProjectId,
        cancellation: &Cancellation,
    ) -> Result<Vec<Worktree>, ProjectError> {
        let catalog = self.read_catalog_shared()?;
        let parent = catalog
            .projects
            .iter()
            .find(|project| project.id == parent_id)
            .cloned()
            .ok_or(ProjectError::ProjectNotFound(parent_id))?;
        validate_worktree_parent(&parent)?;

        let mut managed = catalog
            .projects
            .into_iter()
            .filter(|project| {
                matches!(
                    project.source,
                    ProjectSource::Worktree { parent, .. } if parent == parent_id
                )
            })
            .map(|project| {
                if cancellation.is_cancelled() {
                    Err(ProjectError::Git(GitError::Cancelled))
                } else {
                    Ok(refresh_project(project))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let listed = git::worktree::list(&self.git_executable, &parent.root, cancellation)?;
        let mut worktrees = Vec::new();
        for row in listed {
            if git::worktree::same_path(&row.root, &parent.root) {
                continue;
            }
            let project = managed
                .iter()
                .position(|project| git::worktree::same_path(&project.root, &row.root))
                .map(|index| managed.remove(index));
            worktrees.push(Worktree {
                root: row.root,
                branch: row.branch,
                locked: row.locked.is_some(),
                prunable: row.prunable,
                project,
            });
        }
        worktrees.extend(managed.into_iter().filter_map(|project| {
            let branch = match &project.source {
                ProjectSource::Worktree {
                    worktree_branch, ..
                } => worktree_branch.clone(),
                ProjectSource::Local | ProjectSource::ManagedRepository { .. } => return None,
            };
            Some(Worktree {
                root: project.root.clone(),
                branch,
                locked: false,
                prunable: false,
                project: Some(project),
            })
        }));
        worktrees.sort_by(|left, right| left.root.cmp(&right.root));
        Ok(worktrees)
    }

    /// Returns the location reserved for a worktree project identifier without
    /// creating it.
    #[must_use]
    pub fn worktree_path(&self, id: ProjectId) -> PathBuf {
        self.data_dir.join(WORKTREES_DIRECTORY).join(id.to_string())
    }

    /// Resolves a catalog entry to the Git service for its root.
    ///
    /// The catalog is read under a shared lock that is released before this
    /// returns, so the caller is free to take a repository lock afterwards
    /// without violating the ordering documented on [`RepositoryLock`].
    ///
    /// [`RepositoryLock`]: crate::RepositoryLock
    pub fn git(&self, id: ProjectId) -> Result<GitService, ProjectError> {
        let catalog = self.read_catalog_shared()?;
        let project = catalog
            .projects
            .into_iter()
            .find(|project| project.id == id)
            .ok_or(ProjectError::ProjectNotFound(id))?;
        Ok(self.git_service(&project.root))
    }

    /// Addresses one repository with this service's Git executable.
    fn git_service(&self, root: &Path) -> GitService {
        GitService::new(root, &self.data_dir).with_git_executable(&self.git_executable)
    }

    fn persist(&self, catalog: &Catalog) -> Result<(), ProjectError> {
        let catalog_path = self.data_dir.join(CATALOG_FILE);
        catalog::persist_catalog(&self.data_dir, &catalog_path, catalog).map_err(|source| {
            ProjectError::Persistence {
                path: catalog_path,
                source,
            }
        })
    }
}

/// Excludes reconciliation while one managed checkout is being cloned and
/// excludes duplicate reconcilers from deleting the same orphan.
struct ManagedImportLock {
    file: File,
}

impl ManagedImportLock {
    fn acquire(data_dir: &Path, id: ProjectId) -> Result<Self, ProjectError> {
        let (file, path) = Self::open(data_dir, id)?;
        file.lock()
            .map_err(|source| ProjectError::ManagedRepositoryLock { path, source })?;
        Ok(Self { file })
    }

    fn try_acquire(data_dir: &Path, id: ProjectId) -> Result<Option<Self>, ProjectError> {
        let (file, path) = Self::open(data_dir, id)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(source)) => {
                Err(ProjectError::ManagedRepositoryLock { path, source })
            }
        }
    }

    fn open(data_dir: &Path, id: ProjectId) -> Result<(File, PathBuf), ProjectError> {
        let locks_directory = data_dir.join(LOCKS_DIRECTORY);
        fs::create_dir_all(&locks_directory).map_err(|source| {
            ProjectError::ManagedRepositoryLock {
                path: locks_directory.clone(),
                source,
            }
        })?;
        let path = locks_directory.join(format!("managed-import-{id}.lock"));
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| ProjectError::ManagedRepositoryLock {
                path: path.clone(),
                source,
            })?;
        Ok((file, path))
    }
}

impl Drop for ManagedImportLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

enum RemovalPolicy {
    CatalogOnly,
    ManagedRepository,
    Worktree { parent: ProjectId },
}

impl From<&ProjectSource> for RemovalPolicy {
    fn from(source: &ProjectSource) -> Self {
        match source {
            ProjectSource::Local => Self::CatalogOnly,
            ProjectSource::ManagedRepository { .. } => Self::ManagedRepository,
            ProjectSource::Worktree { parent, .. } => Self::Worktree { parent: *parent },
        }
    }
}

struct DisplayWorktrees<'a>(&'a [PathBuf]);

impl std::fmt::Display for DisplayWorktrees<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, path) in self.0.iter().enumerate() {
            if index > 0 {
                formatter.write_str(", ")?;
            }
            write!(formatter, "'{}'", path.display())?;
        }
        Ok(())
    }
}

struct DisplayProjects<'a>(&'a [Project]);

impl std::fmt::Display for DisplayProjects<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, project) in self.0.iter().enumerate() {
            if index > 0 {
                formatter.write_str(", ")?;
            }
            write!(
                formatter,
                "{} ('{}' at '{}')",
                project.id,
                project.display_name,
                project.root.display()
            )?;
        }
        Ok(())
    }
}

fn resolve_value(projects: &[Project], value: &str) -> Result<Project, ProjectError> {
    if let Ok(id) = value.parse::<ProjectId>()
        && let Some(project) = projects.iter().find(|project| project.id == id)
    {
        return Ok(project.clone());
    }

    let lowercase = value.to_ascii_lowercase();
    if is_uuid_prefix(&lowercase) {
        let matches = projects
            .iter()
            .filter(|project| project.id.to_string().starts_with(&lowercase))
            .cloned()
            .collect::<Vec<_>>();
        if !matches.is_empty() {
            return unique_match(value, matches);
        }
    }

    let written_path = PathBuf::from(value);
    if is_explicit_path_selector(&written_path)
        && let Some(selector_path) = normalize_selector_path(&written_path)
    {
        let path_matches = projects
            .iter()
            .filter(|project| {
                normalize_selector_path(&project.root)
                    .is_some_and(|root| selector_paths_equal(&root, &selector_path))
            })
            .cloned()
            .collect::<Vec<_>>();
        if !path_matches.is_empty() {
            return unique_match(value, path_matches);
        }
    }

    let exact_names = projects
        .iter()
        .filter(|project| project.display_name == value)
        .cloned()
        .collect::<Vec<_>>();
    if !exact_names.is_empty() {
        return unique_match(value, exact_names);
    }

    let folded = value.to_lowercase();
    let insensitive_names = projects
        .iter()
        .filter(|project| project.display_name.to_lowercase() == folded)
        .cloned()
        .collect::<Vec<_>>();
    if !insensitive_names.is_empty() {
        return unique_match(value, insensitive_names);
    }

    Err(ProjectError::ProjectSelectorNotFound {
        selector: value.to_owned(),
    })
}

fn resolve_current_directory(
    projects: &[Project],
    directory: &Path,
) -> Result<Project, ProjectError> {
    let canonical =
        fs::canonicalize(directory).map_err(|source| ProjectError::UnreadableDirectory {
            path: directory.to_path_buf(),
            source,
        })?;
    let mut matches = projects
        .iter()
        .filter(|project| canonical.starts_with(&project.root))
        .cloned()
        .collect::<Vec<_>>();
    let Some(deepest) = matches
        .iter()
        .map(|project| project.root.components().count())
        .max()
    else {
        return Err(ProjectError::ProjectSelectorNotFound {
            selector: canonical.display().to_string(),
        });
    };
    matches.retain(|project| project.root.components().count() == deepest);
    unique_match(&canonical.display().to_string(), matches)
}

fn unique_match(selector: &str, mut candidates: Vec<Project>) -> Result<Project, ProjectError> {
    if candidates.len() == 1 {
        return Ok(candidates.pop().expect("one candidate was checked above"));
    }
    candidates.sort_by_key(|project| project.id);
    Err(ProjectError::AmbiguousProjectSelector {
        selector: selector.to_owned(),
        candidates,
    })
}

fn is_uuid_prefix(value: &str) -> bool {
    (8..36).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
}

/// Bare words are names. Only an absolute path or a dot-relative path is
/// allowed to depend on the process working directory.
fn is_explicit_path_selector(path: &Path) -> bool {
    path.is_absolute()
        || matches!(
            path.components().next(),
            Some(Component::CurDir | Component::ParentDir)
        )
}

/// Canonicalizes the longest existing prefix and appends any missing suffix.
///
/// Canonicalizing only the whole selector would make a deleted catalog root
/// impossible to select. Resolving the existing prefix also normalizes macOS's
/// `/var` -> `/private/var` alias and Windows's verbatim path spelling before
/// comparing the missing final components.
fn normalize_selector_path(path: &Path) -> Option<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let mut cursor = absolute.as_path();
    let mut missing = Vec::<OsString>::new();
    loop {
        if let Ok(mut canonical) = fs::canonicalize(cursor) {
            for component in missing.iter().rev() {
                canonical.push(component);
            }
            return Some(platform_comparable_path(canonical));
        }
        let name = cursor.file_name()?.to_os_string();
        missing.push(name);
        cursor = cursor.parent()?;
    }
}

#[cfg(windows)]
fn platform_comparable_path(path: PathBuf) -> PathBuf {
    let displayed = path.to_string_lossy();
    if let Some(rest) = displayed.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = displayed.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path
    }
}

#[cfg(not(windows))]
fn platform_comparable_path(path: PathBuf) -> PathBuf {
    path
}

#[cfg(any(windows, target_os = "macos"))]
fn selector_paths_equal(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn selector_paths_equal(left: &Path, right: &Path) -> bool {
    left == right
}

fn refuse_parent_with_worktrees(catalog: &Catalog, id: ProjectId) -> Result<(), ProjectError> {
    let mut worktrees = catalog
        .projects
        .iter()
        .filter_map(|project| match &project.source {
            ProjectSource::Worktree { parent, .. } if *parent == id => Some(project.root.clone()),
            ProjectSource::Local
            | ProjectSource::ManagedRepository { .. }
            | ProjectSource::Worktree { .. } => None,
        })
        .collect::<Vec<_>>();
    worktrees.sort();
    if worktrees.is_empty() {
        Ok(())
    } else {
        Err(ProjectError::ParentHasWorktrees { id, worktrees })
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum WorktreeCheckoutState {
    Available,
    Missing,
}

fn recorded_worktree_state(project: &Project) -> Result<WorktreeCheckoutState, ProjectError> {
    match fs::canonicalize(&project.root) {
        Ok(root)
            if selector_paths_equal(
                &platform_comparable_path(root.clone()),
                &platform_comparable_path(project.root.clone()),
            ) =>
        {
            Ok(WorktreeCheckoutState::Available)
        }
        Ok(_) => Err(unsafe_worktree_removal(
            project,
            "checkout does not canonically equal its catalogued path",
        )),
        Err(_)
            if normalize_selector_path(&project.root).is_some_and(|normalized| {
                selector_paths_equal(&normalized, &platform_comparable_path(project.root.clone()))
            }) =>
        {
            Ok(WorktreeCheckoutState::Missing)
        }
        Err(_) => Err(unsafe_worktree_removal(
            project,
            "unavailable checkout path cannot be resolved canonically",
        )),
    }
}

fn validate_worktree_destination(
    catalog: &Catalog,
    data_dir: &Path,
    destination: &Path,
) -> Result<PathBuf, ProjectError> {
    if !destination.is_absolute() {
        return Err(ProjectError::WorktreeDestinationNotAbsolute {
            path: destination.to_path_buf(),
        });
    }
    match fs::symlink_metadata(destination) {
        Ok(_) => {
            return Err(ProjectError::WorktreeDestinationExists {
                path: destination.to_path_buf(),
            });
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(ProjectError::WorktreeDestinationParentUnavailable {
                path: destination.to_path_buf(),
                source,
            });
        }
    }

    let absolute = destination.to_path_buf();
    let parent =
        absolute
            .parent()
            .ok_or_else(|| ProjectError::WorktreeDestinationParentUnavailable {
                path: destination.to_path_buf(),
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "destination has no parent directory",
                ),
            })?;
    let name =
        absolute
            .file_name()
            .ok_or_else(|| ProjectError::WorktreeDestinationParentUnavailable {
                path: destination.to_path_buf(),
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "destination has no final path component",
                ),
            })?;
    let canonical_parent = fs::canonicalize(parent).map_err(|source| {
        ProjectError::WorktreeDestinationParentUnavailable {
            path: destination.to_path_buf(),
            source,
        }
    })?;
    match fs::metadata(&canonical_parent) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(ProjectError::WorktreeDestinationParentUnavailable {
                path: destination.to_path_buf(),
                source: io::Error::new(
                    io::ErrorKind::NotADirectory,
                    "destination parent is not a directory",
                ),
            });
        }
        Err(source) => {
            return Err(ProjectError::WorktreeDestinationParentUnavailable {
                path: destination.to_path_buf(),
                source,
            });
        }
    }
    let canonical_destination = canonical_parent.join(name);
    match fs::symlink_metadata(&canonical_destination) {
        Ok(_) => {
            return Err(ProjectError::WorktreeDestinationExists {
                path: destination.to_path_buf(),
            });
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(ProjectError::WorktreeDestinationParentUnavailable {
                path: destination.to_path_buf(),
                source,
            });
        }
    }
    refuse_worktree_root_location(catalog, data_dir, &canonical_destination, None)?;
    Ok(canonical_destination)
}

fn refuse_worktree_root_location(
    catalog: &Catalog,
    data_dir: &Path,
    destination: &Path,
    ignored: Option<ProjectId>,
) -> Result<(), ProjectError> {
    let canonical_data_dir = fs::canonicalize(data_dir).map_err(|source| {
        ProjectError::WorktreeDestinationParentUnavailable {
            path: destination.to_path_buf(),
            source,
        }
    })?;
    if path_starts_with(destination, &canonical_data_dir) {
        return Err(ProjectError::WorktreeDestinationInsideDataDirectory {
            path: destination.to_path_buf(),
            data_dir: canonical_data_dir,
        });
    }
    for project in catalog
        .projects
        .iter()
        .filter(|project| Some(project.id) != ignored)
    {
        if path_starts_with(destination, &project.root) {
            return Err(ProjectError::WorktreeDestinationInsideProject {
                path: destination.to_path_buf(),
                project_id: project.id,
                project_root: project.root.clone(),
            });
        }
        if path_starts_with(&project.root, destination) {
            return Err(ProjectError::WorktreeDestinationContainsProject {
                path: destination.to_path_buf(),
                project_id: project.id,
                project_root: project.root.clone(),
            });
        }
    }
    Ok(())
}

fn path_starts_with(candidate: &Path, root: &Path) -> bool {
    let mut candidate = candidate.components();
    root.components().all(|root_component| {
        candidate.next().is_some_and(|candidate_component| {
            components_equal(candidate_component, root_component)
        })
    })
}

#[cfg(any(windows, target_os = "macos"))]
fn components_equal(left: Component<'_>, right: Component<'_>) -> bool {
    left.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn components_equal(left: Component<'_>, right: Component<'_>) -> bool {
    left == right
}

fn verify_worktree_relationship(
    catalog: &Catalog,
    id: ProjectId,
    expected_parent: ProjectId,
    expected_root: &Path,
    expected_parent_root: &Path,
) -> Result<Project, ProjectError> {
    let project = catalog
        .projects
        .iter()
        .find(|project| project.id == id)
        .cloned()
        .ok_or(ProjectError::ProjectNotFound(id))?;
    let parent_id = match RemovalPolicy::from(&project.source) {
        RemovalPolicy::Worktree { parent } => parent,
        RemovalPolicy::CatalogOnly | RemovalPolicy::ManagedRepository => {
            return Err(unsafe_worktree_removal(
                &project,
                "catalog entry is not a managed worktree",
            ));
        }
    };
    if parent_id != expected_parent {
        return Err(unsafe_worktree_removal(
            &project,
            "worktree parent changed while the repository lock was being acquired",
        ));
    }
    if project.root != expected_root {
        return Err(unsafe_worktree_removal(
            &project,
            "worktree path changed while the repository lock was being acquired",
        ));
    }
    let parent = catalog
        .projects
        .iter()
        .find(|entry| entry.id == parent_id)
        .ok_or_else(|| unsafe_worktree_removal(&project, "catalogued parent no longer exists"))?;
    if parent.root != expected_parent_root {
        return Err(unsafe_worktree_removal(
            &project,
            "parent path changed while the repository lock was being acquired",
        ));
    }
    Ok(project)
}

fn worktree_removal_required(project: &Project) -> ProjectError {
    ProjectError::WorktreeRemovalRequired {
        id: project.id,
        path: project.root.clone(),
    }
}

fn unsafe_removal(project: &Project, reason: impl Into<String>) -> ProjectError {
    ProjectError::UnsafeManagedRemoval {
        id: project.id,
        path: project.root.clone(),
        reason: reason.into(),
    }
}

fn unsafe_worktree_removal(project: &Project, reason: impl Into<String>) -> ProjectError {
    ProjectError::UnsafeWorktreeRemoval {
        id: project.id,
        path: project.root.clone(),
        reason: reason.into(),
    }
}

fn unsafe_worktree_move(project: &Project, reason: impl Into<String>) -> ProjectError {
    ProjectError::UnsafeWorktreeMove {
        id: project.id,
        path: project.root.clone(),
        reason: reason.into(),
    }
}

fn worktree_parent_for_move(project: &Project) -> Result<ProjectId, ProjectError> {
    match RemovalPolicy::from(&project.source) {
        RemovalPolicy::Worktree { parent } => Ok(parent),
        RemovalPolicy::CatalogOnly | RemovalPolicy::ManagedRepository => Err(unsafe_worktree_move(
            project,
            "catalog entry is not a managed worktree",
        )),
    }
}

fn validate_worktree_move_source(project: &Project) -> Result<(), ProjectError> {
    match fs::canonicalize(&project.root) {
        Ok(root)
            if selector_paths_equal(
                &platform_comparable_path(root.clone()),
                &platform_comparable_path(project.root.clone()),
            ) =>
        {
            Ok(())
        }
        Ok(_) => Err(unsafe_worktree_move(
            project,
            "checkout does not canonically equal its catalogued path",
        )),
        Err(_) => Err(unsafe_worktree_move(
            project,
            "catalogued checkout is unavailable",
        )),
    }
}

fn verify_worktree_move_relationship(
    catalog: &Catalog,
    id: ProjectId,
    expected_parent: ProjectId,
    expected_root: &Path,
    expected_parent_root: &Path,
) -> Result<Project, ProjectError> {
    let project = catalog
        .projects
        .iter()
        .find(|project| project.id == id)
        .cloned()
        .ok_or(ProjectError::ProjectNotFound(id))?;
    let parent_id = worktree_parent_for_move(&project)?;
    if parent_id != expected_parent {
        return Err(unsafe_worktree_move(
            &project,
            "worktree parent changed while the repository lock was being acquired",
        ));
    }
    if project.root != expected_root {
        return Err(unsafe_worktree_move(
            &project,
            "worktree path changed while the repository lock was being acquired",
        ));
    }
    let parent = catalog
        .projects
        .iter()
        .find(|entry| entry.id == parent_id)
        .ok_or_else(|| unsafe_worktree_move(&project, "catalogued parent no longer exists"))?;
    if parent.root != expected_parent_root {
        return Err(unsafe_worktree_move(
            &project,
            "parent path changed while the repository lock was being acquired",
        ));
    }
    validate_worktree_parent(parent)?;
    Ok(project)
}

fn validate_worktree_parent(project: &Project) -> Result<(), ProjectError> {
    if matches!(project.source, ProjectSource::Worktree { .. })
        || project.root.join(".git").is_file()
    {
        Err(ProjectError::WorktreeParentUnsupported {
            id: project.id,
            path: project.root.clone(),
        })
    } else {
        Ok(())
    }
}

fn validate_local_directory(path: &Path) -> Result<PathBuf, ProjectError> {
    let metadata = fs::metadata(path).map_err(|source| match source.kind() {
        io::ErrorKind::NotFound => ProjectError::InvalidDirectory {
            path: path.to_path_buf(),
            reason: "path does not exist".to_owned(),
        },
        _ => ProjectError::UnreadableDirectory {
            path: path.to_path_buf(),
            source,
        },
    })?;
    if !metadata.is_dir() {
        return Err(ProjectError::InvalidDirectory {
            path: path.to_path_buf(),
            reason: "path is not a directory".to_owned(),
        });
    }

    let canonical = fs::canonicalize(path).map_err(|source| ProjectError::UnreadableDirectory {
        path: path.to_path_buf(),
        source,
    })?;
    fs::read_dir(&canonical).map_err(|source| ProjectError::UnreadableDirectory {
        path: canonical.clone(),
        source,
    })?;
    Ok(canonical)
}

/// Recomputes the derived state of a catalog entry.
///
/// This runs on every ambient read, so an unreadable repository degrades to
/// "no Git metadata" instead of failing the caller. Only [`import_local`], an
/// explicit action against one directory, reports inspection failures.
///
/// [`import_local`]: ProjectService::import_local
fn refresh_project(mut project: Project) -> Project {
    project.available = fs::metadata(&project.root).is_ok_and(|metadata| metadata.is_dir())
        && fs::read_dir(&project.root).is_ok();
    project.git = if project.available {
        inspect_git(&project.root).unwrap_or_default()
    } else {
        None
    };
    project
}

/// Describes a project root as a Git repository, or reports that it is not one.
///
/// Spawns nothing: this runs for every catalog entry on every ambient read.
fn inspect_git(path: &Path) -> Result<Option<GitStatus>, ProjectError> {
    git::status::inspect(path).map_err(Into::into)
}

fn display_name(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| root.display().to_string())
}

fn sort_recents(projects: &mut [Project]) {
    projects.sort_by(|left, right| {
        right
            .last_opened
            .cmp(&left.last_opened)
            .then_with(|| left.id.cmp(&right.id))
    });
}

#[cfg(test)]
mod tests {
    use std::{
        fs, io,
        path::{Path, PathBuf},
        thread,
        time::Duration,
    };

    use super::{
        ManagedImportLock, ProjectError, ProjectSelector, ProjectService, WorktreeBase,
        is_explicit_path_selector, sort_recents,
    };
    use crate::{
        catalog::{
            CATALOG_VERSION, MINIMUM_SUPPORTED_CATALOG_VERSION,
            entry::{Project, ProjectId, ProjectSource},
        },
        git::{Cancellation, CloneCancellation, GitError},
        list_directory,
        paths::{
            CATALOG_FILE, CATALOG_LOCK_FILE, CHECKOUT_DIRECTORY, DATA_DIRECTORY_ENV,
            REPOSITORIES_DIRECTORY, WORKTREES_DIRECTORY,
        },
        testing::{
            Fixture, PROCESS_BRANCH_ENV, PROCESS_GIT_EXECUTABLE_ENV, PROCESS_PROJECT_ID_ENV,
            PROCESS_PROJECT_ROOT_ENV, PROCESS_READY_FILE_ENV, SCRUBBED_ENVIRONMENT, commit_all,
            git, initialize_repository, spawn_child, wait_for_child_signal,
        },
    };

    /// Coarse enough for the ~15ms system clock granularity on Windows.
    const CLOCK_TICK: Duration = Duration::from_millis(25);

    const VERSION_ONE_CATALOG: &str = include_str!("catalog/fixtures/v1.json");
    const VERSION_TWO_CATALOG: &str = include_str!("catalog/fixtures/v2.json");

    /// A path in the form the catalog records it.
    ///
    /// Every root is canonicalized on the way in, and on two of the three
    /// platforms the tests run on that is not the path the fixture handed out:
    /// macOS resolves the temporary directory's `/var` symlink to
    /// `/private/var`, and Windows expands the 8.3 short name `TEMP` usually
    /// carries and adds the extended-length prefix. Comparing against the
    /// fixture's own spelling asserts that the temporary directory has a boring
    /// name, which is not the property under test anywhere it appears.
    fn as_catalogued(root: &Path) -> PathBuf {
        fs::canonicalize(root).expect("a fixture root is always canonicalizable")
    }

    fn catalog_fixture(template: &str, replacements: &[(&str, &Path)]) -> Vec<u8> {
        let mut resolved = template.to_owned();
        for (placeholder, path) in replacements {
            let quoted_path = serde_json::to_string(&path.to_string_lossy()).unwrap();
            resolved = resolved.replace(&format!("\"{placeholder}\""), &quoted_path);
        }
        resolved.into_bytes()
    }

    fn catalogue_worktree(
        service: &mut ProjectService,
        parent: ProjectId,
        branch: &str,
    ) -> Project {
        let id = ProjectId::new();
        let root = service.worktree_path(id);
        fs::create_dir_all(&root).unwrap();
        store_worktree(service, parent, id, root, branch)
    }

    fn catalogue_git_worktree(
        service: &mut ProjectService,
        parent: &Project,
        branch: &str,
    ) -> Project {
        let id = ProjectId::new();
        let root = service.worktree_path(id);
        git(
            &parent.root,
            [
                "worktree",
                "add",
                "-b",
                branch,
                "--",
                root.to_str().unwrap(),
            ],
        );
        store_worktree(service, parent.id, id, root, branch)
    }

    fn create_branch_worktree(
        service: &mut ProjectService,
        parent: ProjectId,
        branch: &str,
    ) -> Project {
        service
            .create_worktree(
                parent,
                &WorktreeBase::NewBranch {
                    name: branch.to_owned(),
                    start_point: None,
                },
                &Cancellation::default(),
            )
            .unwrap()
    }

    fn store_worktree(
        service: &mut ProjectService,
        parent: ProjectId,
        id: ProjectId,
        root: PathBuf,
        branch: &str,
    ) -> Project {
        let worktree = Project {
            id,
            display_name: branch.to_owned(),
            root: as_catalogued(&root),
            source: ProjectSource::Worktree {
                parent,
                worktree_branch: Some(branch.to_owned()),
            },
            last_opened: time::OffsetDateTime::now_utc(),
            available: true,
            git: None,
        };
        let _lock = service.lock_exclusive().unwrap();
        let mut candidate = service.read_catalog().unwrap();
        assert!(candidate.projects.iter().any(|entry| entry.id == parent));
        candidate.projects.push(worktree.clone());
        sort_recents(&mut candidate.projects);
        service.persist(&candidate).unwrap();
        service.catalog = candidate;
        worktree
    }

    fn managed_remote(project: &Project) -> &str {
        match &project.source {
            ProjectSource::ManagedRepository { remote } => remote,
            ProjectSource::Local | ProjectSource::Worktree { .. } => {
                panic!("project {} is not a managed repository", project.id)
            }
        }
    }

    #[test]
    fn catalog_round_trip_preserves_project_data() {
        let fixture = Fixture::new();
        let project_root = fixture.directory("sample");
        let mut service = fixture.service();

        let imported = service.import_local(&project_root).unwrap();
        let reloaded = ProjectService::load_from_data_dir(&fixture.data_dir).unwrap();

        assert_eq!(reloaded.list().unwrap(), service.list().unwrap());
        assert_eq!(reloaded.list().unwrap(), vec![imported]);
    }

    #[test]
    fn selectors_resolve_in_precedence_order_and_current_directory_uses_deepest_root() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("selector-parent");
        let nested_root = parent_root.join("NestedProject");
        let working_directory = nested_root.join("src");
        fs::create_dir_all(&working_directory).unwrap();
        let mut service = fixture.service();
        service.import_local(&parent_root).unwrap();
        let nested = service.import_local(&nested_root).unwrap();
        let id = nested.id.to_string();

        for selector in [
            ProjectSelector::from(id.as_str()),
            ProjectSelector::from(&id[..8]),
            ProjectSelector::from(nested.root.to_string_lossy().into_owned()),
            ProjectSelector::from("NestedProject"),
            ProjectSelector::from("nestedproject"),
            ProjectSelector::current_directory(&working_directory),
        ] {
            assert_eq!(service.resolve(&selector).unwrap().id, nested.id);
        }
    }

    #[test]
    fn ambiguous_selector_reports_every_candidate() {
        let fixture = Fixture::new();
        let first_root = fixture.root.path().join("first/shared");
        let second_root = fixture.root.path().join("second/shared");
        fs::create_dir_all(&first_root).unwrap();
        fs::create_dir_all(&second_root).unwrap();
        let mut service = fixture.service();
        let first = service.import_local(first_root).unwrap();
        let second = service.import_local(second_root).unwrap();

        let error = service
            .resolve(&ProjectSelector::from("shared"))
            .unwrap_err();
        let ProjectError::AmbiguousProjectSelector { candidates, .. } = error else {
            panic!("expected an ambiguous selector, got {error:?}");
        };
        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().any(|project| project.id == first.id));
        assert!(candidates.iter().any(|project| project.id == second.id));
    }

    #[test]
    fn bare_words_are_names_not_current_directory_relative_paths() {
        assert!(!is_explicit_path_selector(Path::new("alpha")));
        assert!(is_explicit_path_selector(Path::new("./alpha")));
        assert!(is_explicit_path_selector(Path::new("../alpha")));
    }

    #[test]
    fn path_selector_resolves_a_deleted_catalog_root() {
        let fixture = Fixture::new();
        let written_root = fixture.directory("deleted-selector");
        let mut service = fixture.service();
        let imported = service.import_local(&written_root).unwrap();
        fs::remove_dir_all(&written_root).unwrap();

        let resolved = service
            .resolve(&ProjectSelector::from(
                written_root.to_string_lossy().into_owned(),
            ))
            .unwrap();

        assert_eq!(resolved.id, imported.id);
        assert!(!resolved.available);
    }

    #[test]
    fn ambiguous_uuid_prefix_reports_every_candidate() {
        let fixture = Fixture::new();
        let first_root = fixture.directory("prefix-first");
        let second_root = fixture.directory("prefix-second");
        let mut service = fixture.service();
        service.import_local(&first_root).unwrap();
        service.import_local(&second_root).unwrap();
        let first_id: ProjectId = "aaaaaaaa-0000-4000-8000-000000000001".parse().unwrap();
        let second_id: ProjectId = "aaaaaaaa-0000-4000-8000-000000000002".parse().unwrap();
        {
            let _lock = service.lock_exclusive().unwrap();
            let mut candidate = service.read_catalog().unwrap();
            candidate
                .projects
                .iter_mut()
                .find(|project| project.root == as_catalogued(&first_root))
                .unwrap()
                .id = first_id;
            candidate
                .projects
                .iter_mut()
                .find(|project| project.root == as_catalogued(&second_root))
                .unwrap()
                .id = second_id;
            service.persist(&candidate).unwrap();
            service.catalog = candidate;
        }

        let error = service
            .resolve(&ProjectSelector::from("aaaaaaaa"))
            .unwrap_err();
        let ProjectError::AmbiguousProjectSelector { candidates, .. } = error else {
            panic!("expected an ambiguous selector, got {error:?}");
        };
        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().any(|project| project.id == first_id));
        assert!(candidates.iter().any(|project| project.id == second_id));
    }

    #[test]
    fn project_error_kind_contract_is_stable() {
        let id: ProjectId = "00000000-0000-4000-8000-000000000001".parse().unwrap();
        let path = PathBuf::from("fixture");
        let project = Project {
            id,
            display_name: "fixture".to_owned(),
            root: path.clone(),
            source: ProjectSource::Local,
            last_opened: time::OffsetDateTime::UNIX_EPOCH,
            available: false,
            git: None,
        };
        let io_error = || io::Error::other("fixture");
        let git_error = || git2::Error::from_str("fixture");
        let json_error = || serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let cases = vec![
            (
                ProjectError::DataDirectoryUnavailable,
                "data_directory_unavailable",
            ),
            (
                ProjectError::InvalidDirectory {
                    path: path.clone(),
                    reason: "fixture".to_owned(),
                },
                "invalid_directory",
            ),
            (
                ProjectError::UnreadableDirectory {
                    path: path.clone(),
                    source: io_error(),
                },
                "unreadable_directory",
            ),
            (
                ProjectError::CatalogRead {
                    path: path.clone(),
                    source: io_error(),
                },
                "catalog_read",
            ),
            (
                ProjectError::CatalogLock {
                    path: path.clone(),
                    source: io_error(),
                },
                "catalog_lock",
            ),
            (
                ProjectError::MalformedCatalog {
                    path: path.clone(),
                    source: json_error(),
                },
                "malformed_catalog",
            ),
            (
                ProjectError::CatalogVersionTooOld {
                    found: 0,
                    minimum: 1,
                },
                "catalog_version_too_old",
            ),
            (
                ProjectError::CatalogVersionTooNew {
                    found: 3,
                    maximum: 2,
                },
                "catalog_version_too_new",
            ),
            (
                ProjectError::InvalidCatalog {
                    path: path.clone(),
                    reason: "fixture".to_owned(),
                },
                "invalid_catalog",
            ),
            (
                ProjectError::ProjectSelectorNotFound {
                    selector: "fixture".to_owned(),
                },
                "project_selector_not_found",
            ),
            (
                ProjectError::AmbiguousProjectSelector {
                    selector: "fixture".to_owned(),
                    candidates: vec![project],
                },
                "ambiguous_project_selector",
            ),
            (ProjectError::ProjectNotFound(id), "project_not_found"),
            (
                ProjectError::ProjectUnavailable {
                    id,
                    path: path.clone(),
                },
                "project_unavailable",
            ),
            (
                ProjectError::GitInspection {
                    path: path.clone(),
                    source: git_error(),
                },
                "git_inspection",
            ),
            (
                ProjectError::Persistence {
                    path: path.clone(),
                    source: io_error(),
                },
                "persistence",
            ),
            (
                ProjectError::InvalidRemote {
                    remote: "fixture".to_owned(),
                },
                "invalid_remote",
            ),
            (ProjectError::GitLaunch { source: io_error() }, "git_launch"),
            (
                ProjectError::CloneFailed {
                    stderr: "fixture".to_owned(),
                },
                "clone_failed",
            ),
            (ProjectError::CloneCancelled, "clone_cancelled"),
            (
                ProjectError::UnsafeManagedRemoval {
                    id,
                    path: path.clone(),
                    reason: "fixture".to_owned(),
                },
                "unsafe_managed_removal",
            ),
            (
                ProjectError::ManagedRemoval {
                    path: path.clone(),
                    source: io_error(),
                },
                "managed_removal",
            ),
            (
                ProjectError::ManagedRepositoryLock {
                    path: path.clone(),
                    source: io_error(),
                },
                "managed_repository_lock",
            ),
            (
                ProjectError::ManagedRepositoryReconciliation {
                    path: path.clone(),
                    source: io_error(),
                },
                "managed_repository_reconciliation",
            ),
            (
                ProjectError::ParentHasWorktrees {
                    id,
                    worktrees: vec![path.clone()],
                },
                "parent_has_worktrees",
            ),
            (
                ProjectError::WorktreeRemovalRequired {
                    id,
                    path: path.clone(),
                },
                "worktree_removal_required",
            ),
            (
                ProjectError::UnsafeWorktreeRemoval {
                    id,
                    path: path.clone(),
                    reason: "fixture".to_owned(),
                },
                "unsafe_worktree_removal",
            ),
            (
                ProjectError::WorktreeParentUnsupported {
                    id,
                    path: path.clone(),
                },
                "worktree_parent_unsupported",
            ),
            (
                ProjectError::DirtyWorktreeRemoval {
                    id,
                    path: path.clone(),
                },
                "dirty_worktree_removal",
            ),
            (
                ProjectError::UnsafeWorktreeMove {
                    id,
                    path: path.clone(),
                    reason: "fixture".to_owned(),
                },
                "unsafe_worktree_move",
            ),
            (
                ProjectError::WorktreeDestinationExists { path: path.clone() },
                "worktree_destination_exists",
            ),
            (
                ProjectError::WorktreeDestinationNotAbsolute { path: path.clone() },
                "worktree_destination_not_absolute",
            ),
            (
                ProjectError::WorktreeDestinationParentUnavailable {
                    path: path.clone(),
                    source: io_error(),
                },
                "worktree_destination_parent_unavailable",
            ),
            (
                ProjectError::WorktreeDestinationInsideProject {
                    path: path.clone(),
                    project_id: id,
                    project_root: path.clone(),
                },
                "worktree_destination_inside_project",
            ),
            (
                ProjectError::WorktreeDestinationContainsProject {
                    path: path.clone(),
                    project_id: id,
                    project_root: path.clone(),
                },
                "worktree_destination_contains_project",
            ),
            (
                ProjectError::WorktreeDestinationInsideDataDirectory {
                    path: path.clone(),
                    data_dir: path.clone(),
                },
                "worktree_destination_inside_data_directory",
            ),
            (
                ProjectError::WorktreeMovedCatalogStale {
                    id,
                    stale_root: path.clone(),
                    destination: path.clone(),
                    reason: "fixture".to_owned(),
                },
                "worktree_moved_catalog_stale",
            ),
            (ProjectError::Git(GitError::Cancelled), "cancelled"),
        ];

        let direct = cases[..cases.len() - 1]
            .iter()
            .map(|(_, kind)| *kind)
            .collect::<Vec<_>>();
        assert_eq!(direct, ProjectError::DIRECT_KINDS);
        for (error, expected) in cases {
            assert_eq!(error.kind(), expected, "unexpected kind for {error:?}");
        }
    }

    #[test]
    fn version_one_catalog_loads_through_the_shared_read_without_rewrite() {
        let fixture = Fixture::new();
        let local_root = fixture.directory("version-one-local");
        let managed_root = fixture.directory("version-one-managed");
        fs::create_dir_all(&fixture.data_dir).unwrap();
        let catalog_path = fixture.data_dir.join(CATALOG_FILE);
        let fixture_bytes = catalog_fixture(
            VERSION_ONE_CATALOG,
            &[
                ("__LOCAL_ROOT__", &as_catalogued(&local_root)),
                ("__MANAGED_ROOT__", &as_catalogued(&managed_root)),
            ],
        );
        fs::write(&catalog_path, &fixture_bytes).unwrap();
        fs::File::create(fixture.data_dir.join(CATALOG_LOCK_FILE)).unwrap();

        let mut service = ProjectService::load_from_data_dir(&fixture.data_dir).unwrap();
        // Clearing the cached snapshot makes the test prove the listing was
        // actually opened under projects.lock.
        service.catalog.projects.clear();
        let stored = service.list_catalog_only().unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].source, ProjectSource::Local);
        assert!(matches!(
            &stored[1].source,
            ProjectSource::ManagedRepository { remote }
                if remote == "github.com/example/version-one"
        ));
        assert_eq!(service.list().unwrap().len(), 2);
        assert_eq!(fs::read(&catalog_path).unwrap(), fixture_bytes);
    }

    #[test]
    fn legacy_managed_row_without_a_remote_does_not_hide_the_catalog() {
        let fixture = Fixture::new();
        let legacy_root = fixture.directory("legacy-managed-without-remote");
        let ordinary_root = fixture.directory("ordinary-beside-legacy");
        fs::create_dir_all(&fixture.data_dir).unwrap();
        let catalog_path = fixture.data_dir.join(CATALOG_FILE);
        let bytes = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "projects": [
                {
                    "id": "00000000-0000-4000-8000-000000000011",
                    "display_name": "legacy managed",
                    "root": as_catalogued(&legacy_root),
                    "source": "managed_repository",
                    "last_opened": "2026-08-06 18:52:03.000000000 +00:00:00"
                },
                {
                    "id": "00000000-0000-4000-8000-000000000012",
                    "display_name": "ordinary",
                    "root": as_catalogued(&ordinary_root),
                    "source": "local",
                    "last_opened": "2026-08-06 18:53:03.000000000 +00:00:00"
                }
            ]
        }))
        .unwrap();
        fs::write(&catalog_path, &bytes).unwrap();

        let service = ProjectService::load_from_data_dir(&fixture.data_dir).unwrap();
        let listed = service.list_catalog_only().unwrap();

        assert_eq!(listed.len(), 2);
        assert!(
            listed
                .iter()
                .all(|project| project.source == ProjectSource::Local)
        );
        assert_eq!(
            fs::read(catalog_path).unwrap(),
            bytes,
            "read-only load rewrote v1"
        );
    }

    #[test]
    fn a_catalog_read_failure_is_never_hidden_by_the_loaded_snapshot() {
        let fixture = Fixture::new();
        let project_root = fixture.directory("visible-catalog-failure");
        let mut service = fixture.service();
        service.import_local(project_root).unwrap();
        fs::write(fixture.data_dir.join(CATALOG_FILE), b"{ broken after load").unwrap();

        assert!(matches!(
            service.list(),
            Err(ProjectError::MalformedCatalog { .. })
        ));
        assert!(matches!(
            service.list_catalog_only(),
            Err(ProjectError::MalformedCatalog { .. })
        ));
    }

    #[test]
    fn version_one_compatible_mutations_preserve_entries_and_stay_version_one() {
        let fixture = Fixture::new();
        let local_root = fixture.directory("version-one-mutation-local");
        let managed_root = fixture.directory("version-one-mutation-managed");
        fs::create_dir_all(&fixture.data_dir).unwrap();
        let catalog_path = fixture.data_dir.join(CATALOG_FILE);
        let fixture_bytes = catalog_fixture(
            VERSION_ONE_CATALOG,
            &[
                ("__LOCAL_ROOT__", &as_catalogued(&local_root)),
                ("__MANAGED_ROOT__", &as_catalogued(&managed_root)),
            ],
        );
        fs::write(&catalog_path, fixture_bytes).unwrap();
        let mut service = ProjectService::load_from_data_dir(&fixture.data_dir).unwrap();
        let originals = service.list_catalog_only().unwrap();

        let new_root = fixture.directory("version-one-compatible-mutation");
        service.import_local(new_root).unwrap();

        let persisted = fs::read(&catalog_path).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&persisted).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["projects"].as_array().unwrap().len(), 3);
        let after_import = service.list_catalog_only().unwrap();
        for original in &originals {
            assert!(
                after_import.contains(original),
                "lost or rewrote {original:?}"
            );
        }

        // Opening is the launcher's routine click path. It updates Recents but
        // still must not make a v1-compatible catalog unreadable to v1.
        let local_id: ProjectId = "00000000-0000-4000-8000-000000000001".parse().unwrap();
        let opened = service.open(local_id).unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(&catalog_path).unwrap()).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(opened.id, originals[0].id);
        assert_eq!(opened.display_name, originals[0].display_name);
        assert_eq!(opened.root, originals[0].root);
        assert_eq!(opened.source, originals[0].source);
    }

    #[test]
    fn frozen_version_two_worktree_loads_and_derived_state_is_recomputed() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("version-two-parent");
        let worktree_id: ProjectId = "00000000-0000-4000-8000-000000000003".parse().unwrap();
        let worktree_root = fixture
            .data_dir
            .join(WORKTREES_DIRECTORY)
            .join(worktree_id.to_string());
        fs::create_dir_all(&worktree_root).unwrap();
        fs::create_dir_all(&fixture.data_dir).unwrap();
        let catalog_path = fixture.data_dir.join(CATALOG_FILE);
        let fixture_bytes = catalog_fixture(
            VERSION_TWO_CATALOG,
            &[
                ("__PARENT_ROOT__", &as_catalogued(&parent_root)),
                ("__WORKTREE_ROOT__", &as_catalogued(&worktree_root)),
            ],
        );
        fs::write(&catalog_path, fixture_bytes).unwrap();

        let mut service = ProjectService::load_from_data_dir(&fixture.data_dir).unwrap();
        fs::remove_dir_all(&worktree_root).unwrap();
        let stored = service
            .list()
            .unwrap()
            .into_iter()
            .find(|project| project.id == worktree_id)
            .unwrap();

        assert!(!stored.available);
        assert!(matches!(
            stored.source,
            ProjectSource::Worktree { parent, ref worktree_branch }
                if parent.to_string() == "00000000-0000-4000-8000-000000000001"
                    && worktree_branch.as_deref() == Some("agent/catalog-v2")
        ));

        let parent_id: ProjectId = "00000000-0000-4000-8000-000000000001".parse().unwrap();
        service.open(parent_id).unwrap();
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(&catalog_path).unwrap()).unwrap();
        assert_eq!(persisted["version"], 2);
        let projects = persisted["projects"].as_array().unwrap();
        assert_eq!(projects[0]["source"], "local");
        assert_eq!(projects[1]["source"], "worktree");
        assert_eq!(projects[1]["worktree_branch"], "agent/catalog-v2");
    }

    /// Set through `Command::env` on a re-executed child rather than
    /// `std::env::set_var`, which is unsound in a multithreaded test binary
    /// under Rust 2024.
    #[test]
    fn the_data_directory_environment_override_redirects_load() {
        let fixture = Fixture::new();
        let project_root = fixture.directory("environment-override");
        let mut child = spawn_child(&fixture.data_dir, "load-env")
            .env(DATA_DIRECTORY_ENV, &fixture.data_dir)
            .env(PROCESS_PROJECT_ROOT_ENV, &project_root)
            .spawn()
            .unwrap();

        assert!(child.wait().unwrap().success());

        // The child called `load`, so finding its import here is what proves
        // the platform data directory was never consulted.
        let projects = fixture.service().list().unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].root, fs::canonicalize(&project_root).unwrap());
    }

    #[test]
    fn an_unset_data_directory_override_resolves_the_platform_directory() {
        let fixture = Fixture::new();
        let mut child = spawn_child(&fixture.data_dir, "default-data-dir")
            .env_remove(DATA_DIRECTORY_ENV)
            .spawn()
            .unwrap();

        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn concurrent_services_both_keep_their_imports() {
        let fixture = Fixture::new();
        let first_root = fixture.directory("first");
        let second_root = fixture.directory("second");
        let mut first = fixture.service();
        let mut second = fixture.service();

        let first_project = first.import_local(&first_root).unwrap();
        let second_project = second.import_local(&second_root).unwrap();

        let ids = fixture
            .service()
            .list()
            .unwrap()
            .iter()
            .map(|project| project.id)
            .collect::<Vec<_>>();
        assert!(ids.contains(&first_project.id));
        assert!(ids.contains(&second_project.id));
    }

    #[test]
    fn an_unrelated_write_does_not_resurrect_a_removed_project() {
        let fixture = Fixture::new();
        let doomed_root = fixture.directory("doomed");
        let other_root = fixture.directory("other");
        let doomed = fixture.service().import_local(&doomed_root).unwrap();

        // Loaded while `doomed` is still catalogued, so its snapshot is stale
        // by the time it writes.
        let mut stale = fixture.service();
        fixture.service().remove(doomed.id).unwrap();
        let other = stale.import_local(&other_root).unwrap();

        let ids = fixture
            .service()
            .list()
            .unwrap()
            .iter()
            .map(|project| project.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, vec![other.id]);
    }

    #[test]
    fn concurrent_mutations_lose_no_entry() {
        const WRITERS: usize = 8;

        let fixture = Fixture::new();
        let roots = (0..WRITERS)
            .map(|index| fixture.directory(&format!("writer-{index}")))
            .collect::<Vec<_>>();

        let data_dir = &fixture.data_dir;
        thread::scope(|scope| {
            for root in &roots {
                scope.spawn(move || {
                    let mut service = ProjectService::load_from_data_dir(data_dir).unwrap();
                    service.import_local(root).unwrap();
                });
            }
        });

        let reloaded = fixture.service();
        assert_eq!(reloaded.list().unwrap().len(), WRITERS);
        for root in &roots {
            let stored = as_catalogued(root);
            assert!(
                reloaded
                    .list()
                    .unwrap()
                    .iter()
                    .any(|project| project.root == stored)
            );
        }
    }

    #[test]
    fn concurrent_process_mutations_lose_no_entry() {
        const WRITERS: usize = 8;

        let fixture = Fixture::new();
        let roots = (0..WRITERS)
            .map(|index| fixture.directory(&format!("process-writer-{index}")))
            .collect::<Vec<_>>();
        let mut children = roots
            .iter()
            .map(|root| {
                spawn_child(&fixture.data_dir, "import")
                    .env(PROCESS_PROJECT_ROOT_ENV, root)
                    .spawn()
                    .unwrap()
            })
            .collect::<Vec<_>>();

        for child in &mut children {
            assert!(child.wait().unwrap().success());
        }

        let projects = fixture.service().list().unwrap();
        assert_eq!(projects.len(), WRITERS);
        for root in roots {
            let stored = as_catalogued(&root);
            assert!(projects.iter().any(|project| project.root == stored));
        }
    }

    #[test]
    fn killed_lock_holder_releases_lock_and_leaves_catalog_loadable() {
        let fixture = Fixture::new();
        let seeded_root = fixture.directory("seeded-before-kill");
        let imported_after_kill_root = fixture.directory("imported-after-kill");
        let seeded = fixture.service().import_local(&seeded_root).unwrap();
        let ready_file = fixture.root.path().join("lock-held");
        let mut child = spawn_child(&fixture.data_dir, "hold-lock")
            .env(PROCESS_READY_FILE_ENV, &ready_file)
            .spawn()
            .unwrap();

        wait_for_child_signal(&mut child, &ready_file);
        child.kill().unwrap();
        child.wait().unwrap();

        let mut reloaded = ProjectService::load_from_data_dir(&fixture.data_dir).unwrap();
        assert_eq!(reloaded.list().unwrap(), vec![seeded]);
        let imported_after_kill = reloaded.import_local(imported_after_kill_root).unwrap();
        let projects = ProjectService::load_from_data_dir(&fixture.data_dir)
            .unwrap()
            .list()
            .unwrap();
        assert_eq!(projects.len(), 2);
        assert!(
            projects
                .iter()
                .any(|project| project.id == imported_after_kill.id)
        );
    }

    #[test]
    fn derived_state_is_not_persisted() {
        let fixture = Fixture::new();
        let project_root = fixture.directory("derived-state");
        let mut service = fixture.service();
        let parent = service.import_local(&project_root).unwrap();
        catalogue_worktree(&mut service, parent.id, "agent/git-worktrees");

        let stored: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture.data_dir.join(CATALOG_FILE)).unwrap())
                .unwrap();
        let projects = stored["projects"].as_array().unwrap();

        assert!(
            projects.iter().all(|project| {
                project.get("available").is_none() && project.get("git").is_none()
            })
        );
        assert_eq!(stored["version"], CATALOG_VERSION);
        let local = projects
            .iter()
            .find(|project| project["source"] == "local")
            .unwrap();
        assert!(local.get("remote").is_none());
        assert!(local.get("parent").is_none());
        assert!(local.get("worktree_branch").is_none());
        let worktree = projects
            .iter()
            .find(|project| project["source"] == "worktree")
            .unwrap();
        assert_eq!(worktree["parent"], parent.id.to_string());
        assert_eq!(worktree["worktree_branch"], "agent/git-worktrees");
        assert!(worktree.get("last_opened").is_some());
    }

    #[test]
    fn a_catalog_only_listing_derives_no_state() {
        let fixture = Fixture::new();
        let project_root = fixture.directory("catalog-only");
        initialize_repository(&project_root);
        let mut service = fixture.service();
        let imported = service.import_local(&project_root).unwrap();
        assert!(imported.available);
        assert!(imported.git.is_some());

        let listed = service.list_catalog_only().unwrap();

        // The root exists and is a repository, so defaults here can only mean
        // the listing never looked at it.
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, imported.id);
        assert_eq!(listed[0].root, imported.root);
        assert_eq!(listed[0].display_name, imported.display_name);
        assert!(!listed[0].available);
        assert_eq!(listed[0].git, None);
        assert_eq!(service.list().unwrap(), vec![imported]);
    }

    #[test]
    fn canonical_paths_are_deduplicated() {
        let fixture = Fixture::new();
        let project_root = fixture.directory("canonical-project");
        let nested = project_root.join("nested");
        fs::create_dir(&nested).unwrap();
        let alias = nested.join("..");
        let mut service = fixture.service();

        let direct = service.import_local(&project_root).unwrap();
        let aliased = service.import_local(&alias).unwrap();

        assert_eq!(direct.id, aliased.id);
        assert_eq!(service.list().unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_paths_are_deduplicated() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let project_root = fixture.directory("symlink-target");
        let alias = fixture.root.path().join("symlink-alias");
        symlink(&project_root, &alias).unwrap();
        let mut service = fixture.service();

        let direct = service.import_local(&project_root).unwrap();
        let linked = service.import_local(&alias).unwrap();

        assert_eq!(direct.id, linked.id);
        assert_eq!(linked.root, fs::canonicalize(project_root).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn symlinked_paths_are_deduplicated() {
        use std::os::windows::fs::symlink_dir;

        let fixture = Fixture::new();
        let project_root = fixture.directory("symlink-target");
        let alias = fixture.root.path().join("symlink-alias");
        symlink_dir(&project_root, &alias).unwrap();
        let mut service = fixture.service();

        let direct = service.import_local(&project_root).unwrap();
        let linked = service.import_local(&alias).unwrap();

        assert_eq!(direct.id, linked.id);
        assert_eq!(linked.root, fs::canonicalize(project_root).unwrap());
    }

    #[test]
    fn recents_move_when_a_project_is_reopened() {
        let fixture = Fixture::new();
        let first_root = fixture.directory("first");
        let second_root = fixture.directory("second");
        let mut service = fixture.service();
        let first = service.import_local(first_root).unwrap();
        thread::sleep(CLOCK_TICK);
        let second = service.import_local(second_root).unwrap();

        assert_eq!(service.list().unwrap()[0].id, second.id);

        thread::sleep(CLOCK_TICK);
        let reopened = service.open(first.id).unwrap();
        let recents = service.list().unwrap();

        assert!(reopened.last_opened > first.last_opened);
        assert_eq!(recents[0].id, first.id);
        assert_eq!(recents[1].id, second.id);
    }

    #[test]
    fn non_git_directories_are_accepted() {
        let fixture = Fixture::new();
        let project_root = fixture.directory("plain-directory");
        let mut service = fixture.service();

        let project = service.import_local(project_root).unwrap();

        assert_eq!(project.source, ProjectSource::Local);
        assert!(project.available);
        assert_eq!(project.git, None);
    }

    #[test]
    fn git_branch_clean_dirty_and_detached_states_are_reported() {
        let fixture = Fixture::new();
        let project_root = fixture.directory("git-project");
        let repository = initialize_repository(&project_root);
        let mut service = fixture.service();

        let clean = service.import_local(&project_root).unwrap();
        assert_eq!(clean.git.as_ref().unwrap().branch.as_deref(), Some("main"));
        assert!(!clean.git.as_ref().unwrap().dirty);

        fs::write(project_root.join("tracked.txt"), "changed\n").unwrap();
        let dirty = service.open(clean.id).unwrap();
        assert!(dirty.git.as_ref().unwrap().dirty);

        let head = repository.head().unwrap().target().unwrap();
        repository.set_head_detached(head).unwrap();
        let detached = service.open(clean.id).unwrap();
        assert_eq!(detached.git.as_ref().unwrap().branch, None);
    }

    #[test]
    fn nested_directories_do_not_inherit_ancestor_git_state() {
        let fixture = Fixture::new();
        let repository_root = fixture.directory("outer-repository");
        initialize_repository(&repository_root);
        let nested = repository_root.join("plain-subdirectory");
        fs::create_dir(&nested).unwrap();
        let mut service = fixture.service();

        let project = service.import_local(&nested).unwrap();

        assert_eq!(project.git, None);
    }

    #[test]
    fn directories_holding_only_ignored_files_stay_clean() {
        let fixture = Fixture::new();
        let project_root = fixture.directory("ignored-only");
        let repository = initialize_repository(&project_root);
        fs::write(project_root.join(".gitignore"), "build/\n").unwrap();
        commit_all(&repository, "ignore build output");
        fs::create_dir(project_root.join("build")).unwrap();
        fs::write(project_root.join("build").join("artifact.bin"), "generated").unwrap();
        let mut service = fixture.service();

        let project = service.import_local(&project_root).unwrap();

        assert!(!project.git.unwrap().dirty);
    }

    #[test]
    fn missing_roots_remain_unavailable_in_the_catalog() {
        let fixture = Fixture::new();
        let project_root = fixture.directory("temporary-project");
        let mut service = fixture.service();
        let imported = service.import_local(&project_root).unwrap();
        fs::remove_dir_all(&project_root).unwrap();

        let listed = service.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].available);
        assert_eq!(listed[0].id, imported.id);
        assert!(matches!(
            service.open(imported.id),
            Err(ProjectError::ProjectUnavailable { id, .. }) if id == imported.id
        ));

        let reloaded = ProjectService::load_from_data_dir(&fixture.data_dir).unwrap();
        assert!(!reloaded.list().unwrap()[0].available);
    }

    #[test]
    fn removal_never_touches_project_files() {
        let fixture = Fixture::new();
        let project_root = fixture.directory("keep-source");
        let nested = project_root.join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(project_root.join("root.txt"), "root sentinel").unwrap();
        fs::write(nested.join("nested.txt"), "nested sentinel").unwrap();
        let mut service = fixture.service();
        let imported = service.import_local(&project_root).unwrap();

        let removed = service.remove(imported.id).unwrap();

        assert_eq!(removed.id, imported.id);
        assert!(service.list().unwrap().is_empty());
        assert_eq!(
            fs::read_to_string(project_root.join("root.txt")).unwrap(),
            "root sentinel"
        );
        assert_eq!(
            fs::read_to_string(nested.join("nested.txt")).unwrap(),
            "nested sentinel"
        );
    }

    #[test]
    fn removal_refuses_a_parent_with_worktrees() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("kept-parent");
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let first = catalogue_worktree(&mut service, parent.id, "agent/first-worktree");
        let second = catalogue_worktree(&mut service, parent.id, "agent/second-worktree");

        let error = service.remove(parent.id).unwrap_err();
        let mut expected = vec![first.root.clone(), second.root.clone()];
        expected.sort();
        let rendered = error.to_string();
        assert!(rendered.contains(&first.root.display().to_string()));
        assert!(rendered.contains(&second.root.display().to_string()));
        assert!(!rendered.contains("ProjectId("));

        assert!(matches!(
            error,
            ProjectError::ParentHasWorktrees { id, worktrees, .. }
                if id == parent.id && worktrees == expected
        ));
        assert!(parent_root.exists());
        assert!(first.root.exists());
        assert!(second.root.exists());
        assert_eq!(service.list_catalog_only().unwrap().len(), 3);
        assert!(matches!(
            service.remove(first.id),
            Err(ProjectError::WorktreeRemovalRequired { id, .. }) if id == first.id
        ));
        assert_eq!(service.list_catalog_only().unwrap().len(), 3);
    }

    #[test]
    fn worktrees_are_created_in_every_supported_mode_without_touching_the_parent_checkout() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("create-worktree-parent");
        initialize_repository(&parent_root);
        fs::write(parent_root.join("sentinel.txt"), "leave me exactly alone\n").unwrap();
        git(&parent_root, ["branch", "existing-worktree"]);
        let sentinel_before = fs::read(parent_root.join("sentinel.txt")).unwrap();
        let status_before = git(&parent_root, ["status", "--porcelain=v1"]);
        let head = git(&parent_root, ["rev-parse", "HEAD"]).trim().to_owned();
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();

        let created = service
            .create_worktree(
                parent.id,
                &WorktreeBase::NewBranch {
                    name: "agent/new-worktree".to_owned(),
                    start_point: Some(head.clone()),
                },
                &Cancellation::default(),
            )
            .unwrap();
        let existing = service
            .create_worktree(
                parent.id,
                &WorktreeBase::ExistingBranch {
                    name: "existing-worktree".to_owned(),
                },
                &Cancellation::default(),
            )
            .unwrap();
        let detached = service
            .create_worktree(
                parent.id,
                &WorktreeBase::Detached {
                    commit: head.clone(),
                },
                &Cancellation::default(),
            )
            .unwrap();

        for worktree in [&created, &existing, &detached] {
            assert_eq!(
                worktree.root,
                fs::canonicalize(service.worktree_path(worktree.id)).unwrap()
            );
            assert!(worktree.root.join(".git").is_file());
            assert!(
                list_directory(&worktree.root)
                    .unwrap()
                    .iter()
                    .all(|entry| entry.name != ".git")
            );
        }
        assert_eq!(
            created
                .git
                .as_ref()
                .and_then(|status| status.branch.as_deref()),
            Some("agent/new-worktree")
        );
        assert_eq!(
            existing
                .git
                .as_ref()
                .and_then(|status| status.branch.as_deref()),
            Some("existing-worktree")
        );
        assert_eq!(
            detached
                .git
                .as_ref()
                .and_then(|status| status.branch.as_deref()),
            None
        );
        assert_eq!(
            fs::read(parent_root.join("sentinel.txt")).unwrap(),
            sentinel_before
        );
        assert_eq!(
            git(&parent_root, ["status", "--porcelain=v1"]),
            status_before
        );

        let administrative_names = fs::read_dir(parent_root.join(".git").join("worktrees"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        for worktree in [&created, &existing, &detached] {
            assert!(administrative_names.contains(&worktree.id.to_string()));
        }
        assert_eq!(
            service
                .worktrees(parent.id, &Cancellation::default())
                .unwrap()
                .len(),
            3
        );

        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture.data_dir.join(CATALOG_FILE)).unwrap())
                .unwrap();
        let detached_row = persisted["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == detached.id.to_string())
            .unwrap();
        assert!(detached_row.get("worktree_branch").is_none());
    }

    #[test]
    fn a_managed_repository_can_parent_a_worktree() {
        let fixture = Fixture::new();
        let remote = fixture.directory("managed-worktree-remote");
        initialize_repository(&remote);
        let mut service = fixture.service();
        let parent = service
            .import_repository(remote.to_str().unwrap(), &Cancellation::default(), |_| {})
            .unwrap();

        let worktree = create_branch_worktree(&mut service, parent.id, "agent/managed-parent");

        assert!(
            worktree.root.starts_with(
                fs::canonicalize(&fixture.data_dir)
                    .unwrap()
                    .join(WORKTREES_DIRECTORY)
            )
        );
        assert!(!worktree.root.starts_with(parent.root.parent().unwrap()));
        assert!(worktree.root.join(".git").is_file());
    }

    #[test]
    fn a_deleted_parent_does_not_strand_its_worktree_or_catalog_row() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("deleted-worktree-parent");
        initialize_repository(&parent_root);
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let worktree = create_branch_worktree(&mut service, parent.id, "agent/orphaned-parent");
        fs::remove_dir_all(&parent_root).unwrap();

        let removed = service
            .remove_worktree(worktree.id, true, &Cancellation::default())
            .unwrap();

        assert_eq!(removed.id, worktree.id);
        assert!(
            worktree.root.exists(),
            "without the parent repository Git cannot safely delete the checkout"
        );
        service.remove(parent.id).unwrap();
        assert!(service.list_catalog_only().unwrap().is_empty());
    }

    #[test]
    fn direct_removal_reconciles_a_checkout_deleted_outside_harkness() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("direct-stale-removal-parent");
        initialize_repository(&parent_root);
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let worktree = create_branch_worktree(&mut service, parent.id, "agent/direct-stale");
        fs::remove_dir_all(&worktree.root).unwrap();

        service
            .remove_worktree(worktree.id, false, &Cancellation::default())
            .unwrap();

        assert!(
            !git(&parent_root, ["worktree", "list", "--porcelain"])
                .contains(&worktree.id.to_string())
        );
        let remaining = service.list_catalog_only().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, parent.id);
        assert_eq!(remaining[0].root, parent.root);
        assert_eq!(remaining[0].source, parent.source);
    }

    #[test]
    fn direct_removal_drops_a_row_after_git_already_removed_the_worktree() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("already-removed-worktree-parent");
        initialize_repository(&parent_root);
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let worktree = create_branch_worktree(&mut service, parent.id, "agent/already-removed");
        git(
            &parent_root,
            [
                "worktree",
                "remove",
                "--force",
                "--",
                worktree.root.to_str().unwrap(),
            ],
        );

        service
            .remove_worktree(worktree.id, false, &Cancellation::default())
            .unwrap();

        assert_eq!(service.list_catalog_only().unwrap().len(), 1);
        assert_eq!(service.list_catalog_only().unwrap()[0].id, parent.id);
    }

    #[test]
    fn a_worktree_opens_like_every_other_project() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("open-worktree-parent");
        initialize_repository(&parent_root);
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let worktree = create_branch_worktree(&mut service, parent.id, "agent/open-me");

        let opened = service.open(worktree.id).unwrap();

        assert_eq!(opened.id, worktree.id);
        assert!(opened.available && opened.git.is_some());
        assert_eq!(service.list().unwrap()[0].id, worktree.id);
    }

    #[test]
    fn detached_worktrees_use_the_resolved_oid_and_can_be_removed() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("detached-removal-parent");
        initialize_repository(&parent_root);
        let head = git(&parent_root, ["rev-parse", "HEAD"]).trim().to_owned();
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let detached = service
            .create_worktree(
                parent.id,
                &WorktreeBase::Detached {
                    commit: "HEAD".to_owned(),
                },
                &Cancellation::default(),
            )
            .unwrap();

        assert_eq!(
            detached.display_name,
            format!("Detached at {}", &head[..12])
        );
        service
            .remove_worktree(detached.id, false, &Cancellation::default())
            .unwrap();
        assert!(!detached.root.exists());
    }

    #[test]
    fn unborn_parent_head_is_a_typed_worktree_creation_refusal() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("unborn-worktree-parent");
        git2::Repository::init(&parent_root).unwrap();
        git(&parent_root, ["symbolic-ref", "HEAD", "refs/heads/main"]);
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();

        let error = service
            .create_worktree(
                parent.id,
                &WorktreeBase::NewBranch {
                    name: "agent/unborn".to_owned(),
                    start_point: None,
                },
                &Cancellation::default(),
            )
            .unwrap_err();

        assert!(matches!(
            error,
            ProjectError::Git(GitError::UnbornBranch { branch, .. }) if branch == "main"
        ));
    }

    #[test]
    fn existing_branches_checked_out_elsewhere_and_nested_parents_are_typed_refusals() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("checked-out-worktree-parent");
        initialize_repository(&parent_root);
        git(&parent_root, ["branch", "held-elsewhere"]);
        let external = fixture.root.path().join("external-held-worktree");
        git(
            &parent_root,
            [
                "worktree",
                "add",
                "--",
                external.to_str().unwrap(),
                "held-elsewhere",
            ],
        );
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();

        let error = service
            .create_worktree(
                parent.id,
                &WorktreeBase::ExistingBranch {
                    name: "held-elsewhere".to_owned(),
                },
                &Cancellation::default(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ProjectError::Git(GitError::BranchCheckedOutInWorktree { branch, worktree })
                if branch == "held-elsewhere"
                    && crate::git::worktree::same_path(&worktree, &external)
        ));

        let managed = create_branch_worktree(&mut service, parent.id, "agent/not-a-parent");
        assert!(matches!(
            service.create_worktree(
                managed.id,
                &WorktreeBase::NewBranch {
                    name: "agent/nested".to_owned(),
                    start_point: None,
                },
                &Cancellation::default(),
            ),
            Err(ProjectError::WorktreeParentUnsupported { id, .. }) if id == managed.id
        ));
    }

    #[test]
    fn external_worktrees_are_reported_without_being_adopted() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("external-list-parent");
        initialize_repository(&parent_root);
        git(&parent_root, ["branch", "external-branch"]);
        let external = fixture.root.path().join("external-read-only");
        git(
            &parent_root,
            [
                "worktree",
                "add",
                "--",
                external.to_str().unwrap(),
                "external-branch",
            ],
        );
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();

        let worktrees = service
            .worktrees(parent.id, &Cancellation::default())
            .unwrap();

        assert_eq!(worktrees.len(), 1);
        assert!(crate::git::worktree::same_path(
            &worktrees[0].root,
            &external
        ));
        assert_eq!(worktrees[0].branch.as_deref(), Some("external-branch"));
        assert!(worktrees[0].project.is_none());
        assert_eq!(service.list_catalog_only().unwrap().len(), 1);
    }

    #[test]
    fn worktree_listing_refuses_an_unrelated_ancestor_repository() {
        let fixture = Fixture::new();
        let repository_root = fixture.directory("ancestor-worktree-repository");
        initialize_repository(&repository_root);
        let nested = repository_root.join("plain-subdirectory");
        fs::create_dir(&nested).unwrap();
        let external = fixture.root.path().join("ancestor-external-worktree");
        git(&repository_root, ["branch", "ancestor-topic"]);
        git(
            &repository_root,
            [
                "worktree",
                "add",
                "--",
                external.to_str().unwrap(),
                "ancestor-topic",
            ],
        );
        let mut service = fixture.service();
        let project = service.import_local(&nested).unwrap();
        assert_eq!(project.git, None);

        let error = service
            .worktrees(project.id, &Cancellation::default())
            .unwrap_err();

        assert!(matches!(
            error,
            ProjectError::Git(GitError::NotARepository { path }) if path == project.root
        ));
    }

    #[test]
    fn live_worktree_branch_wins_without_overwriting_the_creation_record() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("live-worktree-branch-parent");
        initialize_repository(&parent_root);
        git(&parent_root, ["branch", "later-branch"]);
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let created = create_branch_worktree(&mut service, parent.id, "agent/original-branch");
        git(&created.root, ["switch", "later-branch"]);

        let listed = service
            .worktrees(parent.id, &Cancellation::default())
            .unwrap();
        let row = &listed[0];

        assert_eq!(row.branch.as_deref(), Some("later-branch"));
        assert!(matches!(
            row.project.as_ref().map(|project| &project.source),
            Some(ProjectSource::Worktree { worktree_branch, .. })
                if worktree_branch.as_deref() == Some("agent/original-branch")
        ));
    }

    #[test]
    fn locked_worktrees_have_a_typed_removal_refusal() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("locked-worktree-parent");
        initialize_repository(&parent_root);
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let worktree = create_branch_worktree(&mut service, parent.id, "agent/locked-worktree");
        git(
            &parent_root,
            [
                "worktree",
                "lock",
                "--reason",
                "portable checkout",
                worktree.root.to_str().unwrap(),
            ],
        );

        let listed = service
            .worktrees(parent.id, &Cancellation::default())
            .unwrap();
        assert!(listed[0].locked);
        let error = service
            .remove_worktree(worktree.id, true, &Cancellation::default())
            .unwrap_err();
        assert!(matches!(
            error,
            ProjectError::Git(GitError::WorktreeLocked { reason, .. })
                if reason.as_deref() == Some("portable checkout")
        ));
        assert!(worktree.root.exists());
    }

    #[test]
    fn moving_a_worktree_preserves_its_identity_changes_and_git_operations() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("move-worktree-parent");
        initialize_repository(&parent_root);
        let destination_parent = fixture.directory("relocated-worktrees");
        let destination = destination_parent.join("moved-checkout");
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let worktree = create_branch_worktree(&mut service, parent.id, "agent/move-me");
        fs::write(
            worktree.root.join("uncommitted.txt"),
            "survives relocation\n",
        )
        .unwrap();

        let moved = service
            .move_worktree(worktree.id, &destination, &Cancellation::default())
            .unwrap();

        let canonical_destination = as_catalogued(&destination);
        assert_eq!(moved.id, worktree.id);
        assert_eq!(moved.display_name, worktree.display_name);
        assert_eq!(moved.source, worktree.source);
        assert_eq!(moved.last_opened, worktree.last_opened);
        assert_eq!(moved.root, canonical_destination);
        assert!(moved.available);
        assert!(moved.git.as_ref().is_some_and(|status| status.dirty));
        assert!(!worktree.root.exists());
        assert_eq!(
            fs::read_to_string(canonical_destination.join("uncommitted.txt")).unwrap(),
            "survives relocation\n"
        );
        assert_eq!(
            ProjectService::load_from_data_dir(&fixture.data_dir)
                .unwrap()
                .list_catalog_only()
                .unwrap()
                .into_iter()
                .find(|project| project.id == worktree.id)
                .unwrap()
                .root,
            canonical_destination
        );

        let git = service.git(worktree.id).unwrap();
        assert!(git.status().unwrap().unwrap().dirty);
        assert!(
            git.stage(["uncommitted.txt"], &Cancellation::default())
                .unwrap()
                .all_succeeded()
        );
        git.commit(
            "Preserve work across relocation",
            &crate::git::CommitOptions::default(),
            &Cancellation::default(),
        )
        .unwrap();
        assert!(!git.status().unwrap().unwrap().dirty);

        service
            .remove_worktree(worktree.id, false, &Cancellation::default())
            .unwrap();
        assert!(!canonical_destination.exists());
    }

    #[test]
    fn worktree_move_destinations_are_validated_before_git_runs() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("move-validation-parent");
        initialize_repository(&parent_root);
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let worktree = create_branch_worktree(&mut service, parent.id, "agent/validate-move");
        let existing = fixture.directory("existing-move-destination");

        assert!(matches!(
            service.move_worktree(
                worktree.id,
                Path::new("relative-worktree-destination"),
                &Cancellation::default(),
            ),
            Err(ProjectError::WorktreeDestinationNotAbsolute { .. })
        ));

        assert!(matches!(
            service.move_worktree(
                parent.id,
                fixture.root.path().join("not-a-worktree"),
                &Cancellation::default(),
            ),
            Err(ProjectError::UnsafeWorktreeMove { id, .. }) if id == parent.id
        ));

        assert!(matches!(
            service.move_worktree(worktree.id, &existing, &Cancellation::default()),
            Err(ProjectError::WorktreeDestinationExists { path }) if path == existing
        ));

        let nested = parent.root.join("nested-move-destination");
        assert!(matches!(
            service.move_worktree(worktree.id, &nested, &Cancellation::default()),
            Err(ProjectError::WorktreeDestinationInsideProject {
                project_id,
                project_root,
                ..
            }) if project_id == parent.id && project_root == parent.root
        ));

        let nested_in_source = worktree.root.join("nested-destination");
        assert!(matches!(
            service.move_worktree(worktree.id, &nested_in_source, &Cancellation::default()),
            Err(ProjectError::WorktreeDestinationInsideDataDirectory { .. })
        ));

        let data_destination = fixture.data_dir.join("worktree-checkout");
        assert!(matches!(
            service.move_worktree(
                worktree.id,
                &data_destination,
                &Cancellation::default(),
            ),
            Err(ProjectError::WorktreeDestinationInsideDataDirectory { path, .. })
                if crate::git::worktree::same_path(&path, &data_destination)
        ));

        let missing_parent = fixture
            .root
            .path()
            .join("missing-destination-parent")
            .join("checkout");
        assert!(matches!(
            service.move_worktree(worktree.id, &missing_parent, &Cancellation::default()),
            Err(ProjectError::WorktreeDestinationParentUnavailable { path, .. })
                if path == missing_parent
        ));
        assert!(worktree.root.exists());
        assert!(
            git(&parent_root, ["worktree", "list", "--porcelain"])
                .contains(worktree.root.to_str().unwrap())
        );
    }

    #[test]
    fn worktree_move_refuses_to_enclose_an_unavailable_catalogued_project() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("ancestor-overlap-parent");
        initialize_repository(&parent_root);
        let container = fixture.directory("ancestor-overlap-container");
        let victim_root = container.join("victim");
        fs::create_dir(&victim_root).unwrap();
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let victim = service.import_local(&victim_root).unwrap();
        let worktree = create_branch_worktree(&mut service, parent.id, "agent/ancestor-overlap");
        fs::remove_dir_all(&container).unwrap();

        let error = service
            .move_worktree(worktree.id, &container, &Cancellation::default())
            .unwrap_err();

        assert!(matches!(
            error,
            ProjectError::WorktreeDestinationContainsProject {
                project_id,
                project_root,
                ..
            } if project_id == victim.id && project_root == victim.root
        ));
        assert!(worktree.root.exists());
        assert!(!container.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_destination_parent_records_the_canonical_root() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let parent_root = fixture.directory("symlink-move-parent");
        initialize_repository(&parent_root);
        let actual_parent = fixture.directory("actual-move-parent");
        let alias_parent = fixture.root.path().join("aliased-move-parent");
        symlink(&actual_parent, &alias_parent).unwrap();
        let destination = alias_parent.join("checkout");
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let worktree = create_branch_worktree(&mut service, parent.id, "agent/symlink-parent");

        let dangling_destination = actual_parent.join("dangling-checkout");
        symlink(actual_parent.join("missing-target"), &dangling_destination).unwrap();
        assert!(matches!(
            service.move_worktree(
                worktree.id,
                &dangling_destination,
                &Cancellation::default(),
            ),
            Err(ProjectError::WorktreeDestinationExists { path })
                if path == dangling_destination
        ));

        let moved = service
            .move_worktree(worktree.id, &destination, &Cancellation::default())
            .unwrap();

        assert_eq!(moved.root, as_catalogued(&actual_parent.join("checkout")));
        assert!(moved.root.exists());
        service
            .remove_worktree(worktree.id, false, &Cancellation::default())
            .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cross_device_worktree_moves_have_a_typed_refusal() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("cross-device-parent");
        initialize_repository(&parent_root);
        let destination = fixture
            .directory("cross-device-destination-parent")
            .join("checkout");
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let worktree = create_branch_worktree(&mut service, parent.id, "agent/cross-device");
        service.git_executable = fixture.shim(
            "cross-device-git",
            "#!/bin/sh\n\
             previous=\n\
             for argument in \"$@\"; do\n\
               if [ \"$previous\" = worktree ] && [ \"$argument\" = move ]; then\n\
                 echo 'fatal: Invalid cross-device link' >&2\n\
                 exit 128\n\
               fi\n\
               previous=\"$argument\"\n\
             done\n\
             exec git \"$@\"\n",
        );

        let error = service
            .move_worktree(worktree.id, &destination, &Cancellation::default())
            .unwrap_err();

        assert!(matches!(
            error,
            ProjectError::Git(GitError::WorktreeMoveAcrossDevices {
                worktree: source,
                destination: refused,
                ..
            }) if crate::git::worktree::same_path(&source, &worktree.root)
                && crate::git::worktree::same_path(&refused, &destination)
        ));
        assert!(worktree.root.exists());
        assert!(!destination.exists());
    }

    #[cfg(unix)]
    #[test]
    fn post_move_persistence_failure_is_typed_and_reconciliation_repairs_it() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new();
        let parent_root = fixture.directory("torn-move-parent");
        initialize_repository(&parent_root);
        let destination = fixture
            .directory("torn-move-destination-parent")
            .join("checkout");
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let worktree = create_branch_worktree(&mut service, parent.id, "agent/torn-move");
        service.git_executable = fixture.shim(
            "persist-failing-move-git",
            &format!(
                "#!/bin/sh\n\
                 previous=\n\
                 operation=other\n\
                 for argument in \"$@\"; do\n\
                   if [ \"$previous\" = worktree ]; then operation=\"$argument\"; break; fi\n\
                   previous=\"$argument\"\n\
                 done\n\
                 git \"$@\"\n\
                 status=$?\n\
                 if [ \"$operation\" = move ] && [ \"$status\" -eq 0 ]; then chmod 500 '{}'; fi\n\
                 exit \"$status\"\n",
                fixture.data_dir.display()
            ),
        );

        let error = service
            .move_worktree(worktree.id, &destination, &Cancellation::default())
            .unwrap_err();
        let mut permissions = fs::metadata(&fixture.data_dir).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&fixture.data_dir, permissions).unwrap();

        assert!(matches!(
            error,
            ProjectError::WorktreeMovedCatalogStale { id, .. } if id == worktree.id
        ));
        assert_eq!(
            service
                .catalog
                .projects
                .iter()
                .find(|project| project.id == worktree.id)
                .unwrap()
                .root,
            worktree.root
        );
        assert!(!worktree.root.exists());
        assert!(destination.exists());

        let outcome = service
            .reconcile_worktrees(parent.id, &Cancellation::default())
            .unwrap();
        assert_eq!(outcome.repaired.len(), 1);
        assert_eq!(outcome.repaired[0].id, worktree.id);
        assert_eq!(outcome.repaired[0].root, as_catalogued(&destination));
    }

    #[cfg(unix)]
    #[test]
    fn a_destination_created_during_git_move_cannot_be_catalogued_as_the_checkout() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("racing-destination-parent");
        initialize_repository(&parent_root);
        let destination_parent = fixture.directory("racing-destination-container");
        let destination = destination_parent.join("checkout");
        let ready = fixture.root.path().join("racing-move-started");
        let release = fixture.root.path().join("release-racing-move");
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let worktree = create_branch_worktree(&mut service, parent.id, "agent/racing-destination");
        service.git_executable = fixture.shim(
            "racing-destination-git",
            &format!(
                "#!/bin/sh\n\
                 previous=\n\
                 operation=other\n\
                 for argument in \"$@\"; do\n\
                   if [ \"$previous\" = worktree ]; then operation=\"$argument\"; break; fi\n\
                   previous=\"$argument\"\n\
                 done\n\
                 if [ \"$operation\" = move ]; then\n\
                   touch '{}'\n\
                   while [ ! -e '{}' ]; do sleep 0.01; done\n\
                 fi\n\
                 exec git \"$@\"\n",
                ready.display(),
                release.display()
            ),
        );
        let source_name = worktree.root.file_name().unwrap().to_owned();
        let worktree_id = worktree.id;
        let requested_destination = destination.clone();
        let movement = thread::spawn(move || {
            service.move_worktree(
                worktree_id,
                &requested_destination,
                &Cancellation::default(),
            )
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !ready.exists() && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(ready.exists(), "the racing move shim never started");
        fs::create_dir(&destination).unwrap();
        fs::write(&release, b"continue").unwrap();

        let error = movement.join().unwrap().unwrap_err();
        assert!(matches!(
            error,
            ProjectError::WorktreeMovedCatalogStale { id, .. } if id == worktree_id
        ));
        let actual_root = destination.join(source_name);
        assert!(actual_root.exists());
        assert_eq!(
            ProjectService::load_from_data_dir(&fixture.data_dir)
                .unwrap()
                .list_catalog_only()
                .unwrap()
                .into_iter()
                .find(|project| project.id == worktree_id)
                .unwrap()
                .root,
            worktree.root
        );

        let mut repair = fixture.service();
        let outcome = repair
            .reconcile_worktrees(parent.id, &Cancellation::default())
            .unwrap();
        assert_eq!(outcome.repaired[0].root, as_catalogued(&actual_root));
    }

    #[test]
    fn locked_worktrees_refuse_moves_with_the_recorded_reason() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("locked-move-parent");
        initialize_repository(&parent_root);
        let destination_parent = fixture.directory("locked-move-destination-parent");
        let destination = destination_parent.join("checkout");
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let worktree = create_branch_worktree(&mut service, parent.id, "agent/locked-move");
        git(
            &parent_root,
            [
                "worktree",
                "lock",
                "--reason",
                "portable checkout",
                worktree.root.to_str().unwrap(),
            ],
        );

        let error = service
            .move_worktree(worktree.id, &destination, &Cancellation::default())
            .unwrap_err();

        assert!(matches!(
            error,
            ProjectError::Git(GitError::WorktreeLocked { reason, .. })
                if reason.as_deref() == Some("portable checkout")
        ));
        assert!(worktree.root.exists());
        assert!(!destination.exists());
    }

    #[test]
    fn reconciliation_repairs_a_git_moved_worktree_before_it_can_be_removed() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("move-repair-parent");
        initialize_repository(&parent_root);
        let destination_parent = fixture.directory("move-repair-destination-parent");
        let destination = destination_parent.join("checkout");
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let worktree = create_branch_worktree(&mut service, parent.id, "agent/repair-move");
        git(
            &parent_root,
            [
                "worktree",
                "move",
                "--",
                worktree.root.to_str().unwrap(),
                destination.to_str().unwrap(),
            ],
        );

        assert!(matches!(
            service.remove_worktree(worktree.id, false, &Cancellation::default()),
            Err(ProjectError::UnsafeWorktreeRemoval { id, .. }) if id == worktree.id
        ));
        assert!(destination.exists());

        let outcome = service
            .reconcile_worktrees(parent.id, &Cancellation::default())
            .unwrap();

        assert!(outcome.removed.is_empty());
        assert_eq!(outcome.repaired.len(), 1);
        assert_eq!(outcome.repaired[0].id, worktree.id);
        assert!(outcome.skipped.is_empty());
        let repaired = service
            .list_catalog_only()
            .unwrap()
            .into_iter()
            .find(|project| project.id == worktree.id)
            .unwrap();
        assert_eq!(repaired.id, worktree.id);
        assert_eq!(repaired.display_name, worktree.display_name);
        assert_eq!(repaired.source, worktree.source);
        assert_eq!(repaired.last_opened, worktree.last_opened);
        assert_eq!(repaired.root, as_catalogued(&destination));
        service
            .remove_worktree(worktree.id, false, &Cancellation::default())
            .unwrap();
        assert!(!destination.exists());
    }

    #[test]
    fn reconciliation_skips_an_unsafe_repair_without_blocking_siblings_or_recovery() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("selective-repair-parent");
        initialize_repository(&parent_root);
        let other_root = fixture.directory("selective-repair-other-project");
        let safe_parent = fixture.directory("selective-repair-safe-parent");
        let unsafe_destination = other_root.join("nested-worktree");
        let safe_destination = safe_parent.join("recovered-worktree");
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let other = service.import_local(&other_root).unwrap();
        let unsafe_worktree =
            create_branch_worktree(&mut service, parent.id, "agent/unsafe-repair");
        let stale_sibling = create_branch_worktree(&mut service, parent.id, "agent/stale-sibling");
        git(
            &parent_root,
            [
                "worktree",
                "move",
                "--",
                unsafe_worktree.root.to_str().unwrap(),
                unsafe_destination.to_str().unwrap(),
            ],
        );
        fs::remove_dir_all(&stale_sibling.root).unwrap();

        let outcome = service
            .reconcile_worktrees(parent.id, &Cancellation::default())
            .unwrap();

        assert_eq!(outcome.removed.len(), 1);
        assert_eq!(outcome.removed[0].id, stale_sibling.id);
        assert!(outcome.repaired.is_empty());
        assert!(outcome.skipped.iter().any(|skip| {
            skip.project.id == unsafe_worktree.id && skip.reason.contains(&other.id.to_string())
        }));
        assert!(unsafe_destination.exists());

        git(
            &parent_root,
            [
                "worktree",
                "move",
                "--",
                unsafe_destination.to_str().unwrap(),
                safe_destination.to_str().unwrap(),
            ],
        );
        let recovered = service
            .reconcile_worktrees(parent.id, &Cancellation::default())
            .unwrap();
        assert_eq!(recovered.repaired.len(), 1);
        assert_eq!(recovered.repaired[0].id, unsafe_worktree.id);
        assert_eq!(recovered.repaired[0].root, as_catalogued(&safe_destination));
    }

    #[test]
    fn dirty_worktree_force_removal_discards_files_but_preserves_the_branch() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("force-worktree-parent");
        initialize_repository(&parent_root);
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let branch = "agent/force-worktree";
        let worktree = create_branch_worktree(&mut service, parent.id, branch);
        fs::write(
            worktree.root.join("uncommitted.txt"),
            "discard only with force",
        )
        .unwrap();

        assert!(matches!(
            service.remove_worktree(worktree.id, false, &Cancellation::default()),
            Err(ProjectError::DirtyWorktreeRemoval { id, .. }) if id == worktree.id
        ));
        assert!(worktree.root.exists());

        service
            .remove_worktree(worktree.id, true, &Cancellation::default())
            .unwrap();

        assert!(!worktree.root.exists());
        git(
            &parent_root,
            ["show-ref", "--verify", &format!("refs/heads/{branch}")],
        );
        assert!(
            !git(&parent_root, ["worktree", "list", "--porcelain"])
                .contains(&worktree.id.to_string())
        );
    }

    #[test]
    fn reconciliation_is_explicit_selective_and_never_deletes_a_remaining_directory() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("prune-worktree-parent");
        initialize_repository(&parent_root);
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let missing = create_branch_worktree(&mut service, parent.id, "agent/prune-missing");
        fs::remove_dir_all(&missing.root).unwrap();

        let before = service
            .worktrees(parent.id, &Cancellation::default())
            .unwrap();
        let stale = before
            .iter()
            .find_map(|worktree| worktree.project.as_ref())
            .unwrap();
        assert_eq!(stale.id, missing.id);
        assert!(!stale.available);

        let outcome = service
            .reconcile_worktrees(parent.id, &Cancellation::default())
            .unwrap();

        assert_eq!(
            outcome
                .removed
                .iter()
                .map(|project| project.id)
                .collect::<Vec<_>>(),
            [missing.id]
        );
        assert!(
            service
                .worktrees(parent.id, &Cancellation::default())
                .unwrap()
                .is_empty()
        );

        let kept = create_branch_worktree(&mut service, parent.id, "agent/prune-kept");
        fs::remove_dir_all(
            parent_root
                .join(".git")
                .join("worktrees")
                .join(kept.id.to_string()),
        )
        .unwrap();
        assert!(
            service
                .reconcile_worktrees(parent.id, &Cancellation::default())
                .unwrap()
                .removed
                .is_empty()
        );
        assert!(kept.root.exists(), "prune deleted a checkout directory");
        assert!(
            service
                .list_catalog_only()
                .unwrap()
                .iter()
                .any(|project| project.id == kept.id)
        );

        let opened_stale =
            create_branch_worktree(&mut service, parent.id, "agent/stays-stale-while-opening");
        fs::remove_dir_all(&opened_stale.root).unwrap();
        service.open(parent.id).unwrap();
        assert!(
            service
                .list_catalog_only()
                .unwrap()
                .iter()
                .any(|project| project.id == opened_stale.id),
            "opening a parent performed an undisclosed repository mutation"
        );
        let outcome = service
            .reconcile_worktrees(parent.id, &Cancellation::default())
            .unwrap();
        assert_eq!(outcome.removed[0].id, opened_stale.id);
    }

    #[test]
    fn reconciliation_never_prunes_a_missing_external_worktree() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("selective-reconcile-parent");
        initialize_repository(&parent_root);
        git(&parent_root, ["branch", "external-portable"]);
        let external = fixture.root.path().join("external-portable-worktree");
        git(
            &parent_root,
            [
                "worktree",
                "add",
                "--",
                external.to_str().unwrap(),
                "external-portable",
            ],
        );
        git(
            &parent_root,
            [
                "worktree",
                "lock",
                "--reason",
                "temporarily unmounted",
                external.to_str().unwrap(),
            ],
        );
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let managed = create_branch_worktree(&mut service, parent.id, "agent/reconcile-owned");
        fs::remove_dir_all(&external).unwrap();
        fs::remove_dir_all(&managed.root).unwrap();

        let outcome = service
            .reconcile_worktrees(parent.id, &Cancellation::default())
            .unwrap();

        assert_eq!(outcome.removed[0].id, managed.id);
        let listed =
            crate::git::worktree::list(Path::new("git"), &parent_root, &Cancellation::default())
                .unwrap();
        let external = listed
            .iter()
            .find(|worktree| crate::git::worktree::same_path(&worktree.root, &external))
            .expect("reconciliation must leave an external worktree registered");
        assert_eq!(external.locked.as_deref(), Some("temporarily unmounted"));
    }

    #[test]
    fn reconciliation_cleans_a_missing_canonical_relocated_worktree() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("unsafe-reconciliation-parent");
        initialize_repository(&parent_root);
        let outside = fixture.root.path().join("unsafe-reconciliation-worktree");
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let id = ProjectId::new();
        git(
            &parent_root,
            [
                "worktree",
                "add",
                "-b",
                "agent/unsafe-reconcile",
                "--",
                outside.to_str().unwrap(),
            ],
        );
        let worktree = store_worktree(
            &mut service,
            parent.id,
            id,
            outside.clone(),
            "agent/unsafe-reconcile",
        );
        fs::remove_dir_all(&outside).unwrap();

        let outcome = service
            .reconcile_worktrees(parent.id, &Cancellation::default())
            .unwrap();

        assert_eq!(outcome.removed.len(), 1);
        assert_eq!(outcome.removed[0].id, id);
        let listed =
            crate::git::worktree::list(Path::new("git"), &parent_root, &Cancellation::default())
                .unwrap();
        assert!(
            !listed
                .iter()
                .any(|worktree| crate::git::worktree::same_path(&worktree.root, &outside))
        );
        assert!(
            !service
                .list_catalog_only()
                .unwrap()
                .iter()
                .any(|project| project.id == worktree.id)
        );
    }

    #[test]
    fn worktree_reads_and_mutations_honor_pre_cancelled_tokens() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("cancelled-worktree-parent");
        initialize_repository(&parent_root);
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let worktree = create_branch_worktree(&mut service, parent.id, "agent/cancelled");
        let move_destination = fixture.directory("cancelled-move-parent").join("checkout");
        let cancellation = Cancellation::default();
        cancellation.cancel();

        assert!(matches!(
            service.worktrees(parent.id, &cancellation),
            Err(ProjectError::Git(GitError::Cancelled))
        ));
        assert!(matches!(
            service.create_worktree(
                parent.id,
                &WorktreeBase::NewBranch {
                    name: "agent/never-created".to_owned(),
                    start_point: None,
                },
                &cancellation,
            ),
            Err(ProjectError::Git(GitError::Cancelled))
        ));
        assert!(matches!(
            service.move_worktree(worktree.id, &move_destination, &cancellation),
            Err(ProjectError::Git(GitError::Cancelled))
        ));
        assert!(matches!(
            service.reconcile_worktrees(parent.id, &cancellation),
            Err(ProjectError::Git(GitError::Cancelled))
        ));
        assert!(matches!(
            service.remove_worktree(worktree.id, false, &cancellation),
            Err(ProjectError::Git(GitError::Cancelled))
        ));
        assert!(worktree.root.exists());
        assert!(
            !move_destination.exists(),
            "cancellation was observed only after the checkout moved"
        );
    }

    #[test]
    fn busy_reconciliation_is_reported_and_leaves_the_catalog_intact() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("busy-reconciliation-parent");
        initialize_repository(&parent_root);
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let worktree = create_branch_worktree(&mut service, parent.id, "agent/busy-reconcile");
        fs::remove_dir_all(&worktree.root).unwrap();
        let ready_file = fixture.root.path().join("reconciliation-lock-held");
        let mut holder = spawn_child(&fixture.data_dir, "hold-repository-lock")
            .env(PROCESS_PROJECT_ROOT_ENV, &parent_root)
            .env(PROCESS_READY_FILE_ENV, &ready_file)
            .spawn()
            .unwrap();
        wait_for_child_signal(&mut holder, &ready_file);

        let error = service
            .reconcile_worktrees(parent.id, &Cancellation::default())
            .unwrap_err();

        holder.kill().unwrap();
        holder.wait().unwrap();
        assert!(matches!(
            error,
            ProjectError::Git(GitError::RepositoryBusy { .. })
        ));
        assert!(
            service
                .list_catalog_only()
                .unwrap()
                .iter()
                .any(|project| project.id == worktree.id)
        );
        assert_eq!(service.open(parent.id).unwrap().id, parent.id);
    }

    #[test]
    fn concurrent_process_worktree_creation_never_corrupts_git_or_the_catalog() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("racing-worktree-parent");
        initialize_repository(&parent_root);
        let parent = fixture.service().import_local(&parent_root).unwrap();
        let mut children = ["agent/process-first", "agent/process-second"]
            .into_iter()
            .map(|branch| {
                spawn_child(&fixture.data_dir, "create-worktree")
                    .env(PROCESS_PROJECT_ID_ENV, parent.id.to_string())
                    .env(PROCESS_BRANCH_ENV, branch)
                    .spawn()
                    .unwrap()
            })
            .collect::<Vec<_>>();

        for child in &mut children {
            assert!(child.wait().unwrap().success());
        }

        let service = fixture.service();
        let worktrees = service
            .worktrees(parent.id, &Cancellation::default())
            .unwrap();
        let managed = worktrees
            .iter()
            .filter(|worktree| worktree.project.is_some())
            .collect::<Vec<_>>();
        assert!(!managed.is_empty() && managed.len() <= 2);
        assert_eq!(
            service
                .list_catalog_only()
                .unwrap()
                .iter()
                .filter(|project| matches!(project.source, ProjectSource::Worktree { .. }))
                .count(),
            managed.len()
        );
        for worktree in managed {
            assert!(worktree.root.join(".git").is_file());
        }
    }

    #[test]
    fn worktree_removal_refuses_a_foreign_checkout_with_no_harkness_identity() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("unsafe-worktree-parent");
        initialize_repository(&parent_root);
        let outside = fixture.root.path().join("outside-worktree");
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let id = ProjectId::new();
        git(
            &parent_root,
            [
                "worktree",
                "add",
                "-b",
                "agent/outside-worktree",
                "--",
                outside.to_str().unwrap(),
            ],
        );
        let worktree = store_worktree(
            &mut service,
            parent.id,
            id,
            outside.clone(),
            "agent/outside-worktree",
        );

        let error = service
            .remove_worktree(worktree.id, false, &Cancellation::default())
            .unwrap_err();

        assert!(matches!(
            error,
            ProjectError::UnsafeWorktreeRemoval { id: refused, .. } if refused == worktree.id
        ));
        assert!(outside.exists(), "refusal deleted the foreign checkout");
        assert!(
            service
                .list_catalog_only()
                .unwrap()
                .iter()
                .any(|project| project.id == worktree.id),
            "refusal dropped the catalog guardrail"
        );
    }

    #[cfg(unix)]
    #[test]
    fn worktree_removal_refuses_a_symlink_escape() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let parent_root = fixture.directory("symlink-worktree-parent");
        initialize_repository(&parent_root);
        let outside = fixture.root.path().join("symlink-worktree-outside");
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let id = ProjectId::new();
        git(
            &parent_root,
            [
                "worktree",
                "add",
                "-b",
                "agent/symlink-worktree",
                "--",
                outside.to_str().unwrap(),
            ],
        );
        fs::create_dir_all(fixture.data_dir.join(WORKTREES_DIRECTORY)).unwrap();
        let reserved = service.worktree_path(id);
        symlink(&outside, &reserved).unwrap();
        let project = Project {
            id,
            display_name: "agent/symlink-worktree".to_owned(),
            root: reserved.clone(),
            source: ProjectSource::Worktree {
                parent: parent.id,
                worktree_branch: Some("agent/symlink-worktree".to_owned()),
            },
            last_opened: time::OffsetDateTime::now_utc(),
            available: true,
            git: None,
        };
        {
            let _lock = service.lock_exclusive().unwrap();
            let mut catalog = service.read_catalog().unwrap();
            catalog.projects.push(project.clone());
            service.persist(&catalog).unwrap();
            service.catalog = catalog;
        }

        assert!(matches!(
            service.remove_worktree(id, false, &Cancellation::default()),
            Err(ProjectError::UnsafeWorktreeRemoval { id: refused, .. }) if refused == id
        ));
        assert!(outside.exists());
        fs::remove_file(&reserved).unwrap();
        git(
            &parent_root,
            [
                "worktree",
                "remove",
                "--force",
                "--",
                outside.to_str().unwrap(),
            ],
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_creation_cleans_git_then_the_directory_then_retries_targeted_removal() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("failed-worktree-parent");
        initialize_repository(&parent_root);
        let operations = fixture.root.path().join("worktree-operations.log");
        let shim = fixture.shim(
            "fail-after-worktree-add",
            &format!(
                "#!/bin/sh\n\
                 previous=\n\
                 operation=other\n\
                 for argument in \"$@\"; do\n\
                   if [ \"$previous\" = worktree ]; then operation=\"$argument\"; break; fi\n\
                   previous=\"$argument\"\n\
                 done\n\
                 printf '%s\\n' \"$operation\" >> \"{}\"\n\
                 git \"$@\"\n\
                 status=$?\n\
                 if [ \"$operation\" = add ] && [ \"$status\" -eq 0 ]; then exit 42; fi\n\
                 exit \"$status\"\n",
                operations.display()
            ),
        );
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        service.git_executable = shim;
        let branch = "agent/failed-worktree";

        let error = service
            .create_worktree(
                parent.id,
                &WorktreeBase::NewBranch {
                    name: branch.to_owned(),
                    start_point: None,
                },
                &Cancellation::default(),
            )
            .unwrap_err();

        assert!(matches!(error, ProjectError::Git(GitError::Failed { .. })));
        assert_eq!(
            fs::read_to_string(&operations)
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            ["add", "remove", "remove"]
        );
        let catalog = service.list_catalog_only().unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].id, parent.id);
        assert!(
            fs::read_dir(fixture.data_dir.join(WORKTREES_DIRECTORY))
                .unwrap()
                .next()
                .is_none()
        );
        assert!(
            !git(&parent_root, ["worktree", "list", "--porcelain"]).contains(
                &fixture
                    .data_dir
                    .join(WORKTREES_DIRECTORY)
                    .display()
                    .to_string()
            )
        );
        git(
            &parent_root,
            ["show-ref", "--verify", &format!("refs/heads/{branch}")],
        );
    }

    #[cfg(unix)]
    #[test]
    fn slow_git_worktree_removal_does_not_hold_the_catalog_lock() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("slow-removal-parent");
        initialize_repository(&parent_root);
        let ready = fixture.root.path().join("worktree-remove-started");
        let release = fixture.root.path().join("release-worktree-remove");
        let shim = fixture.shim(
            "slow-worktree-remove",
            &format!(
                "#!/bin/sh\n\
                 previous=\n\
                 operation=other\n\
                 for argument in \"$@\"; do\n\
                   if [ \"$previous\" = worktree ]; then operation=\"$argument\"; break; fi\n\
                   previous=\"$argument\"\n\
                 done\n\
                 if [ \"$operation\" = remove ]; then\n\
                   touch '{}'\n\
                   while [ ! -e '{}' ]; do sleep 0.01; done\n\
                 fi\n\
                 exec git \"$@\"\n",
                ready.display(),
                release.display()
            ),
        );
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let worktree = create_branch_worktree(&mut service, parent.id, "agent/slow-removal");
        service.git_executable = shim;

        let removal = thread::spawn(move || {
            service.remove_worktree(worktree.id, false, &Cancellation::default())
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !ready.exists() && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(ready.exists(), "the removal shim never started");

        let data_dir = fixture.data_dir.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let listing = thread::spawn(move || {
            let result = ProjectService::load_from_data_dir(data_dir)
                .and_then(|service| service.list_catalog_only());
            let _ = sender.send(result);
        });
        let listed = receiver.recv_timeout(Duration::from_secs(1));
        fs::write(&release, b"continue").unwrap();
        let removed = removal.join().unwrap().unwrap();
        listing.join().unwrap();

        assert!(
            listed.unwrap().is_ok(),
            "catalog read blocked behind Git removal"
        );
        assert_eq!(removed.id, worktree.id);
    }

    #[cfg(unix)]
    #[test]
    fn slow_git_worktree_move_does_not_hold_the_catalog_lock() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("slow-move-parent");
        initialize_repository(&parent_root);
        let destination = fixture
            .directory("slow-move-destination-parent")
            .join("checkout");
        let ready = fixture.root.path().join("worktree-move-started");
        let release = fixture.root.path().join("release-worktree-move");
        let shim = fixture.shim(
            "slow-worktree-move",
            &format!(
                "#!/bin/sh\n\
                 previous=\n\
                 operation=other\n\
                 for argument in \"$@\"; do\n\
                   if [ \"$previous\" = worktree ]; then operation=\"$argument\"; break; fi\n\
                   previous=\"$argument\"\n\
                 done\n\
                 if [ \"$operation\" = move ]; then\n\
                   touch '{}'\n\
                   while [ ! -e '{}' ]; do sleep 0.01; done\n\
                 fi\n\
                 exec git \"$@\"\n",
                ready.display(),
                release.display()
            ),
        );
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let worktree = create_branch_worktree(&mut service, parent.id, "agent/slow-move");
        service.git_executable = shim;

        let moved_id = worktree.id;
        let movement = thread::spawn(move || {
            service.move_worktree(moved_id, &destination, &Cancellation::default())
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !ready.exists() && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(ready.exists(), "the move shim never started");

        let mut competing = ProjectService::load_from_data_dir(&fixture.data_dir).unwrap();
        assert!(matches!(
            competing.remove_worktree(moved_id, false, &Cancellation::default()),
            Err(ProjectError::Git(GitError::RepositoryBusy { .. }))
        ));

        let data_dir = fixture.data_dir.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let listing = thread::spawn(move || {
            let result = ProjectService::load_from_data_dir(data_dir)
                .and_then(|service| service.list_catalog_only());
            let _ = sender.send(result);
        });
        let listed = receiver.recv_timeout(Duration::from_secs(1));
        fs::write(&release, b"continue").unwrap();
        let moved = movement.join().unwrap().unwrap();
        listing.join().unwrap();

        assert!(
            listed.unwrap().is_ok(),
            "catalog read blocked behind Git move"
        );
        assert_eq!(moved.id, moved_id);
    }

    #[test]
    fn worktree_removal_uses_git_cleans_the_catalog_and_restores_version_one() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("remove-worktree-parent");
        initialize_repository(&parent_root);
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let branch = "agent/remove-clean-worktree";
        let worktree = catalogue_git_worktree(&mut service, &parent, branch);
        let branch_record = format!("branch refs/heads/{branch}");
        assert!(
            git(&parent.root, ["worktree", "list", "--porcelain"])
                .lines()
                .any(|line| line == branch_record)
        );

        let removed = service
            .remove_worktree(worktree.id, false, &Cancellation::default())
            .unwrap();

        assert_eq!(removed.id, worktree.id);
        assert!(!worktree.root.exists());
        assert!(
            !git(&parent.root, ["worktree", "list", "--porcelain"])
                .lines()
                .any(|line| line == branch_record)
        );
        let remaining = service.list_catalog_only().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, parent.id);
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture.data_dir.join(CATALOG_FILE)).unwrap())
                .unwrap();
        assert_eq!(persisted["version"], 1);
        service.remove(parent.id).unwrap();
    }

    #[test]
    fn worktree_removal_refuses_uncommitted_files_without_dropping_the_entry() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("dirty-worktree-parent");
        initialize_repository(&parent_root);
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let worktree = catalogue_git_worktree(&mut service, &parent, "agent/dirty-worktree");
        fs::write(worktree.root.join("untracked.txt"), "keep me").unwrap();

        let error = service
            .remove_worktree(worktree.id, false, &Cancellation::default())
            .unwrap_err();

        assert!(matches!(
            error,
            ProjectError::DirtyWorktreeRemoval { id, .. } if id == worktree.id
        ));
        assert!(worktree.root.join("untracked.txt").exists());
        assert!(
            service
                .list_catalog_only()
                .unwrap()
                .iter()
                .any(|project| project.id == worktree.id)
        );
    }

    #[test]
    fn managed_removal_reports_source_before_worktree_children() {
        let fixture = Fixture::new();
        let local_root = fixture.directory("local-parent-with-worktree");
        let mut service = fixture.service();
        let local = service.import_local(local_root).unwrap();
        let worktree = catalogue_worktree(&mut service, local.id, "agent/local-child");

        assert!(matches!(
            service.remove_managed(local.id),
            Err(ProjectError::UnsafeManagedRemoval { id, .. }) if id == local.id
        ));
        assert!(matches!(
            service.remove_managed(worktree.id),
            Err(ProjectError::WorktreeRemovalRequired { id, .. }) if id == worktree.id
        ));
    }

    #[test]
    fn invalid_directories_have_typed_errors() {
        let fixture = Fixture::new();
        let missing = fixture.root.path().join("missing");
        let regular_file = fixture.root.path().join("regular-file");
        fs::write(&regular_file, "not a directory").unwrap();
        let mut service = fixture.service();

        assert!(matches!(
            service.import_local(missing),
            Err(ProjectError::InvalidDirectory { .. })
        ));
        assert!(matches!(
            service.import_local(regular_file),
            Err(ProjectError::InvalidDirectory { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_directories_have_typed_errors() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new();
        let project_root = fixture.directory("unreadable");
        let original_permissions = fs::metadata(&project_root).unwrap().permissions();
        fs::set_permissions(&project_root, fs::Permissions::from_mode(0o000)).unwrap();
        let result = fixture.service().import_local(&project_root);
        fs::set_permissions(&project_root, original_permissions).unwrap();

        assert!(matches!(
            result,
            Err(ProjectError::UnreadableDirectory { .. })
        ));
    }

    #[test]
    fn malformed_and_unsupported_catalogs_have_distinct_errors() {
        let fixture = Fixture::new();
        fs::create_dir_all(&fixture.data_dir).unwrap();
        let catalog_path = fixture.data_dir.join(CATALOG_FILE);
        fs::write(&catalog_path, b"{ definitely not json").unwrap();

        assert!(matches!(
            ProjectService::load_from_data_dir(&fixture.data_dir),
            Err(ProjectError::MalformedCatalog { .. })
        ));

        fs::write(&catalog_path, br#"{"version":0,"projects":[]}"#).unwrap();
        assert!(matches!(
            ProjectService::load_from_data_dir(&fixture.data_dir),
            Err(ProjectError::CatalogVersionTooOld {
                found: 0,
                minimum: MINIMUM_SUPPORTED_CATALOG_VERSION
            })
        ));

        // A newer version is reported as such even when the rest of the file no
        // longer matches the current schema.
        let future_version = CATALOG_VERSION + 1;
        fs::write(
            &catalog_path,
            format!(r#"{{"version":{future_version},"entries":{{"unexpected":1}}}}"#),
        )
        .unwrap();
        assert!(matches!(
            ProjectService::load_from_data_dir(&fixture.data_dir),
            Err(ProjectError::CatalogVersionTooNew { found, maximum })
                if found == future_version && maximum == CATALOG_VERSION
        ));
    }

    #[test]
    fn same_version_unknown_or_source_inappropriate_fields_are_rejected() {
        let fixture = Fixture::new();
        let root = fixture.directory("invalid-source-fields");
        fs::create_dir_all(&fixture.data_dir).unwrap();
        let catalog_path = fixture.data_dir.join(CATALOG_FILE);
        let base = serde_json::json!({
            "id": "00000000-0000-4000-8000-000000000001",
            "display_name": "invalid",
            "root": as_catalogued(&root),
            "last_opened": "2026-08-06 18:52:03.000000000 +00:00:00"
        });
        let invalid_projects = [
            {
                let mut project = base.clone();
                project["source"] = "local".into();
                project["parent"] = "00000000-0000-4000-8000-000000000001".into();
                project
            },
            {
                let mut project = base.clone();
                project["source"] = "worktree".into();
                project["worktree_branch"] = "agent/missing-parent".into();
                project
            },
            {
                let mut project = base.clone();
                project["source"] = "managed_repository".into();
                project
            },
            {
                let mut project = base.clone();
                project["source"] = "local".into();
                project["future_field"] = true.into();
                project
            },
            {
                let mut project = base;
                project["source"] = "future_source".into();
                project
            },
        ];

        for project in invalid_projects {
            fs::write(
                &catalog_path,
                serde_json::to_vec(&serde_json::json!({
                    "version": CATALOG_VERSION,
                    "projects": [project]
                }))
                .unwrap(),
            )
            .unwrap();
            assert!(matches!(
                ProjectService::load_from_data_dir(&fixture.data_dir),
                Err(ProjectError::MalformedCatalog { .. })
            ));
        }

        fs::write(
            &catalog_path,
            format!(
                r#"{{"version":{},"projects":[{{"source":"future_source"}}]}}"#,
                CATALOG_VERSION + 1
            ),
        )
        .unwrap();
        assert!(matches!(
            ProjectService::load_from_data_dir(&fixture.data_dir),
            Err(ProjectError::CatalogVersionTooNew { .. })
        ));
    }

    #[test]
    fn invalid_worktree_relationships_are_rejected_on_read() {
        let fixture = Fixture::new();
        let first_root = fixture.directory("invalid-worktree-first");
        let second_root = fixture.directory("invalid-worktree-second");
        fs::create_dir_all(&fixture.data_dir).unwrap();
        let catalog_path = fixture.data_dir.join(CATALOG_FILE);
        let first = "00000000-0000-4000-8000-000000000001";
        let second = "00000000-0000-4000-8000-000000000002";
        let worktree = |id: &str, parent: &str, root: &Path| {
            serde_json::json!({
                "id": id,
                "display_name": id,
                "root": as_catalogued(root),
                "source": "worktree",
                "parent": parent,
                "worktree_branch": format!("agent/{id}"),
                "last_opened": "2026-08-06 18:52:03.000000000 +00:00:00"
            })
        };
        let invalid = [
            vec![worktree(first, first, &first_root)],
            vec![worktree(first, second, &first_root)],
            vec![
                worktree(first, second, &first_root),
                worktree(second, first, &second_root),
            ],
        ];

        for projects in invalid {
            fs::write(
                &catalog_path,
                serde_json::to_vec(&serde_json::json!({
                    "version": CATALOG_VERSION,
                    "projects": projects
                }))
                .unwrap(),
            )
            .unwrap();
            assert!(matches!(
                ProjectService::load_from_data_dir(&fixture.data_dir),
                Err(ProjectError::InvalidCatalog { .. })
            ));
        }
    }

    #[test]
    fn even_an_acyclic_worktree_parent_chain_is_rejected() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("chain-parent");
        let first_root = fixture.directory("chain-first");
        let second_root = fixture.directory("chain-second");
        fs::create_dir_all(&fixture.data_dir).unwrap();
        let catalog_path = fixture.data_dir.join(CATALOG_FILE);
        let parent = "00000000-0000-4000-8000-000000000001";
        let first = "00000000-0000-4000-8000-000000000002";
        let second = "00000000-0000-4000-8000-000000000003";
        fs::write(
            &catalog_path,
            serde_json::to_vec(&serde_json::json!({
                "version": CATALOG_VERSION,
                "projects": [
                    {
                        "id": parent,
                        "display_name": "parent",
                        "root": as_catalogued(&parent_root),
                        "source": "local",
                        "last_opened": "2026-08-06 18:52:03.000000000 +00:00:00"
                    },
                    {
                        "id": first,
                        "display_name": "first",
                        "root": as_catalogued(&first_root),
                        "source": "worktree",
                        "parent": parent,
                        "worktree_branch": "agent/first",
                        "last_opened": "2026-08-06 18:53:03.000000000 +00:00:00"
                    },
                    {
                        "id": second,
                        "display_name": "second",
                        "root": as_catalogued(&second_root),
                        "source": "worktree",
                        "parent": first,
                        "worktree_branch": "agent/second",
                        "last_opened": "2026-08-06 18:54:03.000000000 +00:00:00"
                    }
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        assert!(matches!(
            ProjectService::load_from_data_dir(&fixture.data_dir),
            Err(ProjectError::InvalidCatalog { .. })
        ));
    }

    #[test]
    fn unreadable_catalog_paths_have_typed_errors() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.data_dir.join(CATALOG_FILE)).unwrap();

        assert!(matches!(
            ProjectService::load_from_data_dir(&fixture.data_dir),
            Err(ProjectError::CatalogRead { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn failed_persistence_does_not_change_in_memory_state() {
        use std::os::unix::fs::PermissionsExt;

        fn set_mode(path: &Path, mode: u32) {
            let mut permissions = fs::metadata(path).unwrap().permissions();
            permissions.set_mode(mode);
            fs::set_permissions(path, permissions).unwrap();
        }

        let fixture = Fixture::new();
        let seeded_root = fixture.directory("seeded-project");
        let project_root = fixture.directory("persistence-project");
        let mut service = fixture.service();
        let seeded = service.import_local(&seeded_root).unwrap();

        // A read-only data directory still opens the catalog and the lock, and
        // fails only where the atomic replacement creates its temporary file.
        set_mode(&fixture.data_dir, 0o500);
        let imported = service.import_local(project_root);
        set_mode(&fixture.data_dir, 0o700);

        assert!(matches!(imported, Err(ProjectError::Persistence { .. })));
        assert_eq!(service.catalog.projects, vec![seeded]);
    }

    #[test]
    fn persistence_atomically_replaces_the_catalog_without_artifacts() {
        let fixture = Fixture::new();
        let first = fixture.directory("atomic-first");
        let second = fixture.directory("atomic-second");
        let mut service = fixture.service();
        service.import_local(first).unwrap();
        service.import_local(second).unwrap();

        // The lock file is created once and survives every atomic replacement.
        let mut entries = fs::read_dir(&fixture.data_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries.sort();
        assert_eq!(entries, vec![CATALOG_FILE, CATALOG_LOCK_FILE]);
        assert!(ProjectService::load_from_data_dir(&fixture.data_dir).is_ok());
    }

    #[test]
    fn future_worktree_path_is_reserved_but_not_created() {
        let fixture = Fixture::new();
        let project_root = fixture.directory("no-worktree");
        let mut service = fixture.service();
        let imported = service.import_local(project_root).unwrap();

        let expected = fixture
            .data_dir
            .join(WORKTREES_DIRECTORY)
            .join(imported.id.to_string());
        assert_eq!(service.worktree_path(imported.id), expected);
        assert!(!expected.exists());
    }

    #[test]
    fn managed_repository_clone_uses_default_branch_and_deduplicates_remote() {
        let fixture = Fixture::new();
        let remote = fixture.directory("remote");
        initialize_repository(&remote);
        let mut service = fixture.service();

        let imported = service
            .import_repository(
                remote.to_str().unwrap(),
                &CloneCancellation::default(),
                |_| {},
            )
            .unwrap();
        let duplicate = service
            .import_repository(
                &format!("file://{}", remote.display()),
                &CloneCancellation::default(),
                |_| {},
            )
            .unwrap();

        assert_eq!(imported.id, duplicate.id);
        assert_eq!(service.list().unwrap().len(), 1);
        assert!(managed_remote(&imported).starts_with("file://"));
        assert_eq!(
            imported.git.as_ref().unwrap().branch.as_deref(),
            Some("main")
        );
        assert_eq!(
            imported.root,
            as_catalogued(
                &fixture
                    .data_dir
                    .join(REPOSITORIES_DIRECTORY)
                    .join(imported.id.to_string())
                    .join(CHECKOUT_DIRECTORY)
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_imports_of_one_remote_keep_one_managed_checkout() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::{Arc, Barrier};

        let fixture = Fixture::new();
        let remote = fixture.directory("concurrent-remote");
        initialize_repository(&remote);
        let fake_git = fixture.root.path().join("synchronized-git");
        fs::write(&fake_git, "#!/bin/sh\necho ready >&2\nexec git \"$@\"\n").unwrap();
        let mut permissions = fs::metadata(&fake_git).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fake_git, permissions).unwrap();

        let barrier = Arc::new(Barrier::new(2));
        let data_dir = &fixture.data_dir;
        let projects = thread::scope(|scope| {
            let imports = (0..2)
                .map(|_| {
                    let barrier = Arc::clone(&barrier);
                    let remote = &remote;
                    let fake_git = &fake_git;
                    scope.spawn(move || {
                        let mut service = ProjectService::load_for_test(data_dir).unwrap();
                        service.git_executable = fake_git.clone();
                        service
                            .import_repository(
                                remote.to_str().unwrap(),
                                &CloneCancellation::default(),
                                |message| {
                                    if message == "ready" {
                                        barrier.wait();
                                    }
                                },
                            )
                            .unwrap()
                    })
                })
                .collect::<Vec<_>>();
            imports
                .into_iter()
                .map(|import| import.join().unwrap())
                .collect::<Vec<_>>()
        });

        assert_eq!(projects[0].id, projects[1].id);
        let catalogued = fixture.service().list().unwrap();
        assert_eq!(catalogued.len(), 1);
        assert_eq!(catalogued[0].id, projects[0].id);
        assert_eq!(
            fs::read_dir(fixture.data_dir.join(REPOSITORIES_DIRECTORY))
                .unwrap()
                .count(),
            1,
        );
    }

    #[test]
    #[ignore = "requires network access to clone a public GitHub repository"]
    fn github_remote_forms_deduplicate_through_import() {
        let fixture = Fixture::new();
        let mut service = ProjectService::load_from_data_dir(&fixture.data_dir).unwrap();

        let imported = service
            .import_repository(
                "https://github.com/octocat/Hello-World.git",
                &CloneCancellation::default(),
                |_| {},
            )
            .unwrap();
        let duplicate = service
            .import_repository(
                "git@github.com:octocat/Hello-World.git",
                &CloneCancellation::default(),
                |_| {},
            )
            .unwrap();

        assert_eq!(imported.id, duplicate.id);
        assert_eq!(managed_remote(&imported), managed_remote(&duplicate));
        assert_eq!(managed_remote(&imported), "github.com/octocat/hello-world");
        assert_eq!(service.list().unwrap().len(), 1);
    }

    #[test]
    fn production_managed_import_rejects_local_remotes() {
        let fixture = Fixture::new();
        let remote = fixture.directory("local-remote");
        initialize_repository(&remote);
        let mut service = ProjectService::load_from_data_dir(&fixture.data_dir).unwrap();

        let error = service
            .import_repository(
                remote.to_str().unwrap(),
                &CloneCancellation::default(),
                |_| {},
            )
            .unwrap_err();

        assert!(matches!(error, ProjectError::InvalidRemote { .. }));
        assert_eq!(
            error.to_string(),
            format!(
                "invalid Git repository remote '{}'; expected a GitHub HTTP(S) URL or SSH remote",
                remote.display()
            )
        );
        assert!(service.list().unwrap().is_empty());
        assert!(!fixture.data_dir.join(REPOSITORIES_DIRECTORY).exists());
    }

    #[test]
    fn failed_and_cancelled_clones_leave_no_catalog_or_destination() {
        let fixture = Fixture::new();
        let invalid_remote = fixture.directory("not-a-repository");
        let mut service = fixture.service();
        let error = service
            .import_repository(
                invalid_remote.to_str().unwrap(),
                &CloneCancellation::default(),
                |_| {},
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ProjectError::CloneFailed { ref stderr } if !stderr.is_empty()
        ));
        assert!(service.list().unwrap().is_empty());

        let remote = fixture.directory("cancel-remote");
        initialize_repository(&remote);
        let cancellation = CloneCancellation::default();
        cancellation.cancel();
        assert!(matches!(
            service.import_repository(remote.to_str().unwrap(), &cancellation, |_| {}),
            Err(ProjectError::CloneCancelled)
        ));
        assert!(service.list().unwrap().is_empty());
        let repositories = fixture.data_dir.join(REPOSITORIES_DIRECTORY);
        assert!(
            !repositories.exists() || fs::read_dir(repositories).unwrap().next().is_none(),
            "partial managed repository directory remained"
        );
    }

    #[test]
    fn managed_repository_reconciliation_removes_only_unlocked_uuid_orphans() {
        let fixture = Fixture::new();
        let service = fixture.service();
        let repositories = fixture.data_dir.join(REPOSITORIES_DIRECTORY);
        let orphan_id = ProjectId::new();
        let active_id = ProjectId::new();
        let unrelated = repositories.join("keep-me");
        let orphan = repositories.join(orphan_id.to_string());
        let active = repositories.join(active_id.to_string());
        fs::create_dir_all(orphan.join(CHECKOUT_DIRECTORY)).unwrap();
        fs::create_dir_all(active.join(CHECKOUT_DIRECTORY)).unwrap();
        fs::create_dir_all(&unrelated).unwrap();
        let active_lock = ManagedImportLock::acquire(&fixture.data_dir, active_id).unwrap();

        assert_eq!(
            service.reconcile_managed_repositories().unwrap(),
            vec![orphan.clone()]
        );
        assert!(!orphan.exists());
        assert!(active.exists());
        assert!(unrelated.exists());

        drop(active_lock);
        assert_eq!(
            service.reconcile_managed_repositories().unwrap(),
            vec![active.clone()]
        );
        assert!(!active.exists());
        assert!(unrelated.exists());
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_stops_clone_descendants_before_cleanup() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new();
        let remote = fixture.directory("slow-remote");
        initialize_repository(&remote);
        let activity = fixture.root.path().join("helper-activity");
        let fake_git = fixture.root.path().join("fake-git");
        fs::write(
            &fake_git,
            format!(
                "#!/bin/sh\n\
                 for argument do checkout=$argument; done\n\
                 test \"$GIT_TERMINAL_PROMPT\" = 0 || exit 97\n\
                 mkdir -p \"$checkout\"\n\
                 (while true; do printf x >> '{}'; sleep 0.01; done) 2>/dev/null &\n\
                 echo ready >&2\n\
                 wait\n",
                activity.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&fake_git).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fake_git, permissions).unwrap();

        let cancellation = CloneCancellation::default();
        let mut service = fixture.service();
        service.git_executable = fake_git;
        let error = service
            .import_repository(remote.to_str().unwrap(), &cancellation, |message| {
                if message == "ready" {
                    cancellation.cancel();
                }
            })
            .unwrap_err();

        assert!(matches!(error, ProjectError::CloneCancelled));
        assert!(service.list().unwrap().is_empty());
        let repositories = fixture.data_dir.join(REPOSITORIES_DIRECTORY);
        assert!(
            !repositories.exists() || fs::read_dir(repositories).unwrap().next().is_none(),
            "partial managed repository directory remained"
        );
        let activity_after_cancel = fs::read(&activity).unwrap();
        thread::sleep(Duration::from_millis(100));
        assert_eq!(
            fs::read(&activity).unwrap(),
            activity_after_cancel,
            "a clone helper survived cancellation"
        );
    }

    #[test]
    fn managed_removal_deletes_checkout_and_catalog_entry() {
        let fixture = Fixture::new();
        let remote = fixture.directory("remove-remote");
        initialize_repository(&remote);
        let mut service = fixture.service();
        let project = service
            .import_repository(
                remote.to_str().unwrap(),
                &CloneCancellation::default(),
                |_| {},
            )
            .unwrap();

        let removed = service.remove_managed(project.id).unwrap();

        assert_eq!(removed.id, project.id);
        assert!(!project.root.exists());
        assert!(service.list().unwrap().is_empty());
    }

    #[test]
    fn managed_removal_refuses_a_parent_with_worktrees() {
        let fixture = Fixture::new();
        let remote = fixture.directory("parent-with-worktree-remote");
        initialize_repository(&remote);
        let mut service = fixture.service();
        let parent = service
            .import_repository(
                remote.to_str().unwrap(),
                &CloneCancellation::default(),
                |_| {},
            )
            .unwrap();
        let worktree = catalogue_worktree(&mut service, parent.id, "agent/managed-worktree");

        let error = service.remove_managed(parent.id).unwrap_err();

        assert!(matches!(
            error,
            ProjectError::ParentHasWorktrees { id, worktrees, .. }
                if id == parent.id && worktrees == vec![worktree.root.clone()]
        ));
        assert!(parent.root.exists(), "the managed parent was deleted");
        assert!(worktree.root.exists());
        assert_eq!(service.list_catalog_only().unwrap().len(), 2);
    }

    #[test]
    fn managed_removal_rejects_local_and_unknown_paths() {
        let fixture = Fixture::new();
        let local = fixture.directory("local-project");
        let mut service = fixture.service();
        let project = service.import_local(&local).unwrap();
        assert!(matches!(
            service.remove_managed(project.id),
            Err(ProjectError::UnsafeManagedRemoval { .. })
        ));
        assert!(local.exists());

        let remote = fixture.directory("guard-remote");
        initialize_repository(&remote);
        let managed = service
            .import_repository(
                remote.to_str().unwrap(),
                &CloneCancellation::default(),
                |_| {},
            )
            .unwrap();
        let outside = fixture.directory("outside");
        // The guard reads the catalog from disk, so the tampered entry has to
        // be persisted rather than only held in memory.
        service
            .catalog
            .projects
            .iter_mut()
            .find(|project| project.id == managed.id)
            .unwrap()
            .root = outside.clone();
        let tampered = service.catalog.clone();
        service.persist(&tampered).unwrap();
        assert!(matches!(
            service.remove_managed(managed.id),
            Err(ProjectError::UnsafeManagedRemoval { .. })
        ));
        assert!(outside.exists());
        assert!(managed.root.exists());
    }

    #[cfg(unix)]
    #[test]
    fn managed_removal_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let remote = fixture.directory("symlink-remote");
        initialize_repository(&remote);
        let mut service = fixture.service();
        let managed = service
            .import_repository(
                remote.to_str().unwrap(),
                &CloneCancellation::default(),
                |_| {},
            )
            .unwrap();
        let outside = fixture.directory("symlink-outside");
        let managed_directory = managed.root.parent().unwrap();
        fs::remove_dir_all(&managed.root).unwrap();
        symlink(&outside, managed_directory.join(CHECKOUT_DIRECTORY)).unwrap();

        assert!(matches!(
            service.remove_managed(managed.id),
            Err(ProjectError::UnsafeManagedRemoval { .. })
        ));
        assert!(outside.exists());
    }

    /// A symlinked data directory is ordinary: `XDG_DATA_HOME` may point across
    /// volumes, and macOS resolves its temporary directories through
    /// `/private`. Comparing a canonical `Project::root` against a literal
    /// `data_dir` made every managed clone permanently undeletable there.
    #[cfg(unix)]
    #[test]
    fn managed_removal_succeeds_through_a_symlinked_data_directory() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let real_data_dir = fixture.directory("real-data");
        symlink(&real_data_dir, &fixture.data_dir).unwrap();
        let remote = fixture.directory("linked-remote");
        initialize_repository(&remote);
        let mut service = fixture.service();
        let managed = service
            .import_repository(
                remote.to_str().unwrap(),
                &CloneCancellation::default(),
                |_| {},
            )
            .unwrap();
        assert!(
            managed
                .root
                .starts_with(fs::canonicalize(&real_data_dir).unwrap())
        );

        service.remove_managed(managed.id).unwrap();

        assert!(!managed.root.exists());
        assert!(service.list().unwrap().is_empty());
    }

    #[test]
    fn reimport_reclones_a_checkout_deleted_outside_harkness() {
        let fixture = Fixture::new();
        let remote = fixture.directory("vanishing-remote");
        initialize_repository(&remote);
        let mut service = fixture.service();
        let imported = service
            .import_repository(
                remote.to_str().unwrap(),
                &CloneCancellation::default(),
                |_| {},
            )
            .unwrap();
        fs::remove_dir_all(imported.root.parent().unwrap()).unwrap();

        let recloned = service
            .import_repository(
                remote.to_str().unwrap(),
                &CloneCancellation::default(),
                |_| {},
            )
            .unwrap();

        assert_ne!(recloned.id, imported.id);
        assert!(recloned.available);
        assert!(recloned.root.join(".git").exists());
        assert_eq!(service.list().unwrap().len(), 1);
    }

    #[test]
    fn reimport_cannot_replace_an_unavailable_parent_with_catalogued_worktrees() {
        let fixture = Fixture::new();
        let remote = fixture.directory("unavailable-parent-remote");
        initialize_repository(&remote);
        let mut service = fixture.service();
        let parent = service
            .import_repository(
                remote.to_str().unwrap(),
                &CloneCancellation::default(),
                |_| {},
            )
            .unwrap();
        let worktree = catalogue_worktree(&mut service, parent.id, "agent/kept-orphan");
        fs::remove_dir_all(&parent.root).unwrap();
        #[cfg(unix)]
        let clone_started = {
            let clone_started = fixture.root.path().join("replacement-clone-started");
            service.git_executable = fixture.shim(
                "record-replacement-clone",
                &format!(
                    "#!/bin/sh\ntouch '{}'\nexec git \"$@\"\n",
                    clone_started.display()
                ),
            );
            clone_started
        };

        let error = service
            .import_repository(
                remote.to_str().unwrap(),
                &CloneCancellation::default(),
                |_| {},
            )
            .unwrap_err();

        assert!(matches!(
            error,
            ProjectError::ParentHasWorktrees { id, worktrees, .. }
                if id == parent.id && worktrees == vec![worktree.root.clone()]
        ));
        let ids = service
            .list_catalog_only()
            .unwrap()
            .into_iter()
            .map(|project| project.id)
            .collect::<Vec<_>>();
        assert!(ids.contains(&parent.id));
        assert!(ids.contains(&worktree.id));
        assert_eq!(ids.len(), 2);
        assert_eq!(
            fs::read_dir(fixture.data_dir.join(REPOSITORIES_DIRECTORY))
                .unwrap()
                .count(),
            1,
            "the failed replacement clone was not cleaned up"
        );
        #[cfg(unix)]
        assert!(
            !clone_started.exists(),
            "a doomed replacement clone was started"
        );
    }

    #[test]
    fn remotes_are_trimmed_before_reaching_git() {
        let fixture = Fixture::new();
        let remote = fixture.directory("padded-remote");
        initialize_repository(&remote);
        let mut service = fixture.service();

        let imported = service
            .import_repository(
                &format!("  {}\n", remote.display()),
                &CloneCancellation::default(),
                |_| {},
            )
            .unwrap();

        assert!(imported.root.join(".git").exists());
    }

    /// Availability and Git state are recomputed for every entry on every read,
    /// so a listing that spawned Git would cost one process per project.
    #[cfg(unix)]
    #[test]
    fn listing_spawns_no_git_process() {
        let fixture = Fixture::new();
        let project_root = fixture.directory("listed-repository");
        initialize_repository(&project_root);
        let sentinel = fixture.root.path().join("git-was-spawned");
        let recording_git = fixture.shim(
            "recording-git",
            &format!("#!/bin/sh\ntouch '{}'\n", sentinel.display()),
        );
        let mut service = fixture.service();
        service.git_executable = recording_git;
        service.import_local(&project_root).unwrap();

        let listed = service.list().unwrap();

        assert_eq!(listed.len(), 1);
        assert!(listed[0].git.is_some(), "the listing reported Git state");
        assert!(!sentinel.exists(), "the listing spawned the Git executable");
    }

    /// Set through `Command::env` on a re-executed child rather than
    /// `std::env::set_var`, which is unsound in a multithreaded test binary
    /// under Rust 2024.
    #[cfg(unix)]
    #[test]
    fn a_redirected_parent_environment_does_not_reach_git() {
        let fixture = Fixture::new();
        let working_directory = fixture.directory("scrubbed");
        let elsewhere = fixture.directory("elsewhere");
        let reported = SCRUBBED_ENVIRONMENT
            .iter()
            .copied()
            .chain([
                "GIT_TERMINAL_PROMPT",
                "GIT_OPTIONAL_LOCKS",
                "LC_ALL",
                "GIT_EDITOR",
            ])
            .collect::<Vec<_>>()
            .join(" ");
        let reporting_git = fixture.shim(
            "reporting-git",
            &format!(
                "#!/bin/sh\n\
                 for name in {reported}; do\n\
                 \x20 eval \"value=\\${{$name-unset}}\"\n\
                 \x20 printf '%s=%s\\n' \"$name\" \"$value\"\n\
                 done\n"
            ),
        );

        let mut child = spawn_child(&fixture.data_dir, "scrubbed-environment");
        child
            .env(PROCESS_PROJECT_ROOT_ENV, &working_directory)
            .env(PROCESS_GIT_EXECUTABLE_ENV, &reporting_git);
        // Every scrubbed name is exported with a value that would visibly
        // change what Git did, so the child asserting `unset` asserts something.
        for name in SCRUBBED_ENVIRONMENT {
            child.env(name, elsewhere.join(name.to_ascii_lowercase()));
        }
        let mut child = child.spawn().unwrap();

        assert!(child.wait().unwrap().success());
    }

    /// The repository lock is taken before the catalog lock, so a removal
    /// blocked by another operation refuses rather than waiting behind it.
    #[test]
    fn managed_removal_refuses_while_another_operation_holds_the_repository() {
        let fixture = Fixture::new();
        let remote = fixture.directory("busy-remote");
        initialize_repository(&remote);
        let mut service = fixture.service();
        let managed = service
            .import_repository(
                remote.to_str().unwrap(),
                &CloneCancellation::default(),
                |_| {},
            )
            .unwrap();
        let ready_file = fixture.root.path().join("managed-lock-held");
        let mut holder = spawn_child(&fixture.data_dir, "hold-repository-lock")
            .env(PROCESS_PROJECT_ROOT_ENV, &managed.root)
            .env(PROCESS_READY_FILE_ENV, &ready_file)
            .spawn()
            .unwrap();
        wait_for_child_signal(&mut holder, &ready_file);

        let error = service.remove_managed(managed.id).unwrap_err();

        holder.kill().unwrap();
        holder.wait().unwrap();
        assert!(
            matches!(error, ProjectError::Git(GitError::RepositoryBusy { .. })),
            "{error}"
        );
        assert!(managed.root.exists(), "the checkout was deleted anyway");
        assert_eq!(service.list().unwrap().len(), 1);
    }
}
