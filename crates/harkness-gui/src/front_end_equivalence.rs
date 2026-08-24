//! One store, two front ends, one set of answers.
//!
//! Every other test in this crate seeds a store through the runtime API and
//! then reads it back through a model, which proves the model's projection and
//! nothing about the other front end. The claim #104 asks for is narrower and
//! harder: that a run *the command line executed* reads back through the
//! window's own models as the same run — same identifiers, same states, same
//! events, same queue of unanswered questions.
//!
//! So the work here is done by the real `harkness` binary, in its own process,
//! against a temporary `HARKNESS_DATA_DIR`; nothing in this file writes a row.
//! The window's side is the three loaders the models actually call —
//! [`load_page_in`], [`open_timeline_in`] with [`load_older_page_in`], and
//! [`load_pending_in`] — rather than a re-derivation of what they would return.
//!
//! # Why this is Qt-linked but display-free
//!
//! The loaders are plain functions that return `Send` data; the `QObject`s that
//! own them are not involved, so this needs no `QGuiApplication`, no QML engine,
//! no Kirigami, and no display. That matters for where it can run: hosted CI has
//! Qt 6 but no KDE Frameworks 6, so `qml_smoke` and `run_surfaces` cannot run
//! there and this can.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use harkness_core::ProjectService;
use harkness_runtime::domain::RunId;
use harkness_runtime::store::EventSeq;
use serde_json::Value;
use tempfile::TempDir;

// `super::` rather than `crate::`: this module is a child of `main.rs`, which
// the three `harness = false` test binaries include as a *nested* module, so
// `crate::` there names the test binary's own root instead of the window's.
use super::approval_model::load_pending_in;
use super::run_list_model::load_page_in;
use super::run_timeline_model::{load_older_page_in, open_timeline_in};

/// Exactly the bytes the `approval_denied` scenario's precondition names.
const FLAGSHIP_SOURCE: &str = "pub const VALUE: &str = \"old\";\n";

/// The command-line binary's file name on this platform.
const CLI_BINARY: &str = if cfg!(windows) {
    "harkness.exe"
} else {
    "harkness"
};

/// Locates the `harkness` binary this test drives.
///
/// `CARGO_BIN_EXE_*` names only the binaries of the package the test belongs
/// to, and the command line lives in another crate, so the path is derived from
/// this test binary's own location instead: cargo puts every workspace binary
/// in `target/<profile>/`, one directory above the `deps/` the test runs from.
/// `HARKNESS_CLI_BIN` overrides that for a build layout this cannot guess.
///
/// Building it from here is not an option — cargo holds the build-directory
/// lock for the whole of `cargo test`, so a nested `cargo build` would wait for
/// a lock only its own parent can release. `cargo test --workspace`, the
/// command `AGENTS.md` names, builds it; a bare `cargo test -p harkness-gui`
/// does not, and this says so rather than skipping, because a test that
/// silently verifies nothing is worse than one that is missing.
fn command_line_binary() -> PathBuf {
    if let Some(named) = std::env::var_os("HARKNESS_CLI_BIN") {
        return PathBuf::from(named);
    }
    let executable = std::env::current_exe().expect("a test binary knows where it is");
    let candidates: Vec<PathBuf> = executable
        .ancestors()
        .skip(1)
        .take(3)
        .map(|directory| directory.join(CLI_BINARY))
        .collect();
    candidates
        .iter()
        .find(|candidate| candidate.is_file())
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "the {CLI_BINARY} binary is not built; run `cargo build -p harkness-cli` or the \
                 whole suite with `cargo test --workspace`. Looked in: {candidates:?}"
            )
        })
}

/// One hermetic project and the data directory both front ends read.
struct World {
    data_dir: TempDir,
    workspace: TempDir,
    binary: PathBuf,
}

impl World {
    fn new() -> Self {
        let binary = command_line_binary();
        let data_dir = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let root = workspace.path().join("ws");
        harkness_test_fixtures::initialize_repository(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), FLAGSHIP_SOURCE).unwrap();
        harkness_test_fixtures::commit_all(
            &git2::Repository::open(&root).unwrap(),
            "equivalence fixture",
        );
        let mut service = ProjectService::load_from_data_dir(data_dir.path()).unwrap();
        service.import_local(&root).unwrap();
        Self {
            data_dir,
            workspace,
            binary,
        }
    }

    fn data_dir(&self) -> &Path {
        self.data_dir.path()
    }

    fn harkness<S: AsRef<OsStr>>(&self, arguments: &[S]) -> Output {
        Command::new(&self.binary)
            .env("HARKNESS_DATA_DIR", self.data_dir())
            .current_dir(self.workspace.path())
            .args(arguments)
            .output()
            .expect("the harkness binary should start")
    }

    /// The `data` envelope of a command that must succeed.
    fn data<S: AsRef<OsStr>>(&self, arguments: &[S]) -> Value {
        let output = self.harkness(arguments);
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        envelope(&output)["data"].clone()
    }
}

