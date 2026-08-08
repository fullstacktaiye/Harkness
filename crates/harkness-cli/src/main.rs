use std::{
    env,
    ffi::OsString,
    io::{self, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};

use clap::{
    Arg, ArgAction, ArgGroup, ArgMatches, Args, Command as ClapCommand, FromArgMatches, Parser,
    Subcommand,
    error::{Error as ClapError, ErrorKind},
};
use harkness_core::{
    Cancellation, GitError, GitStatus, Project, ProjectError, ProjectId, ProjectSelector,
    ProjectService, ProjectSource, UpstreamStatus, Worktree, WorktreeBase,
};
use serde::Serialize;
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;

const ENVELOPE_VERSION: u8 = 1;
const EXIT_OPERATION_FAILED: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_REFUSED: u8 = 3;
const EXIT_NOT_FOUND: u8 = 4;
const EXIT_CONFLICT: u8 = 5;
const EXIT_CANCELLED: u8 = 130;
const CLI_ERROR_KINDS: &[&str] = &[
    "usage_error",
    "current_directory_unavailable",
    "interrupt_handler_unavailable",
    "wire_projection_failed",
    "confirmation_required",
    "managed_project_requires_delete",
    "local_project_requires_forget",
    "worktree_requires_remove",
];

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Parser)]
#[command(
    name = "harkness",
    version,
    about = "Manage Harkness projects and workspaces",
    arg_required_else_help = true,
    disable_help_subcommand = true,
    after_help = "When --project is omitted from project show, forget, or delete, Harkness walks upward from the current directory and uses the deepest catalogued project root. This lets an agent run inside a repository or worktree without copying its project identifier.\n\nExit codes: 0 success, 1 operation failed, 2 usage error, 3 guardrail refusal, 4 not found, 5 conflict or busy, 130 cancelled."
)]
struct Cli {
    /// Emit one versioned machine-readable result object on standard output.
    /// Help and version output remain plain text.
    #[arg(long, global = true)]
    json: bool,

    /// Use an explicit Harkness data directory instead of HARKNESS_DATA_DIR.
    #[arg(long, global = true, value_name = "PATH")]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect and manage catalogued projects.
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Manage linked Git worktree workspaces.
    Worktree {
        #[command(subcommand)]
        command: WorktreeCommand,
    },
    /// Describe the versioned machine-readable CLI contract.
    Contract,
}

#[derive(Debug, Args)]
struct ProjectSelection {
    /// Select by full ID, UUID prefix (8+ characters), explicit path, or display name.
    #[arg(long, value_name = "SELECTOR")]
    project: Option<String>,
}

#[derive(Debug, Subcommand)]
enum ProjectCommand {
    /// List every catalogued project.
    List {
        /// Skip filesystem and Git status probes.
        #[arg(long)]
        no_status: bool,
    },
    /// Show the selected project.
    Show(ProjectSelection),
    /// Resolve a selector without performing another operation.
    Resolve {
        /// Full ID, UUID prefix, explicit path, or display name.
        selector: String,
    },
    /// Import an existing local directory.
    Import {
        /// Directory to add to the catalog.
        path: PathBuf,
    },
    /// Clone a GitHub repository into Harkness-managed storage.
    Clone {
        /// GitHub HTTPS URL or SSH remote.
        remote: String,
    },
    /// Remove orphaned managed-repository storage left by killed imports.
    Reconcile,
    /// Forget a local project without touching its files.
    Forget(ProjectSelection),
    /// Delete a Harkness-managed clone and remove it from the catalog.
    Delete {
        #[command(flatten)]
        selection: ProjectSelection,
        /// Confirm deletion of the selected checkout.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum WorktreeCommand {
    /// List linked worktrees for a parent project ID.
    List { parent_id: ProjectId },
    /// Create a Harkness-managed linked worktree.
    Create(CreateWorktree),
    /// Remove a Harkness-managed linked worktree.
    Remove {
        worktree_id: ProjectId,
        /// Discard uncommitted changes in the worktree.
        #[arg(long)]
        force: bool,
        /// Confirm that forced removal may discard uncommitted files.
        #[arg(long, requires = "force")]
        yes: bool,
    },
    /// Remove stale Harkness-owned worktree records selectively.
    Reconcile { parent_id: ProjectId },
}

#[derive(Debug)]
struct CreateWorktree {
    parent_id: ProjectId,
    base: WorktreeBase,
}

impl FromArgMatches for CreateWorktree {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, ClapError> {
        let mut matches = matches.clone();
        Self::from_arg_matches_mut(&mut matches)
    }

