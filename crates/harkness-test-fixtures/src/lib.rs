//! Hermetic filesystem, repository, and process fixtures shared by Harkness tests.

use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use git2::{IndexAddOption, Repository, Signature, Time};
use tempfile::TempDir;

/// Fixed so repository fixtures hash identically between runs.
pub const COMMIT_EPOCH_SECONDS: i64 = 1_700_000_000;

/// Bare executable names used by the deterministic mock-agent process cases.
/// Printed by the passing scenario-process fixture child.
///
/// A caller asserts on this rather than on the exit status alone, because
/// libtest exits zero when `--exact` selects no test at all.
pub const SCENARIO_FIXTURE_PASS_MARKER: &str = "HARKNESS_SCENARIO_FIXTURE_PASSED";

pub const SCENARIO_PROCESS_PROGRAMS: [&str; 5] = [
    "fixture-fail",
    "fixture-hang",
    "fixture-cancellable",
    "fixture-disallowed",
    "fixture-pass",
];

const FIXTURE_GIT_TIMEOUT: Duration = Duration::from_secs(120);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const REDIRECTING_ENVIRONMENT: [&str; 8] = [
    "GIT_DIR",
    "GIT_COMMON_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CEILING_DIRECTORIES",
    "GIT_NAMESPACE",
];
const INJECTING_ENVIRONMENT: [&str; 3] =
    ["GIT_CONFIG", "GIT_CONFIG_PARAMETERS", "GIT_CONFIG_COUNT"];
const INJECTED_CONFIGURATION_PREFIXES: [&str; 2] = ["GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"];

/// Reads one required path from a child-process environment variable.
pub fn child_path(variable: &str) -> PathBuf {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("child path variable {variable} was not set"))
}

/// Signals that a re-executed fixture child reached its protected state.
pub fn signal_ready(variable: &str) {
    fs::write(child_path(variable), b"ready").unwrap();
}

/// Keeps a re-executed fixture child alive until its parent terminates it.
pub fn park() -> ! {
    loop {
        thread::park();
    }
}

/// Emits the process-fixture readiness marker and remains alive for supervision.
#[doc(hidden)]
pub fn scenario_process_ready_then_park() -> ! {
    println!("HARKNESS_SCENARIO_FIXTURE_READY");
    std::io::stdout().flush().unwrap();
    park()
}

/// Declares the four ignored child roles referenced by mock-agent scenarios.
///
/// Invoke this once in any integration-test binary that executes those
/// scenarios, then call [`Fixture::install_scenario_process_fixtures`] so the
/// frozen bare executable names resolve to platform-native copies of that test
/// binary. Re-execution selects exactly one generated ignored test.
#[macro_export]
macro_rules! scenario_process_fixture_tests {
    () => {
        #[test]
        #[ignore = "re-executed by a mock-agent process scenario"]
        fn scenario_process_fixture_failure_child() {
            panic!("intentional fixture process failure");
        }

        #[test]
        #[ignore = "re-executed by a mock-agent process scenario"]
        fn scenario_process_fixture_hang_child() {
            $crate::scenario_process_ready_then_park();
        }

        #[test]
        #[ignore = "re-executed by a mock-agent process scenario"]
        fn scenario_process_fixture_cancellable_child() {
            $crate::scenario_process_ready_then_park();
        }

        #[test]
        #[ignore = "policy must deny this fixture before execution"]
        fn scenario_process_fixture_disallowed_child() {
            panic!("a policy-denied fixture process executed");
        }

        /// Exits zero, so a scenario expecting `passed: true` has a program to
        /// name that is not a host utility. The flagship previously named
        /// `cargo`, which needs a whole toolchain and an environment the tool
        /// runner deliberately clears.
        ///
        /// It prints [`SCENARIO_FIXTURE_PASS_MARKER`](crate::SCENARIO_FIXTURE_PASS_MARKER)
        /// because "the child exited zero" is not evidence on its own: libtest
        /// exits zero when `--exact` matches nothing, so a caller that failed
        /// to resolve this program at all would observe success from a process
        /// that ran no test.
        #[test]
        #[ignore = "re-executed by a mock-agent process scenario"]
        fn scenario_process_fixture_pass_child() {
            println!("{}", $crate::SCENARIO_FIXTURE_PASS_MARKER);
        }
    };
}

/// Prepares a re-execution of the current test binary in a named role.
pub fn spawn_child(
    test_name: &str,
    role_variable: &str,
    role: &str,
    data_variable: &str,
    data_dir: &Path,
) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .arg("--exact")
        .arg(test_name)
        .arg("--ignored")
        .env(role_variable, role)
        .env(data_variable, data_dir);
    command
}

