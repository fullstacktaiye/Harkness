use std::{
    collections::VecDeque,
    fs,
    io::{self, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use git2::{ErrorCode, Repository, StatusOptions};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

const CATALOG_VERSION: u32 = 1;
const CATALOG_FILE: &str = "projects.json";
const WORKTREES_DIRECTORY: &str = "worktrees";
const REPOSITORIES_DIRECTORY: &str = "repositories";
const CHECKOUT_DIRECTORY: &str = "checkout";

/// Git repeats a progress phase on every update, so retaining the whole stream
/// would put megabytes of overwritten counters into a failure message. The tail
/// is what matters: Git prints its diagnosis last.
const RETAINED_GIT_OUTPUT_SEGMENTS: usize = 20;

/// A stable identifier for a project catalog entry.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ProjectId(Uuid);

impl ProjectId {
    /// Generates a new random project identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ProjectId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ProjectId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::str::FromStr for ProjectId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

/// Describes how a project entered the catalog.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectSource {
    /// A directory that already exists on the local machine.
    Local,
    /// A clone owned by Harkness and stored below its repositories directory.
    ManagedRepository,
}

/// Git information collected from a project directory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitStatus {
    /// The checked-out branch, or `None` for a detached head.
    pub branch: Option<String>,
    /// Whether the worktree contains tracked or untracked changes.
    pub dirty: bool,
}

/// A durable project catalog entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Project {
    /// Stable catalog identifier.
    pub id: ProjectId,
    /// Human-readable name derived from the imported directory.
    pub display_name: String,
    /// Canonical path to the project directory.
    pub root: PathBuf,
    /// How this project entered the catalog.
    pub source: ProjectSource,
    /// Canonical GitHub remote identity for managed repositories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// The last time the project was imported or successfully reopened.
    pub last_opened: OffsetDateTime,
    /// Whether the root currently exists and can be read.
    ///
    /// Derived from the filesystem on every read, so it is deliberately not
    /// persisted; a stored copy would only ever be stale.
    #[serde(skip)]
    pub available: bool,
    /// Git metadata when the root is the working directory of a Git
    /// repository.
    ///
    /// Derived alongside [`Project::available`], and likewise not persisted.
    #[serde(skip)]
    pub git: Option<GitStatus>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Catalog {
    version: u32,
    projects: Vec<Project>,
}

/// The forward-compatible prefix of every catalog file.
#[derive(Deserialize)]
struct CatalogVersion {
    version: u32,
}

impl Default for Catalog {
    fn default() -> Self {
        Self {
            version: CATALOG_VERSION,
            projects: Vec::new(),
        }
    }
}

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

    /// The catalog contains invalid JSON or does not match the catalog schema.
    #[error("project catalog '{}' is malformed: {source}", path.display())]
    MalformedCatalog {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// The catalog was written by an unsupported schema version.
    #[error("project catalog version {found} is unsupported (expected {expected})")]
    UnsupportedCatalogVersion { found: u32, expected: u32 },

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
    #[error("invalid Git repository remote '{remote}'")]
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
}

/// Cooperative cancellation token for a repository clone.
#[derive(Clone, Debug, Default)]
pub struct CloneCancellation(Arc<AtomicBool>);

impl CloneCancellation {
    /// Requests cancellation. The Git child is killed and its partial clone is removed.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Loads and updates the durable local project catalog.
///
/// A service instance owns a snapshot of the catalog taken when it was loaded,
/// and every mutation rewrites the whole file. Harkness therefore assumes a
/// single writer at a time: two processes holding their own service would each
/// persist their own snapshot, and the last write would win. Front ends that
/// need to share a catalog should serialize access or reload between changes.
pub struct ProjectService {
    data_dir: PathBuf,
    catalog: Catalog,
}

impl ProjectService {
    /// Loads the catalog from the platform user data directory.
    pub fn load() -> Result<Self, ProjectError> {
        let data_dir = dirs::data_dir()
            .ok_or(ProjectError::DataDirectoryUnavailable)?
            .join("harkness");
        Self::load_from_data_dir(data_dir)
    }

    /// Loads a catalog rooted at an explicit Harkness data directory.
    ///
    /// This constructor is useful for isolated applications and tests. The
    /// catalog file is always named `projects.json` within `data_dir`.
    pub fn load_from_data_dir(data_dir: impl Into<PathBuf>) -> Result<Self, ProjectError> {
        let data_dir = data_dir.into();
        let catalog_path = data_dir.join(CATALOG_FILE);
        let catalog = match catalog_path.try_exists() {
            Ok(false) => Catalog::default(),
            Ok(true) => {
                let bytes =
                    fs::read(&catalog_path).map_err(|source| ProjectError::CatalogRead {
                        path: catalog_path.clone(),
                        source,
                    })?;
                // Read the version before the body: a future schema would fail
                // to deserialize as a v1 catalog, and reporting that as
                // "malformed" would hide the one cause the user can act on.
                let probe: CatalogVersion = serde_json::from_slice(&bytes).map_err(|source| {
                    ProjectError::MalformedCatalog {
                        path: catalog_path.clone(),
                        source,
                    }
                })?;
                if probe.version != CATALOG_VERSION {
                    return Err(ProjectError::UnsupportedCatalogVersion {
                        found: probe.version,
                        expected: CATALOG_VERSION,
                    });
                }

                serde_json::from_slice(&bytes).map_err(|source| ProjectError::MalformedCatalog {
                    path: catalog_path.clone(),
                    source,
                })?
            }
            Err(source) => {
                return Err(ProjectError::CatalogRead {
                    path: catalog_path,
                    source,
                });
            }
        };

        Ok(Self { data_dir, catalog })
    }

    /// Lists projects by most-recently-opened order with current availability
    /// and Git metadata.
    ///
    /// Listing never fails: a project whose Git metadata cannot be read is
    /// reported with `git: None` rather than hiding the rest of the catalog.
    #[must_use]
    pub fn list(&self) -> Vec<Project> {
        let mut projects = self
            .catalog
            .projects
            .iter()
            .cloned()
            .map(refresh_project)
            .collect::<Vec<_>>();
        sort_recents(&mut projects);
        projects
    }

    /// Imports a readable local directory, or reopens its existing canonical
    /// catalog entry when it was already imported.
    pub fn import_local(&mut self, path: impl AsRef<Path>) -> Result<Project, ProjectError> {
        let canonical_root = validate_local_directory(path.as_ref())?;
        let mut candidate = self.catalog.clone();

        let project = if let Some(project) = candidate
            .projects
            .iter_mut()
            .find(|project| project.root == canonical_root)
        {
            project.available = true;
            project.last_opened = OffsetDateTime::now_utc();
            project.git = inspect_git(&canonical_root)?;
            project.clone()
        } else {
            let project = Project {
                id: ProjectId::new(),
                display_name: display_name(&canonical_root),
                root: canonical_root.clone(),
                source: ProjectSource::Local,
                remote: None,
                available: true,
                last_opened: OffsetDateTime::now_utc(),
                git: inspect_git(&canonical_root)?,
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
        let mut candidate = self.catalog.clone();
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
        let mut candidate = self.catalog.clone();
        let index = candidate
            .projects
            .iter()
            .position(|project| project.id == id)
            .ok_or(ProjectError::ProjectNotFound(id))?;
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
        cancellation: &CloneCancellation,
        mut on_progress: impl FnMut(String),
    ) -> Result<Project, ProjectError> {
        // Git reads its argument literally, so surrounding whitespace from a
        // pasted URL would reach it as part of the protocol name.
        let remote = remote.trim();
        let normalized = normalize_remote(remote)?;
        let mut candidate = self.catalog.clone();
        if let Some(index) = candidate
            .projects
            .iter()
            .position(|project| project.remote.as_deref() == Some(&normalized))
        {
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
            // The checkout was deleted outside Harkness. Reporting a
            // successful import of a path that no longer exists would strand
            // the user, so the stale entry is dropped and the clone repeated.
            candidate.projects.remove(index);
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
            run_git_clone(remote, &checkout, cancellation, &mut on_progress)?;
            let canonical_root = validate_local_directory(&checkout)?;
            let project = Project {
                id,
                display_name: repository_name(&normalized),
                root: canonical_root.clone(),
                source: ProjectSource::ManagedRepository,
                remote: Some(normalized),
                last_opened: OffsetDateTime::now_utc(),
                available: true,
                git: inspect_git(&canonical_root)?,
            };
            candidate.projects.push(project.clone());
            sort_recents(&mut candidate.projects);
            self.persist(&candidate)?;
            Ok(project)
        })();

        match imported {
            Ok(project) => {
                self.catalog = candidate;
                Ok(project)
            }
            Err(error) => {
                let _ = fs::remove_dir_all(&managed_directory);
                Err(error)
            }
        }
    }

    /// Deletes a checkout only after proving it is the managed path for `id`.
    ///
    /// Front ends must obtain explicit confirmation naming [`Project::root`]
    /// before calling this destructive operation.
    pub fn remove_managed(&mut self, id: ProjectId) -> Result<Project, ProjectError> {
        let project = self
            .catalog
            .projects
            .iter()
            .find(|project| project.id == id)
            .cloned()
            .ok_or(ProjectError::ProjectNotFound(id))?;
        if project.source != ProjectSource::ManagedRepository || project.remote.is_none() {
            return Err(unsafe_removal(
                &project,
                "catalog entry is not a managed clone",
            ));
        }

        // Both sides must be canonical. `Project::root` was canonicalized at
        // import, so a symlink anywhere above the data directory would make a
        // literal comparison against `data_dir` fail for every managed clone.
        // Equality also subsumes a containment check: a checkout that resolves
        // outside managed storage, or through a symlink, cannot match.
        let repositories_root = fs::canonicalize(self.data_dir.join(REPOSITORIES_DIRECTORY))
            .map_err(|_| unsafe_removal(&project, "managed repositories root is unavailable"))?;
        let managed_directory = repositories_root.join(id.to_string());
        let canonical_checkout = fs::canonicalize(&project.root)
            .map_err(|_| unsafe_removal(&project, "checkout is unavailable"))?;
        if canonical_checkout != managed_directory.join(CHECKOUT_DIRECTORY) {
            return Err(unsafe_removal(
                &project,
                "checkout is not the managed path reserved for this project",
            ));
        }

        fs::remove_dir_all(&managed_directory).map_err(|source| ProjectError::ManagedRemoval {
            path: managed_directory,
            source,
        })?;
        let mut candidate = self.catalog.clone();
        candidate.projects.retain(|entry| entry.id != id);
        self.persist(&candidate)?;
        self.catalog = candidate;
        Ok(project)
    }

    /// Returns the reserved future worktree location without creating it.
    #[must_use]
    pub fn worktree_path(&self, id: ProjectId) -> PathBuf {
        self.data_dir.join(WORKTREES_DIRECTORY).join(id.to_string())
    }

    fn persist(&self, catalog: &Catalog) -> Result<(), ProjectError> {
        let catalog_path = self.data_dir.join(CATALOG_FILE);
        persist_catalog(&self.data_dir, &catalog_path, catalog).map_err(|source| {
            ProjectError::Persistence {
                path: catalog_path,
                source,
            }
        })
    }
}

fn unsafe_removal(project: &Project, reason: impl Into<String>) -> ProjectError {
    ProjectError::UnsafeManagedRemoval {
        id: project.id,
        path: project.root.clone(),
        reason: reason.into(),
    }
}

fn run_git_clone(
    remote: &str,
    checkout: &Path,
    cancellation: &CloneCancellation,
    on_progress: &mut impl FnMut(String),
) -> Result<(), ProjectError> {
    let mut child = Command::new("git")
        .args(["clone", "--progress", "--", remote])
        .arg(checkout)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| ProjectError::GitLaunch { source })?;
    let stderr = child.stderr.take().expect("piped stderr");
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || read_git_output(stderr, &sender));

    loop {
        while let Ok(message) = receiver.try_recv() {
            on_progress(message);
        }
        if cancellation.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err(ProjectError::CloneCancelled);
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|source| ProjectError::GitLaunch { source })?
        {
            let stderr = reader
                .join()
                .unwrap_or_else(|_| "Git output reader failed".to_owned());
            while let Ok(message) = receiver.try_recv() {
                on_progress(message);
            }
            return if status.success() {
                Ok(())
            } else {
                Err(ProjectError::CloneFailed { stderr })
            };
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Forwards Git's standard-error segments and returns the retained tail.
///
/// Git separates the updates within a progress phase with carriage returns and
/// only emits a newline when the phase ends, so reading lines would report
/// nothing for the whole of the slowest phase and then deliver every
/// overwritten counter at once. Both separators end a segment here.
fn read_git_output(stderr: impl Read, sender: &mpsc::Sender<String>) -> String {
    let mut reader = BufReader::new(stderr);
    let mut retained: VecDeque<String> = VecDeque::new();
    let mut segment = Vec::new();
    let mut buffer = [0u8; 4096];

    let end_segment = |segment: &mut Vec<u8>, retained: &mut VecDeque<String>| {
        if segment.is_empty() {
            return;
        }
        let message = String::from_utf8_lossy(segment).trim().to_owned();
        segment.clear();
        if message.is_empty() {
            return;
        }
        if retained.len() == RETAINED_GIT_OUTPUT_SEGMENTS {
            retained.pop_front();
        }
        retained.push_back(message.clone());
        let _ = sender.send(message);
    };

    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                for &byte in &buffer[..read] {
                    if byte == b'\n' || byte == b'\r' {
                        end_segment(&mut segment, &mut retained);
                    } else {
                        segment.push(byte);
                    }
                }
            }
            Err(error) => {
                segment.extend_from_slice(format!("failed to read Git output: {error}").as_bytes());
                break;
            }
        }
    }
    end_segment(&mut segment, &mut retained);
    Vec::from(retained).join("\n")
}

