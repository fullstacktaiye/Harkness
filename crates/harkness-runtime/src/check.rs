//! Read-only projection of state-bound `check.run` calls.
//!
//! The projection never executes a command and never takes a repository lock.
//! It reads ordinary run, step, tool-call, and artifact records, verifies the
//! stored composite workspace snapshot through `harkness-context`, and parses
//! machine output with strict memory and record-count bounds.

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use harkness_context::{FilesystemProbe, FreshnessState, SnapshotWire, WorkspaceSnapshot};
use harkness_core::CheckConfiguration;
use harkness_core::{CheckParser, Project};
use harkness_git::{Cancellation, GitService};
use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::agent::{
    AgentAction, MockAgent, ObservationPattern, Scenario, ScenarioError, ScenarioId, ScenarioStep,
    WorkspaceRef,
};
use crate::approval::{ApprovalDecision, ApprovalScope, ApprovalState, DecidedVia};
use crate::coordinator::{RunCoordinator, RuntimeError};
use crate::domain::{ArtifactId, RunId, Task, ToolCallState};
use crate::policy::PolicyEngine;
use crate::store::{Availability, Store, StoreError};
use crate::tool::{RegistryError, ToolRegistry, WorkspaceMetadata};
use crate::tools::{CheckOutputParser, CheckRun, CheckRunOutput};

/// Most check-bearing runs inspected for one project refresh.
pub const MAX_CHECK_RUNS: usize = 100;
/// Most diagnostics retained from one check.
pub const MAX_CHECK_DIAGNOSTICS: usize = 200;
/// Largest one machine-output line parsed in memory.
pub const MAX_DIAGNOSTIC_LINE_BYTES: usize = 64 * 1024;
/// Longest user-facing diagnostic text retained inline.
pub const MAX_DIAGNOSTIC_TEXT_BYTES: usize = 4 * 1024;

/// Failure to launch or supervise one configured check.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CheckLaunchError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Scenario(#[from] ScenarioError),
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
}

/// Runs one configured check through the ordinary coordinator, policy,
/// approval, scheduler, tool, event-log, and artifact-store path.
///
/// The caller is the user-facing approval surface. It must only call this
/// function after an explicit action, and the workspace must already carry a
/// separate positive trust decision. `via` is persisted on the exact-call
/// approval this function grants once the coordinator publishes it.
pub fn run_configured_check(
    store: Arc<Store>,
    project: &Project,
    check: &CheckConfiguration,
    via: DecidedVia,
    cancellation: &Cancellation,
) -> Result<RunId, CheckLaunchError> {
    let mut registry = ToolRegistry::new();
    registry.register(CheckRun)?;
    let policy = PolicyEngine::load(store.data_dir(), &project.root);
    let coordinator = RunCoordinator::new(Arc::clone(&store), Arc::new(registry), policy);
    let task = Task::new(
        format!("Check: {}", check.label),
        &project.root,
        Some(project.id),
        OffsetDateTime::now_utc(),
    );
    let workspace = WorkspaceRef::from_task(&task, &crate::store::PassThrough);
    let task_id = coordinator.start_task(task)?;
    let parser = match check.parser {
        CheckParser::Plain => CheckOutputParser::Plain,
        CheckParser::CargoJson => CheckOutputParser::CargoJson,
    };
    let scenario = Scenario::new(
        ScenarioId::new("configured_check")?,
        vec![
            ScenarioStep::new(
                ObservationPattern::RunStarted { task_title: None },
                AgentAction::CallTool {
                    tool_id: "check.run".parse().expect("published tool id is valid"),
                    tool_version: "1.0.0".parse().expect("published tool version is valid"),
                    input: json!({
                        "check_id": check.id,
                        "label": check.label,
                        "command": check.command,
                        "cwd": check.cwd,
                        "env": check.env,
                        "timeout_seconds": check.timeout_seconds,
                        "parser": parser,
                    }),
                },
            ),
            ScenarioStep::new(
                ObservationPattern::ToolResult {
                    artifact_media_type: None,
                    output_contains: None,
                },
                AgentAction::CompleteRun {
                    summary: format!("Completed check {}", check.label),
                },
            ),
        ],
    )?;
    let run_id = coordinator.start_run_with_workspace_metadata(
        task_id,
        Box::new(MockAgent::from_scenario(scenario)),
        workspace,
        WorkspaceMetadata::from_project(project),
    )?;

    loop {
        let snapshot = coordinator.run_snapshot(run_id)?;
        if cancellation.is_cancelled() {
            if !snapshot.run.state().is_terminal() {
                coordinator.cancel_run(run_id)?;
            }
            return Ok(run_id);
        }
        if let Some(request) = snapshot
            .approvals
            .iter()
            .find(|request| request.state() == ApprovalState::Pending)
        {
            coordinator.decide_approval(ApprovalDecision::grant(
                request.id(),
                ApprovalScope::ExactCall,
                via,
                OffsetDateTime::now_utc(),
            ))?;
        }
        if snapshot.run.state().is_terminal() {
            return Ok(run_id);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// How strongly Harkness can attest to a recorded check.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckEvidenceClass {
    /// Harkness supervised the child and stored its result.
    HarknessMediated,
}

/// Terminal or in-flight meaning of one check call.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckOutcome {
    Queued,
    WaitingForApproval,
    Running,
    Passed,
    Failed,
    TimedOut,
    Denied,
    Cancelled,
    Interrupted,
}

/// Whether the exact state checked is still on disk.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CheckFreshness {
    Current,
    Stale { changed: Vec<String> },
    Unverifiable { reason: String },
}