fn envelope(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout was not JSON ({error}): {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn strings(values: &Value, field: &str) -> Vec<String> {
    values
        .as_array()
        .unwrap_or_else(|| panic!("{values} is not an array"))
        .iter()
        .map(|value| value[field].as_str().unwrap().to_owned())
        .collect()
}

/// Every event the window's timeline holds for `run`, oldest first.
///
/// Paged backwards the way `RunTimelineModel` pages it, so a run longer than
/// one page is compared whole rather than compared to its newest screenful.
fn window_timeline(data_dir: &Path, run: RunId) -> Vec<(u64, String)> {
    let (receiver, mut page) = open_timeline_in(data_dir, run)
        .unwrap()
        .expect("the data directory has recorded runs");
    assert!(
        receiver.is_none(),
        "a finished run nothing in this process is driving opens no subscription"
    );
    let mut rows = Vec::new();
    loop {
        let oldest = page.rows.first().map(|row| row.first_seq);
        rows.splice(
            0..0,
            page.rows.iter().map(|row| (row.seq, row.kind.clone())),
        );
        match (page.beginning, oldest) {
            (true, _) | (_, None) => break,
            (false, Some(cursor)) => {
                page = load_older_page_in(data_dir, run, EventSeq::new(cursor)).unwrap();
            }
        }
    }
    rows
}

/// The claim: one store, two readers, no disagreement.
///
/// Two runs rather than one, because a single row agrees with itself under any
/// ordering; and the timeline of *each* of them, because the run whose events
/// are interesting is not the same run as the one whose state is.
#[test]
fn the_window_and_the_command_line_report_the_same_runs_states_and_events() {
    let world = World::new();

    // One trusted read-only invocation, which records a run of its own, and one
    // agent run that asks for a workspace edit nobody can answer. Between them
    // the store holds a succeeded run, a failed one, a denied tool call and an
    // answered approval — and no process fixture is needed, because the denial
    // lands before anything would have been executed.
    world.data(&[
        "--json",
        "tool",
        "invoke",
        "workspace.inspect",
        "--input",
        "{}",
        "--project",
        "ws",
        "--trust-workspace",
    ]);
    let denied = world.harkness(&[
        "--json",
        "agent",
        "run",
        "--scenario",
        "approval_denied",
        "--project",
        "ws",
    ]);
    assert_eq!(
        denied.status.code(),
        Some(3),
        "an unanswerable approval is refused: {}",
        String::from_utf8_lossy(&denied.stderr)
    );
    assert_eq!(
        envelope(&denied)["error"]["kind"],
        "approval_required_noninteractive"
    );

    // ---- the run list --------------------------------------------------
    let listed = world.data(&["--json", "run", "list"]);
    let command_line: Vec<(String, String)> = strings(&listed["runs"], "id")
        .into_iter()
        .zip(strings(&listed["runs"], "state"))
        .collect();
    assert_eq!(command_line.len(), 2, "both runs were recorded");

    let page = load_page_in(world.data_dir(), None).unwrap();
    let window: Vec<(String, String)> = page
        .rows
        .iter()
        .map(|row| (row.run_id.clone(), row.state.clone()))
        .collect();
    assert_eq!(
        window, command_line,
        "the window's run list and `run list` disagree"
    );
    assert!(
        page.next.is_none(),
        "two runs fit in one page, so neither reader has more to fetch"
    );

    // ---- each run's timeline -------------------------------------------
    for (run_id, _) in &command_line {
        let shown = world.data(&["--json", "run", "show", run_id, "--limit", "1000"]);
        assert_eq!(shown["next_cursor"], Value::Null, "the whole log was read");
        let mut command_line_events: Vec<(u64, String)> = shown["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| {
                (
                    event["seq"].as_u64().unwrap(),
                    event["kind"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        command_line_events.sort_by_key(|(seq, _)| *seq);
        assert!(
            !command_line_events.is_empty(),
            "every run records at least the event that started it"
        );

        let window_events = window_timeline(world.data_dir(), run_id.parse::<RunId>().unwrap());
        assert_eq!(
            window_events, command_line_events,
            "the window's timeline and `run show` disagree about run {run_id}"
        );
    }

    // ---- the queue of unanswered questions ------------------------------
    let pending = world.data(&["--json", "approvals", "list"]);
    assert_eq!(
        load_pending_in(world.data_dir()).unwrap().len(),
        pending["approvals"].as_array().unwrap().len(),
        "the window's approval queue and `approvals list` disagree"
    );
    // Both say the same thing, and what they say is true: the denial was
    // recorded rather than left outstanding.
    assert!(pending["approvals"].as_array().unwrap().is_empty());
    let answered = world.data(&["--json", "approvals", "list", "--all"]);
    assert_eq!(
        strings(&answered["approvals"], "state"),
        vec!["denied".to_owned()],
    );
}