/// Waits for a child to create `signal`, failing instead of hanging.
pub fn wait_for_child_signal(child: &mut Child, signal: &Path) {
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
        thread::sleep(POLL_INTERVAL);
    }
}

/// Waits for `signal` to appear, failing instead of hanging.
pub fn wait_for_file(signal: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !signal.exists() {
        assert!(
            Instant::now() < deadline,
            "'{}' did not appear within 10 seconds",
            signal.display()
        );
        thread::sleep(POLL_INTERVAL);
    }
}

/// Creates a repository on `main` holding one committed file.
pub fn initialize_repository(path: &Path) -> Repository {
    let repository = Repository::init(path).unwrap();
    repository.set_head("refs/heads/main").unwrap();
    configure_commit_identity(&repository);
    fs::write(path.join("tracked.txt"), "initial\n").unwrap();
    commit_all(&repository, "initial");
    repository
}

/// Gives fixture commits a local identity and disables signing.
pub fn configure_commit_identity(repository: &Repository) {
    let mut config = repository.config().unwrap();
    config.set_str("user.name", "Harkness Tests").unwrap();
    config
        .set_str("user.email", "tests@harkness.invalid")
        .unwrap();
    config.set_bool("commit.gpgsign", false).unwrap();
}

/// Creates a bare repository whose unborn `HEAD` names `main`.
pub fn initialize_bare_repository(path: &Path) -> Repository {
    let repository = Repository::init_bare(path).unwrap();
    repository.set_head("refs/heads/main").unwrap();
    repository
}

/// Runs bounded, environment-scrubbed system Git for fixture setup.
///
/// Output is drained concurrently so a broken fixture cannot deadlock on a
/// full pipe. Repository-redirecting variables and injected configuration are
/// removed for the same reason production Git invocations remove them: tests
/// must never mutate a repository selected by their parent process.
pub fn git(
    working_directory: &Path,
    arguments: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> String {
    let mut command = Command::new("git");
    command
        .args(["--no-pager", "-c", "core.hooksPath="])
        .args(arguments)
        .current_dir(working_directory)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "harkness-has-no-editor")
        .env("GIT_SEQUENCE_EDITOR", "harkness-has-no-editor")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_configuration_file())
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in REDIRECTING_ENVIRONMENT.iter().chain(&INJECTING_ENVIRONMENT) {
        command.env_remove(name);
    }
    for (name, _) in std::env::vars_os() {
        if INJECTED_CONFIGURATION_PREFIXES
            .iter()
            .any(|prefix| name.to_str().is_some_and(|name| name.starts_with(prefix)))
        {
            command.env_remove(name);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = command.spawn().unwrap();
    let stdout = child.stdout.take().expect("piped fixture Git stdout");
    let stderr = child.stderr.take().expect("piped fixture Git stderr");
    let stdout_reader = thread::spawn(move || read_all(stdout));
    let stderr_reader = thread::spawn(move || read_all(stderr));
    let deadline = Instant::now() + FIXTURE_GIT_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            terminate_process_group(&mut child);
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            panic!(
                "fixture Git did not finish within {} seconds",
                FIXTURE_GIT_TIMEOUT.as_secs()
            );
        }
        thread::sleep(POLL_INTERVAL);
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    assert!(status.success(), "{}", String::from_utf8_lossy(&stderr));
    String::from_utf8_lossy(&stdout).into_owned()
}

#[cfg(windows)]
fn null_configuration_file() -> &'static str {
    "NUL"
}

#[cfg(not(windows))]
fn null_configuration_file() -> &'static str {
    "/dev/null"
}

#[cfg(unix)]
fn terminate_process_group(child: &mut Child) {
    // The child leads the process group configured above, so the negative PID
    // terminates fixture helpers before the output-reader threads are joined.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn terminate_process_group(child: &mut Child) {
    let _ = child.kill();
}

fn read_all(mut stream: impl Read) -> Vec<u8> {
    let mut output = Vec::new();
    let _ = stream.read_to_end(&mut output);
    output
}

