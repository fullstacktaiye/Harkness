//! The credential scan: a run that leaks through every channel it has.
//!
//! Unit tests prove each redaction rule in isolation. This proves the thing that
//! actually matters and that no unit test can: after a whole run has happened,
//! the sentinel values are *nowhere on disk*. It reads the bytes back rather
//! than the API — `runtime.db` and its write-ahead log, every artifact file, and
//! the diagnostic log — because an assertion made through a projection tests the
//! projection, and the threat here is a column somebody forgot to route through
//! the redactor.
//!
//! One test, deliberately. [`observe::init`] installs a process-global
//! subscriber, so a second test in this binary could not choose a different data
//! directory to log into, and splitting the scan would leave each half unable to
//! see the other's channels.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use harkness_core::ProjectId;
use harkness_runtime::agent::{
    MockAgent, ObservationPattern, Scenario, ScenarioId, ScenarioStep, WorkspaceRef,
};
use harkness_runtime::coordinator::RunCoordinator;
use harkness_runtime::domain::{RunId, Task};
use harkness_runtime::observe::{self, SecretRegistry};
use harkness_runtime::policy::{PolicyEngine, UserPolicy};
use harkness_runtime::store::Store;
use harkness_runtime::tool::{
    ExecutionContext, ProgressEvent, RiskLevel, Tool, ToolError, ToolIdentity, ToolMetadata,
    ToolRegistry,
};
use harkness_runtime::trust::{TrustState, WorkspaceTrust};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use time::OffsetDateTime;

/// The password half of a URL's userinfo — the shape the issue names.
const URL_SECRET: &str = "hunter2";

/// A credential with a published prefix, which no other rule would find.
const TOKEN_SECRET: &str = "ghp_0123456789abcdefghijklmnopqrstuvwxyzAB";

/// A value with no shape at all, recognizable only because it was declared.
const DECLARED_SECRET: &str = "opaque-passphrase-nothing-would-guess";

/// Everything that must not survive anywhere, in one list.
const SENTINELS: &[&str] = &[URL_SECRET, TOKEN_SECRET, DECLARED_SECRET];

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct LeakInput {
    /// Whether the call should end in a failure instead of a result.
    fail: bool,
}

#[derive(Debug, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct LeakOutput {
    remote: String,
    note: String,
}

/// A tool that pushes the sentinels down every channel a tool has.
///
/// Progress, an artifact's *label*, an artifact's bytes, the result payload, and
/// a failure message. Each is a different code path into the store, and each has
/// been the kind of thing a redaction pass forgets.
struct Leaky;

impl Tool for Leaky {
    type Input = LeakInput;
    type Output = LeakOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse("fixture.leaky", "1.0.0").unwrap(),
            "Leaky fixture",
            "Emits credentials through every channel a tool has.",
            RiskLevel::Observe,
        )
    }

    fn execute(
        &self,
        input: LeakInput,
        context: &mut ExecutionContext,
    ) -> Result<LeakOutput, ToolError> {
        context.report(ProgressEvent::message(format!(
            "fetching https://user:{URL_SECRET}@example.com/repo.git"
        )));
        context.report(ProgressEvent::message(format!(
            "authorization: Bearer {TOKEN_SECRET}"
        )));

        // The label is caller text in a bounded column, which is exactly the
        // place a tool naming its artifact after what it just leaked would land.
        let mut stream =
            context.open_artifact(&format!("log-for-{DECLARED_SECRET}.txt"), "text/plain")?;
        for line in [
            format!("remote: https://user:{URL_SECRET}@example.com/repo.git"),
            format!("token={TOKEN_SECRET}"),
            format!("passphrase was {DECLARED_SECRET}"),
            "a perfectly ordinary line".to_owned(),
        ] {
            stream
                .write_all(format!("{line}\n").as_bytes())
                .map_err(|error| ToolError::execution_failed(error.to_string()))?;
        }
        stream.finish()?;

        if input.fail {
            return Err(ToolError::execution_failed(format!(
                "could not reach https://user:{URL_SECRET}@example.com with token={TOKEN_SECRET} \
                 or {DECLARED_SECRET}"
            )));
        }
        Ok(LeakOutput {
            remote: format!("https://user:{URL_SECRET}@example.com/repo.git"),
            note: format!("used {DECLARED_SECRET} and {TOKEN_SECRET}"),
        })
    }
}

fn call(fail: bool) -> harkness_runtime::agent::AgentAction {
    harkness_runtime::agent::AgentAction::CallTool {
        tool_id: "fixture.leaky".parse().unwrap(),
        tool_version: "1.0.0".parse().unwrap(),
        input: serde_json::json!({ "fail": fail }),
    }
}

fn await_terminal(coordinator: &RunCoordinator, run: RunId) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = coordinator.run_snapshot(run).unwrap();
        if snapshot.run.state().is_terminal() {
            return;
        }
        assert!(Instant::now() < deadline, "run {run} did not finish");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Every regular file under `root`, recursively.
fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_under(&path));
        } else {
            found.push(path);
        }
    }
    found
}