    fn from_arg_matches_mut(matches: &mut ArgMatches) -> Result<Self, ClapError> {
        let parent_id = matches
            .remove_one::<ProjectId>("parent_id")
            .ok_or_else(|| {
                ClapError::raw(ErrorKind::MissingRequiredArgument, "parent ID is required")
            })?;
        let new = matches.remove_one::<String>("new");
        let existing = matches.remove_one::<String>("existing");
        let detached = matches.remove_one::<String>("detached");
        let start_point = matches.remove_one::<String>("start");
        let base = match (new, existing, detached) {
            (Some(name), None, None) => WorktreeBase::NewBranch { name, start_point },
            (None, Some(name), None) if start_point.is_none() => {
                WorktreeBase::ExistingBranch { name }
            }
            (None, None, Some(commit)) if start_point.is_none() => {
                WorktreeBase::Detached { commit }
            }
            _ => {
                return Err(ClapError::raw(
                    ErrorKind::ArgumentConflict,
                    "choose exactly one of --new, --existing, or --detached; --start is valid only with --new",
                ));
            }
        };
        Ok(Self { parent_id, base })
    }

    fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), ClapError> {
        let mut matches = matches.clone();
        self.update_from_arg_matches_mut(&mut matches)
    }

    fn update_from_arg_matches_mut(&mut self, matches: &mut ArgMatches) -> Result<(), ClapError> {
        *self = Self::from_arg_matches_mut(matches)?;
        Ok(())
    }
}

impl Args for CreateWorktree {
    fn augment_args(command: ClapCommand) -> ClapCommand {
        command
            .arg(
                Arg::new("parent_id")
                    .required(true)
                    .value_parser(clap::value_parser!(ProjectId))
                    .help("Catalog ID of the repository that owns the worktree"),
            )
            .arg(
                Arg::new("new")
                    .long("new")
                    .value_name("BRANCH")
                    .action(ArgAction::Set)
                    .help("Create and check out a new branch"),
            )
            .arg(
                Arg::new("existing")
                    .long("existing")
                    .value_name("BRANCH")
                    .action(ArgAction::Set)
                    .help("Check out an existing local branch"),
            )
            .arg(
                Arg::new("detached")
                    .long("detached")
                    .value_name("REVISION")
                    .action(ArgAction::Set)
                    .help("Check out a detached revision"),
            )
            .arg(
                Arg::new("start")
                    .long("start")
                    .value_name("REVISION")
                    .action(ArgAction::Set)
                    .requires("new")
                    .conflicts_with_all(["existing", "detached"])
                    .help("Start a new branch at this revision instead of HEAD"),
            )
            .group(
                ArgGroup::new("base")
                    .required(true)
                    .multiple(false)
                    .args(["new", "existing", "detached"]),
            )
    }

    fn augment_args_for_update(command: ClapCommand) -> ClapCommand {
        Self::augment_args(command)
    }
}

enum CommandResult {
    Human(String),
    Json(Value),
}

fn command_result(
    json_output: bool,
    human: impl FnOnce() -> String,
    data: impl FnOnce() -> Result<Value, CliError>,
) -> Result<CommandResult, CliError> {
    if json_output {
        Ok(CommandResult::Json(data()?))
    } else {
        Ok(CommandResult::Human(human()))
    }
}

#[derive(Clone, Copy, Debug)]
enum RefusalKind {
    ConfirmationRequired,
    ManagedProjectRequiresDelete,
    LocalProjectRequiresForget,
    WorktreeRequiresRemove,
}

