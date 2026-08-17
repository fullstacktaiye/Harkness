//! Read-only projection of state-bound `check.run` calls.
//!
//! The projection never executes a command and never takes a repository lock.
//! It reads ordinary run, step, tool-call, and artifact records, verifies the
//! stored composite workspace snapshot through `harkness-context`, and parses
//! machine output with strict memory and record-count bounds.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, BufRead, BufReader};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use harkness_context::{
    FilesystemProbe, FreshnessState, SnapshotWire, WorkspaceReading, WorkspaceSnapshot,
};
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
use crate::domain::{ArtifactId, RunId, Task, ToolCallId, ToolCallState};
use crate::policy::PolicyEngine;
use crate::store::{Availability, Store, StoreError};
use crate::tool::{RegistryError, ToolRegistry, WorkspaceMetadata};
use crate::tools::{
    CHECK_SNAPSHOT_ARTIFACT_SCHEMA_VERSION, CheckOutputParser, CheckRun, CheckRunInput,
    CheckRunOutput, CheckSnapshotArtifact,
};

/// Most diagnostics retained from one check.
pub const MAX_CHECK_DIAGNOSTICS: usize = 200;
/// Largest one machine-output line parsed in memory.
pub const MAX_DIAGNOSTIC_LINE_BYTES: usize = 64 * 1024;
/// Most machine-output bytes inspected during one check refresh.
pub const MAX_DIAGNOSTIC_SCAN_BYTES: usize = 8 * 1024 * 1024;
/// Most machine-output records inspected during one check refresh.
pub const MAX_DIAGNOSTIC_SCAN_LINES: usize = 10_000;
/// Longest user-facing diagnostic text retained inline.
pub const MAX_DIAGNOSTIC_TEXT_BYTES: usize = 4 * 1024;
/// How long a cancelled check is waited on before its caller is released.
const CANCELLATION_GRACE: Duration = Duration::from_secs(30);

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
    /// The configuration names an environment variable `check.run` cannot set.
    #[error(
        "project check {check} configures environment variable {name}, which check.run does not \
         declare; only {baseline} may be overridden"
    )]
    UndeclaredEnvironment {
        /// Configured check identifier.
        check: String,
        /// The refused name, as configured.
        name: String,
        /// The names that can be set, for the message.
        baseline: String,
    },
}

/// Builds the coordinator a caller runs project checks through.
///
/// One of these belongs to the *process*, not to a call. `RunCoordinator::new`
/// constructs a `Scheduler`, and the scheduler is what serializes mutating calls
/// per workspace and caps child processes across all of them. A coordinator
/// built per check therefore shares a scheduler with nothing: two checks started
/// for two different projects, or a check started beside any other coordinator
/// already live, would be serialized against nothing and could exceed the global
/// child-process limit together. Building the registry and reloading user policy
/// per call was the smaller cost of the same mistake.
///
/// Policy is loaded once here for the user layer. The coordinator re-specializes
/// it per run against that run's own workspace, so one coordinator serves every
/// project in the process.
///
/// # Errors
///
/// Returns [`RegistryError`] when `check.run` cannot be declared.
pub fn check_coordinator(store: Arc<Store>) -> Result<RunCoordinator, CheckLaunchError> {
    let mut registry = ToolRegistry::new();
    registry.register(CheckRun)?;
    let policy = PolicyEngine::load(store.data_dir(), store.data_dir());
    Ok(RunCoordinator::new(store, Arc::new(registry), policy))
}