/// One honest best-effort compiler association.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckDiagnostic {
    /// Workspace-relative file, absent when the compiler did not name one.
    pub path: Option<String>,
    /// One-based line, absent when no primary span named one.
    pub line: Option<u32>,
    /// One-based column, absent when no primary span named one.
    pub column: Option<u32>,
    /// Compiler level such as error, warning, or note.
    pub level: String,
    /// Bounded inert text. Front ends must still use plain-text rendering.
    pub message: String,
}

/// One recorded check call ready for either front end.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckSummary {
    pub run_id: String,
    pub check_id: String,
    pub label: String,
    pub command: Vec<String>,
    pub outcome: CheckOutcome,
    pub evidence_class: CheckEvidenceClass,
    pub created_at: String,
    pub finished_at: Option<String>,
    pub duration_ms: Option<u64>,
    pub state_digest: Option<String>,
    pub state_head: Option<String>,
    pub freshness: CheckFreshness,
    pub diagnostics: Vec<CheckDiagnostic>,
    pub diagnostics_omitted: usize,
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

/// Builds the newest recorded check summaries for one project.
pub fn project_checks(store: &Store, project: &Project) -> Result<Vec<CheckSummary>, StoreError> {
    let mut summaries = Vec::new();
    let mut seen = HashSet::new();
    for run_id in store.project_tool_run_ids(project.id, "check.run", MAX_CHECK_RUNS)? {
        for call in store.load_run_tool_calls(run_id)? {
            if call.tool_id() != "check.run" {
                continue;
            }
            let parsed = call
                .output()
                .cloned()
                .map(serde_json::from_value::<CheckRunOutput>)
                .transpose();
            let (output, invalid_output) = match parsed {
                Ok(output) => (output, None),
                Err(error) => (None, Some(error.to_string())),
            };
            let input = call.input();
            let check_id = output.as_ref().map_or_else(
                || text_field(input, "check_id", "unknown"),
                |out| out.check_id.clone(),
            );
            if !seen.insert(check_id.clone()) {
                continue;
            }
            let label = output.as_ref().map_or_else(
                || text_field(input, "label", &check_id),
                |out| out.label.clone(),
            );
            let command = input
                .get("command")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(Value::as_str)
                        .map(bounded_text)
                        .collect()
                })
                .unwrap_or_default();
            let (freshness, diagnostics, diagnostics_omitted) = match output.as_ref() {
                Some(output) => {
                    let freshness = verify_output_state(store, project, run_id, output);
                    let (diagnostics, omitted) = parse_diagnostics(store, project, run_id, output);
                    (freshness, diagnostics, omitted)
                }
                None => (
                    CheckFreshness::Unverifiable {
                        reason: invalid_output.unwrap_or_else(|| {
                            "the check has not recorded a workspace state yet".to_owned()
                        }),
                    },
                    Vec::new(),
                    0,
                ),
            };
            let outcome = outcome(call.state(), output.as_ref());
            summaries.push(CheckSummary {
                run_id: run_id.to_string(),
                check_id,
                label,
                command,
                outcome,
                evidence_class: CheckEvidenceClass::HarknessMediated,
                created_at: timestamp(call.created_at()),
                finished_at: call.finished_at().map(timestamp),
                duration_ms: output.as_ref().map(|output| output.process.duration_ms),
                state_digest: output
                    .as_ref()
                    .map(|output| output.workspace_state.digest.clone()),
                state_head: output
                    .as_ref()
                    .and_then(|output| output.workspace_state.head.clone()),
                freshness,
                diagnostics,
                diagnostics_omitted,
                stdout_tail: output.as_ref().map_or_else(String::new, |output| {
                    bounded_text(&output.process.stdout_tail.text)
                }),
                stderr_tail: output.as_ref().map_or_else(String::new, |output| {
                    bounded_text(&output.process.stderr_tail.text)
                }),
                stdout_truncated: output
                    .as_ref()
                    .is_some_and(|output| output.process.stdout_tail.truncated),
                stderr_truncated: output
                    .as_ref()
                    .is_some_and(|output| output.process.stderr_tail.truncated),
            });
        }
    }
    Ok(summaries)
}

