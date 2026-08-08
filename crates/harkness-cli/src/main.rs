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
    ArgGroup, Args, Parser, Subcommand,
    error::{Error as ClapError, ErrorKind},
};
use harkness_core::{
    Branch, BranchCheckout, BranchKind, BranchListOptions, Cancellation, CommitOptions,
    CommitOutcome, CreateBranchOptions, DetailedStatus, FetchOptions, FetchOutcome, FileChange,
    GitError, GitStatus, HeadState, PendingOperation, Project, ProjectError, ProjectSelector,
    ProjectService, ProjectSource, PullOptions, PullOutcome, PullStrategy, PushOptions,
    PushOutcome, RefUpdate, StageOutcome, StagePathResult, StatusRefreshOutcome, UpstreamStatus,
    Worktree, WorktreeBase,
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
    "path_operation_failed",
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
    after_help = "When --project is omitted from any command that accepts it, Harkness walks upward from the current directory and uses the deepest catalogued project root. This lets an agent run inside a repository or worktree without copying its project identifier.\n\nExit codes: 0 success, 1 operation failed, 2 usage error, 3 guardrail refusal, 4 not found, 5 conflict or busy, 130 cancelled."
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
    /// Inspect and change Git repositories through the shared core service.
    Git {
        #[command(subcommand)]
        command: GitCommand,
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
    /// List linked worktrees for the selected parent project.
    List(ProjectSelection),
    /// Create a Harkness-managed linked worktree on a new branch.
    #[command(visible_alias = "create")]
    Add(AddWorktree),
    /// Remove a Harkness-managed linked worktree.
    Remove {
        #[command(flatten)]
        selection: ProjectSelection,
        /// Discard uncommitted changes in the worktree.
        #[arg(long)]
        force: bool,
    },
    /// Remove stale Harkness-owned worktree records selectively.
    #[command(visible_alias = "reconcile")]
    Prune(ProjectSelection),
}

#[derive(Debug, Subcommand)]
enum GitCommand {
    /// Report repository state. JSON always includes path records.
    Status {
        #[command(flatten)]
        selection: ProjectSelection,
        /// Include changed paths in human-readable output.
        #[arg(long)]
        paths: bool,
    },
    /// Update local remote-tracking refs.
    Fetch {
        #[command(flatten)]
        selection: ProjectSelection,
        /// Remote to fetch instead of resolving one from the repository.
        #[arg(long, value_name = "NAME")]
        remote: Option<String>,
        /// Delete remote-tracking branches removed from the remote.
        #[arg(long)]
        prune: bool,
    },
    /// Fetch and reconcile the checked-out branch with its upstream.
    Pull(PullArguments),
    /// Publish the checked-out branch under the same name.
    Push {
        #[command(flatten)]
        selection: ProjectSelection,
        /// Configure the published branch as the local branch's upstream.
        #[arg(long)]
        set_upstream: bool,
        /// Permit pushing the remote's default branch.
        #[arg(long)]
        allow_default_branch: bool,
        /// Replace the remote tip only if it still matches the last fetch.
        #[arg(long)]
        force_with_lease: bool,
    },
    /// Inspect and manage branches.
    Branch {
        #[command(subcommand)]
        command: GitBranchCommand,
    },
    /// Add paths to the index.
    Stage(StageArguments),
    /// Remove paths from the index without changing the working tree.
    Unstage {
        #[command(flatten)]
        selection: ProjectSelection,
        /// Repository-relative or absolute paths to unstage.
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<PathBuf>,
    },
    /// Create a commit from the index.
    Commit {
        #[command(flatten)]
        selection: ProjectSelection,
        /// Commit message.
        #[arg(long, value_name = "MSG")]
        message: String,
        /// Replace the current commit.
        #[arg(long)]
        amend: bool,
        /// Permit a commit with an unchanged tree.
        #[arg(long)]
        allow_empty: bool,
    },
}

#[derive(Debug, Subcommand)]
enum GitBranchCommand {
    /// List local branches and optionally remote-tracking branches.
    List {
        #[command(flatten)]
        selection: ProjectSelection,
        /// Include remote-tracking branches.
        #[arg(long)]
        all: bool,
    },
    /// Create a local branch.
    Create {
        #[command(flatten)]
        selection: ProjectSelection,
        /// Name of the branch to create.
        name: String,
        /// Revision at which to create the branch instead of HEAD.
        #[arg(long, value_name = "REF")]
        from: Option<String>,
        /// Check out the branch after creating it.
        #[arg(long)]
        checkout: bool,
    },
    /// Check out an existing local branch.
    Checkout {
        #[command(flatten)]
        selection: ProjectSelection,
        name: String,
    },
    /// Delete a local branch.
    Delete {
        #[command(flatten)]
        selection: ProjectSelection,
        name: String,
        /// Delete a branch with unmerged commits.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("strategy")
        .multiple(false)
        .args(["ff_only", "rebase", "merge"])
))]
struct PullArguments {
    #[command(flatten)]
    selection: ProjectSelection,
    /// Refuse unless the branch can advance without a merge or rewrite.
    #[arg(long)]
    ff_only: bool,
    /// Replay local commits on top of the upstream.
    #[arg(long)]
    rebase: bool,
    /// Merge the upstream into the local branch.
    #[arg(long)]
    merge: bool,
}