/// Runs one configured check through the ordinary coordinator, policy,
/// approval, scheduler, tool, event-log, and artifact-store path.
///
/// The caller is the user-facing approval surface. It must only call this
/// function after an explicit action, and the workspace must already carry a
/// separate positive trust decision. `via` is persisted on the exact-call
/// approval this function grants once the coordinator publishes it.
///
/// `coordinator` is the process's own — see [`check_coordinator`] — so that
/// every check in this process shares one scheduler and the caps it enforces.
pub fn run_configured_check(
    coordinator: &RunCoordinator,
    project: &Project,
    check: &CheckConfiguration,
    via: DecidedVia,
    cancellation: &Cancellation,
) -> Result<RunId, CheckLaunchError> {
    // Refused here rather than at spawn. A tool descriptor publishes the
    // environment names it may set, so an input map cannot widen the authority
    // behind an already-approved tool identity — and `check.run` declares none
    // beyond the baseline. A catalog may still carry any name, so a check
    // configuring one used to reach the supervisor, fail inside
    // `AllowlistedEnv::apply_overrides`, and surface as a generic
    // `execution_failed` after a run, a task and an approval had been recorded.
    // Saying so before any of that exists names the variable and the limit.
    if let Some(name) = check
        .env
        .keys()
        .find(|name| !crate::trust::BASELINE_ENVIRONMENT.contains(&name.as_str()))
    {
        return Err(CheckLaunchError::UndeclaredEnvironment {
            check: check.id.clone(),
            name: name.clone(),
            baseline: crate::trust::BASELINE_ENVIRONMENT.join(", "),
        });
    }
    let task = Task::new(
        format!("Check: {}", check.label),
        &project.root,
        Some(project.id),
        OffsetDateTime::now_utc(),
    );
    // Built with the store's own redactor, not `PassThrough`. The coordinator
    // validates this reference against `WorkspaceRef::from_task(&task,
    // store.redactor())` and refuses the run when either field differs, so a
    // store opened with a redactor that rewrites any part of the workspace-root
    // text — which is what a redactor is for — rejected every check before it
    // started.
    let workspace = WorkspaceRef::from_task(&task, &**coordinator.store().redactor());
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

    let mut cancel_requested_at = None;
    loop {
        let snapshot = coordinator.run_snapshot(run_id)?;
        if snapshot.run.state().is_terminal() {
            return Ok(run_id);
        }
        if cancellation.is_cancelled() {
            // Asked once, then waited for. Returning the moment cancellation was
            // requested handed back a run that had not stopped: the child was
            // still executing, and a single-shot caller exiting there left it
            // running with nothing to reap it. The cancellation chain reaches the
            // child's process group, so this waits for that to land instead of
            // racing it.
            match cancel_requested_at {
                None => {
                    coordinator.cancel_run(run_id)?;
                    cancel_requested_at = Some(Instant::now());
                }
                // The teardown below is itself bounded, so passing this means
                // something is wedged. Releasing the caller beats hanging it; the
                // run's own recorded state still says it never became terminal.
                Some(requested) if requested.elapsed() >= CANCELLATION_GRACE => {
                    return Ok(run_id);
                }
                Some(_) => {}
            }
            thread::sleep(Duration::from_millis(10));
            continue;
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
        thread::sleep(Duration::from_millis(10));
    }
}

/// How strongly Harkness can attest to a recorded check.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityClass {
    /// Harkness executed the typed tool and directly observed its result.
    HarknessObserved,
    /// Harkness performed work requested by an external party.
    HarknessMediated,
    /// An ACP integration reported the activity.
    AcpReported,
    /// The activity was inferred from a captured workspace state.
    SnapshotInferred,
    /// Harkness has no evidence for how the activity occurred.
    Unobserved,
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
    pub recorded_cwd: Option<String>,
    pub recorded_env: BTreeMap<String, String>,
    pub recorded_timeout: Option<u64>,
    pub recorded_parser: String,
    pub definition_current: bool,
    pub outcome: CheckOutcome,
    pub evidence_class: ActivityClass,
    pub created_at: String,
    pub finished_at: Option<String>,
    pub duration_ms: Option<u64>,
    pub state_digest: Option<String>,
    pub state_head: Option<String>,
    pub workspace_clean: Option<bool>,
    pub workspace_matches_index: Option<bool>,
    pub freshness: CheckFreshness,
    pub diagnostics: Vec<CheckDiagnostic>,
    pub diagnostics_omitted: usize,
    pub diagnostics_scan_truncated: bool,
    /// Why stored output could not be inspected, when it could not be.
    ///
    /// Absent means the streams were read. Present means the diagnostics above
    /// are not the whole answer and no count can say how much is missing —
    /// distinct from `diagnostics_omitted`, which counts records this build
    /// chose to drop from output it did read.
    pub diagnostics_unavailable: Option<String>,
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub artifact_byte_limit: u64,
    pub stdout_artifact_truncated: bool,
    pub stderr_artifact_truncated: bool,
}

/// Builds the newest recorded check summaries for one project.
pub fn project_checks(store: &Store, project: &Project) -> Result<Vec<CheckSummary>, StoreError> {
    let mut summaries = Vec::new();
    let configured = project.effective_checks();
    let by_id = configured
        .iter()
        .map(|check| (check.id.as_str(), check))
        .collect::<HashMap<_, _>>();
    let ids = configured
        .iter()
        .map(|check| check.id.clone())
        .collect::<Vec<_>>();
    // One read of the workspace for the whole projection.
    //
    // Freshness is "does this recorded snapshot still describe the workspace",
    // and answering it needs the workspace read once — a `git status` plus a
    // hash of everything dirty or untracked. Asking each snapshot to verify
    // itself repeated that read once per configured check, so opening this panel
    // on a project with the three Cargo defaults paid for three full
    // verifications of one unchanged workspace, and a 32-check project paid for
    // 32, on every open and every refresh.
    //
    // Lazy because a project whose checks have never run needs no read at all,
    // and deliberately not cached beyond this call: a projection taken now must
    // describe the workspace now.
    let mut reading = None;
    for call_id in store.project_latest_check_call_ids(project.id, &ids)? {
        let call = store.load_tool_call(call_id)?;
        let run_id = call.run_id();
        let parsed_input = serde_json::from_value::<CheckRunInput>(call.input().clone());
        let input = parsed_input.as_ref().ok();
        let parsed_output = call
            .output()
            .cloned()
            .map(serde_json::from_value::<CheckRunOutput>)
            .transpose();
        let (output, invalid_output) = match parsed_output {
            Ok(output) => (output, None),
            Err(error) => (None, Some(error.to_string())),
        };
        let check_id = input.map_or_else(
            || text_field(call.input(), "check_id", "unknown"),
            |input| input.check_id.clone(),
        );
        let Some(current) = by_id.get(check_id.as_str()) else {
            continue;
        };
        // Every recorded string is bounded and stripped of control characters,
        // including the ones that came from the call's own input. `check.run` is
        // registered as a mutating tool, so an agent can invoke it directly with
        // a label or an argv of its choosing; nothing downstream of this
        // projection re-checks them, and the front ends render what they are
        // given.
        let label = output.as_ref().map_or_else(
            || input.map_or_else(|| current.label.clone(), |input| input.label.clone()),
            |output| output.label.clone(),
        );
        let label = bounded_text(&label);
        let command = input.map_or_else(Vec::new, |input| {
            input
                .command
                .iter()
                .map(|part| bounded_text(part))
                .collect()
        });
        let recorded_cwd = input
            .and_then(|input| input.cwd.as_deref())
            .map(bounded_text);
        let recorded_env = input
            .and_then(|input| input.env.as_ref())
            .map(|env| {
                env.iter()
                    .map(|(name, value)| (bounded_text(name), bounded_text(value)))
                    .collect()
            })
            .unwrap_or_default();
        let recorded_timeout = input.and_then(|input| input.timeout_seconds);
        let recorded_parser = input
            .map(|input| parser_name(input.parser).to_owned())
            .unwrap_or_else(|| "unknown".to_owned());
        let definition_current = input.is_some_and(|input| definition_matches(input, current));
        let (state, parsed_diagnostics) = match output.as_ref() {
            Some(output) => (
                verify_output_state(store, project, run_id, call.id(), output, &mut reading),
                parse_diagnostics(store, project, run_id, call.id(), input, output),
            ),
            None => (
                StateVerification {
                    freshness: CheckFreshness::Unverifiable {
                        reason: invalid_output.unwrap_or_else(|| {
                            "the check has not recorded a workspace state yet".to_owned()
                        }),
                    },
                    workspace_clean: None,
                    workspace_matches_index: None,
                },
                DiagnosticProjection::default(),
            ),
        };
        let outcome = outcome(call.state(), output.as_ref());
        summaries.push(CheckSummary {
            run_id: run_id.to_string(),
            check_id,
            label,
            command,
            recorded_cwd,
            recorded_env,
            recorded_timeout,
            recorded_parser,
            definition_current,
            outcome,
            evidence_class: ActivityClass::HarknessObserved,
            created_at: timestamp(call.created_at()),
            finished_at: call.finished_at().map(timestamp),
            duration_ms: output.as_ref().map(|output| output.process.duration_ms),
            state_digest: output
                .as_ref()
                .map(|output| output.workspace_state.digest.clone()),
            state_head: output
                .as_ref()
                .and_then(|output| output.workspace_state.head.clone()),
            workspace_clean: state.workspace_clean,
            workspace_matches_index: state.workspace_matches_index,
            freshness: state.freshness,
            diagnostics: parsed_diagnostics.diagnostics,
            diagnostics_omitted: parsed_diagnostics.omitted,
            diagnostics_scan_truncated: parsed_diagnostics.scan_truncated,
            diagnostics_unavailable: parsed_diagnostics.unavailable,
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
            artifact_byte_limit: output
                .as_ref()
                .map_or(0, |output| output.artifact_byte_limit),
            stdout_artifact_truncated: output
                .as_ref()
                .is_some_and(|output| output.stdout_artifact_truncated),
            stderr_artifact_truncated: output
                .as_ref()
                .is_some_and(|output| output.stderr_artifact_truncated),
        });
    }
    Ok(summaries)
}