fn outcome(state: ToolCallState, output: Option<&CheckRunOutput>) -> CheckOutcome {
    match state {
        ToolCallState::Pending => CheckOutcome::Queued,
        ToolCallState::AwaitingApproval => CheckOutcome::WaitingForApproval,
        ToolCallState::Running => CheckOutcome::Running,
        ToolCallState::Succeeded => output.map_or(CheckOutcome::Failed, |output| {
            if output.process.timed_out {
                CheckOutcome::TimedOut
            } else if output.passed {
                CheckOutcome::Passed
            } else {
                CheckOutcome::Failed
            }
        }),
        ToolCallState::Failed => CheckOutcome::Failed,
        ToolCallState::Denied => CheckOutcome::Denied,
        ToolCallState::Cancelled => CheckOutcome::Cancelled,
        ToolCallState::Interrupted => CheckOutcome::Interrupted,
    }
}

fn verify_output_state(
    store: &Store,
    project: &Project,
    run_id: RunId,
    output: &CheckRunOutput,
) -> CheckFreshness {
    let id = match output
        .workspace_state
        .snapshot_artifact
        .id
        .parse::<ArtifactId>()
    {
        Ok(id) => id,
        Err(error) => return unverifiable(format!("invalid snapshot artifact identity: {error}")),
    };
    let artifact = match store.artifact(id) {
        Ok(artifact) => artifact,
        Err(error) => return unverifiable(error.to_string()),
    };
    if artifact.run_id() != run_id {
        return unverifiable("snapshot artifact belongs to another run".to_owned());
    }
    if artifact.availability() != Availability::Available {
        return unverifiable(format!("snapshot artifact is {}", artifact.availability()));
    }
    let file = match store.open_artifact(id) {
        Ok(file) => file,
        Err(error) => return unverifiable(error.to_string()),
    };
    let wire = match serde_json::from_reader::<_, SnapshotWire>(file) {
        Ok(wire) => wire,
        Err(error) => return unverifiable(format!("invalid snapshot artifact: {error}")),
    };
    let snapshot = match WorkspaceSnapshot::try_from(wire) {
        Ok(snapshot) => snapshot,
        Err(error) => return unverifiable(error.to_string()),
    };
    if snapshot.digest().to_string() != output.workspace_state.digest {
        return unverifiable("snapshot artifact disagrees with the recorded digest".to_owned());
    }
    let git = GitService::new(&project.root, store.data_dir());
    let probe = FilesystemProbe::new(&project.root);
    match snapshot.verify(&git, &probe, &Cancellation::default()) {
        Ok(FreshnessState::Fresh) => CheckFreshness::Current,
        Ok(FreshnessState::Stale { changed }) => CheckFreshness::Stale {
            changed: changed
                .into_iter()
                .take(MAX_CHECK_DIAGNOSTICS)
                .map(|changed| {
                    changed
                        .path
                        .map_or_else(|| changed.component.to_string(), |path| path.display())
                })
                .collect(),
        },
        Ok(FreshnessState::Unverifiable { reason }) => {
            unverifiable(format!("{reason:?}").to_lowercase())
        }
        Err(error) => unverifiable(error.to_string()),
        _ => unverifiable("unknown freshness state".to_owned()),
    }
}