fn normalize_remote(remote: &str) -> Result<String, ProjectError> {
    let remote = remote.trim();
    let invalid = || ProjectError::InvalidRemote {
        remote: remote.to_owned(),
    };
    let local = |path: &str| {
        fs::canonicalize(path)
            .map(|path| format!("file://{}", path.display()))
            .map_err(|_| invalid())
    };

    let Some(path) = remote
        .strip_prefix("https://github.com/")
        .or_else(|| remote.strip_prefix("http://github.com/"))
        .or_else(|| remote.strip_prefix("ssh://git@github.com/"))
        .or_else(|| remote.strip_prefix("git@github.com:"))
    else {
        return local(remote.strip_prefix("file://").unwrap_or(remote));
    };

    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let (Some(owner), Some(repository)) = (
        parts.next().filter(|part| !part.is_empty()),
        parts.next().filter(|part| !part.is_empty()),
    ) else {
        return Err(invalid());
    };
    if parts.next().is_some() {
        return Err(invalid());
    }
    Ok(format!(
        "github.com/{}/{}",
        owner.to_ascii_lowercase(),
        repository.to_ascii_lowercase()
    ))
}

fn repository_name(normalized_remote: &str) -> String {
    normalized_remote
        .rsplit('/')
        .next()
        .unwrap_or(normalized_remote)
        .to_owned()
}