/// Whether `haystack` holds `needle` as raw bytes.
///
/// A byte search rather than a string one, because `runtime.db` is a binary file
/// and decoding it would decide what counts as present.
fn contains(haystack: &[u8], needle: &str) -> bool {
    let needle = needle.as_bytes();
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[test]
fn no_sentinel_credential_survives_anywhere_a_run_writes() {
    let data_dir = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();

    // Before anything runs, so the diagnostic log is one of the channels under
    // test rather than a file that happened not to exist.
    let outcome = observe::init(
        Some(data_dir.path()),
        observe::Options::default().with_default_filter("harkness_runtime=trace"),
    );
    let log_path = match outcome {
        observe::InitOutcome::Logging { path, .. } => path,
        other => panic!("the diagnostic log should have been arranged: {other:?}"),
    };

    // The one rule that cannot work by shape. This is the call
    // `ToolProcess::spawn` makes for every sensitive variable it hands a child.
    assert_eq!(
        SecretRegistry::process().declare(DECLARED_SECRET),
        observe::Declared::Accepted
    );

    let store = Arc::new(Store::open(data_dir.path()).unwrap());
    let project = ProjectId::new();
    store
        .put_workspace_trust(
            &WorkspaceTrust::decide(
                project,
                workspace.path(),
                TrustState::Trusted,
                OffsetDateTime::now_utc(),
            )
            .unwrap(),
        )
        .unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(Leaky).unwrap();
    let coordinator = RunCoordinator::new(
        Arc::clone(&store),
        Arc::new(registry),
        PolicyEngine::new(UserPolicy::default(), None),
    )
    .unwrap();

    // The title is caller text in its own column, so it carries a sentinel too.
    let task = Task::new(
        format!("clone https://user:{URL_SECRET}@example.com"),
        workspace.path(),
        Some(project),
        OffsetDateTime::now_utc(),
    );
    let reference = WorkspaceRef::from_task(&task, &**store.redactor());
    let task_id = coordinator.start_task(task).unwrap();

    // Two runs: one that succeeds, so the result payload is exercised, and one
    // that fails, so the failure message is.
    for fail in [false, true] {
        let scenario = Scenario::new(
            ScenarioId::new(if fail { "leak_fail" } else { "leak_ok" }).unwrap(),
            vec![
                ScenarioStep::new(
                    ObservationPattern::RunStarted { task_title: None },
                    call(fail),
                ),
                ScenarioStep::new(
                    if fail {
                        ObservationPattern::ToolFailed { error_kind: None }
                    } else {
                        ObservationPattern::ToolResult {
                            artifact_media_type: None,
                            output_contains: None,
                        }
                    },
                    harkness_runtime::agent::AgentAction::CompleteRun {
                        summary: "done".to_owned(),
                    },
                ),
            ],
        )
        .unwrap();
        let run = coordinator
            .start_run(
                task_id,
                Box::new(MockAgent::from_scenario(scenario)),
                reference.clone(),
            )
            .unwrap();
        await_terminal(&coordinator, run);
    }

    // Flushed so the database file — rather than only its write-ahead log —
    // holds the rows, and so a reader of these bytes sees what a later process
    // would see.
    store.checkpoint().unwrap();
    drop(coordinator);

    // A line is emitted per event at this filter level, but a subscriber writes
    // when it writes; giving it a moment costs nothing and removes the only
    // source of flakiness in the scan.
    thread::sleep(Duration::from_millis(50));
    assert!(
        log_path.exists(),
        "the run should have produced diagnostic lines"
    );

    let scanned: Vec<PathBuf> = files_under(data_dir.path());
    assert!(
        scanned.iter().any(|path| path.ends_with("runtime.db")),
        "the database itself must be among the scanned files: {scanned:?}"
    );
    assert!(
        scanned
            .iter()
            .any(|path| path.starts_with(data_dir.path().join("artifacts"))),
        "the run should have written artifacts: {scanned:?}"
    );
    assert!(scanned.contains(&log_path), "the log must be scanned too");

    for path in &scanned {
        let bytes = fs::read(path).unwrap();
        for sentinel in SENTINELS {
            assert!(
                !contains(&bytes, sentinel),
                "{} still holds {sentinel}",
                path.display()
            );
        }
    }

    // The complement: redaction that removed everything would pass the scan
    // above and be useless. The artifact still has to hold the line that never
    // carried a secret, and the markers have to say which rule fired.
    let artifacts: Vec<Vec<u8>> = files_under(&data_dir.path().join("artifacts"))
        .iter()
        .map(|path| fs::read(path).unwrap())
        .collect();
    assert!(
        artifacts
            .iter()
            .any(|bytes| contains(bytes, "a perfectly ordinary line")),
        "an artifact's untouched content must survive redaction"
    );
    assert!(
        artifacts
            .iter()
            .any(|bytes| contains(bytes, "«redacted:url_userinfo»")),
        "the artifact stream must show which rule scrubbed it"
    );
    assert!(
        artifacts
            .iter()
            .any(|bytes| contains(bytes, "«redacted:declared_secret»")),
        "the declared-secret rule must reach an artifact's bytes"
    );

    let log = fs::read(&log_path).unwrap();
    assert!(
        contains(&log, "run started"),
        "the diagnostic log should describe the run it recorded"
    );
    assert!(
        contains(&log, "«redacted:"),
        "the log writer must apply the same rules the store does"
    );
}