/// Whether the recorded call is the configured check, in every field the
/// configuration names.
///
/// The label is one of those fields. Leaving it out let a call reproduce the
/// configured invocation exactly, store an arbitrary label beside it, and still
/// be reported as `definition_current` — so the name a reader identifies the
/// evidence by was the one field an agent could choose freely.
fn definition_matches(input: &CheckRunInput, configured: &CheckConfiguration) -> bool {
    input.check_id == configured.id
        && input.label == configured.label
        && input.command == configured.command
        && input.cwd == configured.cwd
        && input.env.as_ref().cloned().unwrap_or_default() == configured.env
        && input.timeout_seconds == configured.timeout_seconds
        && matches!(
            (input.parser, configured.parser),
            (CheckOutputParser::Plain, CheckParser::Plain)
                | (CheckOutputParser::CargoJson, CheckParser::CargoJson)
        )
}

const fn parser_name(parser: CheckOutputParser) -> &'static str {
    match parser {
        CheckOutputParser::Plain => "plain",
        CheckOutputParser::CargoJson => "cargo_json",
    }
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
    call_id: ToolCallId,
    output: &CheckRunOutput,
    reading: &mut Option<WorkspaceReading>,
) -> StateVerification {
    let failed = |reason: String| StateVerification {
        freshness: unverifiable(reason),
        workspace_clean: None,
        workspace_matches_index: None,
    };
    let id = match output
        .workspace_state
        .snapshot_artifact
        .id
        .parse::<ArtifactId>()
    {
        Ok(id) => id,
        Err(error) => {
            return failed(format!("invalid snapshot artifact identity: {error}"));
        }
    };
    let artifact = match store.artifact(id) {
        Ok(artifact) => artifact,
        Err(error) => return failed(error.to_string()),
    };
    if artifact.run_id() != run_id {
        return failed("snapshot artifact belongs to another run".to_owned());
    }
    if artifact.tool_call_id() != Some(call_id) {
        return failed("snapshot artifact belongs to another tool call".to_owned());
    }
    if artifact.availability() != Availability::Available {
        return failed(format!("snapshot artifact is {}", artifact.availability()));
    }
    let file = match store.open_artifact(id) {
        Ok(file) => file,
        Err(error) => return failed(error.to_string()),
    };
    let encoded = match serde_json::from_reader::<_, CheckSnapshotArtifact>(file) {
        Ok(encoded) => encoded,
        Err(error) => return failed(format!("invalid snapshot artifact: {error}")),
    };
    if encoded.schema_version != CHECK_SNAPSHOT_ARTIFACT_SCHEMA_VERSION {
        return failed(format!(
            "snapshot artifact schema version {} is unsupported",
            encoded.schema_version
        ));
    }
    let snapshot_json = match BASE64.decode(&encoded.snapshot_json_base64) {
        Ok(bytes) => bytes,
        Err(error) => return failed(format!("invalid encoded snapshot: {error}")),
    };
    let wire = match serde_json::from_slice::<SnapshotWire>(&snapshot_json) {
        Ok(wire) => wire,
        Err(error) => return failed(format!("invalid encoded snapshot: {error}")),
    };
    let snapshot = match WorkspaceSnapshot::try_from(wire) {
        Ok(snapshot) => snapshot,
        Err(error) => return failed(error.to_string()),
    };
    if snapshot.digest().to_string() != output.workspace_state.digest {
        return failed("snapshot artifact disagrees with the recorded digest".to_owned());
    }
    let workspace_clean = snapshot.files().is_empty();
    let workspace_matches_index =
        snapshot.files().tracked_dirty().is_empty() && snapshot.files().untracked().is_empty();
    let workspace = match reading {
        Some(reading) => reading,
        None => {
            let git = GitService::new(&project.root, store.data_dir());
            let probe = FilesystemProbe::new(&project.root);
            match WorkspaceReading::capture(&git, &probe, &Cancellation::default()) {
                Ok(captured) => reading.insert(captured),
                Err(error) => return failed(error.to_string()),
            }
        }
    };
    let freshness = match snapshot.verify_against(workspace) {
        FreshnessState::Fresh => CheckFreshness::Current,
        FreshnessState::Stale { changed } => CheckFreshness::Stale {
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
        FreshnessState::Unverifiable { reason } => unverifiable(reason.as_str().to_owned()),
        _ => unverifiable("unknown freshness state".to_owned()),
    };
    StateVerification {
        freshness,
        workspace_clean: Some(workspace_clean),
        workspace_matches_index: Some(workspace_matches_index),
    }
}

struct StateVerification {
    freshness: CheckFreshness,
    workspace_clean: Option<bool>,
    workspace_matches_index: Option<bool>,
}

fn parse_diagnostics(
    store: &Store,
    project: &Project,
    run_id: RunId,
    call_id: ToolCallId,
    input: Option<&CheckRunInput>,
    output: &CheckRunOutput,
) -> DiagnosticProjection {
    if output.parser != CheckOutputParser::CargoJson {
        return DiagnosticProjection::default();
    }
    let mut projection = DiagnosticProjection::default();
    let artifact_truncated = output.stdout_artifact_truncated || output.stderr_artifact_truncated;
    let execution_root =
        execution_root(&project.root, input.and_then(|input| input.cwd.as_deref()));
    for reference in [
        &output.process.stdout_artifact,
        &output.process.stderr_artifact,
    ] {
        // Every way of losing a stream is recorded rather than skipped. Silently
        // continuing reported a pruned or unreadable artifact as "no
        // diagnostics" with nothing omitted and no truncation — the same answer
        // as a clean run, from a check that may have found forty errors. The
        // freshness verification directly above says `unverifiable` for exactly
        // these failures, and this says the equivalent.
        let id = match reference.id.parse::<ArtifactId>() {
            Ok(id) => id,
            Err(error) => {
                projection.record_unavailable(format!("invalid artifact identity: {error}"));
                continue;
            }
        };
        let artifact = match store.artifact(id) {
            Ok(artifact) => artifact,
            Err(error) => {
                projection.record_unavailable(error.to_string());
                continue;
            }
        };
        if artifact.run_id() != run_id || artifact.tool_call_id() != Some(call_id) {
            projection.record_unavailable("output artifact belongs to another call".to_owned());
            continue;
        }
        if artifact.availability() != Availability::Available {
            projection
                .record_unavailable(format!("output artifact is {}", artifact.availability()));
            continue;
        }
        let file = match store.open_artifact(id) {
            Ok(file) => file,
            Err(error) => {
                projection.record_unavailable(error.to_string());
                continue;
            }
        };
        parse_cargo_stream(
            BufReader::new(file),
            &project.root,
            execution_root.as_deref(),
            &mut projection,
        );
        if projection.scan_truncated {
            break;
        }
    }
    projection.scan_truncated |= artifact_truncated;
    projection
}

fn parse_cargo_stream(
    mut reader: impl BufRead,
    root: &Path,
    execution_root: Option<&Path>,
    projection: &mut DiagnosticProjection,
) {
    while projection.scanned_bytes < MAX_DIAGNOSTIC_SCAN_BYTES
        && projection.scanned_lines < MAX_DIAGNOSTIC_SCAN_LINES
    {
        let remaining = MAX_DIAGNOSTIC_SCAN_BYTES - projection.scanned_bytes;
        let line = match read_bounded_line(&mut reader, remaining) {
            Ok(Some(line)) => line,
            // End of stream is the ordinary exit. A read error is not: it means
            // the rest of this stream was never inspected, and saying nothing
            // would report whatever was found so far as the whole answer.
            Ok(None) => break,
            Err(error) => {
                projection.record_unavailable(format!("output artifact read failed: {error}"));
                break;
            }
        };
        projection.scanned_bytes = projection.scanned_bytes.saturating_add(line.consumed);
        projection.scanned_lines = projection.scanned_lines.saturating_add(1);
        if line.oversized {
            projection.omitted = projection.omitted.saturating_add(1);
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&line.bytes) else {
            continue;
        };
        if value.get("reason").and_then(Value::as_str) != Some("compiler-message") {
            continue;
        }
        let Some(message) = value.get("message") else {
            continue;
        };
        if projection.diagnostics.len() >= MAX_CHECK_DIAGNOSTICS {
            projection.omitted = projection.omitted.saturating_add(1);
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
            .and_then(|path| contained_display_path(root, execution_root, path));
        projection.diagnostics.push(CheckDiagnostic {
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
    if (projection.scanned_bytes >= MAX_DIAGNOSTIC_SCAN_BYTES
        || projection.scanned_lines >= MAX_DIAGNOSTIC_SCAN_LINES)
        && reader
            .fill_buf()
            .is_ok_and(|remaining| !remaining.is_empty())
    {
        projection.scan_truncated = true;
    }
}

#[derive(Default)]
struct DiagnosticProjection {
    diagnostics: Vec<CheckDiagnostic>,
    omitted: usize,
    scan_truncated: bool,
    scanned_bytes: usize,
    scanned_lines: usize,
    unavailable: Option<String>,
}

impl DiagnosticProjection {
    /// Records why a stream could not be read. The first reason is kept: it is
    /// the one that happened before any later failure it may have caused, and a
    /// reader needs one sentence rather than a list.
    fn record_unavailable(&mut self, reason: String) {
        if self.unavailable.is_none() {
            self.unavailable = Some(bounded_text(&reason));
        }
    }
}

struct BoundedLine {
    bytes: Vec<u8>,
    consumed: usize,
    oversized: bool,
}

fn read_bounded_line(
    reader: &mut impl BufRead,
    scan_remaining: usize,
) -> io::Result<Option<BoundedLine>> {
    let mut bytes = Vec::new();
    let mut consumed = 0usize;
    let mut oversized = false;
    let mut ended = false;
    while consumed < scan_remaining {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            ended = true;
            break;
        }
        let admitted = available.len().min(scan_remaining - consumed);
        let chunk = &available[..admitted];
        let through_newline = chunk
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(chunk.len(), |index| index + 1);
        let retained = MAX_DIAGNOSTIC_LINE_BYTES
            .saturating_add(1)
            .saturating_sub(bytes.len())
            .min(through_newline);
        bytes.extend_from_slice(&chunk[..retained]);
        if retained < through_newline || bytes.len() > MAX_DIAGNOSTIC_LINE_BYTES {
            oversized = true;
        }
        let line_ended = through_newline < chunk.len()
            || chunk.get(through_newline.wrapping_sub(1)) == Some(&b'\n');
        reader.consume(through_newline);
        consumed += through_newline;
        if line_ended {
            ended = true;
            break;
        }
    }
    if consumed == 0 && ended {
        return Ok(None);
    }
    if oversized {
        bytes.clear();
    }
    Ok(Some(BoundedLine {
        bytes,
        consumed,
        oversized,
    }))
}

fn execution_root(root: &Path, cwd: Option<&str>) -> Option<PathBuf> {
    let relative = Path::new(cwd.unwrap_or("."));
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return None;
    }
    Some(root.join(relative))
}

fn contained_display_path(root: &Path, execution_root: Option<&Path>, raw: &str) -> Option<String> {
    let path = PathBuf::from(raw);
    let relative = if path.is_absolute() {
        path.strip_prefix(root).ok()?.to_path_buf()
    } else {
        execution_root?
            .join(path)
            .strip_prefix(root)
            .ok()?
            .to_path_buf()
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
    use std::borrow::Cow;
    use std::collections::BTreeMap;
    use std::fmt;
    use std::io::{Cursor, Write};
    use std::path::Path;
    use std::sync::Arc;

    use harkness_core::{CheckConfiguration, CheckParser, Project, ProjectId, ProjectSource};
    use harkness_git::Cancellation;
    use harkness_test_fixtures::initialize_repository;
    use tempfile::tempdir;
    use time::OffsetDateTime;

    use crate::approval::DecidedVia;
    use crate::domain::{Run, Step, Task, ToolCall};
    use crate::store::{Redactor, Store};
    use crate::tool::ArtifactRef;
    use crate::tools::{
        BoundedText, CheckOutputParser, CheckRunOutput, CheckWorkspaceState, ProcessExecOutput,
    };
    use crate::trust::{TrustState, WorkspaceTrust};

    use super::{
        CheckFreshness, CheckLaunchError, CheckOutcome, check_coordinator, parse_cargo_stream,
        project_checks, run_configured_check,
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
        let mut projection = super::DiagnosticProjection::default();

        parse_cargo_stream(
            Cursor::new(input.as_bytes()),
            Path::new("/workspace"),
            Some(Path::new("/workspace")),
            &mut projection,
        );

        assert_eq!(projection.diagnostics.len(), 2);
        assert_eq!(
            projection.diagnostics[0].path.as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(projection.diagnostics[0].line, Some(7));
        assert_eq!(projection.diagnostics[0].column, Some(4));
        assert_eq!(projection.diagnostics[0].message, "bad <tag> ");
        assert_eq!(projection.diagnostics[1].path, None);
        assert_eq!(projection.omitted, 0);
    }

    #[test]
    fn absolute_paths_are_associated_only_below_the_workspace() {
        let workspace = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let contained = workspace.path().join("src").join("main.rs");
        let escaped = outside.path().join("main.rs");
        assert_eq!(
            super::contained_display_path(
                workspace.path(),
                Some(workspace.path()),
                &contained.to_string_lossy(),
            )
            .as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(
            super::contained_display_path(
                workspace.path(),
                Some(workspace.path()),
                &escaped.to_string_lossy(),
            ),
            None
        );
    }

    #[test]
    fn cargo_paths_are_resolved_from_the_recorded_working_directory() {
        assert_eq!(
            super::contained_display_path(
                Path::new("/workspace"),
                Some(Path::new("/workspace/frontend")),
                "src/lib.rs",
            )
            .as_deref(),
            Some("frontend/src/lib.rs")
        );
    }

    #[test]
    fn a_huge_newline_free_record_is_discarded_with_bounded_retention() {
        let input = vec![b'x'; super::MAX_DIAGNOSTIC_LINE_BYTES * 4];
        let mut projection = super::DiagnosticProjection::default();

        parse_cargo_stream(
            Cursor::new(input),
            Path::new("/workspace"),
            Some(Path::new("/workspace")),
            &mut projection,
        );

        assert!(projection.diagnostics.is_empty());
        assert_eq!(projection.omitted, 1);
        assert!(!projection.scan_truncated);
    }

    #[test]
    fn diagnostic_scanning_stops_at_a_named_total_byte_budget() {
        let input = vec![b'x'; super::MAX_DIAGNOSTIC_SCAN_BYTES + 1];
        let mut projection = super::DiagnosticProjection::default();

        parse_cargo_stream(
            Cursor::new(input),
            Path::new("/workspace"),
            Some(Path::new("/workspace")),
            &mut projection,
        );

        assert_eq!(projection.scanned_bytes, super::MAX_DIAGNOSTIC_SCAN_BYTES);
        assert!(projection.scan_truncated);
    }

    #[test]
    fn diagnostics_are_read_only_from_the_exact_tool_call_artifacts() {
        let workspace = tempdir().unwrap();
        let data = tempdir().unwrap();
        initialize_repository(workspace.path());
        let project = Project {
            id: ProjectId::new(),
            display_name: "fixture".to_owned(),
            root: workspace.path().canonicalize().unwrap(),
            source: ProjectSource::Local,
            checks: Some(Vec::new()),
            last_opened: OffsetDateTime::UNIX_EPOCH,
            available: true,
            git: None,
        };
        let store = Store::open(data.path()).unwrap();
        let now = OffsetDateTime::now_utc();
        let task = Task::new("fixture", &project.root, Some(project.id), now);
        store.insert_task(&task).unwrap();
        let run = Run::new(task.id(), now);
        store.insert_run(&run).unwrap();
        let step = Step::new(run.id(), 0, "fixture", now);
        store.insert_step(&step).unwrap();
        let input = serde_json::json!({"check_id":"one"});
        let first = ToolCall::new(&step, "check.run", "1.0.0", input.clone(), now);
        let second = ToolCall::new(&step, "check.run", "1.0.0", input, now);
        store.insert_tool_call(&first).unwrap();
        store.insert_tool_call(&second).unwrap();
        let first_stdout = stored_call_artifact(
            &store,
            run.id(),
            first.id(),
            cargo_diagnostic("src/first.rs").as_bytes(),
        );
        let first_stderr = stored_call_artifact(&store, run.id(), first.id(), b"");
        let _second_stdout = stored_call_artifact(
            &store,
            run.id(),
            second.id(),
            cargo_diagnostic("src/second.rs").as_bytes(),
        );
        let output = diagnostic_output(first_stdout, first_stderr);

        let projection =
            super::parse_diagnostics(&store, &project, run.id(), first.id(), None, &output);

        assert_eq!(projection.diagnostics.len(), 1);
        assert_eq!(
            projection.diagnostics[0].path.as_deref(),
            Some("src/first.rs")
        );
    }

    fn stored_call_artifact(
        store: &Store,
        run_id: crate::domain::RunId,
        call_id: crate::domain::ToolCallId,
        content: &[u8],
    ) -> ArtifactRef {
        let mut sink = store
            .create_artifact(
                run_id,
                "check-stdout.log",
                "text/plain",
                OffsetDateTime::now_utc(),
            )
            .unwrap()
            .for_tool_call(call_id);
        sink.write_all(content).unwrap();
        let artifact = sink.finish().unwrap();
        ArtifactRef {
            id: artifact.id().to_string(),
            media_type: artifact.media_type().to_owned(),
            byte_len: artifact.byte_size(),
        }
    }

    fn cargo_diagnostic(path: &str) -> String {
        format!(
            "{{\"reason\":\"compiler-message\",\"message\":{{\"level\":\"error\",\"message\":\"bad\",\"spans\":[{{\"file_name\":\"{path}\",\"line_start\":1,\"column_start\":1,\"is_primary\":true}}]}}}}\n"
        )
    }

    fn diagnostic_output(stdout: ArtifactRef, stderr: ArtifactRef) -> CheckRunOutput {
        CheckRunOutput {
            check_id: "one".to_owned(),
            label: "One".to_owned(),
            passed: false,
            parser: CheckOutputParser::CargoJson,
            workspace_state: CheckWorkspaceState {
                digest: "unused".to_owned(),
                head: None,
                branch: None,
                snapshot_artifact: stderr.clone(),
            },
            artifact_byte_limit: 0,
            stdout_artifact_truncated: false,
            stderr_artifact_truncated: false,
            process: ProcessExecOutput {
                exit_code: Some(1),
                signal: None,
                timed_out: false,
                timeout_seconds: 1,
                duration_ms: 1,
                stdout_tail: BoundedText {
                    text: String::new(),
                    byte_len: stdout.byte_len,
                    truncated: false,
                },
                stderr_tail: BoundedText {
                    text: String::new(),
                    byte_len: stderr.byte_len,
                    truncated: false,
                },
                stdout_artifact: stdout,
                stderr_artifact: stderr,
            },
        }
    }

    #[test]
    fn configured_check_uses_runtime_records_artifacts_and_composite_freshness() {
        let workspace = tempdir().unwrap();
        let data = tempdir().unwrap();
        initialize_repository(workspace.path());
        let mut project = Project {
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
        project.checks = Some(vec![check.clone()]);
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
            &check_coordinator(Arc::clone(&store)).unwrap(),
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

    #[test]
    fn redefining_one_id_does_not_relabel_old_evidence_as_current() {
        let workspace = tempdir().unwrap();
        let data = tempdir().unwrap();
        initialize_repository(workspace.path());
        let executable = std::env::current_exe().unwrap();
        let original = CheckConfiguration {
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
        let mut project = Project {
            id: ProjectId::new(),
            display_name: "fixture".to_owned(),
            root: workspace.path().canonicalize().unwrap(),
            source: ProjectSource::Local,
            checks: Some(vec![original.clone()]),
            last_opened: OffsetDateTime::UNIX_EPOCH,
            available: true,
            git: None,
        };
        let store = Arc::new(Store::open(data.path()).unwrap());
        trust(&store, &project);
        run_configured_check(
            &check_coordinator(Arc::clone(&store)).unwrap(),
            &project,
            &original,
            DecidedVia::Cli,
            &Cancellation::default(),
        )
        .unwrap();

        project.checks.as_mut().unwrap()[0].command = vec!["false".to_owned()];
        let summaries = project_checks(&store, &project).unwrap();

        assert_eq!(summaries.len(), 1);
        assert!(!summaries[0].definition_current);
        assert_eq!(summaries[0].command, original.command);
    }

    #[test]
    fn one_check_remains_visible_after_more_than_one_hundred_newer_runs_of_another() {
        let workspace = tempdir().unwrap();
        let data = tempdir().unwrap();
        initialize_repository(workspace.path());
        let executable = std::env::current_exe().unwrap();
        let command = vec![
            executable.to_string_lossy().into_owned(),
            "--ignored".to_owned(),
            "--exact".to_owned(),
            "scenario_process_fixture_pass_child".to_owned(),
            "--nocapture".to_owned(),
        ];
        let check = |id: &str| CheckConfiguration {
            id: id.to_owned(),
            label: id.to_owned(),
            command: command.clone(),
            cwd: None,
            env: BTreeMap::new(),
            parser: CheckParser::Plain,
            timeout_seconds: Some(10),
        };
        let frequent = check("frequent");
        let older = check("older");
        let project = Project {
            id: ProjectId::new(),
            display_name: "fixture".to_owned(),
            root: workspace.path().canonicalize().unwrap(),
            source: ProjectSource::Local,
            checks: Some(vec![frequent.clone(), older.clone()]),
            last_opened: OffsetDateTime::UNIX_EPOCH,
            available: true,
            git: None,
        };
        let store = Arc::new(Store::open(data.path()).unwrap());
        trust(&store, &project);
        run_configured_check(
            &check_coordinator(Arc::clone(&store)).unwrap(),
            &project,
            &older,
            DecidedVia::Cli,
            &Cancellation::default(),
        )
        .unwrap();
        let coordinator = check_coordinator(Arc::clone(&store)).unwrap();
        for _ in 0..101 {
            run_configured_check(
                &coordinator,
                &project,
                &frequent,
                DecidedVia::Cli,
                &Cancellation::default(),
            )
            .unwrap();
        }

        let ids = project_checks(&store, &project)
            .unwrap()
            .into_iter()
            .map(|summary| summary.check_id)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(ids, ["frequent".to_owned(), "older".to_owned()].into());
    }

    #[test]
    fn a_large_valid_catalog_definition_is_persisted_as_a_check_call() {
        let workspace = tempdir().unwrap();
        let data = tempdir().unwrap();
        initialize_repository(workspace.path());
        let mut command = vec![
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        ];
        command.extend((0..14).map(|_| "x".repeat(harkness_core::MAX_CHECK_TEXT_BYTES)));
        let check = CheckConfiguration {
            id: "large".to_owned(),
            label: "Large".to_owned(),
            command,
            cwd: None,
            env: BTreeMap::new(),
            parser: CheckParser::Plain,
            timeout_seconds: Some(10),
        };
        assert!(serde_json::to_vec(&check).unwrap().len() > 55 * 1024);
        assert_eq!(
            CheckConfiguration::validate_all(std::slice::from_ref(&check)),
            Ok(())
        );
        let project = Project {
            id: ProjectId::new(),
            display_name: "fixture".to_owned(),
            root: workspace.path().canonicalize().unwrap(),
            source: ProjectSource::Local,
            checks: Some(vec![check.clone()]),
            last_opened: OffsetDateTime::UNIX_EPOCH,
            available: true,
            git: None,
        };
        let store = Arc::new(Store::open(data.path()).unwrap());
        trust(&store, &project);

        let run = run_configured_check(
            &check_coordinator(Arc::clone(&store)).unwrap(),
            &project,
            &check,
            DecidedVia::Cli,
            &Cancellation::default(),
        )
        .unwrap();
        let summaries = project_checks(&store, &project).unwrap();

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].run_id, run.to_string());
        assert!(summaries[0].definition_current);
    }

    #[derive(Debug)]
    struct StreamChangingRedactor;

    impl Redactor for StreamChangingRedactor {
        fn redact_text<'a>(&self, text: &'a str) -> Cow<'a, str> {
            Cow::Borrowed(text)
        }

        fn wrap_stream(&self, sink: Box<dyn Write + Send>) -> Box<dyn Write + Send> {
            Box::new(StreamChangingWriter(sink))
        }
    }

    struct StreamChangingWriter(Box<dyn Write + Send>);

    impl fmt::Debug for StreamChangingWriter {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("StreamChangingWriter")
        }
    }

    impl Write for StreamChangingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.write_all(&vec![b'x'; bytes.len()])?;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.0.flush()
        }
    }

    #[test]
    fn snapshot_identity_survives_a_stream_changing_redactor() {
        let workspace = tempdir().unwrap();
        let data = tempdir().unwrap();
        initialize_repository(workspace.path());
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
        let project = Project {
            id: ProjectId::new(),
            display_name: "fixture".to_owned(),
            root: workspace.path().canonicalize().unwrap(),
            source: ProjectSource::Local,
            checks: Some(vec![check.clone()]),
            last_opened: OffsetDateTime::UNIX_EPOCH,
            available: true,
            git: None,
        };
        let store = Arc::new(
            Store::open(data.path())
                .unwrap()
                .redacting(Arc::new(StreamChangingRedactor)),
        );
        trust(&store, &project);

        run_configured_check(
            &check_coordinator(Arc::clone(&store)).unwrap(),
            &project,
            &check,
            DecidedVia::Cli,
            &Cancellation::default(),
        )
        .unwrap();

        assert_eq!(
            project_checks(&store, &project).unwrap()[0].freshness,
            CheckFreshness::Current
        );
    }

    /// Rewrites the workspace root wherever it appears, which is the ordinary
    /// job of a redactor and the one thing the coordinator compares a run's
    /// workspace reference against. Everything else is left alone so this test
    /// fails for the reference alone.
    #[derive(Debug)]
    struct RootRedactor {
        root: String,
    }

    impl Redactor for RootRedactor {
        fn redact_text<'a>(&self, text: &'a str) -> Cow<'a, str> {
            if text.contains(&self.root) {
                Cow::Owned(text.replace(&self.root, "<workspace>"))
            } else {
                Cow::Borrowed(text)
            }
        }

        fn wrap_stream(&self, sink: Box<dyn Write + Send>) -> Box<dyn Write + Send> {
            sink
        }
    }

    /// A check built its own workspace reference with `PassThrough` while the
    /// coordinator built the one it validates against from the store's redactor,
    /// so any store whose redactor touched the root text refused every check
    /// before it started. The identity redactor in the test above cannot see
    /// this, because both sides agree when neither rewrites anything.
    #[test]
    fn a_check_runs_against_a_store_whose_redactor_rewrites_the_workspace_root() {
        let workspace = tempdir().unwrap();
        let data = tempdir().unwrap();
        initialize_repository(workspace.path());
        let root = workspace.path().canonicalize().unwrap();
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
        let project = Project {
            id: ProjectId::new(),
            display_name: "fixture".to_owned(),
            root: root.clone(),
            source: ProjectSource::Local,
            checks: Some(vec![check.clone()]),
            last_opened: OffsetDateTime::UNIX_EPOCH,
            available: true,
            git: None,
        };
        let store = Arc::new(
            Store::open(data.path())
                .unwrap()
                .redacting(Arc::new(RootRedactor {
                    root: root.to_string_lossy().into_owned(),
                })),
        );
        trust(&store, &project);

        run_configured_check(
            &check_coordinator(Arc::clone(&store)).unwrap(),
            &project,
            &check,
            DecidedVia::Cli,
            &Cancellation::default(),
        )
        .unwrap();

        let summaries = project_checks(&store, &project).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].outcome, CheckOutcome::Passed);
    }

    /// The catalog admits 128 arbitrary environment names, `check.run` declares
    /// none, and the two only met inside the process supervisor — after a task, a
    /// run and an approval had been recorded — where the refusal arrived as a
    /// generic execution failure. It is now refused before any of that, naming
    /// the variable.
    #[test]
    fn an_environment_name_the_check_tool_cannot_set_is_refused_before_a_run_exists() {
        let workspace = tempdir().unwrap();
        let data = tempdir().unwrap();
        initialize_repository(workspace.path());
        let mut check = CheckConfiguration {
            id: "fixture".to_owned(),
            label: "Fixture".to_owned(),
            command: vec!["true".to_owned()],
            cwd: None,
            env: BTreeMap::from([("RUSTFLAGS".to_owned(), "-D warnings".to_owned())]),
            parser: CheckParser::Plain,
            timeout_seconds: Some(10),
        };
        let project = Project {
            id: ProjectId::new(),
            display_name: "fixture".to_owned(),
            root: workspace.path().canonicalize().unwrap(),
            source: ProjectSource::Local,
            checks: Some(vec![check.clone()]),
            last_opened: OffsetDateTime::UNIX_EPOCH,
            available: true,
            git: None,
        };
        let store = Arc::new(Store::open(data.path()).unwrap());
        trust(&store, &project);

        let error = run_configured_check(
            &check_coordinator(Arc::clone(&store)).unwrap(),
            &project,
            &check,
            DecidedVia::Cli,
            &Cancellation::default(),
        )
        .expect_err("an undeclared environment name cannot start a check");
        assert!(matches!(
            error,
            CheckLaunchError::UndeclaredEnvironment { ref name, .. } if name == "RUSTFLAGS"
        ));
        assert!(error.to_string().contains("RUSTFLAGS"));
        // Nothing was recorded: the refusal happens before the task exists.
        assert!(project_checks(&store, &project).unwrap().is_empty());

        // A baseline name is still configurable, so the field is not inert.
        check.env = BTreeMap::from([("LANG".to_owned(), "C".to_owned())]);
        run_configured_check(
            &check_coordinator(Arc::clone(&store)).unwrap(),
            &project,
            &check,
            DecidedVia::Cli,
            &Cancellation::default(),
        )
        .unwrap();
    }

    /// The scheduler that serializes mutating calls per workspace and caps child
    /// processes globally belongs to the coordinator, so a coordinator built per
    /// check capped each check against nothing. One coordinator has to serve
    /// every project in the process — which means its policy layer cannot be
    /// pinned to whichever workspace happened to build it.
    #[test]
    fn one_coordinator_runs_checks_for_more_than_one_project() {
        let data = tempdir().unwrap();
        let store = Arc::new(Store::open(data.path()).unwrap());
        let coordinator = check_coordinator(Arc::clone(&store)).unwrap();
        let executable = std::env::current_exe().unwrap();
        let workspaces = [tempdir().unwrap(), tempdir().unwrap()];
        let projects = workspaces
            .iter()
            .enumerate()
            .map(|(index, workspace)| {
                initialize_repository(workspace.path());
                let check = CheckConfiguration {
                    id: format!("fixture-{index}"),
                    label: format!("Fixture {index}"),
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
                    timeout_seconds: Some(30),
                };
                let project = Project {
                    id: ProjectId::new(),
                    display_name: format!("fixture-{index}"),
                    root: workspace.path().canonicalize().unwrap(),
                    source: ProjectSource::Local,
                    checks: Some(vec![check.clone()]),
                    last_opened: OffsetDateTime::UNIX_EPOCH,
                    available: true,
                    git: None,
                };
                trust(&store, &project);
                (project, check)
            })
            .collect::<Vec<_>>();

        for (project, check) in &projects {
            run_configured_check(
                &coordinator,
                project,
                check,
                DecidedVia::Cli,
                &Cancellation::default(),
            )
            .unwrap();
        }

        for (project, _) in &projects {
            let summaries = project_checks(&store, project).unwrap();
            assert_eq!(summaries.len(), 1);
            assert_eq!(summaries[0].outcome, CheckOutcome::Passed);
        }
    }

    fn trust(store: &Store, project: &Project) {
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
    }
}
