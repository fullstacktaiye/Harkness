//! `harkness run` — list, show, cancel, and retry recorded runs.
//!
//! Every body here is a projection over [`RunCoordinator`]; nothing decides
//! anything about a run that the coordinator does not decide.
//!
//! # Cancelling and retrying are not the same reach
//!
//! `retry_run` reads durable state, so it works against any run in the store.
//! `cancel_run` reaches an in-process worker: the run's cancellation token and
//! the parked approval waiter both live in the process that started it, and a
//! decision persisted by a second process would never wake either. A one-shot
//! command invocation therefore cancels only a run it is itself driving —
//! `agent run` interrupted with Ctrl-C — and reports `run_not_active` for
//! anything else, which is also what it reports for a run that already
//! finished.

use std::path::Path;

use clap::{Args, Subcommand};
use harkness_core::{Project, ProjectService};
use harkness_git::Cancellation;
use harkness_runtime::agent::MockAgent;
use harkness_runtime::coordinator::{RunCoordinator, RuntimeError};
use harkness_runtime::domain::{Run, RunId, Task};
use harkness_runtime::store::{
    DEFAULT_EVENT_PAGE_LIMIT, DEFAULT_RUN_PAGE_LIMIT, EventPage, EventSeq, RunPage,
};
use harkness_runtime::tool::WorkspaceMetadata;
use serde_json::json;

use crate::runtime_support::{
    ApprovalMode, apply_workspace_trust, approval_value, artifact_value, decode_run_cursor,
    encode_run_cursor, event_line, event_value, load_run_view, open_existing_runtime, open_runtime,
    parse_event_limit, parse_run_limit, run_line, run_value, run_verdict, step_value,
    supervise_run, task_value, tool_call_value, workspace_ref,
};
use crate::{CliError, CommandResult, command_result, load_service, resolve_project, single_line};

/// Ordering of one timeline page.
///
/// A separate spelling from [`EventOrder`](harkness_runtime::store::EventOrder)
/// because clap renders these kebab-cased for the flag, which is the house
/// style for a flag value. The two are kept in step by
/// [`page`](TimelineOrder::page), which is the only place the flag becomes a
/// request: `EventPage` is `#[non_exhaustive]`, so its own constructors are how
/// a direction is selected from outside the runtime.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, clap::ValueEnum)]
pub(crate) enum TimelineOrder {
    /// Oldest first, continuing towards the tip.
    #[default]
    Oldest,
    /// Newest first, continuing towards the beginning of the run.
    Newest,
}

impl TimelineOrder {
    fn page(self, limit: usize) -> EventPage {
        match self {
            Self::Oldest => EventPage::oldest(limit),
            Self::Newest => EventPage::newest(limit),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Oldest => "oldest",
            Self::Newest => "newest",
        }
    }
}

#[derive(Debug, Subcommand)]
pub(crate) enum RunCommand {
    /// List recorded runs, newest first.
    List(ListArguments),
    /// Show one run with its timeline, calls, approvals, and artifacts.
    Show(ShowArguments),
    /// Cancel a run this invocation is driving.
    Cancel {
        /// Run identifier.
        #[arg(value_name = "RUN_ID")]
        run_id: String,
    },
    /// Start a fresh attempt at the task a finished run attempted.
    Retry(RetryArguments),
}

#[derive(Debug, Args)]
pub(crate) struct ListArguments {
    /// Maximum number of runs in this page.
    #[arg(
        long,
        value_name = "COUNT",
        default_value_t = DEFAULT_RUN_PAGE_LIMIT,
        value_parser = parse_run_limit,
    )]
    limit: usize,
    /// Opaque continuation token returned as `next_cursor` by an earlier page.
    #[arg(long, value_name = "TOKEN")]
    cursor: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct ShowArguments {
    /// Run identifier.
    #[arg(value_name = "RUN_ID")]
    run_id: String,
    /// Maximum number of timeline events in this page.
    #[arg(
        long,
        value_name = "COUNT",
        default_value_t = DEFAULT_EVENT_PAGE_LIMIT,
        value_parser = parse_event_limit,
    )]
    limit: usize,
    /// Exclusive sequence number to continue a timeline page from.
    #[arg(long, value_name = "SEQ")]
    cursor: Option<u64>,
    /// Which end of the timeline the page is read from.
    #[arg(long, value_enum, default_value_t = TimelineOrder::Oldest)]
    order: TimelineOrder,
}

#[derive(Debug, Args)]
pub(crate) struct RetryArguments {
    /// Run identifier of the attempt being re-attempted.
    #[arg(value_name = "RUN_ID")]
    run_id: String,
    /// Mock-agent scenario the new attempt replays.
    #[arg(long, value_name = "NAME", value_parser = crate::agent_commands::parse_scenario)]
    scenario: String,
    /// Answer approval requests on the terminal instead of denying them.
    #[arg(long)]
    interactive: bool,
    /// Record a positive trust decision for the selected workspace first.
    #[arg(long)]
    trust_workspace: bool,
}