#[derive(Debug, Args)]
struct StageArguments {
    #[command(flatten)]
    selection: ProjectSelection,
    /// Repository-relative or absolute paths to stage.
    #[arg(required_unless_present = "all", value_name = "PATH")]
    paths: Vec<PathBuf>,
    /// Stage every change, including deletions.
    #[arg(long, conflicts_with = "paths")]
    all: bool,
}

#[derive(Debug, Args)]
struct AddWorktree {
    #[command(flatten)]
    selection: ProjectSelection,
    /// New branch to create for the worktree, or revision when detached.
    #[arg(long, value_name = "NAME")]
    branch: String,
    /// Start the branch, or detached checkout, at this revision.
    #[arg(long, value_name = "REF")]
    from: Option<String>,
    /// Check out an existing local branch instead of creating it.
    #[arg(long, conflicts_with_all = ["from", "detach"])]
    existing: bool,
    /// Create a detached checkout at --from, or at --branch when --from is absent.
    #[arg(long)]
    detach: bool,
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
    PathOperation {
        operation: &'static str,
        details: Value,
    },
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

impl From<GitError> for CliError {
    fn from(error: GitError) -> Self {
        Self::Project(ProjectError::from(error))
    }
}

impl CliError {
    fn kind(&self) -> &'static str {
        match self {
            Self::Project(error) => error.kind(),
            Self::CurrentDirectory(_) => "current_directory_unavailable",
            Self::InterruptHandler(_) => "interrupt_handler_unavailable",
            Self::WireProjection(_) => "wire_projection_failed",
            Self::PathOperation { .. } => "path_operation_failed",
            Self::Refused { kind, .. } => kind.as_str(),
        }
    }

    fn message(&self) -> String {
        match self {
            Self::Project(error) => error.to_string(),
            Self::WireProjection(message) => message.clone(),
            Self::PathOperation { operation, .. } => {
                format!("{operation} failed for one or more paths")
            }
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
            Self::CurrentDirectory(_)
            | Self::InterruptHandler(_)
            | Self::WireProjection(_)
            | Self::PathOperation { .. } => EXIT_OPERATION_FAILED,
            Self::Refused { .. } => EXIT_REFUSED,
        }
    }

