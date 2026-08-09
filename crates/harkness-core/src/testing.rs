//! Fixtures shared by the crate's tests.
//!
//! The re-execution harness lives here because catalog locking, repository
//! locking and environment scrubbing are all only observable across an OS
//! process boundary. Keeping the child inside this test binary avoids a fixture
//! crate while ensuring it exercises the production code paths.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

use git2::{IndexAddOption, Repository, Signature, Time};
use harkness_git::{Cancellation, CommitOptions, GitError, GitService, WorktreeBase};
use tempfile::TempDir;

use crate::project::{ProjectError, ProjectService};

/// Fixed so repository fixtures hash identically between runs.
pub(crate) const COMMIT_EPOCH_SECONDS: i64 = 1_700_000_000;

pub(crate) const PROCESS_CHILD_TEST: &str = "testing::process_child";
pub(crate) const PROCESS_ROLE_ENV: &str = "HARKNESS_CATALOG_TEST_ROLE";
pub(crate) const PROCESS_DATA_DIR_ENV: &str = "HARKNESS_CATALOG_TEST_DATA_DIR";
pub(crate) const PROCESS_PROJECT_ROOT_ENV: &str = "HARKNESS_CATALOG_TEST_PROJECT_ROOT";
pub(crate) const PROCESS_READY_FILE_ENV: &str = "HARKNESS_CATALOG_TEST_READY_FILE";
pub(crate) const PROCESS_GIT_EXECUTABLE_ENV: &str = "HARKNESS_CATALOG_TEST_GIT_EXECUTABLE";
pub(crate) const PROCESS_PROJECT_ID_ENV: &str = "HARKNESS_CATALOG_TEST_PROJECT_ID";
pub(crate) const PROCESS_BRANCH_ENV: &str = "HARKNESS_CATALOG_TEST_BRANCH";

/// The inherited variables the runner must remove before it spawns Git.
///
/// Every one of them redirects Git at another repository, at other refs, or at
/// configuration nobody in Harkness wrote. Shared between the parent, which
/// exports them all so the test proves something, and the child, which asserts
/// none of them survived.
pub(crate) const SCRUBBED_ENVIRONMENT: [&str; 12] = [
    "GIT_DIR",
    "GIT_COMMON_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CEILING_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_KEY_0",
];

/// Re-entered by the tests below as a child process, dispatching on a role.
#[test]
#[ignore = "only run as a child process by the locking and environment tests"]
fn process_child() {
    let role = std::env::var(PROCESS_ROLE_ENV).expect("child role was not set");
    let data_dir = std::env::var_os(PROCESS_DATA_DIR_ENV).expect("child data dir was not set");
    let mut service = ProjectService::load_from_data_dir(&data_dir).unwrap();

    match role.as_str() {
        "import" => {
            service.import_local(child_project_root()).unwrap();
        }
        "hold-lock" => {
            let _lock = service.lock_exclusive().unwrap();
            signal_ready();
            park();
        }
        "load-env" => {
            // `load`, not `load_from_data_dir`: the point is that the
            // environment alone redirects the platform data directory.
            // Asserted before the import, so a regression fails here
            // instead of writing to the developer's real catalog.
            let mut overridden = ProjectService::load().unwrap();
            assert_eq!(overridden.data_dir(), service.data_dir());
            overridden.import_local(child_project_root()).unwrap();
        }
        "default-data-dir" => {
            // Spawned with the override removed, so this asserts the
            // unset path still resolves the platform data directory.
            assert_eq!(
                crate::paths::data_directory(),
                dirs::data_dir().map(|data_dir| data_dir.join("harkness"))
            );
        }
        "hold-repository-lock" => {
            let _lock =
                GitService::new(child_project_root(), PathBuf::from(&data_dir).join("locks"))
                    .lock(&Cancellation::default())
                    .unwrap();
            signal_ready();
            park();
        }
        "create-worktree" => {
            let parent = std::env::var(PROCESS_PROJECT_ID_ENV)
                .expect("child parent id was not set")
                .parse()
                .expect("child parent id was invalid");
            let name = std::env::var(PROCESS_BRANCH_ENV).expect("child branch was not set");
            match service.create_worktree(
                parent,
                &WorktreeBase::NewBranch {
                    name,
                    start_point: None,
                },
                &Cancellation::default(),
            ) {
                Ok(_) | Err(ProjectError::Git(GitError::RepositoryBusy { .. })) => {}
                Err(error) => panic!("unexpected concurrent worktree result: {error}"),
            }
        }
        "commit-with-isolated-config" => {
            let root = child_project_root();
            initialize_repository(&root);
            fs::write(root.join("tracked.txt"), "committed through system Git\n").unwrap();
            let git = GitService::new(&root, PathBuf::from(&data_dir).join("locks"));
            git.stage(["tracked.txt"], &Cancellation::default())
                .unwrap();
            git.commit(
                "isolated fixture commit",
                &CommitOptions::default(),
                &Cancellation::default(),
            )
            .unwrap();
        }
        "scrubbed-environment" => {
            // Set on this process by its parent, so a shim that still sees
            // them can only have inherited them through the runner.
            for name in SCRUBBED_ENVIRONMENT {
                assert!(
                    std::env::var_os(name).is_some(),
                    "the parent did not export {name}, so this proves nothing"
                );
            }
            let git_executable = std::env::var_os(PROCESS_GIT_EXECUTABLE_ENV)
                .expect("child Git executable was not set");
            let root = child_project_root();
            initialize_repository(&root);
            GitService::new(&root, PathBuf::from(&data_dir).join("locks"))
                .with_git_executable(git_executable)
                .worktrees(&Cancellation::default())
                .unwrap();
        }
        _ => panic!("unknown test child role: {role}"),
    }
}

