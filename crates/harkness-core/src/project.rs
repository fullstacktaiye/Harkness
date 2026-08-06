use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
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

/// Describes how a project entered the catalog.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectSource {
    /// A directory that already exists on the local machine.
    Local,
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
        fs,
        path::{Path, PathBuf},
        thread,
        time::Duration,
    };

    use git2::{IndexAddOption, Repository, Signature, Time};
    use tempfile::TempDir;

    use super::{
        CATALOG_FILE, CATALOG_VERSION, ProjectError, ProjectService, ProjectSource,
        WORKTREES_DIRECTORY,
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