pub(crate) fn run_run(
    command: RunCommand,
    data_dir: Option<&Path>,
    json_output: bool,
    cancellation: &Cancellation,
) -> Result<CommandResult, CliError> {
    match command {
        RunCommand::List(arguments) => list(arguments, data_dir, json_output),
        RunCommand::Show(arguments) => show(arguments, data_dir, json_output),
        RunCommand::Cancel { run_id } => cancel(&run_id, data_dir, json_output),
        RunCommand::Retry(arguments) => retry(arguments, data_dir, json_output, cancellation),
    }
}

/// Parses a run identifier, refusing a malformed one before any store opens.
fn parse_run_id(value: &str) -> Result<RunId, CliError> {
    value.parse::<RunId>().map_err(|_| {
        CliError::Usage(format!(
            "{:?} is not a Harkness run identifier",
            single_line(value)
        ))
    })
}

fn list(
    arguments: ListArguments,
    data_dir: Option<&Path>,
    json_output: bool,
) -> Result<CommandResult, CliError> {
    let service = load_service(data_dir)?;
    let cursor = arguments
        .cursor
        .as_deref()
        .map(decode_run_cursor)
        .transpose()?;
    // A listing must not create the run store. No database means nothing has
    // been recorded, which is the empty page rather than an error.
    let Some(coordinator) = open_existing_runtime(service.data_dir())? else {
        return listing_result(json_output, &[], None, &[]);
    };
    let page = match cursor {
        Some(cursor) => RunPage::after(cursor, arguments.limit),
        None => RunPage::new(arguments.limit),
    };
    let listing = coordinator.list_runs(page).map_err(CliError::Runtime)?;
    let titles = listing
        .runs
        .iter()
        .map(|run| {
            coordinator
                .store()
                .load_task(run.task_id())
                .ok()
                .map(|task| task.title().to_owned())
        })
        .collect::<Vec<_>>();
    let next = listing
        .next_cursor
        .as_ref()
        .map(encode_run_cursor)
        .transpose()?;
    listing_result(json_output, &listing.runs, next, &titles)
}

fn listing_result(
    json_output: bool,
    runs: &[Run],
    next_cursor: Option<String>,
    titles: &[Option<String>],
) -> Result<CommandResult, CliError> {
    command_result(
        json_output,
        || {
            if runs.is_empty() && next_cursor.is_none() {
                return "no runs recorded".to_owned();
            }
            let mut lines = runs
                .iter()
                .enumerate()
                .map(|(index, run)| run_line(run, titles.get(index).and_then(Option::as_deref)))
                .collect::<Vec<_>>();
            // The same note `git log` prints, for the same reason: a page that
            // ends without saying so reads as the end of the history.
            if next_cursor.is_some() {
                lines.push("more runs available; use --json to obtain next_cursor".to_owned());
            }
            lines.join("\n")
        },
        || {
            Ok(json!({
                "kind": "run_list",
                "runs": runs
                    .iter()
                    .enumerate()
                    .map(|(index, run)| {
                        let mut value = run_value(run);
                        if let (Some(object), Some(Some(title))) =
                            (value.as_object_mut(), titles.get(index))
                        {
                            object.insert("task_title".to_owned(), json!(title));
                        }
                        value
                    })
                    .collect::<Vec<_>>(),
                "next_cursor": next_cursor,
            }))
        },
    )
}

fn show(
    arguments: ShowArguments,
    data_dir: Option<&Path>,
    json_output: bool,
) -> Result<CommandResult, CliError> {
    let run_id = parse_run_id(&arguments.run_id)?;
    let service = load_service(data_dir)?;
    let Some(coordinator) = open_existing_runtime(service.data_dir())? else {
        return Err(missing_run(run_id));
    };
    // The view carries the run, its task, steps, calls, approvals, and artifact
    // metadata; the timeline comes from the paged reader beside it. Deliberately
    // not `run_snapshot`, whose `events` field is the *whole* log — reading one
    // page of a hundred-thousand-event run must not materialize the other
    // ninety-nine thousand first.
    let view = load_run_view(&coordinator, run_id)?;
    let mut page = arguments.order.page(arguments.limit);
    if let Some(cursor) = arguments.cursor {
        page = page.after(EventSeq::new(cursor));
    }
    let timeline = coordinator
        .event_page(run_id, page)
        .map_err(CliError::Runtime)?;
    let data = json!({
        "kind": "run_show",
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
        "events": timeline.events.iter().map(event_value).collect::<Vec<_>>(),
        "order": arguments.order.as_str(),
        "next_cursor": timeline.next_cursor.map(EventSeq::get),
    });
    command_result(
        json_output,
        || {
            let mut lines = vec![run_line(&view.run, Some(view.task.title()))];
            lines.extend(view.tool_calls.iter().map(|call| {
                format!(
                    "call\t{}\t{}\t{}",
                    call.tool_id(),
                    call.tool_version(),
                    call.state().as_str()
                )
            }));
            lines.extend(view.approvals.iter().map(|request| {
                format!(
                    "approval\t{}\t{}\t{}",
                    request.id(),
                    request.state().as_str(),
                    single_line(request.input_summary())
                )
            }));
            lines.extend(view.artifacts.iter().map(|artifact| {
                format!(
                    "artifact\t{}\t{}\t{}\t{}\t{}",
                    artifact.id(),
                    single_line(artifact.name()),
                    single_line(artifact.media_type()),
                    artifact.byte_size(),
                    artifact.availability().as_str()
                )
            }));
            lines.extend(timeline.events.iter().map(event_line));
            if timeline.next_cursor.is_some() {
                lines.push("more events available; use --json to obtain next_cursor".to_owned());
            }
            lines.join("\n")
        },
        || Ok(data.clone()),
    )
}