fn child_project_root() -> PathBuf {
    std::env::var_os(PROCESS_PROJECT_ROOT_ENV)
        .map(PathBuf::from)
        .expect("child project root was not set")
}

fn signal_ready() {
    let ready_file =
        std::env::var_os(PROCESS_READY_FILE_ENV).expect("child ready file was not set");
    fs::write(ready_file, b"ready").unwrap();
}

fn park() -> ! {
    loop {
        thread::park();
    }
}

/// Prepares a re-execution of this test binary in the given role.
pub(crate) fn spawn_child(data_dir: &Path, role: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .arg("--exact")
        .arg(PROCESS_CHILD_TEST)
        .arg("--ignored")
        .env(PROCESS_ROLE_ENV, role)
        .env(PROCESS_DATA_DIR_ENV, data_dir);
    command
}

/// Waits for a child to create its ready file, failing rather than hanging.
pub(crate) fn wait_for_child_signal(child: &mut Child, signal: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if signal.exists() {
            return;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("test child exited before signalling readiness: {status}");
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("test child did not signal readiness within 10 seconds");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Creates a repository on `main` holding one committed file.
pub(crate) fn initialize_repository(path: &Path) -> Repository {
    let repository = Repository::init(path).unwrap();
    repository.set_head("refs/heads/main").unwrap();
    configure_commit_identity(&repository);
    fs::write(path.join("tracked.txt"), "initial\n").unwrap();
    commit_all(&repository, "initial");
    repository
}

/// Gives system Git a hermetic identity for fixture commits.
///
/// Existing fixtures create commits through libgit2 and never consult Git
/// configuration. The commit service shells out, so its fixtures must not
/// depend on a developer's global identity or signing policy.
pub(crate) fn configure_commit_identity(repository: &Repository) {
    let mut config = repository.config().unwrap();
    config.set_str("user.name", "Harkness Tests").unwrap();
    config
        .set_str("user.email", "tests@harkness.invalid")
        .unwrap();
    config.set_bool("commit.gpgsign", false).unwrap();
}

/// Runs system Git for a fixture, returning its standard output.
///
/// This test-only setup deliberately stays outside production code; every
/// caller operates only on local fixture repositories and remotes.
pub(crate) fn git(
    working_directory: &Path,
    arguments: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
) -> String {
    let output = Command::new("git")
        .current_dir(working_directory)
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Commits every non-ignored file in the worktree onto the current head.
pub(crate) fn commit_all(repository: &Repository, message: &str) {
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

/// A temporary root holding an isolated Harkness data directory.
pub(crate) struct Fixture {
    pub(crate) root: TempDir,
    pub(crate) data_dir: PathBuf,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let data_dir = root.path().join("data");
        Self { root, data_dir }
    }

    pub(crate) fn directory(&self, name: &str) -> PathBuf {
        let path = self.root.path().join(name);
        fs::create_dir(&path).unwrap();
        path
    }

    pub(crate) fn service(&self) -> ProjectService {
        ProjectService::load_for_test(&self.data_dir).unwrap()
    }

    /// Writes an executable stand-in for the system Git executable.
    #[cfg(unix)]
    pub(crate) fn shim(&self, name: &str, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = self.root.path().join(name);
        fs::write(&path, script).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }
}