impl RefusalKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ConfirmationRequired => "confirmation_required",
            Self::ManagedProjectRequiresDelete => "managed_project_requires_delete",
            Self::LocalProjectRequiresForget => "local_project_requires_forget",
            Self::WorktreeRequiresRemove => "worktree_requires_remove",
        }
    }
}

#[derive(Debug)]
enum CliError {
    Project(ProjectError),
    CurrentDirectory(io::Error),
    InterruptHandler(io::Error),
    WireProjection(String),
    Refused {
        kind: RefusalKind,
        message: String,
        details: Value,
    },
}

impl From<ProjectError> for CliError {
    fn from(error: ProjectError) -> Self {
        Self::Project(error)
    }
}

impl CliError {
    fn kind(&self) -> &'static str {
        match self {
            Self::Project(error) => error.kind(),
            Self::CurrentDirectory(_) => "current_directory_unavailable",
            Self::InterruptHandler(_) => "interrupt_handler_unavailable",
            Self::WireProjection(_) => "wire_projection_failed",
            Self::Refused { kind, .. } => kind.as_str(),
        }
    }

    fn message(&self) -> String {
        match self {
            Self::Project(error) => error.to_string(),
            Self::WireProjection(message) => message.clone(),
            Self::CurrentDirectory(error) => {
                format!("the current working directory could not be determined: {error}")
            }
            Self::InterruptHandler(error) => {
                format!("the Ctrl-C cancellation handler could not be installed: {error}")
            }
            Self::Refused { message, .. } => message.clone(),
        }
    }

    fn exit_code(&self) -> u8 {
        match self {
            Self::Project(error) => project_exit_code(error),
            Self::CurrentDirectory(_) | Self::InterruptHandler(_) | Self::WireProjection(_) => {
                EXIT_OPERATION_FAILED
            }
            Self::Refused { .. } => EXIT_REFUSED,
        }
    }

    fn details(&self) -> Value {
        match self {
            Self::Project(error) => project_error_details(error),
            Self::Refused { details, .. } => details.clone(),
            Self::CurrentDirectory(_) | Self::InterruptHandler(_) | Self::WireProjection(_) => {
                json!({})
            }
        }
    }
}

#[derive(Serialize)]
struct SuccessEnvelope<'a> {
    v: u8,
    r#type: &'static str,
    ok: bool,
    data: &'a Value,
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    v: u8,
    r#type: &'static str,
    ok: bool,
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    kind: &'a str,
    message: &'a str,
    details: &'a Value,
}

#[derive(Serialize)]
struct ProgressEnvelope<'a> {
    v: u8,
    r#type: &'static str,
    message: &'a str,
}

fn main() -> ExitCode {
    let arguments = env::args_os().collect::<Vec<_>>();
    let requested_json = requested_json(&arguments);
    let cli = match Cli::try_parse_from(arguments) {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            let code = error.exit_code();
            let _ = error.print();
            return ExitCode::from(code as u8);
        }
        Err(error) => {
            let code = error.exit_code() as u8;
            let output = if requested_json {
                emit_error("usage_error", &clap_error_message(&error), &json!({}))
            } else {
                error.print()
            };
            return finish_output(output, code);
        }
    };

    let json_output = cli.json;
    let cancellation = match install_interrupt_handler() {
        Ok(cancellation) => cancellation,
        Err(error) => return finish_error(json_output, CliError::InterruptHandler(error)),
    };
    match run(cli, &cancellation) {
        Ok(result) => finish_result(result),
        Err(error) => finish_error(json_output, error),
    }
}

fn requested_json(arguments: &[OsString]) -> bool {
    arguments
        .iter()
        .skip(1)
        .take_while(|argument| *argument != "--")
        .any(|argument| argument == "--json")
}

fn clap_error_message(error: &ClapError) -> String {
    error
        .to_string()
        .lines()
        .next()
        .unwrap_or("invalid command-line arguments")
        .strip_prefix("error: ")
        .unwrap_or("invalid command-line arguments")
        .to_owned()
}

