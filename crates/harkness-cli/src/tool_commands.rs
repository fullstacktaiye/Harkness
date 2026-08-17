//! `harkness tool` — publish the typed tool contract and invoke one tool.
//!
//! `list` and `describe` read the registry alone: no data directory, no run
//! store, no lease. `invoke` is the opposite — it is deliberately *not* a
//! bypass. There is no route from this module to a tool body except the one an
//! agent takes: registry resolution, schema validation, policy, an approval if
//! policy asks for one, scheduler admission, and executor delivery, with a
//! task, a run, a step, a tool call, an event log and any artifacts recorded
//! exactly as an agent's call records them.
//!
//! # Why a direct invocation still has an agent
//!
//! Only the coordinator may dispatch a tool, and it dispatches what an
//! [`Agent`](harkness_runtime::agent::Agent) asks for. A one-call
//! [`Scenario`] is therefore what "invoke this tool and stop" is spelled as. A
//! scenario is a straight line — one expectation per transition — so the script
//! expects a result, and a call that fails or is denied makes the *run* fail
//! with `scenario_divergence`. That spelling is about the script, not about the
//! tool: the recorded call carries the real outcome, and it is the call's
//! verdict this command reports and exits on. The run's own failure travels in
//! `details` so nothing is hidden.
//!
//! # There is no `--timeout`
//!
//! A tool's declared [`ToolTimeout`](harkness_runtime::tool::ToolTimeout) is
//! what bounds a call, and the coordinator's dispatch carries no per-call
//! override for it. Adding one is `harkness-runtime` API rather than a flag, so
//! it is left out here. `process.exec` and `test.run` accept `timeout_seconds`
//! in their own published input, which is how a child process is bounded.

use std::io::Read;
use std::path::Path;

use clap::{Args, Subcommand};
use harkness_git::Cancellation;
use harkness_runtime::agent::{
    AgentAction, MockAgent, ObservationPattern, Scenario, ScenarioId, ScenarioStep,
};
use harkness_runtime::domain::Task;
use harkness_runtime::tool::{
    EnvironmentName, ToolDescriptor, ToolId, ToolRegistry, ToolVersion, WorkspaceMetadata,
};
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::runtime_support::{
    ApprovalMode, artifact_value, contract_registry, open_runtime, run_value, step_value,
    supervise_run, task_value, tool_call_value, tool_call_verdict, workspace_ref,
};
use crate::{CliError, CommandResult, command_result, load_service, resolve_project, single_line};

#[derive(Debug, Subcommand)]
pub(crate) enum ToolCommand {
    /// List every registered tool with its declared contract.
    List,
    /// Describe one tool, including its input and output schemas.
    Describe(DescribeArguments),
    /// Execute one tool through the full runtime pipeline.
    Invoke(InvokeArguments),
}

