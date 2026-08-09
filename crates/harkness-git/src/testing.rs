//! Fixtures shared by this crate's tests.

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
    Cancellation, CommitOptions, GitService,
    runner::{GitAccess, GitCommand},
};

/// Fixed so repository fixtures hash identically between runs.
pub(crate) const COMMIT_EPOCH_SECONDS: i64 = 1_700_000_000;

const PROCESS_CHILD_TEST: &str = "testing::process_child";
const PROCESS_ROLE_ENV: &str = "HARKNESS_GIT_TEST_ROLE";
const PROCESS_LOCK_DIR_ENV: &str = "HARKNESS_GIT_TEST_LOCK_DIR";
pub(crate) const PROCESS_PROJECT_ROOT_ENV: &str = "HARKNESS_GIT_TEST_PROJECT_ROOT";
pub(crate) const PROCESS_READY_FILE_ENV: &str = "HARKNESS_GIT_TEST_READY_FILE";

#[test]
#[ignore = "only run as a child process by repository locking tests"]
fn process_child() {
    let role = std::env::var(PROCESS_ROLE_ENV).expect("child role was not set");
    let lock_dir = std::env::var_os(PROCESS_LOCK_DIR_ENV).expect("child lock dir was not set");

    match role.as_str() {
        "hold-repository-lock" => {
            let _repository = GitService::new(child_project_root(), lock_dir)
                .lock(&Cancellation::default())
                .unwrap();
            signal_ready();
            park();
        }
        "commit-with-isolated-config" => {
            let root = child_project_root();
            initialize_repository(&root);
            fs::write(root.join("tracked.txt"), "committed through system Git\n").unwrap();
            let git = GitService::new(&root, lock_dir);
            git.stage(["tracked.txt"], &Cancellation::default())
                .unwrap();
            git.commit(
                "isolated fixture commit",
                &CommitOptions::default(),
                &Cancellation::default(),
            )
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

pub(crate) fn spawn_child(lock_dir: &Path, role: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .arg("--exact")
        .arg(PROCESS_CHILD_TEST)
        .arg("--ignored")
        .env(PROCESS_ROLE_ENV, role)
        .env(PROCESS_LOCK_DIR_ENV, lock_dir);
    command
}

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

pub(crate) fn initialize_repository(path: &Path) -> Repository {
    let repository = Repository::init(path).unwrap();
    repository.set_head("refs/heads/main").unwrap();
    configure_commit_identity(&repository);
    fs::write(path.join("tracked.txt"), "initial\n").unwrap();
    commit_all(&repository, "initial");
    repository
}

pub(crate) fn configure_commit_identity(repository: &Repository) {
    let mut config = repository.config().unwrap();
    config.set_str("user.name", "Harkness Tests").unwrap();
    config
        .set_str("user.email", "tests@harkness.invalid")
        .unwrap();
    config.set_bool("commit.gpgsign", false).unwrap();
}

fn initialize_bare_repository(path: &Path) -> Repository {
    let repository = Repository::init_bare(path).unwrap();
    repository.set_head("refs/heads/main").unwrap();
    repository
}

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

pub(crate) struct Fixture {
    pub(crate) root: TempDir,
    pub(crate) data_dir: PathBuf,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let data_dir = root.path().join("locks");
        Self { root, data_dir }
    }

    pub(crate) fn directory(&self, name: &str) -> PathBuf {
        let path = self.root.path().join(name);
        fs::create_dir(&path).unwrap();
        path
    }

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