fn finish_result(result: CommandResult) -> ExitCode {
    let output = match result {
        CommandResult::Human(human) if human.is_empty() => Ok(()),
        CommandResult::Human(human) => write_line(&mut io::stdout().lock(), human.as_bytes()),
        CommandResult::Json(data) => emit_success(&data),
    };
    finish_output(output, 0)
}

fn finish_error(json_output: bool, error: CliError) -> ExitCode {
    let code = error.exit_code();
    let output = if json_output {
        emit_error(error.kind(), &error.message(), &error.details())
    } else {
        write_line(&mut io::stderr().lock(), error.message().as_bytes())
    };
    finish_output(output, code)
}

fn finish_output(output: io::Result<()>, intended_code: u8) -> ExitCode {
    match output {
        Ok(()) => ExitCode::from(intended_code),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(
                io::stderr().lock(),
                "failed to write command output: {error}"
            );
            ExitCode::from(EXIT_OPERATION_FAILED)
        }
    }
}

fn run(cli: Cli, cancellation: &Cancellation) -> Result<CommandResult, CliError> {
    let Cli {
        json,
        data_dir,
        command,
    } = cli;
    match command {
        Command::Project { command } => {
            run_project(command, data_dir.as_deref(), json, cancellation)
        }
        Command::Worktree { command } => {
            run_worktree(command, data_dir.as_deref(), json, cancellation)
        }
        Command::Contract => Ok(contract_result(json)),
    }
}

fn run_project(
    command: ProjectCommand,
    data_dir: Option<&Path>,
    json_output: bool,
    cancellation: &Cancellation,
) -> Result<CommandResult, CliError> {
    let mut service = load_service(data_dir)?;
    match command {
        ProjectCommand::List { no_status } => {
            let projects = if no_status {
                service.list_catalog_only()?
            } else {
                service.list()?
            };
            command_result(
                json_output,
                || {
                    projects
                        .iter()
                        .map(|project| project_line(project, !no_status))
                        .collect::<Vec<_>>()
                        .join("\n")
                },
                || Ok(json!({ "projects": project_values(&projects, !no_status)? })),
            )
        }
        ProjectCommand::Show(selection) => {
            let project = resolve_project(&service, selection.project.as_deref())?;
            command_result(
                json_output,
                || project_line(&project, true),
                || Ok(json!({ "project": project_value(&project, true)? })),
            )
        }
        ProjectCommand::Resolve { selector } => {
            let project = service.resolve(&ProjectSelector::from(selector))?;
            command_result(
                json_output,
                || project_line(&project, true),
                || Ok(json!({ "project": project_value(&project, true)? })),
            )
        }
        ProjectCommand::Import { path } => {
            let project = service.import_local(path)?;
            command_result(
                json_output,
                || format!("imported {}", project_line(&project, true)),
                || Ok(json!({ "project": project_value(&project, true)? })),
            )
        }
        ProjectCommand::Clone { remote } => {
            let project = service.import_repository(&remote, cancellation, |message| {
                emit_progress(json_output, &message)
            })?;
            command_result(
                json_output,
                || format!("cloned {}", project_line(&project, true)),
                || Ok(json!({ "project": project_value(&project, true)? })),
            )
        }
        ProjectCommand::Reconcile => {
            let removed = service.reconcile_managed_repositories()?;
            command_result(
                json_output,
                || format!("reconciled {} orphaned managed repositories", removed.len()),
                || {
                    Ok(json!({
                        "removed": removed
                            .iter()
                            .map(|path| path.to_string_lossy().into_owned())
                            .collect::<Vec<_>>()
                    }))
                },
            )
        }
        ProjectCommand::Forget(selection) => {
            let project = resolve_project(&service, selection.project.as_deref())?;
            if matches!(project.source, ProjectSource::ManagedRepository { .. }) {
                return Err(refusal(
                    RefusalKind::ManagedProjectRequiresDelete,
                    format!(
                        "project {} is a managed clone; use 'project delete --yes' so its checkout is not orphaned",
                        project.id
                    ),
                    json!({}),
                ));
            }
            let removed = service.remove(project.id)?;
            command_result(
                json_output,
                || format!("forgot {}", project_line(&removed, false)),
                || Ok(json!({ "project": project_value(&removed, false)? })),
            )
        }
        ProjectCommand::Delete { selection, yes } => {
            let project = resolve_project(&service, selection.project.as_deref())?;
            match project.source {
                ProjectSource::Local => {
                    return Err(refusal(
                        RefusalKind::LocalProjectRequiresForget,
                        format!(
                            "project {} is a local directory; use 'project forget' to preserve its files",
                            project.id
                        ),
                        json!({}),
                    ));
                }
                ProjectSource::Worktree { .. } => {
                    return Err(refusal(
                        RefusalKind::WorktreeRequiresRemove,
                        format!(
                            "project {} is a managed worktree; use 'worktree remove {}' so Git metadata is cleaned up",
                            project.id, project.id
                        ),
                        json!({}),
                    ));
                }
                ProjectSource::ManagedRepository { .. } => {}
            }
            if !yes {
                return Err(refusal(
                    RefusalKind::ConfirmationRequired,
                    format!(
                        "refusing to delete project {} at '{}'; pass --yes to confirm",
                        project.id,
                        project.root.display()
                    ),
                    json!({ "override_flag": "--yes" }),
                ));
            }
            let removed = service.remove_managed(project.id)?;
            command_result(
                json_output,
                || format!("deleted {}", project_line(&removed, false)),
                || Ok(json!({ "project": project_value(&removed, false)? })),
            )
        }
    }
}