#[derive(Debug, Args)]
pub(crate) struct DescribeArguments {
    /// Dotted tool identifier, such as `fs.read`.
    #[arg(value_name = "TOOL_ID")]
    tool_id: String,
    /// Exact registered version; the highest stable one when omitted.
    #[arg(long, value_name = "SEMVER")]
    tool_version: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct InvokeArguments {
    /// Dotted tool identifier, such as `fs.read`.
    #[arg(value_name = "TOOL_ID")]
    tool_id: String,
    /// Exact registered version; the highest stable one when omitted.
    #[arg(long, value_name = "SEMVER")]
    tool_version: Option<String>,
    /// Tool input as JSON, or `-` to read the document from standard input.
    #[arg(long, value_name = "JSON")]
    input: String,
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

pub(crate) fn run_tool(
    command: ToolCommand,
    data_dir: Option<&Path>,
    json_output: bool,
    cancellation: &Cancellation,
) -> Result<CommandResult, CliError> {
    match command {
        ToolCommand::List => list(json_output),
        ToolCommand::Describe(arguments) => describe(arguments, json_output),
        ToolCommand::Invoke(arguments) => invoke(arguments, data_dir, json_output, cancellation),
    }
}

fn parse_tool_id(value: &str) -> Result<ToolId, CliError> {
    value.parse::<ToolId>().map_err(|error| {
        CliError::Usage(format!(
            "{:?} is not a Harkness tool identifier: {error}",
            single_line(value)
        ))
    })
}

fn parse_tool_version(value: &str) -> Result<ToolVersion, CliError> {
    value.parse::<ToolVersion>().map_err(|error| {
        CliError::Usage(format!(
            "{:?} is not a Harkness tool version: {error}",
            single_line(value)
        ))
    })
}

fn list(json_output: bool) -> Result<CommandResult, CliError> {
    let registry = contract_registry()?;
    // Ordered by identifier and then by version precedence, which the registry
    // guarantees, so the projection is diff-stable regardless of registration
    // order.
    let descriptors = registry.descriptors().collect::<Vec<_>>();
    command_result(
        json_output,
        || {
            descriptors
                .iter()
                .map(|descriptor| {
                    format!(
                        "{}\t{}\t{}\t{}",
                        descriptor.id(),
                        descriptor.version(),
                        descriptor.risk().as_str(),
                        single_line(descriptor.title())
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        },
        || {
            Ok(json!({
                "kind": "tool_list",
                "tools": descriptors
                    .iter()
                    .map(|descriptor| descriptor_value(descriptor, false))
                    .collect::<Vec<_>>(),
            }))
        },
    )
}

fn describe(arguments: DescribeArguments, json_output: bool) -> Result<CommandResult, CliError> {
    let registry = contract_registry()?;
    let descriptor = resolve(
        &registry,
        &arguments.tool_id,
        arguments.tool_version.as_deref(),
    )?;
    let versions = registry
        .versions(descriptor.id())
        .into_iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let data = json!({
        "kind": "tool_describe",
        "tool": descriptor_value(descriptor, true),
        "versions": versions,
    });
    command_result(
        json_output,
        || {
            format!(
                "{}\t{}\t{}\t{}",
                descriptor.id(),
                descriptor.version(),
                descriptor.risk().as_str(),
                single_line(descriptor.description())
            )
        },
        || Ok(data.clone()),
    )
}

/// Resolves a descriptor, reporting a wrong name and a stale pin differently.
fn resolve<'a>(
    registry: &'a ToolRegistry,
    id: &str,
    version: Option<&str>,
) -> Result<&'a ToolDescriptor, CliError> {
    let id = parse_tool_id(id)?;
    let version = version.map(parse_tool_version).transpose()?;
    registry
        .resolve(&id, version.as_ref())
        .map(|tool| tool.descriptor())
        .map_err(|error| CliError::RuntimeOutcome {
            kind: crate::runtime_support::tool_kind(error.kind()),
            code: crate::runtime_support::tool_exit_code(error.kind()),
            message: error.to_string(),
            details: json!({ "tool_id": id.to_string() }),
        })
}

/// The published contract of one tool.
///
/// Schemas are generated from the tool's own `Input` and `Output` types at
/// registration, so what is printed here cannot disagree with what the body
/// deserializes. They are omitted from `tool list` because publishing nine
/// schema documents in a listing buries the listing.
fn descriptor_value(descriptor: &ToolDescriptor, schemas: bool) -> Value {
    let mut value = json!({
        "id": descriptor.id().as_str(),
        "version": descriptor.version().to_string(),
        "title": descriptor.title(),
        "description": descriptor.description(),
        "risk": descriptor.risk().as_str(),
        "capabilities": descriptor
            .capabilities()
            .iter()
            .map(|capability| capability.as_str().to_owned())
            .collect::<Vec<_>>(),
        "environment": descriptor
            .environment()
            .iter()
            .map(EnvironmentName::as_str)
            .collect::<Vec<_>>(),
        "timeout": descriptor.timeout(),
        "spawns_processes": descriptor.spawns_processes(),
    });
    if schemas && let Some(object) = value.as_object_mut() {
        object.insert("input_schema".to_owned(), descriptor.input_schema().clone());
        object.insert(
            "output_schema".to_owned(),
            descriptor.output_schema().clone(),
        );
    }
    value
}

fn invoke(
    arguments: InvokeArguments,
    data_dir: Option<&Path>,
    json_output: bool,
    cancellation: &Cancellation,
) -> Result<CommandResult, CliError> {
    let input = read_input(&arguments.input)?;
    let service = load_service(data_dir)?;
    let project = resolve_project(&service, arguments.project.as_deref())?;
    let coordinator = open_runtime(service.data_dir())?;
    // Resolved before anything is recorded, so a wrong name or a stale pin
    // costs no task, run, or step. This registry and the coordinator's are
    // separate values built by one function, so resolving here and dispatching
    // there cannot pick different tools — and pinning the resolved `(id,
    // version)` into the request is what stops a second resolution disagreeing
    // with the first once the call is recorded.
    let identity = {
        let registry = contract_registry()?;
        resolve(
            &registry,
            &arguments.tool_id,
            arguments.tool_version.as_deref(),
        )?
        .identity()
        .clone()
    };
    crate::runtime_support::apply_workspace_trust(
        &coordinator,
        &project,
        arguments.trust_workspace,
        &format!("invoking {identity}"),
    )?;

    let scenario = Scenario::new(
        ScenarioId::new("direct_tool_invocation").expect("a fixed snake-case id is valid"),
        vec![
            ScenarioStep::new(
                ObservationPattern::RunStarted { task_title: None },
                AgentAction::CallTool {
                    tool_id: identity.id.clone(),
                    tool_version: identity.version.clone(),
                    // Passed verbatim. The registry validates it against the
                    // published schema before any body runs, which is the whole
                    // point of routing a direct invocation through the pipeline.
                    input: input.clone(),
                },
            ),
            ScenarioStep::new(
                ObservationPattern::ToolResult {
                    artifact_media_type: None,
                    output_contains: None,
                },
                AgentAction::CompleteRun {
                    summary: format!("Invoked {identity} from the command line."),
                },
            ),
        ],
    )
    .map_err(|error| CliError::WireProjection(error.to_string()))?;

    let task = Task::new(
        format!("Invoke {identity}"),
        &project.root,
        Some(project.id),
        OffsetDateTime::now_utc(),
    );
    let workspace = workspace_ref(coordinator.store(), &task);
    let task_id = coordinator.start_task(task).map_err(CliError::Runtime)?;
    let run_id = coordinator
        .start_run_with_workspace_metadata(
            task_id,
            Box::new(MockAgent::from_scenario(scenario)),
            workspace,
            WorkspaceMetadata::from_project(&project),
        )
        .map_err(CliError::Runtime)?;
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
    let snapshot = outcome.snapshot;
    // Exactly one call: the script requests one tool and stops. Absent means
    // the run never reached the request, which is reported as the run's own
    // failure rather than as a missing tool call.
    let Some(call) = snapshot.tool_calls.first() else {
        return Err(CliError::RuntimeOutcome {
            kind: "run_failed",
            code: crate::EXIT_OPERATION_FAILED,
            message: format!("run {run_id} recorded no tool call to report"),
            details: json!({ "run": run_value(&snapshot.run) }),
        });
    };
    let data = json!({
        "kind": "tool_invoke",
        "run_id": run_id,
        "task": task_value(&snapshot.task),
        "tool_call": tool_call_value(call),
        "run": run_value(&snapshot.run),
        "steps": snapshot.steps.iter().map(step_value).collect::<Vec<_>>(),
        "approvals": snapshot
            .approvals
            .iter()
            .map(|request| crate::runtime_support::approval_value(request, None))
            .collect::<Vec<_>>(),
        "artifacts": snapshot.artifacts.iter().map(artifact_value).collect::<Vec<_>>(),
        // The timeline itself is not in the envelope. It was streamed as
        // progress while the call ran and `run show` reproduces every entry of
        // it from the log; repeating it here would make one invocation's result
        // grow with how much the tool had to say.
        "event_count": snapshot.events.len(),
    });
    if let Some(verdict) = tool_call_verdict(call, outcome.denied_noninteractively, &data) {
        return Err(verdict);
    }
    command_result(
        json_output,
        || {
            format!(
                "{}\t{}\t{}\t{}",
                call.tool_id(),
                call.tool_version(),
                call.state().as_str(),
                run_id
            )
        },
        || Ok(data.clone()),
    )
}

/// Reads `--input` as a JSON document, from the flag or from standard input.
///
/// `-` is the stdin spelling the repository already uses for a document
/// argument. Whatever comes back is handed to the registry unaltered: it is the
/// published schema's job to refuse a bad shape, and refusing here would be a
/// second validator to keep in step with the first.
fn read_input(source: &str) -> Result<Value, CliError> {
    let document = if source == "-" {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|error| CliError::Usage(format!("--input could not be read: {error}")))?;
        text
    } else {
        source.to_owned()
    };
    serde_json::from_str(&document)
        .map_err(|error| CliError::Usage(format!("--input is not a JSON document: {error}")))
}