    fn details(&self) -> Value {
        match self {
            Self::Project(error) => project_error_details(error),
            Self::Refused { details, .. } => details.clone(),
            Self::PathOperation { details, .. } => details.clone(),
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
        Command::Git { command } => run_git(command, data_dir.as_deref(), json, cancellation),
        Command::Worktree { command } => {
            run_worktree(command, data_dir.as_deref(), json, cancellation)
        }
        Command::Contract => Ok(contract_result(json)),
    }
}

fn run_git(
    command: GitCommand,
    data_dir: Option<&Path>,
    json_output: bool,
    cancellation: &Cancellation,
) -> Result<CommandResult, CliError> {
    let service = load_service(data_dir)?;
    match command {
        GitCommand::Status { selection, paths } => {
            let git = selected_git(&service, selection)?;
            let status = git.detailed_status(cancellation)?;
            command_result(
                json_output,
                || detailed_status_line(&status, paths),
                || Ok(json!({ "status": detailed_status_value(&status) })),
            )
        }
        GitCommand::Fetch {
            selection,
            remote,
            prune,
        } => {
            let git = selected_git(&service, selection)?;
            let outcome = git.fetch(&FetchOptions { remote, prune }, cancellation, |message| {
                emit_progress(json_output, &message)
            })?;
            command_result(
                json_output,
                || {
                    format!(
                        "fetched {} ({})",
                        outcome.remote,
                        change_word(outcome.updated)
                    )
                },
                || Ok(fetch_outcome_value(&outcome)),
            )
        }
        GitCommand::Pull(arguments) => {
            let git = selected_git(&service, arguments.selection)?;
            let strategy = if arguments.ff_only {
                PullStrategy::FastForwardOnly
            } else if arguments.rebase {
                PullStrategy::Rebase
            } else if arguments.merge {
                PullStrategy::Merge
            } else {
                PullStrategy::FastForwardOnly
            };
            let outcome = git.pull(
                &PullOptions {
                    remote: None,
                    strategy,
                },
                cancellation,
                |message| emit_progress(json_output, &message),
            )?;
            command_result(
                json_output,
                || {
                    format!(
                        "pulled {}/{} ({})",
                        outcome.remote,
                        outcome.branch,
                        change_word(outcome.updated)
                    )
                },
                || Ok(pull_outcome_value(&outcome)),
            )
        }
        GitCommand::Push {
            selection,
            set_upstream,
            allow_default_branch,
            force_with_lease,
        } => {
            let git = selected_git(&service, selection)?;
            let outcome = git.push(
                &PushOptions {
                    remote: None,
                    set_upstream,
                    force_with_lease,
                    allow_default_branch,
                },
                cancellation,
                |message| emit_progress(json_output, &message),
            )?;
            command_result(
                json_output,
                || {
                    format!(
                        "pushed {}/{} ({})",
                        outcome.remote,
                        outcome.branch,
                        ref_update_name(outcome.update)
                    )
                },
                || Ok(push_outcome_value(&outcome)),
            )
        }
        GitCommand::Branch { command } => {
            run_git_branch(command, &service, json_output, cancellation)
        }
        GitCommand::Stage(arguments) => {
            let git = selected_git(&service, arguments.selection)?;
            if arguments.all {
                let status = git.stage_all(cancellation)?;
                command_result(
                    json_output,
                    || "staged all changes".to_owned(),
                    || Ok(json!({ "status": detailed_status_value(&status) })),
                )
            } else {
                let outcome = git.stage(arguments.paths, cancellation)?;
                if !outcome.all_succeeded() {
                    return Err(CliError::PathOperation {
                        operation: "staging",
                        details: stage_outcome_value(&outcome),
                    });
                }
                command_result(
                    json_output,
                    || stage_outcome_line("staged", &outcome),
                    || Ok(stage_outcome_value(&outcome)),
                )
            }
        }
        GitCommand::Unstage { selection, paths } => {
            let git = selected_git(&service, selection)?;
            let outcome = git.unstage(paths, cancellation)?;
            if !outcome.all_succeeded() {
                return Err(CliError::PathOperation {
                    operation: "unstaging",
                    details: stage_outcome_value(&outcome),
                });
            }
            command_result(
                json_output,
                || stage_outcome_line("unstaged", &outcome),
                || Ok(stage_outcome_value(&outcome)),
            )
        }
        GitCommand::Commit {
            selection,
            message,
            amend,
            allow_empty,
        } => {
            let git = selected_git(&service, selection)?;
            let outcome = git.commit(
                &message,
                &CommitOptions::default()
                    .with_amend(amend)
                    .with_allow_empty(allow_empty),
                cancellation,
            )?;
            command_result(
                json_output,
                || format!("committed {}", outcome.commit_id),
                || Ok(commit_outcome_value(&outcome)),
            )
        }
    }
}

fn run_git_branch(
    command: GitBranchCommand,
    service: &ProjectService,
    json_output: bool,
    cancellation: &Cancellation,
) -> Result<CommandResult, CliError> {
    match command {
        GitBranchCommand::List { selection, all } => {
            let git = selected_git(service, selection)?;
            let branches = git.branches(
                &BranchListOptions {
                    include_remote_tracking: all,
                    ..BranchListOptions::default()
                },
                cancellation,
            )?;
            command_result(
                json_output,
                || {
                    branches
                        .iter()
                        .map(branch_line)
                        .collect::<Vec<_>>()
                        .join("\n")
                },
                || {
                    Ok(json!({
                        "branches": branches.iter().map(branch_value).collect::<Vec<_>>()
                    }))
                },
            )
        }
        GitBranchCommand::Create {
            selection,
            name,
            from,
            checkout,
        } => {
            let git = selected_git(service, selection)?;
            git.create_branch(
                &name,
                &CreateBranchOptions {
                    start_point: from,
                    checkout,
                },
                cancellation,
            )?;
            command_result(
                json_output,
                || format!("created branch {name}"),
                || Ok(json!({ "branch": name, "checked_out": checkout })),
            )
        }
        GitBranchCommand::Checkout { selection, name } => {
            let git = selected_git(service, selection)?;
            git.checkout_branch(&name, cancellation)?;
            command_result(
                json_output,
                || format!("checked out {name}"),
                || Ok(json!({ "branch": name })),
            )
        }
        GitBranchCommand::Delete {
            selection,
            name,
            force,
        } => {
            let git = selected_git(service, selection)?;
            git.delete_branch(&name, force, cancellation)?;
            command_result(
                json_output,
                || format!("deleted branch {name}"),
                || Ok(json!({ "branch": name })),
            )
        }
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
        WorktreeCommand::List(selection) => {
            let parent = resolve_project(&service, selection.project.as_deref())?;
            let worktrees = service.worktrees(parent.id, cancellation)?;
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
        WorktreeCommand::Add(arguments) => {
            let parent = resolve_project(&service, arguments.selection.project.as_deref())?;
            let base = if arguments.existing {
                WorktreeBase::ExistingBranch {
                    name: arguments.branch,
                }
            } else if arguments.detach {
                WorktreeBase::Detached {
                    commit: arguments.from.unwrap_or(arguments.branch),
                }
            } else {
                WorktreeBase::NewBranch {
                    name: arguments.branch,
                    start_point: arguments.from,
                }
            };
            let project = service.create_worktree(parent.id, &base, cancellation)?;
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
        WorktreeCommand::Remove { selection, force } => {
            let worktree = resolve_project(&service, selection.project.as_deref())?;
            let project = service.remove_worktree(worktree.id, force, cancellation)?;
            command_result(
                json_output,
                || format!("removed {}", project.display_name),
                || Ok(json!({ "project": project_value(&project, false)? })),
            )
        }
        WorktreeCommand::Prune(selection) => {
            let parent = resolve_project(&service, selection.project.as_deref())?;
            let removed = service.prune_worktrees(parent.id, cancellation)?;
            command_result(
                json_output,
                || format!("pruned {} stale worktree entries", removed.len()),
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

fn selected_git(
    service: &ProjectService,
    selection: ProjectSelection,
) -> Result<harkness_core::GitService, CliError> {
    let project = resolve_project(service, selection.project.as_deref())?;
    service.git(project.id).map_err(Into::into)
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
    let (root, path_is_lossy) = wire_path(&project.root);
    Ok(json!({
        "id": project.id.to_string(),
        "display_name": project.display_name,
        "root": root,
        "path_is_lossy": path_is_lossy,
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
    let (root, path_is_lossy) = wire_path(&project.root);
    json!({
        "id": project.id.to_string(),
        "display_name": project.display_name,
        "root": root,
        "path_is_lossy": path_is_lossy,
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

fn optional_git_value(status: Option<&GitStatus>) -> Value {
    status.map_or(Value::Null, git_value)
}

fn detailed_status_value(status: &DetailedStatus) -> Value {
    json!({
        "head": head_value(&status.head),
        "upstream": status.upstream.as_ref().map_or(Value::Null, upstream_value),
        "pending": status.pending.map_or(Value::Null, |pending| json!(pending_name(pending))),
        "entries": status.entries.iter().map(status_entry_value).collect::<Vec<_>>(),
    })
}

fn head_value(head: &HeadState) -> Value {
    match head {
        HeadState::Unborn { branch } => json!({
            "kind": "unborn",
            "branch": branch,
        }),
        HeadState::Branch { name } => json!({
            "kind": "branch",
            "name": name,
        }),
        HeadState::Detached { commit } => json!({
            "kind": "detached",
            "commit": commit,
        }),
    }
}

fn status_entry_value(entry: &harkness_core::StatusEntry) -> Value {
    let (path, path_is_lossy) = wire_path(&entry.path);
    let (rename_source, rename_source_is_lossy) =
        entry
            .rename_source
            .as_ref()
            .map_or((Value::Null, Value::Null), |source| {
                let (source, is_lossy) = wire_path(source);
                (json!(source), json!(is_lossy))
            });
    json!({
        "path": path,
        "path_is_lossy": path_is_lossy,
        "staged": entry.staged.map_or(Value::Null, |change| json!(file_change_name(change))),
        "unstaged": entry.unstaged.map_or(Value::Null, |change| json!(file_change_name(change))),
        "rename_source": rename_source,
        "rename_source_is_lossy": rename_source_is_lossy,
        "conflicted": entry.conflicted,
    })
}

fn fetch_outcome_value(outcome: &FetchOutcome) -> Value {
    json!({
        "remote": outcome.remote,
        "updated": outcome.updated,
        "status": optional_git_value(outcome.status.as_ref()),
    })
}

fn pull_outcome_value(outcome: &PullOutcome) -> Value {
    json!({
        "remote": outcome.remote,
        "branch": outcome.branch,
        "strategy": pull_strategy_name(outcome.strategy),
        "updated": outcome.updated,
        "status": optional_git_value(outcome.status.as_ref()),
    })
}

fn push_outcome_value(outcome: &PushOutcome) -> Value {
    json!({
        "remote": outcome.remote,
        "branch": outcome.branch,
        "upstream_configured": outcome.upstream_configured,
        "update": ref_update_name(outcome.update),
        "updated": outcome.updated(),
        "status": optional_git_value(outcome.status.as_ref()),
    })
}

fn branch_value(branch: &Branch) -> Value {
    json!({
        "name": branch.name,
        "kind": match branch.kind {
            BranchKind::Local => "local",
            BranchKind::RemoteTracking => "remote_tracking",
        },
        "tip": branch.tip.to_string(),
        "upstream": branch.upstream.as_ref().map_or(Value::Null, upstream_value),
        "checkout": branch_checkout_value(&branch.checkout),
    })
}

fn branch_checkout_value(checkout: &BranchCheckout) -> Value {
    match checkout {
        BranchCheckout::NotCheckedOut => json!({ "kind": "not_checked_out" }),
        BranchCheckout::CurrentWorktree => json!({ "kind": "current_worktree" }),
        BranchCheckout::OtherWorktree(path) => {
            let (path, path_is_lossy) = wire_path(path);
            json!({
                "kind": "other_worktree",
                "path": path,
                "path_is_lossy": path_is_lossy,
            })
        }
    }
}

fn stage_outcome_value(outcome: &StageOutcome) -> Value {
    json!({
        "all_succeeded": outcome.all_succeeded(),
        "paths": outcome.paths.iter().map(|path| {
            let (written, path_is_lossy) = wire_path(&path.path);
            let result = match &path.result {
                StagePathResult::Succeeded => json!({ "kind": "succeeded" }),
                StagePathResult::Failed(error) => json!({
                    "kind": "failed",
                    "error": {
                        "kind": error.kind(),
                        "message": error.to_string(),
                        "details": git_error_details(error),
                    }
                }),
                StagePathResult::NotAttempted => json!({ "kind": "not_attempted" }),
                _ => json!({ "kind": "unknown" }),
            };
            json!({
                "path": written,
                "path_is_lossy": path_is_lossy,
                "result": result,
            })
        }).collect::<Vec<_>>(),
        "status": status_refresh_value(&outcome.status),
    })
}

fn commit_outcome_value(outcome: &CommitOutcome) -> Value {
    json!({
        "commit_id": outcome.commit_id,
        "amended": outcome.amended,
        "status": status_refresh_value(&outcome.status),
    })
}

fn status_refresh_value(status: &StatusRefreshOutcome) -> Value {
    match status {
        StatusRefreshOutcome::Skipped => json!({ "kind": "skipped" }),
        StatusRefreshOutcome::Refreshed(status) => json!({
            "kind": "refreshed",
            "status": detailed_status_value(status),
        }),
        StatusRefreshOutcome::Failed(error) => json!({
            "kind": "failed",
            "error": {
                "kind": error.kind(),
                "message": error.to_string(),
                "details": git_error_details(error),
            }
        }),
        _ => json!({ "kind": "unknown" }),
    }
}

fn detailed_status_line(status: &DetailedStatus, include_paths: bool) -> String {
    let mut lines = vec![match &status.head {
        HeadState::Unborn { branch } => {
            format!("unborn {}", branch.as_deref().unwrap_or("unnamed branch"))
        }
        HeadState::Branch { name } => format!("branch {name}"),
        HeadState::Detached { commit } => format!("detached {commit}"),
    }];
    if let Some(upstream) = &status.upstream {
        lines.push(format!(
            "upstream {} (+{} -{})",
            upstream.name, upstream.ahead, upstream.behind
        ));
    }
    if let Some(pending) = status.pending {
        lines.push(format!("pending {}", pending_name(pending)));
    }
    if include_paths {
        lines.extend(status.entries.iter().map(|entry| {
            format!(
                "{}\t{}\t{}",
                entry.staged.map_or("-", file_change_name),
                entry.unstaged.map_or("-", file_change_name),
                entry.path.to_string_lossy()
            )
        }));
    } else {
        lines.push(format!("{} changed paths", status.entries.len()));
    }
    lines.join("\n")
}

fn branch_line(branch: &Branch) -> String {
    let checkout = match &branch.checkout {
        BranchCheckout::NotCheckedOut => "".to_owned(),
        BranchCheckout::CurrentWorktree => "\tcurrent".to_owned(),
        BranchCheckout::OtherWorktree(path) => format!("\t{}", path.display()),
    };
    format!("{}\t{}{}", branch.name, branch.tip, checkout)
}

fn stage_outcome_line(verb: &str, outcome: &StageOutcome) -> String {
    let succeeded = outcome.paths.iter().filter(|path| path.succeeded()).count();
    format!("{verb} {succeeded}/{} paths", outcome.paths.len())
}

fn wire_path(path: &Path) -> (String, bool) {
    (
        path.to_string_lossy().into_owned(),
        path.as_os_str().to_str().is_none(),
    )
}

const fn file_change_name(change: FileChange) -> &'static str {
    match change {
        FileChange::Added => "added",
        FileChange::Modified => "modified",
        FileChange::Deleted => "deleted",
        FileChange::Renamed => "renamed",
        FileChange::Copied => "copied",
        FileChange::TypeChanged => "type_changed",
        FileChange::Untracked => "untracked",
        FileChange::Unmerged => "unmerged",
    }
}

const fn pending_name(pending: PendingOperation) -> &'static str {
    match pending {
        PendingOperation::Merge => "merge",
        PendingOperation::Rebase => "rebase",
        PendingOperation::CherryPick => "cherry_pick",
        PendingOperation::Revert => "revert",
        PendingOperation::Bisect => "bisect",
        PendingOperation::ApplyMailbox => "apply_mailbox",
        PendingOperation::Other => "other",
        _ => "other",
    }
}

const fn pull_strategy_name(strategy: PullStrategy) -> &'static str {
    match strategy {
        PullStrategy::FastForwardOnly => "fast_forward_only",
        PullStrategy::Merge => "merge",
        PullStrategy::Rebase => "rebase",
    }
}

const fn ref_update_name(update: RefUpdate) -> &'static str {
    match update {
        RefUpdate::Unchanged => "unchanged",
        RefUpdate::Created => "created",
        RefUpdate::FastForward => "fast_forward",
        RefUpdate::Forced => "forced",
        RefUpdate::Unknown => "unknown",
        _ => "unknown",
    }
}

const fn change_word(updated: bool) -> &'static str {
    if updated { "updated" } else { "unchanged" }
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
    let (root, path_is_lossy) = wire_path(&worktree.root);
    json!({
        "id": worktree.project.as_ref().map(|project| project.id.to_string()),
        "branch": worktree.branch,
        "root": root,
        "path_is_lossy": path_is_lossy,
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
        GitError::PathOutsideRepository { .. }
        | GitError::EmptyCommitMessage
        | GitError::NothingStaged
        | GitError::AmendUnbornBranch
        | GitError::CurrentBranchDeletion { .. }
        | GitError::DefaultBranchDeletion { .. }
        | GitError::UnmergedBranchDeletion { .. }
        | GitError::NoUpstream { .. }
        | GitError::UnbornBranch { .. }
        | GitError::LocalUpstreamUnsupported { .. }
        | GitError::OperationInProgress { .. }
        | GitError::DefaultBranchPush { .. }
        | GitError::DefaultBranchUnknown { .. }
        | GitError::DetachedHead { .. } => EXIT_REFUSED,
        _ => EXIT_OPERATION_FAILED,
    }
}

fn project_error_details(error: &ProjectError) -> Value {
    match error {
        ProjectError::AmbiguousProjectSelector { candidates, .. } => json!({
            "candidates": candidates.iter().map(candidate_value).collect::<Vec<_>>()
        }),
        ProjectError::DirtyWorktreeRemoval { .. } => {
            json!({ "override_flag": "--force" })
        }
        ProjectError::Git(error) => git_error_details(error),
        _ => json!({}),
    }
}

fn git_error_details(error: &GitError) -> Value {
    match error {
        GitError::NothingStaged => json!({ "override_flag": "--allow-empty" }),
        GitError::UnmergedBranchDeletion { .. } => json!({ "override_flag": "--force" }),
        GitError::NoUpstream { .. } => json!({ "override_flag": "--set-upstream" }),
        GitError::DefaultBranchPush { .. } | GitError::DefaultBranchUnknown { .. } => {
            json!({ "override_flag": "--allow-default-branch" })
        }
        GitError::Interrupted {
            pending, status, ..
        } => json!({
            "pending": pending_name(*pending),
            "status": status.as_deref().map_or(Value::Null, git_value),
        }),
        _ => json!({}),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, io};

    use super::{
        CLI_ERROR_KINDS, CliError, EXIT_OPERATION_FAILED, EXIT_REFUSED, GitError, Project,
        ProjectError, RefusalKind, project_value, requested_json,
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
            CliError::PathOperation {
                operation: "staging",
                details: serde_json::json!({}),
            },
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

    #[cfg(unix)]
    #[test]
    fn non_utf8_project_roots_use_the_cli_path_policy() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt, path::PathBuf};

        let project = Project {
            id: Default::default(),
            display_name: "lossy".to_owned(),
            root: PathBuf::from(OsString::from_vec(b"/tmp/lossy-\xff".to_vec())),
            source: harkness_core::ProjectSource::Local,
            last_opened: time::OffsetDateTime::UNIX_EPOCH,
            available: true,
            git: None,
        };

        let value = project_value(&project, true).unwrap();

        assert_eq!(value["path_is_lossy"], true);
        assert!(value["root"].as_str().unwrap().contains('\u{fffd}'));
    }
}