fn cancel(
    run_id: &str,
    data_dir: Option<&Path>,
    json_output: bool,
) -> Result<CommandResult, CliError> {
    let run_id = parse_run_id(run_id)?;
    let service = load_service(data_dir)?;
    let Some(coordinator) = open_existing_runtime(service.data_dir())? else {
        return Err(missing_run(run_id));
    };
    // Read first, so a run that never existed is `not_found` rather than
    // "this process is not driving it".
    coordinator
        .store()
        .load_run(run_id)
        .map_err(CliError::Store)?;
    coordinator.cancel_run(run_id).map_err(CliError::Runtime)?;
    let run = coordinator
        .store()
        .load_run(run_id)
        .map_err(CliError::Store)?;
    command_result(
        json_output,
        || format!("cancelled run {run_id}"),
        || Ok(json!({ "kind": "run_cancel", "run": run_value(&run) })),
    )
}

/// Starts a fresh attempt and drives it to a terminal state.
///
/// The new run is supervised rather than left running, because this process is
/// the only thing driving it: a command that started an attempt and returned
/// would exit, and the coordinator's own teardown would cancel the run it had
/// just reported as started. So a retry ends where the attempt ends, exactly as
/// `agent run` does, and reports the same verdict.
fn retry(
    arguments: RetryArguments,
    data_dir: Option<&Path>,
    json_output: bool,
    cancellation: &Cancellation,
) -> Result<CommandResult, CliError> {
    let original = parse_run_id(&arguments.run_id)?;
    let service = load_service(data_dir)?;
    let coordinator = open_runtime(service.data_dir())?;
    let agent = MockAgent::scenario(&arguments.scenario).map_err(|error| {
        CliError::Usage(format!(
            "--scenario is not a built-in mock agent script: {error}"
        ))
    })?;
    // The workspace comes from the run being re-attempted, never from
    // `--project` or the current directory. A retry is a fresh attempt at a task
    // that already names its workspace, so asking the caller to name it again
    // only creates ways to name a different one: the coordinator would refuse
    // with `workspace_mismatch` after `--trust-workspace` had already written a
    // durable positive decision for whatever project was named by mistake.
    let task = task_of(&coordinator, original)?;
    let project = project_of(&service, &task)?;
    apply_workspace_trust(
        &coordinator,
        &project,
        arguments.trust_workspace,
        "re-attempting a run",
    )?;
    let workspace = workspace_ref(coordinator.store(), &task);
    let run_id = coordinator
        .retry_run_with_workspace_metadata(
            original,
            Box::new(agent),
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
    let view = outcome.view;
    // `retry_of` and `workspace_may_be_modified` are read back off the new
    // run's own record rather than restated here, so what is printed is what
    // was persisted.
    let warning = view.run.workspace_may_be_modified();
    let data = json!({
        "kind": "run_retry",
        "retry_of": original,
        "run_id": run_id,
        "scenario": arguments.scenario,
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
        "event_count": outcome.streamed.count,
        "timeline_complete": outcome.streamed.complete,
    });
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
        move || {
            let mut line = format!(
                "{run_id}\t{}\tretry of {original}",
                view.run.state().as_str()
            );
            if warning {
                line.push_str(
                    "\nthe earlier attempt started work that could write; \
                     the workspace may already have been changed",
                );
            }
            line
        },
        || Ok(data.clone()),
    )
}

/// The task a run was an attempt at.
fn task_of(coordinator: &RunCoordinator, run: RunId) -> Result<Task, CliError> {
    let record = coordinator.store().load_run(run).map_err(CliError::Store)?;
    coordinator
        .store()
        .load_task(record.task_id())
        .map_err(CliError::Store)
}

/// The catalogued project a recorded task's workspace belongs to.
///
/// A task with no project identity cannot be scheduled at all — the coordinator
/// refuses with `workspace_identity_required` — so a retry says the same thing
/// here rather than resolving something else and failing later.
fn project_of(service: &ProjectService, task: &Task) -> Result<Project, CliError> {
    let Some(project_id) = task.project_id() else {
        return Err(CliError::Runtime(RuntimeError::WorkspaceIdentityRequired {
            task: task.id(),
        }));
    };
    resolve_project(service, Some(&project_id.to_string()))
}

/// The refusal a command reports when no run store exists at all.
///
/// Spelled with the runtime's own `not_found`, so "there is no such run" reads
/// the same whether the store is missing or merely does not hold that row.
fn missing_run(run: RunId) -> CliError {
    CliError::Runtime(RuntimeError::Store(
        harkness_runtime::store::StoreError::NotFound {
            record: "run",
            id: run.to_string(),
        },
    ))
}
