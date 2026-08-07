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
use tempfile::TempDir;

use crate::{
    git::{Cancellation, GitAccess, GitCommand, GitService},
    project::ProjectService,
};

/// Fixed so repository fixtures hash identically between runs.
pub(crate) const COMMIT_EPOCH_SECONDS: i64 = 1_700_000_000;

pub(crate) const PROCESS_CHILD_TEST: &str = "testing::process_child";
pub(crate) const PROCESS_ROLE_ENV: &str = "HARKNESS_CATALOG_TEST_ROLE";
pub(crate) const PROCESS_DATA_DIR_ENV: &str = "HARKNESS_CATALOG_TEST_DATA_DIR";
pub(crate) const PROCESS_PROJECT_ROOT_ENV: &str = "HARKNESS_CATALOG_TEST_PROJECT_ROOT";
pub(crate) const PROCESS_READY_FILE_ENV: &str = "HARKNESS_CATALOG_TEST_READY_FILE";
pub(crate) const PROCESS_GIT_EXECUTABLE_ENV: &str = "HARKNESS_CATALOG_TEST_GIT_EXECUTABLE";

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
            let _lock = GitService::new(child_project_root(), &data_dir)
                .lock(&Cancellation::default())
                .unwrap();
            signal_ready();
            park();
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
            let output =
                GitCommand::new(git_executable, child_project_root(), GitAccess::LocalRead)
                    .arg("status")
                    .run(&Cancellation::default())
                    .unwrap();
            let reported = String::from_utf8(output.stdout).unwrap();
            let mut expected = SCRUBBED_ENVIRONMENT
                .iter()
                .map(|name| format!("{name}=unset"))
                .collect::<Vec<_>>();
            expected.extend(
                [
                    "GIT_TERMINAL_PROMPT=0",
                    "GIT_OPTIONAL_LOCKS=0",
                    "LC_ALL=C",
                    "GIT_EDITOR=harkness-has-no-editor",
                ]
                .map(str::to_owned),
            );
            assert_eq!(reported.lines().collect::<Vec<_>>(), expected);
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

/// Waits for a file to appear, failing rather than hanging.
///
/// For the processes no test holds a handle to: a Git shim started by the code
/// under test, which signals that it is running and therefore that there is
/// something to cancel.
pub(crate) fn wait_for_file(signal: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !signal.exists() {
        assert!(
            Instant::now() < deadline,
            "'{}' did not appear within 10 seconds",
            signal.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// Creates a repository on `main` holding one committed file.
pub(crate) fn initialize_repository(path: &Path) -> Repository {
    let repository = Repository::init(path).unwrap();
    repository.set_head("refs/heads/main").unwrap();
    fs::write(path.join("tracked.txt"), "initial\n").unwrap();
    commit_all(&repository, "initial");
    repository
}

/// Creates a bare repository whose HEAD names `main` before it exists.
///
/// Git refuses to update the checked-out branch of a non-bare repository, so
/// every push fixture needs one of these rather than a second working tree.
/// HEAD is set explicitly because libgit2's default branch is not necessarily
/// the one [`initialize_repository`] creates, and a bare repository whose HEAD
/// dangles is one that cannot be cloned.
pub(crate) fn initialize_bare_repository(path: &Path) -> Repository {
    let repository = Repository::init_bare(path).unwrap();
    repository.set_head("refs/heads/main").unwrap();
    repository
}

/// Creates a bare remote holding one commit on `main`, and a clone of it.
///
/// The clone is made by real `git clone`, which is also what writes
/// `refs/remotes/origin/HEAD`: the ref the default-branch refusal falls back
/// to, and one that adding a remote by hand would never produce.
pub(crate) fn remote_with_clone(fixture: &Fixture, name: &str) -> (PathBuf, PathBuf) {
    let source = fixture.directory(&format!("{name}-source"));
    initialize_repository(&source);
    let remote = fixture.directory(&format!("{name}-remote.git"));
    initialize_bare_repository(&remote);
    git(&source, ["push", "--", remote.to_str().unwrap(), "main"]);

    let clone = fixture.root.path().join(format!("{name}-clone"));
    git(
        fixture.root.path(),
        [
            "clone",
            "--",
            remote.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    (remote, clone)
}

/// Runs system Git for a fixture, returning its standard output.
///
/// Bounded by the local-write timeout even for the network verbs it runs
/// against local paths, so a fixture that goes wrong fails the test instead of
/// hanging the suite.
pub(crate) fn git(
    working_directory: &Path,
    arguments: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
) -> String {
    let output = GitCommand::new("git", working_directory, GitAccess::LocalWrite)
        .args(arguments)
        .run(&Cancellation::default())
        .unwrap();
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