fn parse_diagnostics(
    store: &Store,
    project: &Project,
    run_id: RunId,
    output: &CheckRunOutput,
) -> (Vec<CheckDiagnostic>, usize) {
    if output.parser != CheckOutputParser::CargoJson {
        return (Vec::new(), 0);
    }
    let mut diagnostics = Vec::new();
    let mut omitted = 0usize;
    let artifacts = match store.run_artifacts(run_id) {
        Ok(artifacts) => artifacts,
        Err(_) => return (diagnostics, omitted),
    };
    for artifact in artifacts {
        if artifact.name() != "check-stdout.log" && artifact.name() != "check-stderr.log" {
            continue;
        }
        let Ok(file) = store.open_artifact(artifact.id()) else {
            continue;
        };
        parse_cargo_stream(
            BufReader::new(file),
            &project.root,
            &mut diagnostics,
            &mut omitted,
        );
    }
    (diagnostics, omitted)
}

fn parse_cargo_stream(
    mut reader: impl BufRead,
    root: &Path,
    diagnostics: &mut Vec<CheckDiagnostic>,
    omitted: &mut usize,
) {
    let mut line = Vec::new();
    loop {
        line.clear();
        let Ok(read) = reader.read_until(b'\n', &mut line) else {
            break;
        };
        if read == 0 {
            break;
        }
        if line.len() > MAX_DIAGNOSTIC_LINE_BYTES {
            *omitted = omitted.saturating_add(1);
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if value.get("reason").and_then(Value::as_str) != Some("compiler-message") {
            continue;
        }
        let Some(message) = value.get("message") else {
            continue;
        };
        if diagnostics.len() >= MAX_CHECK_DIAGNOSTICS {
            *omitted = omitted.saturating_add(1);
            continue;
        }
        let span = message
            .get("spans")
            .and_then(Value::as_array)
            .and_then(|spans| {
                spans
                    .iter()
                    .find(|span| span.get("is_primary").and_then(Value::as_bool) == Some(true))
            });
        let path = span
            .and_then(|span| span.get("file_name"))
            .and_then(Value::as_str)
            .and_then(|path| contained_display_path(root, path));
        diagnostics.push(CheckDiagnostic {
            path,
            line: span
                .and_then(|span| span.get("line_start"))
                .and_then(Value::as_u64)
                .and_then(|line| u32::try_from(line).ok()),
            column: span
                .and_then(|span| span.get("column_start"))
                .and_then(Value::as_u64)
                .and_then(|column| u32::try_from(column).ok()),
            level: bounded_text(
                message
                    .get("level")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
            ),
            message: bounded_text(
                message
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("compiler diagnostic"),
            ),
        });
    }
}

fn contained_display_path(root: &Path, raw: &str) -> Option<String> {
    let path = PathBuf::from(raw);
    let relative = if path.is_absolute() {
        path.strip_prefix(root).ok()?.to_path_buf()
    } else {
        path
    };
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return None;
    }
    // Git and the review surfaces publish repository paths with `/` on every
    // platform. Cargo may spell an absolute Windows path with `\`, so normalize
    // only after containment has been proved with native `Path` semantics.
    Some(relative.to_string_lossy().replace('\\', "/"))
}

fn text_field(value: &Value, key: &str, fallback: &str) -> String {
    bounded_text(value.get(key).and_then(Value::as_str).unwrap_or(fallback))
}