fn run_worktree(
    command: WorktreeCommand,
    data_dir: Option<&Path>,
    json_output: bool,
    cancellation: &Cancellation,
) -> Result<CommandResult, CliError> {
    let mut service = load_service(data_dir)?;
    match command {
        WorktreeCommand::List { parent_id } => {
            let worktrees = service.worktrees(parent_id, cancellation)?;
            command_result(
                json_output,
                || {
                    worktrees
                        .iter()
                        .map(worktree_line)
                        .collect::<Vec<_>>()
                        .join("\n")
                },
                || {
                    Ok(json!({
                        "worktrees": worktrees.iter().map(worktree_value).collect::<Vec<_>>()
                    }))
                },
            )
        }
        WorktreeCommand::Create(arguments) => {
            let project =
                service.create_worktree(arguments.parent_id, &arguments.base, cancellation)?;
            command_result(
                json_output,
                || {
                    format!(
                        "created {}\t{}\t{}",
                        project.id,
                        project.display_name,
                        project.root.display()
                    )
                },
                || Ok(json!({ "project": project_value(&project, true)? })),
            )
        }
        WorktreeCommand::Remove {
            worktree_id,
            force,
            yes,
        } => {
            if force && !yes {
                return Err(refusal(
                    RefusalKind::ConfirmationRequired,
                    format!(
                        "forced removal of worktree {worktree_id} may discard uncommitted files; pass --yes to confirm"
                    ),
                    json!({ "override_flag": "--yes" }),
                ));
            }
            let project = service.remove_worktree(worktree_id, force, cancellation)?;
            command_result(
                json_output,
                || format!("removed {}", project.display_name),
                || Ok(json!({ "project": project_value(&project, false)? })),
            )
        }
        WorktreeCommand::Reconcile { parent_id } => {
            let removed = service.reconcile_worktrees(parent_id, cancellation)?;
            command_result(
                json_output,
                || format!("reconciled {} stale worktree entries", removed.len()),
                || Ok(json!({ "removed": project_values(&removed, false)? })),
            )
        }
    }
}

fn load_service(data_dir: Option<&Path>) -> Result<ProjectService, CliError> {
    match data_dir {
        Some(data_dir) => ProjectService::load_from_data_dir(data_dir),
        None => ProjectService::load(),
    }
    .map_err(Into::into)
}

fn resolve_project(service: &ProjectService, selector: Option<&str>) -> Result<Project, CliError> {
    let selector = match selector {
        Some(selector) => ProjectSelector::from(selector),
        None => ProjectSelector::current_directory(
            env::current_dir().map_err(CliError::CurrentDirectory)?,
        ),
    };
    service.resolve(&selector).map_err(Into::into)
}

