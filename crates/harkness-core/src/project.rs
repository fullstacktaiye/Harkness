use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
};

use thiserror::Error;
use time::OffsetDateTime;

use crate::{
    catalog::{
        self, Catalog,
        entry::{GitStatus, Project, ProjectId, ProjectSource},
        lock,
    },
    git::{self, Cancellation, GitAccess, GitCommand, GitError, GitService, clone},
    paths::{
        self, CATALOG_FILE, CHECKOUT_DIRECTORY, REPOSITORIES_DIRECTORY, UnreservedPath,
        WORKTREES_DIRECTORY, canonical_reserved_root,
    },
    remote::{normalize_remote_with_local, repository_name},
};

/// Actionable failures returned by [`ProjectService`].
#[derive(Debug, Error)]
#[non_exhaustive]
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

    /// A project cannot be removed while its worktrees remain catalogued.
    #[error("project {id} still has catalogued worktrees; remove them first: {display}")]
    ParentHasWorktrees {
        id: ProjectId,
        worktrees: Vec<PathBuf>,
        display: String,
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

    /// A Git operation on a catalogued project failed.
    #[error(transparent)]
    Git(GitError),
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

    /// Reads the catalog under a shared lock, or `None` if it cannot be read.
    fn read_catalog_shared(&self) -> Option<Catalog> {
        lock::read_catalog_shared(&self.data_dir)
    }

    /// Lists projects by most-recently-opened order with current availability
    /// and Git metadata.
    ///
    /// Listing never fails: a project whose Git metadata cannot be read is
    /// reported with `git: None` rather than hiding the rest of the catalog,
    /// and a catalog that is briefly unreadable falls back to the snapshot
    /// taken at load rather than reporting an empty Recents.
    #[must_use]
    pub fn list(&self) -> Vec<Project> {
        // Git and availability checks touch the filesystem once per project,
        // so they run after the shared lock is released.
        let catalog = self
            .read_catalog_shared()
            .unwrap_or_else(|| self.catalog.clone());
        let mut projects = catalog
            .projects
            .iter()
            .cloned()
            .map(refresh_project)
            .collect::<Vec<_>>();
        sort_recents(&mut projects);
        projects
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
    #[must_use]
    pub fn list_catalog_only(&self) -> Vec<Project> {
        let catalog = self
            .read_catalog_shared()
            .unwrap_or_else(|| self.catalog.clone());
        // The in-memory fallback carries the derived state of the last write,
        // so the defaults are asserted rather than assumed.
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
        projects
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
                // Otherwise the checkout was deleted outside Harkness.
                // Reporting a successful import of a path that no longer
                // exists would strand the user, so the clone is repeated and
                // the stale entry replaced once the fresh checkout exists.
            }
        }

        let id = ProjectId::new();
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
            .read_catalog_shared()
            .unwrap_or_else(|| self.catalog.clone())
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
            RemovalPolicy::ManagedRepository { remote } => {
                debug_assert!(
                    !remote.trim().is_empty(),
                    "catalog validation rejected this"
                )
            }
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

    /// Removes a Harkness-managed worktree through Git, then drops its catalog
    /// entry. A dirty worktree is refused by Git; this operation never forces
    /// deletion of uncommitted files.
    ///
    /// The repository lock is acquired through the parent before the catalog
    /// lock. The parent and worktree relationship is then re-verified under the
    /// catalog lock so a future creation path cannot race this removal.
    pub fn remove_worktree(&mut self, id: ProjectId) -> Result<Project, ProjectError> {
        let preliminary_catalog = self
            .read_catalog_shared()
            .unwrap_or_else(|| self.catalog.clone());
        let preliminary = preliminary_catalog
            .projects
            .iter()
            .find(|project| project.id == id)
            .cloned()
            .ok_or(ProjectError::ProjectNotFound(id))?;
        let preliminary_parent_id = match RemovalPolicy::from(&preliminary.source) {
            RemovalPolicy::Worktree { parent } => parent,
            RemovalPolicy::CatalogOnly | RemovalPolicy::ManagedRepository { .. } => {
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
        let repository_lock = self
            .git_service(&preliminary_parent.root)
            .lock(&Cancellation::default())?;

        let _catalog_lock = self.lock_exclusive()?;
        let mut candidate = self.read_catalog()?;
        let project = candidate
            .projects
            .iter()
            .find(|project| project.id == id)
            .cloned()
            .ok_or(ProjectError::ProjectNotFound(id))?;
        let parent_id = match RemovalPolicy::from(&project.source) {
            RemovalPolicy::Worktree { parent } => parent,
            RemovalPolicy::CatalogOnly | RemovalPolicy::ManagedRepository { .. } => {
                return Err(unsafe_worktree_removal(
                    &project,
                    "catalog entry is not a managed worktree",
                ));
            }
        };
        if parent_id != preliminary_parent_id {
            return Err(unsafe_worktree_removal(
                &project,
                "worktree parent changed while the repository lock was being acquired",
            ));
        }
        let parent = candidate
            .projects
            .iter()
            .find(|entry| entry.id == parent_id)
            .ok_or_else(|| {
                unsafe_worktree_removal(&project, "catalogued parent no longer exists")
            })?;
        if parent.root != preliminary_parent.root {
            return Err(unsafe_worktree_removal(
                &project,
                "parent path changed while the repository lock was being acquired",
            ));
        }
        refuse_parent_with_worktrees(&candidate, id)?;

        canonical_reserved_root(
            &self.data_dir.join(WORKTREES_DIRECTORY),
            &PathBuf::from(id.to_string()),
            &project.root,
        )
        .map_err(|unreserved| {
            unsafe_worktree_removal(
                &project,
                match unreserved {
                    UnreservedPath::StorageRootUnavailable => {
                        "managed worktrees root is unavailable"
                    }
                    UnreservedPath::CandidateUnavailable => "worktree checkout is unavailable",
                    UnreservedPath::Mismatch => {
                        "checkout is not the managed path reserved for this worktree"
                    }
                },
            )
        })?;

        GitCommand::new(
            &self.git_executable,
            &preliminary_parent.root,
            GitAccess::LocalWrite,
        )
        .with_repository_lock(repository_lock)
        .args(["worktree", "remove", "--"])
        .arg(&project.root)
        .run(&Cancellation::default())?;

        candidate.projects.retain(|entry| entry.id != id);
        self.persist(&candidate)?;
        self.catalog = candidate;
        Ok(project)
    }

    /// Returns the reserved future worktree location without creating it.
    ///
    /// A future creation verb must acquire the repository lock before the
    /// exclusive catalog lock and re-check that the parent still exists while
    /// holding that catalog lock before it writes a worktree entry.
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
        let catalog = self
            .read_catalog_shared()
            .unwrap_or_else(|| self.catalog.clone());
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemovalPolicy<'a> {
    CatalogOnly,
    ManagedRepository { remote: &'a str },
    Worktree { parent: ProjectId },
}

impl<'a> From<&'a ProjectSource> for RemovalPolicy<'a> {
    fn from(source: &'a ProjectSource) -> Self {
        match source {
            ProjectSource::Local => Self::CatalogOnly,
            ProjectSource::ManagedRepository { remote } => Self::ManagedRepository { remote },
            ProjectSource::Worktree { parent, .. } => Self::Worktree { parent: *parent },
        }
    }
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
        let display = worktrees
            .iter()
            .map(|path| format!("'{}'", path.display()))
            .collect::<Vec<_>>()
            .join(", ");
        Err(ProjectError::ParentHasWorktrees {
            id,
            worktrees,
            display,
        })
    }
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
        fs,
        path::{Path, PathBuf},
        thread,
        time::Duration,
    };

    use super::{ProjectError, ProjectService, sort_recents};
    use crate::{
        catalog::{
            CATALOG_VERSION, MINIMUM_SUPPORTED_CATALOG_VERSION,
            entry::{Project, ProjectId, ProjectSource},
        },
        git::{CloneCancellation, GitError},
        paths::{
            CATALOG_FILE, CATALOG_LOCK_FILE, CHECKOUT_DIRECTORY, DATA_DIRECTORY_ENV,
            REPOSITORIES_DIRECTORY, WORKTREES_DIRECTORY,
        },
        testing::{
            Fixture, PROCESS_GIT_EXECUTABLE_ENV, PROCESS_PROJECT_ROOT_ENV, PROCESS_READY_FILE_ENV,
            SCRUBBED_ENVIRONMENT, commit_all, git, initialize_repository, spawn_child,
            wait_for_child_signal,
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
                worktree_branch: branch.to_owned(),
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

        assert_eq!(reloaded.catalog.version, CATALOG_VERSION);
        assert_eq!(reloaded.list(), service.list());
        assert_eq!(reloaded.list(), vec![imported]);
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
        assert_eq!(service.catalog.version, CATALOG_VERSION);
        // If the shared read fails, both listing methods fall back to this
        // snapshot. Clearing it makes the test prove the file was actually
        // opened under projects.lock.
        service.catalog.projects.clear();
        let stored = service.list_catalog_only();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].source, ProjectSource::Local);
        assert!(matches!(
            &stored[1].source,
            ProjectSource::ManagedRepository { remote }
                if remote == "github.com/example/version-one"
        ));
        assert_eq!(service.list().len(), 2);
        assert_eq!(fs::read(&catalog_path).unwrap(), fixture_bytes);
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
        let originals = service.list_catalog_only();

        let new_root = fixture.directory("version-one-compatible-mutation");
        service.import_local(new_root).unwrap();

        let persisted = fs::read(&catalog_path).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&persisted).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["projects"].as_array().unwrap().len(), 3);
        let after_import = service.list_catalog_only();
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
            .into_iter()
            .find(|project| project.id == worktree_id)
            .unwrap();

        assert!(!stored.available);
        assert!(matches!(
            stored.source,
            ProjectSource::Worktree { parent, ref worktree_branch }
                if parent.to_string() == "00000000-0000-4000-8000-000000000001"
                    && worktree_branch == "agent/catalog-v2"
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
        let projects = fixture.service().list();
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
        assert_eq!(reloaded.list().len(), WRITERS);
        for root in &roots {
            let stored = as_catalogued(root);
            assert!(reloaded.list().iter().any(|project| project.root == stored));
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

        let projects = fixture.service().list();
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
        assert_eq!(reloaded.list(), vec![seeded]);
        let imported_after_kill = reloaded.import_local(imported_after_kill_root).unwrap();
        let projects = ProjectService::load_from_data_dir(&fixture.data_dir)
            .unwrap()
            .list();
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

        let listed = service.list_catalog_only();

        // The root exists and is a repository, so defaults here can only mean
        // the listing never looked at it.
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, imported.id);
        assert_eq!(listed[0].root, imported.root);
        assert_eq!(listed[0].display_name, imported.display_name);
        assert!(!listed[0].available);
        assert_eq!(listed[0].git, None);
        assert_eq!(service.list(), vec![imported]);
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
        assert_eq!(service.list().len(), 1);
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

        assert_eq!(service.list()[0].id, second.id);

        thread::sleep(CLOCK_TICK);
        let reopened = service.open(first.id).unwrap();
        let recents = service.list();

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

        let listed = service.list();
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].available);
        assert_eq!(listed[0].id, imported.id);
        assert!(matches!(
            service.open(imported.id),
            Err(ProjectError::ProjectUnavailable { id, .. }) if id == imported.id
        ));

        let reloaded = ProjectService::load_from_data_dir(&fixture.data_dir).unwrap();
        assert!(!reloaded.list()[0].available);
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
        assert!(service.list().is_empty());
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
        assert_eq!(service.list_catalog_only().len(), 3);
        assert!(matches!(
            service.remove(first.id),
            Err(ProjectError::WorktreeRemovalRequired { id, .. }) if id == first.id
        ));
        assert_eq!(service.list_catalog_only().len(), 3);
    }

    #[test]
    fn worktree_removal_uses_git_cleans_the_catalog_and_restores_version_one() {
        let fixture = Fixture::new();
        let parent_root = fixture.directory("remove-worktree-parent");
        initialize_repository(&parent_root);
        let mut service = fixture.service();
        let parent = service.import_local(&parent_root).unwrap();
        let worktree = catalogue_git_worktree(&mut service, &parent, "agent/remove-clean-worktree");
        assert!(
            git(&parent.root, ["worktree", "list", "--porcelain"])
                .contains(&worktree.root.display().to_string())
        );

        let removed = service.remove_worktree(worktree.id).unwrap();

        assert_eq!(removed.id, worktree.id);
        assert!(!worktree.root.exists());
        assert!(
            !git(&parent.root, ["worktree", "list", "--porcelain"])
                .contains(&worktree.root.display().to_string())
        );
        let remaining = service.list_catalog_only();
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

        let error = service.remove_worktree(worktree.id).unwrap_err();

        assert!(matches!(error, ProjectError::Git(GitError::Failed { .. })));
        assert!(worktree.root.join("untracked.txt").exists());
        assert!(
            service
                .list_catalog_only()
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
    fn an_acyclic_worktree_parent_chain_is_valid() {
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

        assert_eq!(
            ProjectService::load_from_data_dir(&fixture.data_dir)
                .unwrap()
                .list_catalog_only()
                .len(),
            3
        );
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
        assert_eq!(service.list().len(), 1);
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
        let catalogued = fixture.service().list();
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
        assert_eq!(service.list().len(), 1);
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
        assert!(service.list().is_empty());
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
        assert!(service.list().is_empty());

        let remote = fixture.directory("cancel-remote");
        initialize_repository(&remote);
        let cancellation = CloneCancellation::default();
        cancellation.cancel();
        assert!(matches!(
            service.import_repository(remote.to_str().unwrap(), &cancellation, |_| {}),
            Err(ProjectError::CloneCancelled)
        ));
        assert!(service.list().is_empty());
        let repositories = fixture.data_dir.join(REPOSITORIES_DIRECTORY);
        assert!(
            !repositories.exists() || fs::read_dir(repositories).unwrap().next().is_none(),
            "partial managed repository directory remained"
        );
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
        assert!(service.list().is_empty());
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
        assert!(service.list().is_empty());
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
        assert_eq!(service.list_catalog_only().len(), 2);
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
        assert!(service.list().is_empty());
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
        assert_eq!(service.list().len(), 1);
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

        let listed = service.list();

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
        assert_eq!(service.list().len(), 1);
    }
}