/// Commits every non-ignored file in the worktree onto the current head.
pub fn commit_all(repository: &Repository, message: &str) {
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

/// Creates a local bare remote and a clone beneath `fixture`.
pub fn remote_with_clone(fixture: &Fixture, name: &str) -> (PathBuf, PathBuf) {
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

/// A temporary root holding an isolated Harkness data directory.
pub struct Fixture {
    /// Lifetime guard and root for the temporary fixture.
    pub root: TempDir,
    /// Data directory passed to Harkness services.
    pub data_dir: PathBuf,
}

impl Fixture {
    /// Creates an empty fixture.
    #[must_use]
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let data_dir = root.path().join("data");
        Self { root, data_dir }
    }

    /// Creates and returns one named directory beneath the fixture root.
    pub fn directory(&self, name: &str) -> PathBuf {
        let path = self.root.path().join(name);
        fs::create_dir(&path).unwrap();
        path
    }

    /// Installs platform-native links to the current integration-test binary
    /// under every deterministic mock-agent process-fixture name.
    ///
    /// The calling test binary must invoke
    /// [`scenario_process_fixture_tests!`](crate::scenario_process_fixture_tests)
    /// so re-execution can select the exact ignored child role named by the
    /// scenario argv.
    pub fn install_scenario_process_fixtures(&self) {
        let executable = std::env::current_exe().unwrap();
        let (first, remaining) = SCENARIO_PROCESS_PROGRAMS
            .split_first()
            .expect("the scenario process fixture set is nonempty");
        let installed = self.scenario_process_program(first);
        if !installed.is_file() && fs::hard_link(&executable, &installed).is_err() {
            fs::copy(&executable, &installed).unwrap();
        }
        for program in remaining {
            let alias = self.scenario_process_program(program);
            if !alias.is_file() && fs::hard_link(&installed, &alias).is_err() {
                fs::copy(&installed, &alias).unwrap();
            }
        }
    }

    /// Prepends the installed process-fixture directory to one child's `PATH`.
    ///
    /// The intended child is the integration-test coordinator. Its real tool
    /// processes inherit only the allowlisted baseline `PATH`, so this scoped
    /// environment is how the bare names frozen in scenario JSON resolve
    /// without mutating the test runner's process-wide environment.
    pub fn configure_scenario_process_path(&self, command: &mut Command) {
        self.install_scenario_process_fixtures();
        command.env("PATH", self.scenario_process_path());
    }

    /// `PATH` value that resolves installed scenario processes before host tools.
    #[must_use]
    pub fn scenario_process_path(&self) -> OsString {
        let inherited = std::env::var_os("PATH")
            .into_iter()
            .flat_map(|value| std::env::split_paths(&value).collect::<Vec<_>>());
        std::env::join_paths(std::iter::once(self.root.path().to_path_buf()).chain(inherited))
            .expect("temporary fixture paths are valid PATH entries")
    }

    /// Resolves one installed scenario-process fixture beneath this root.
    #[must_use]
    pub fn scenario_process_program(&self, program: &str) -> PathBuf {
        assert!(
            SCENARIO_PROCESS_PROGRAMS.contains(&program),
            "unknown scenario process fixture {program}"
        );
        self.root
            .path()
            .join(format!("{program}{}", std::env::consts::EXE_SUFFIX))
    }

    /// Writes an executable stand-in for system Git.
    #[cfg(unix)]
    pub fn shim(&self, name: &str, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = self.root.path().join(name);
        fs::write(&path, script).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }
}

impl Default for Fixture {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{Fixture, child_path, git, initialize_repository};

    const CHILD_TEST: &str = "tests::fixture_git_child";
    const CHILD_ROLE_ENV: &str = "HARKNESS_FIXTURE_TEST_ROLE";
    const CHILD_ROOT_ENV: &str = "HARKNESS_FIXTURE_TEST_ROOT";

    #[test]
    #[ignore = "only run as a child process by the fixture Git test"]
    fn fixture_git_child() {
        assert_eq!(std::env::var(CHILD_ROLE_ENV).unwrap(), "git-environment");
        let root = child_path(CHILD_ROOT_ENV);
        let reported = git(&root, ["rev-parse", "--show-toplevel"]);
        assert_eq!(
            std::fs::canonicalize(reported.trim()).unwrap(),
            std::fs::canonicalize(root).unwrap()
        );
    }

    #[test]
    fn fixture_git_ignores_an_inherited_repository_redirect() {
        let fixture = Fixture::new();
        let root = fixture.directory("intended");
        initialize_repository(&root);
        let decoy = fixture.directory("decoy.git");
        git2::Repository::init_bare(&decoy).unwrap();

        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(CHILD_TEST)
            .arg("--ignored")
            .env(CHILD_ROLE_ENV, "git-environment")
            .env(CHILD_ROOT_ENV, &root)
            .env("GIT_DIR", &decoy)
            .status()
            .unwrap();

        assert!(status.success());
    }
}