fn refusal(kind: RefusalKind, message: String, details: Value) -> CliError {
    CliError::Refused {
        kind,
        message,
        details,
    }
}

fn project_line(project: &Project, status_checked: bool) -> String {
    let availability = if !status_checked {
        "unchecked"
    } else if project.available {
        "available"
    } else {
        "missing"
    };
    format!(
        "{}\t{}\t{}\t{}\t{availability}",
        project.id,
        project.display_name,
        project.root.display(),
        project_source_human_name(&project.source)
    )
}

fn project_values(projects: &[Project], status_checked: bool) -> Result<Vec<Value>, CliError> {
    projects
        .iter()
        .map(|project| project_value(project, status_checked))
        .collect()
}

fn project_value(project: &Project, status_checked: bool) -> Result<Value, CliError> {
    let last_opened = project.last_opened.format(&Rfc3339).map_err(|error| {
        CliError::WireProjection(format!(
            "project {} has a timestamp that cannot be represented as RFC 3339: {error}",
            project.id
        ))
    })?;
    let (remote, parent, worktree_branch) = match &project.source {
        ProjectSource::Local => (Value::Null, Value::Null, Value::Null),
        ProjectSource::ManagedRepository { remote } => (json!(remote), Value::Null, Value::Null),
        ProjectSource::Worktree {
            parent,
            worktree_branch,
        } => (
            Value::Null,
            json!(parent.to_string()),
            worktree_branch
                .as_ref()
                .map_or(Value::Null, |branch| json!(branch)),
        ),
    };
    let available = status_checked.then_some(project.available);
    let git = if status_checked {
        project.git.as_ref().map_or(Value::Null, git_value)
    } else {
        Value::Null
    };
    Ok(json!({
        "id": project.id.to_string(),
        "display_name": project.display_name,
        "root": project.root.to_string_lossy().into_owned(),
        "source": project_source_name(&project.source),
        "remote": remote,
        "parent": parent,
        "worktree_branch": worktree_branch,
        "last_opened": last_opened,
        "status_checked": status_checked,
        "available": available,
        "git": git,
    }))
}

fn candidate_value(project: &Project) -> Value {
    json!({
        "id": project.id.to_string(),
        "display_name": project.display_name,
        "root": project.root.to_string_lossy().into_owned(),
        "source": project_source_name(&project.source),
    })
}

fn git_value(status: &GitStatus) -> Value {
    json!({
        "branch": status.branch,
        "dirty": status.dirty,
        "upstream": status.upstream.as_ref().map_or(Value::Null, upstream_value),
        "staged": status.staged,
        "unstaged": status.unstaged,
    })
}

fn upstream_value(upstream: &UpstreamStatus) -> Value {
    json!({
        "name": upstream.name,
        "ahead": upstream.ahead,
        "behind": upstream.behind,
    })
}

fn project_source_name(source: &ProjectSource) -> &'static str {
    match source {
        ProjectSource::Local => "local",
        ProjectSource::ManagedRepository { .. } => "managed_repository",
        ProjectSource::Worktree { .. } => "worktree",
    }
}

fn project_source_human_name(source: &ProjectSource) -> &'static str {
    match source {
        ProjectSource::ManagedRepository { .. } => "managed",
        ProjectSource::Local => "local",
        ProjectSource::Worktree { .. } => "worktree",
    }
}

fn worktree_line(worktree: &Worktree) -> String {
    let id = worktree
        .project
        .as_ref()
        .map_or_else(|| "-".to_owned(), |project| project.id.to_string());
    let branch = worktree.branch.as_deref().unwrap_or("detached HEAD");
    let owner = if worktree.project.is_some() {
        "harkness"
    } else {
        "external"
    };
    let state = if worktree.locked {
        "locked"
    } else if worktree.prunable {
        "prunable"
    } else {
        "active"
    };
    format!(
        "{id}\t{branch}\t{}\t{owner}\t{state}",
        worktree.root.display()
    )
}