fn bounded_text(value: &str) -> String {
    let mut end = value.len().min(MAX_DIAGNOSTIC_TEXT_BYTES);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end]
        .chars()
        .map(|character| {
            if character.is_control() && !matches!(character, '\n' | '\t') {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn timestamp(value: time::OffsetDateTime) -> String {
    value
        .format(&Rfc3339)
        .expect("UTC runtime timestamps are RFC 3339 representable")
}

fn unverifiable(reason: String) -> CheckFreshness {
    CheckFreshness::Unverifiable {
        reason: bounded_text(&reason),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Cursor;
    use std::path::Path;
    use std::sync::Arc;

    use harkness_core::{CheckConfiguration, CheckParser, Project, ProjectId, ProjectSource};
    use harkness_git::Cancellation;
    use harkness_test_fixtures::initialize_repository;
    use tempfile::tempdir;
    use time::OffsetDateTime;

    use crate::approval::DecidedVia;
    use crate::store::Store;
    use crate::trust::{TrustState, WorkspaceTrust};

    use super::{
        CheckDiagnostic, CheckFreshness, CheckOutcome, parse_cargo_stream, project_checks,
        run_configured_check,
    };

    #[test]
    fn cargo_json_associates_only_primary_contained_spans() {
        let input = concat!(
            r#"{"reason":"compiler-message","message":{"level":"error","message":"bad <tag>\u0007","spans":[{"file_name":"src/lib.rs","line_start":7,"column_start":4,"is_primary":true}]}}"#,
            "\n",
            r#"{"reason":"compiler-message","message":{"level":"warning","message":"outside","spans":[{"file_name":"../outside.rs","line_start":1,"column_start":1,"is_primary":true}]}}"#,
            "\n",
            r#"{"reason":"compiler-artifact"}"#,
            "\n",
        );
        let mut diagnostics = Vec::<CheckDiagnostic>::new();
        let mut omitted = 0;

        parse_cargo_stream(
            Cursor::new(input.as_bytes()),
            Path::new("/workspace"),
            &mut diagnostics,
            &mut omitted,
        );

        assert_eq!(diagnostics.len(), 2);
        assert_eq!(diagnostics[0].path.as_deref(), Some("src/lib.rs"));
        assert_eq!(diagnostics[0].line, Some(7));
        assert_eq!(diagnostics[0].column, Some(4));
        assert_eq!(diagnostics[0].message, "bad <tag> ");
        assert_eq!(diagnostics[1].path, None);
        assert_eq!(omitted, 0);
    }

    #[test]
    fn absolute_paths_are_associated_only_below_the_workspace() {
        let workspace = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let contained = workspace.path().join("src").join("main.rs");
        let escaped = outside.path().join("main.rs");
        assert_eq!(
            super::contained_display_path(workspace.path(), &contained.to_string_lossy())
                .as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(
            super::contained_display_path(workspace.path(), &escaped.to_string_lossy()),
            None
        );
    }

    #[test]
    fn configured_check_uses_runtime_records_artifacts_and_composite_freshness() {
        let workspace = tempdir().unwrap();
        let data = tempdir().unwrap();
        initialize_repository(workspace.path());
        let project = Project {
            id: ProjectId::new(),
            display_name: "fixture".to_owned(),
            root: workspace.path().canonicalize().unwrap(),
            source: ProjectSource::Local,
            checks: None,
            last_opened: OffsetDateTime::UNIX_EPOCH,
            available: true,
            git: None,
        };
        let executable = std::env::current_exe().unwrap();
        let check = CheckConfiguration {
            id: "fixture".to_owned(),
            label: "Fixture".to_owned(),
            command: vec![
                executable.to_string_lossy().into_owned(),
                "--ignored".to_owned(),
                "--exact".to_owned(),
                "scenario_process_fixture_pass_child".to_owned(),
                "--nocapture".to_owned(),
            ],
            cwd: None,
            env: BTreeMap::new(),
            parser: CheckParser::Plain,
            timeout_seconds: Some(10),
        };
        let store = Arc::new(Store::open(data.path()).unwrap());
        store
            .put_workspace_trust(
                &WorkspaceTrust::decide(
                    project.id,
                    &project.root,
                    TrustState::Trusted,
                    OffsetDateTime::now_utc(),
                )
                .unwrap(),
            )
            .unwrap();

        let run = run_configured_check(
            Arc::clone(&store),
            &project,
            &check,
            DecidedVia::Cli,
            &Cancellation::default(),
        )
        .unwrap();
        let summaries = project_checks(&store, &project).unwrap();

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].run_id, run.to_string());
        assert_eq!(summaries[0].outcome, CheckOutcome::Passed);
        assert_eq!(summaries[0].freshness, CheckFreshness::Current);
        assert!(
            summaries[0]
                .stdout_tail
                .contains(harkness_test_fixtures::SCENARIO_FIXTURE_PASS_MARKER)
        );
        let artifacts = store.run_artifacts(run).unwrap();
        assert!(
            artifacts
                .iter()
                .any(|artifact| artifact.name() == "check-workspace-state.json")
        );
        assert!(
            artifacts
                .iter()
                .any(|artifact| artifact.name() == "check-stdout.log")
        );

        std::fs::write(workspace.path().join("after-check.txt"), "changed\n").unwrap();
        let stale = project_checks(&store, &project).unwrap();
        assert!(matches!(stale[0].freshness, CheckFreshness::Stale { .. }));
    }
}
