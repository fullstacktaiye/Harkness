//! `harkness agent` — run one deterministic mock-agent scenario end to end.
//!
//! This is the flagship workflow's scriptable leg. The command starts a run,
//! streams its persisted timeline to standard error as progress envelopes,
//! answers the approvals the run parks on, and prints exactly one result
//! envelope naming the run.
//!
//! # Absence of an answer is refusal
//!
//! A trusted workspace still requires approval for anything above `Observe`, so
//! every scenario that writes or executes will park at least once. Without
//! `--interactive` this command *denies* those requests: there is no terminal
//! to ask, and no flag turns silence into consent. The run then ends with kind
//! `approval_required_noninteractive` at exit 3. With `--interactive` the
//! question goes to standard error and the answer is read from standard input,
//! so `--json` output stays exactly one object on standard output either way.

use std::path::Path;

use clap::{Args, Subcommand};
use harkness_git::Cancellation;
use harkness_runtime::agent::MockAgent;
use harkness_runtime::domain::Task;
use harkness_runtime::tool::WorkspaceMetadata;
use serde_json::json;
use time::OffsetDateTime;

use crate::runtime_support::{
    ApprovalMode, apply_workspace_trust, approval_value, artifact_value, open_runtime, run_value,
    run_verdict, step_value, supervise_run, task_value, tool_call_value, workspace_ref,
};
use crate::{CliError, CommandResult, command_result, load_service, resolve_project, single_line};

#[derive(Debug, Subcommand)]
pub(crate) enum AgentCommand {
    /// List the deterministic scenarios this build can replay.
    Scenarios,
    /// Replay one scenario through the coordinator against a workspace.
    Run(RunArguments),
}

#[derive(Debug, Args)]
pub(crate) struct RunArguments {
    /// Built-in mock-agent scenario to replay.
    #[arg(long, value_name = "NAME", value_parser = parse_scenario)]
    scenario: String,
    /// Select by full ID, UUID prefix (8+ characters), explicit path, or display name.
    #[arg(long, value_name = "SELECTOR")]
    project: Option<String>,
    /// Answer approval requests on the terminal instead of denying them.
    #[arg(long)]
    interactive: bool,
    /// Record a positive trust decision for the selected workspace first.
    #[arg(long)]
    trust_workspace: bool,
}

/// Accepts only a scenario this build registers.
///
/// Validated by clap rather than by the coordinator so an unknown name is a
/// usage error at exit 2 with the whole registry listed, instead of a run that
/// is recorded and then fails.
pub(crate) fn parse_scenario(value: &str) -> Result<String, String> {
    if MockAgent::scenario_names().contains(&value) {
        return Ok(value.to_owned());
    }
    Err(format!(
        "unknown scenario {:?}; this build replays {}",
        single_line(value),
        MockAgent::scenario_names().join(", ")
    ))
}

pub(crate) fn run_agent(
    command: AgentCommand,
    data_dir: Option<&Path>,
    json_output: bool,
    cancellation: &Cancellation,
) -> Result<CommandResult, CliError> {
    match command {
        AgentCommand::Scenarios => scenarios(json_output),
        AgentCommand::Run(arguments) => run(arguments, data_dir, json_output, cancellation),
    }
}

fn scenarios(json_output: bool) -> Result<CommandResult, CliError> {
    let names = MockAgent::scenario_names();
    command_result(
        json_output,
        || names.join("\n"),
        || Ok(json!({ "kind": "agent_scenarios", "scenarios": names })),
    )
}

fn run(
    arguments: RunArguments,
    data_dir: Option<&Path>,
    json_output: bool,
    cancellation: &Cancellation,
) -> Result<CommandResult, CliError> {
    let service = load_service(data_dir)?;
    let project = resolve_project(&service, arguments.project.as_deref())?;
    let coordinator = open_runtime(service.data_dir())?;
    apply_workspace_trust(
        &coordinator,
        &project,
        arguments.trust_workspace,
        "running an agent scenario",
    )?;
    // Resolved through the registry the coordinator was built with, so the
    // script and the tools it names come from one build.
    let agent = MockAgent::scenario(&arguments.scenario)
        .map_err(|error| CliError::Usage(error.to_string()))?;
    let scenario_version = agent.definition().version();
    let task = Task::new(
        format!("Agent scenario {}", arguments.scenario),
        &project.root,
        Some(project.id),
        OffsetDateTime::now_utc(),
    );
    let workspace = workspace_ref(&task);
    let task_id = coordinator.start_task(task).map_err(CliError::Runtime)?;
    let run_id = coordinator
        .start_run_with_workspace_metadata(
            task_id,
            Box::new(agent),
            workspace,
            WorkspaceMetadata::from_project(&project),
        )
        .map_err(CliError::Runtime)?;
    // Subscribed after the run starts, which loses nothing: a subscription
    // replays the durable history before it delivers anything live.
    let receiver = coordinator.subscribe(run_id).map_err(CliError::Runtime)?;
    let mode = if arguments.interactive {
        ApprovalMode::Interactive
    } else {
        ApprovalMode::Noninteractive
    };
    let outcome = supervise_run(
        &coordinator,
        run_id,
        &receiver,
        mode,
        json_output,
        cancellation,
    )?;
    let view = outcome.view;
    let data = json!({
        "kind": "agent_run",
        "run_id": run_id,
        "scenario": arguments.scenario,
        "scenario_version": scenario_version,
        "run": run_value(&view.run),
        "task": task_value(&view.task),
        "steps": view.steps.iter().map(step_value).collect::<Vec<_>>(),
        "tool_calls": view.tool_calls.iter().map(tool_call_value).collect::<Vec<_>>(),
        "approvals": view
            .approvals
            .iter()
            .map(|request| approval_value(request, None))
            .collect::<Vec<_>>(),
        "artifacts": view.artifacts.iter().map(artifact_value).collect::<Vec<_>>(),
        // Streamed, not repeated. Every event went to standard error as it was
        // recorded and `run show` reads the whole log back; a result envelope
        // that grew with the run would be the one thing this command promises
        // not to be.
        "event_count": outcome.streamed.count,
        "last_event_seq": outcome.streamed.last_seq,
        "timeline_complete": outcome.streamed.complete,
    });
    // The run's own verdict decides the exit status, exactly as a project
    // check's does: a caller must be able to act on the process status without
    // parsing standard output.
    if let Some(verdict) = run_verdict(
        &view.run,
        &view.tool_calls,
        &outcome.denied_noninteractively,
        &data,
    ) {
        return Err(verdict);
    }
    command_result(
        json_output,
        || {
            format!(
                "{}\t{}\t{}",
                run_id,
                view.run.state().as_str(),
                arguments.scenario
            )
        },
        || Ok(data.clone()),
    )
}