fn worktree_value(worktree: &Worktree) -> Value {
    json!({
        "id": worktree.project.as_ref().map(|project| project.id.to_string()),
        "branch": worktree.branch,
        "root": worktree.root.to_string_lossy(),
        "owner": if worktree.project.is_some() { "harkness" } else { "external" },
        "locked": worktree.locked,
        "prunable": worktree.prunable,
    })
}

fn contract_result(json_output: bool) -> CommandResult {
    let data = json!({
        "envelope_version": ENVELOPE_VERSION,
        "exit_codes": {
            "success": 0,
            "operation_failed": EXIT_OPERATION_FAILED,
            "usage_error": EXIT_USAGE,
            "guardrail_refusal": EXIT_REFUSED,
            "not_found": EXIT_NOT_FOUND,
            "conflict_or_busy": EXIT_CONFLICT,
            "cancelled": EXIT_CANCELLED,
        },
        "error_kinds": {
            "cli": CLI_ERROR_KINDS,
            "project": ProjectError::DIRECT_KINDS,
            "git": GitError::KINDS,
        },
        "streams": {
            "result": "stdout",
            "progress": "stderr",
        },
    });
    if json_output {
        CommandResult::Json(data)
    } else {
        CommandResult::Human(
            serde_json::to_string_pretty(&data)
                .unwrap_or_else(|_| "contract unavailable".to_owned()),
        )
    }
}

fn emit_success(data: &Value) -> io::Result<()> {
    write_json_line(
        &mut io::stdout().lock(),
        &SuccessEnvelope {
            v: ENVELOPE_VERSION,
            r#type: "success",
            ok: true,
            data,
        },
    )
}

fn emit_progress(json_output: bool, message: &str) {
    let output = if json_output {
        write_json_line(
            &mut io::stderr().lock(),
            &ProgressEnvelope {
                v: ENVELOPE_VERSION,
                r#type: "progress",
                message,
            },
        )
    } else {
        write_line(&mut io::stderr().lock(), message.as_bytes())
    };
    let _ = output;
}

fn emit_error(kind: &str, message: &str, details: &Value) -> io::Result<()> {
    write_json_line(
        &mut io::stdout().lock(),
        &ErrorEnvelope {
            v: ENVELOPE_VERSION,
            r#type: "error",
            ok: false,
            error: ErrorBody {
                kind,
                message,
                details,
            },
        },
    )
}

fn write_json_line(writer: &mut impl Write, value: &impl Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, value).map_err(|error| {
        if let Some(kind) = error.io_error_kind() {
            io::Error::new(kind, error)
        } else {
            io::Error::other(error.to_string())
        }
    })?;
    writer.write_all(b"\n")
}

fn write_line(writer: &mut impl Write, value: &[u8]) -> io::Result<()> {
    writer.write_all(value)?;
    writer.write_all(b"\n")
}

fn install_interrupt_handler() -> io::Result<Cancellation> {
    let cancellation = Cancellation::default();
    INTERRUPTED.store(false, Ordering::Release);
    #[cfg(any(unix, windows))]
    {
        extern "C" fn request_cancellation(_signal: libc::c_int) {
            INTERRUPTED.store(true, Ordering::Release);
        }

        // SAFETY: the handler only performs an atomic store, which is
        // async-signal-safe. It has the C ABI and remains valid for the rest of
        // this process. SIGINT is the portable Ctrl-C signal on supported CI
        // platforms.
        let previous = unsafe {
            libc::signal(
                libc::SIGINT,
                request_cancellation as *const () as libc::sighandler_t,
            )
        };
        if previous == libc::SIG_ERR as libc::sighandler_t {
            return Err(io::Error::last_os_error());
        }
        let watched = cancellation.clone();
        thread::spawn(move || {
            while !INTERRUPTED.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(10));
            }
            watched.cancel();
        });
    }
    Ok(cancellation)
}