fn persist_catalog(data_dir: &Path, catalog_path: &Path, catalog: &Catalog) -> io::Result<()> {
    fs::create_dir_all(data_dir)?;
    let mut temporary = NamedTempFile::new_in(data_dir)?;
    serde_json::to_writer_pretty(&mut temporary, catalog).map_err(io::Error::other)?;
    temporary.write_all(b"\n")?;
    temporary.as_file_mut().sync_all()?;
    temporary
        .persist(catalog_path)
        .map_err(|error| error.error)?;

    // The file's contents are already durable; what the rename still needs is a
    // sync of the directory holding the new entry. Windows has no equivalent
    // handle to sync, so this is a Unix-only step.
    #[cfg(unix)]
    fs::File::open(data_dir)?.sync_all()?;

    Ok(())
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

fn inspect_git(path: &Path) -> Result<Option<GitStatus>, ProjectError> {
    let repository = match Repository::discover(path) {
        Ok(repository) => repository,
        Err(error) if error.code() == ErrorCode::NotFound => return Ok(None),
        Err(source) => {
            return Err(ProjectError::GitInspection {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    // `discover` walks upward, so a plain directory nested inside a repository
    // would otherwise report that ancestor's branch and dirty state as its
    // own. Only the repository's own working directory counts as a Git
    // project; bare repositories have no working directory at all.
    let is_repository_root = repository
        .workdir()
        .and_then(|workdir| fs::canonicalize(workdir).ok())
        .is_some_and(|workdir| workdir == path);
    if !is_repository_root {
        return Ok(None);
    }

    let branch = match repository.head() {
        Ok(head) if head.is_branch() => Some(
            head.shorthand()
                .map_err(|source| ProjectError::GitInspection {
                    path: path.to_path_buf(),
                    source,
                })?
                .to_owned(),
        ),
        Ok(_) => None,
        Err(error) if matches!(error.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => None,
        Err(source) => {
            return Err(ProjectError::GitInspection {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    // `dirty` only asks whether any entry differs, so untracked directories are
    // left unrecursed: libgit2 still reports the directory itself, at a
    // fraction of the cost of walking a large `target/` or `node_modules/`.
    let mut options = StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(false)
        .include_ignored(false);
    let dirty = !repository
        .statuses(Some(&mut options))
        .map_err(|source| ProjectError::GitInspection {
            path: path.to_path_buf(),
            source,
        })?
        .is_empty();

    Ok(Some(GitStatus { branch, dirty }))
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
        sync::mpsc,
        thread,
        time::Duration,
    };

    use git2::{IndexAddOption, Repository, Signature, Time};
    use tempfile::TempDir;

    use super::{
        CATALOG_FILE, CATALOG_VERSION, CHECKOUT_DIRECTORY, CloneCancellation, ProjectError,
        ProjectService, ProjectSource, REPOSITORIES_DIRECTORY, WORKTREES_DIRECTORY,
    };

    /// Coarse enough for the ~15ms system clock granularity on Windows.
    const CLOCK_TICK: Duration = Duration::from_millis(25);

    /// Fixed so repository fixtures hash identically between runs.
    const COMMIT_EPOCH_SECONDS: i64 = 1_700_000_000;

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
    fn derived_state_is_not_persisted() {
        let fixture = Fixture::new();
        let project_root = fixture.directory("derived-state");
        let mut service = fixture.service();
        service.import_local(&project_root).unwrap();

        let stored = fs::read_to_string(fixture.data_dir.join(CATALOG_FILE)).unwrap();

        assert!(!stored.contains("available"), "{stored}");
        assert!(!stored.contains("git"), "{stored}");
        assert!(stored.contains("last_opened"), "{stored}");
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

        fs::write(&catalog_path, br#"{"version":99,"projects":[]}"#).unwrap();
        assert!(matches!(
            ProjectService::load_from_data_dir(&fixture.data_dir),
            Err(ProjectError::UnsupportedCatalogVersion {
                found: 99,
                expected: CATALOG_VERSION
            })
        ));

        // A newer version is reported as such even when the rest of the file no
        // longer matches the current schema.
        fs::write(
            &catalog_path,
            br#"{"version":99,"entries":{"unexpected":1}}"#,
        )
        .unwrap();
        assert!(matches!(
            ProjectService::load_from_data_dir(&fixture.data_dir),
            Err(ProjectError::UnsupportedCatalogVersion { found: 99, .. })
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

    #[test]
    fn failed_persistence_does_not_change_in_memory_state() {
        let fixture = Fixture::new();
        let project_root = fixture.directory("persistence-project");
        fs::create_dir(&fixture.data_dir).unwrap();
        let mut service = fixture.service();
        fs::remove_dir(&fixture.data_dir).unwrap();
        fs::write(&fixture.data_dir, "blocks directory recreation").unwrap();

        assert!(matches!(
            service.import_local(project_root),
            Err(ProjectError::Persistence { .. })
        ));
        assert!(service.catalog.projects.is_empty());
    }

    #[test]
    fn persistence_atomically_replaces_the_catalog_without_artifacts() {
        let fixture = Fixture::new();
        let first = fixture.directory("atomic-first");
        let second = fixture.directory("atomic-second");
        let mut service = fixture.service();
        service.import_local(first).unwrap();
        service.import_local(second).unwrap();

        let entries = fs::read_dir(&fixture.data_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![CATALOG_FILE]);
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
        assert_eq!(imported.source, ProjectSource::ManagedRepository);
        assert!(imported.remote.as_deref().unwrap().starts_with("file://"));
        assert_eq!(
            imported.git.as_ref().unwrap().branch.as_deref(),
            Some("main")
        );
        assert_eq!(
            imported.root,
            fixture
                .data_dir
                .join(REPOSITORIES_DIRECTORY)
                .join(imported.id.to_string())
                .join(CHECKOUT_DIRECTORY)
        );
    }

    #[test]
    fn github_https_and_ssh_remotes_share_a_normalized_identity() {
        let expected = "github.com/example/project";
        assert_eq!(
            super::normalize_remote("https://github.com/Example/Project.git").unwrap(),
            expected
        );
        assert_eq!(
            super::normalize_remote("git@github.com:example/project.git").unwrap(),
            expected
        );
        assert_eq!(
            super::normalize_remote("ssh://git@github.com/EXAMPLE/PROJECT/").unwrap(),
            expected
        );
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
        service
            .catalog
            .projects
            .iter_mut()
            .find(|project| project.id == managed.id)
            .unwrap()
            .root = outside.clone();
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

    /// Git overwrites a progress phase with carriage returns and only emits a
    /// newline when the phase ends, so line-oriented reads report nothing for
    /// the whole of the slowest phase.
    #[test]
    fn carriage_returns_end_a_progress_segment() {
        let (sender, receiver) = mpsc::channel();
        let output = "Cloning into 'x'...\nReceiving objects:  50% (1/2)\rReceiving objects: 100% (2/2), done.\n";

        let retained = super::read_git_output(io::Cursor::new(output), &sender);
        drop(sender);

        assert_eq!(
            receiver.iter().collect::<Vec<_>>(),
            [
                "Cloning into 'x'...",
                "Receiving objects:  50% (1/2)",
                "Receiving objects: 100% (2/2), done.",
            ]
        );
        assert!(retained.ends_with("Receiving objects: 100% (2/2), done."));
    }

    #[test]
    fn retained_git_output_keeps_only_the_diagnostic_tail() {
        let (sender, receiver) = mpsc::channel();
        let mut output = (0..500).fold(String::new(), |mut output, index| {
            output.push_str(&format!("Receiving objects: {index}%\r"));
            output
        });
        output.push_str("fatal: repository not found\n");

        let retained = super::read_git_output(io::Cursor::new(output), &sender);
        drop(sender);

        assert_eq!(receiver.iter().count(), 501, "every update is forwarded");
        assert_eq!(
            retained.lines().count(),
            super::RETAINED_GIT_OUTPUT_SEGMENTS
        );
        assert!(retained.ends_with("fatal: repository not found"));
    }

    fn initialize_repository(path: &Path) -> Repository {
        let repository = Repository::init(path).unwrap();
        repository.set_head("refs/heads/main").unwrap();
        fs::write(path.join("tracked.txt"), "initial\n").unwrap();
        commit_all(&repository, "initial");
        repository
    }

    /// Commits every non-ignored file in the worktree onto the current head.
    fn commit_all(repository: &Repository, message: &str) {
        let mut index = repository.index().unwrap();
        index.add_all(["*"], IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repository.find_tree(tree_id).unwrap();
        let signature = Signature::new(
            "Harkness Tests",
            "tests@harkness.invalid",
            &Time::new(COMMIT_EPOCH_SECONDS, 0),
        )
        .unwrap();
        let parents = repository
            .head()
            .ok()
            .and_then(|head| head.target())
            .map(|id| repository.find_commit(id).unwrap())
            .into_iter()
            .collect::<Vec<_>>();

        repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                message,
                &tree,
                &parents.iter().collect::<Vec<_>>(),
            )
            .unwrap();
    }

    struct Fixture {
        root: TempDir,
        data_dir: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let data_dir = root.path().join("data");
            Self { root, data_dir }
        }

        fn directory(&self, name: &str) -> PathBuf {
            let path = self.root.path().join(name);
            fs::create_dir(&path).unwrap();
            path
        }

        fn service(&self) -> ProjectService {
            ProjectService::load_from_data_dir(&self.data_dir).unwrap()
        }
    }
}