fn project_exit_code(error: &ProjectError) -> u8 {
    match error {
        ProjectError::CloneCancelled => EXIT_CANCELLED,
        ProjectError::ProjectSelectorNotFound { .. } | ProjectError::ProjectNotFound(_) => {
            EXIT_NOT_FOUND
        }
        ProjectError::AmbiguousProjectSelector { .. } | ProjectError::ParentHasWorktrees { .. } => {
            EXIT_CONFLICT
        }
        ProjectError::UnsafeManagedRemoval { .. }
        | ProjectError::WorktreeRemovalRequired { .. }
        | ProjectError::UnsafeWorktreeRemoval { .. }
        | ProjectError::WorktreeParentUnsupported { .. }
        | ProjectError::DirtyWorktreeRemoval { .. } => EXIT_REFUSED,
        ProjectError::Git(error) => git_exit_code(error),
        _ => EXIT_OPERATION_FAILED,
    }
}

fn git_exit_code(error: &GitError) -> u8 {
    match error {
        GitError::Cancelled => EXIT_CANCELLED,
        GitError::NoSuchBranch { .. } | GitError::NotARepository { .. } => EXIT_NOT_FOUND,
        GitError::RepositoryBusy { .. }
        | GitError::BranchAlreadyExists { .. }
        | GitError::BranchCheckedOutInWorktree { .. }
        | GitError::WorktreeLocked { .. } => EXIT_CONFLICT,
        _ => EXIT_OPERATION_FAILED,
    }
}

fn project_error_details(error: &ProjectError) -> Value {
    match error {
        ProjectError::AmbiguousProjectSelector { candidates, .. } => json!({
            "candidates": candidates.iter().map(candidate_value).collect::<Vec<_>>()
        }),
        ProjectError::DirtyWorktreeRemoval { .. } => {
            json!({ "override_flags": ["--force", "--yes"] })
        }
        _ => json!({}),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, io};

    use super::{
        CLI_ERROR_KINDS, CliError, EXIT_OPERATION_FAILED, EXIT_REFUSED, GitError, ProjectError,
        RefusalKind, requested_json,
    };

    #[test]
    fn guardrail_and_operation_failures_have_distinct_exit_codes() {
        let refusal = CliError::Refused {
            kind: RefusalKind::ConfirmationRequired,
            message: "confirmation required".to_owned(),
            details: serde_json::json!({ "override_flag": "--yes" }),
        };
        assert_eq!(refusal.exit_code(), EXIT_REFUSED);
        assert_eq!(refusal.details()["override_flag"], "--yes");

        let failure = CliError::Project(ProjectError::CloneFailed {
            stderr: "network failed".to_owned(),
        });
        assert_eq!(failure.exit_code(), EXIT_OPERATION_FAILED);
    }

    #[test]
    fn error_kind_namespaces_are_unique() {
        let kinds = CLI_ERROR_KINDS
            .iter()
            .chain(ProjectError::DIRECT_KINDS)
            .chain(GitError::KINDS)
            .copied()
            .collect::<Vec<_>>();
        let unique = kinds.iter().copied().collect::<HashSet<_>>();
        assert_eq!(unique.len(), kinds.len(), "error kind collision: {kinds:?}");
    }

    #[test]
    fn cli_error_kind_contract_is_stable() {
        let refused = |kind| CliError::Refused {
            kind,
            message: "fixture".to_owned(),
            details: serde_json::json!({}),
        };
        let cases = [
            CliError::CurrentDirectory(io::Error::other("fixture")),
            CliError::InterruptHandler(io::Error::other("fixture")),
            CliError::WireProjection("fixture".to_owned()),
            refused(RefusalKind::ConfirmationRequired),
            refused(RefusalKind::ManagedProjectRequiresDelete),
            refused(RefusalKind::LocalProjectRequiresForget),
            refused(RefusalKind::WorktreeRequiresRemove),
        ];
        let kinds = cases.iter().map(CliError::kind).collect::<Vec<_>>();
        assert_eq!(kinds, CLI_ERROR_KINDS[1..]);
    }

    #[test]
    fn json_detection_stops_at_the_argument_terminator() {
        let arguments = ["harkness", "worktree", "list", "--", "--json"].map(Into::into);
        assert!(!requested_json(&arguments));
    }
}
