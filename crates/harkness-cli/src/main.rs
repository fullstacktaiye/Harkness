use std::{
    env,
    io::{self, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

use clap::{ArgGroup, Args, Parser, Subcommand, error::ErrorKind};
use harkness_core::{
    Cancellation, GitError, Project, ProjectError, ProjectId, ProjectSelector, ProjectService,
    ProjectSource, Worktree, WorktreeBase,
};
use serde::Serialize;
use serde_json::{Map, Value, json};

const EXIT_OPERATION_FAILED: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_REFUSED: u8 = 3;
const EXIT_NOT_FOUND: u8 = 4;
const EXIT_CONFLICT: u8 = 5;
const EXIT_CANCELLED: u8 = 130;

#[derive(Debug, Parser)]
#[command(
    name = "harkness",
    version,
    about = "Manage Harkness projects and workspaces",
    arg_required_else_help = true,
    disable_help_subcommand = true,
    after_help = "When --project is omitted, Harkness walks upward from the current directory and uses the deepest catalogued project root. This lets an agent run inside a repository or worktree without copying its project identifier.\n\nExit codes: 0 success, 1 operation failed, 2 usage error, 3 guardrail refusal, 4 not found, 5 conflict or busy, 130 cancelled."
)]
struct Cli {
    /// Emit exactly one machine-readable result object on standard output.
    #[arg(long, global = true)]
    json: bool,

    /// Select by full ID, UUID prefix (8+ characters), path, or display name.
    #[arg(long, global = true, value_name = "SELECTOR")]
    project: Option<String>,

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
}

#[derive(Debug, Subcommand)]
enum ProjectCommand {
    /// List every catalogued project.
    List,
    /// Show the selected project.
    Show,
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
    /// Forget a local project without touching its files.
    Forget,
    /// Delete a Harkness-managed clone and remove it from the catalog.
    Delete {
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
    },
    /// Remove stale Harkness-owned worktree records selectively.
    Reconcile { parent_id: ProjectId },
}

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("base")
        .required(true)
        .multiple(false)
        .args(["new", "existing", "detached"])
))]
struct CreateWorktree {
    parent_id: ProjectId,

    /// Create and check out a new branch.
    #[arg(long, value_name = "BRANCH")]
    new: Option<String>,

    /// Check out an existing local branch.
    #[arg(long, value_name = "BRANCH")]
    existing: Option<String>,

    /// Check out a detached revision.
    #[arg(long, value_name = "REVISION")]
    detached: Option<String>,

    /// Start a new branch at this revision instead of HEAD.
    #[arg(long, requires = "new", value_name = "REVISION")]
    start: Option<String>,
}

struct CommandResult {
    human: String,
    data: Value,
}

impl CommandResult {
    fn new(human: impl Into<String>, data: Value) -> Self {
        Self {
            human: human.into(),
            data,
        }
    }
}

#[derive(Debug)]
enum CliError {
    Project(ProjectError),
    Usage(String),
    CurrentDirectory(io::Error),
    Refused {
        kind: &'static str,
        message: String,
        override_flag: Option<&'static str>,
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
            Self::Usage(_) => "usage_error",
            Self::CurrentDirectory(_) => "current_directory_unavailable",
            Self::Refused { kind, .. } => kind,
        }
    }

    fn message(&self) -> String {
        match self {
            Self::Project(error) => error.to_string(),
            Self::Usage(message) => message.clone(),
            Self::CurrentDirectory(error) => {
                format!("the current working directory could not be determined: {error}")
            }
            Self::Refused { message, .. } => message.clone(),
        }
    }

    fn exit_code(&self) -> u8 {
        match self {
            Self::Project(error) => project_exit_code(error),
            Self::Usage(_) => EXIT_USAGE,
            Self::CurrentDirectory(_) => EXIT_OPERATION_FAILED,
            Self::Refused { .. } => EXIT_REFUSED,
        }
    }

    fn details(&self) -> Value {
        match self {
            Self::Project(error) => project_error_details(error),
            Self::Refused {
                override_flag: Some(flag),
                ..
            } => json!({ "override_flag": flag }),
            Self::Usage(_) | Self::CurrentDirectory(_) | Self::Refused { .. } => json!({}),
        }
    }
}

#[derive(Serialize)]
struct SuccessEnvelope<'a> {
    ok: bool,
    data: &'a Value,
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
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
    r#type: &'static str,
    message: &'a str,
}

fn main() -> ExitCode {
    let arguments = env::args_os().collect::<Vec<_>>();
    let requested_json = arguments
        .iter()
        .skip(1)
        .any(|argument| argument == "--json");
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
            let code = error.exit_code();
            if requested_json {
                emit_error("usage_error", &error.to_string(), &json!({}));
            } else {
                let _ = error.print();
            }
            return ExitCode::from(code as u8);
        }
    };

    let json = cli.json;
    match run(cli) {
        Ok(result) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&SuccessEnvelope {
                        ok: true,
                        data: &result.data,
                    })
                    .expect("JSON values always serialize")
                );
            } else if !result.human.is_empty() {
                println!("{}", result.human);
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            let code = error.exit_code();
            if json {
                emit_error(error.kind(), &error.message(), &error.details());
            } else {
                eprintln!("{}", error.message());
            }
            ExitCode::from(code)
        }
    }
}

fn run(cli: Cli) -> Result<CommandResult, CliError> {
    let Cli {
        json,
        project,
        data_dir,
        command,
    } = cli;
    match command {
        Command::Project { command } => run_project(command, project, data_dir.as_deref(), json),
        Command::Worktree { command } => {
            if project.is_some() {
                return Err(CliError::Usage(
                    "--project is not used by worktree commands; pass the required project ID"
                        .to_owned(),
                ));
            }
            run_worktree(command, data_dir.as_deref())
        }
    }
}

fn run_project(
    command: ProjectCommand,
    selector: Option<String>,
    data_dir: Option<&Path>,
    json_output: bool,
) -> Result<CommandResult, CliError> {
    let mut service = load_service(data_dir)?;
    match command {
        ProjectCommand::List => {
            reject_selector(&selector, "project list")?;
            let projects = service.list()?;
            let human = projects
                .iter()
                .map(project_line)
                .collect::<Vec<_>>()
                .join("\n");
            Ok(CommandResult::new(
                human,
                json!({
                    "projects": projects.iter().map(project_value).collect::<Vec<_>>()
                }),
            ))
        }
        ProjectCommand::Show => {
            let project = resolve_project(&service, selector.as_deref())?;
            Ok(CommandResult::new(
                project_line(&project),
                json!({ "project": project_value(&project) }),
            ))
        }
        ProjectCommand::Import { path } => {
            reject_selector(&selector, "project import")?;
            let project = service.import_local(path)?;
            Ok(CommandResult::new(
                format!("imported {}", project_line(&project)),
                json!({ "project": project_value(&project) }),
            ))
        }
        ProjectCommand::Clone { remote } => {
            reject_selector(&selector, "project clone")?;
            let project =
                service.import_repository(&remote, &Cancellation::default(), |message| {
                    emit_progress(json_output, &message)
                })?;
            Ok(CommandResult::new(
                format!("cloned {}", project_line(&project)),
                json!({ "project": project_value(&project) }),
            ))
        }
        ProjectCommand::Forget => {
            let project = resolve_project(&service, selector.as_deref())?;
            if matches!(project.source, ProjectSource::ManagedRepository { .. }) {
                return Err(CliError::Refused {
                    kind: "managed_project_requires_delete",
                    message: format!(
                        "project {} is a managed clone; use 'project delete --yes' so its checkout is not orphaned",
                        project.id
                    ),
                    override_flag: None,
                });
            }
            let removed = service.remove(project.id)?;
            Ok(CommandResult::new(
                format!("forgot {}", project_line(&removed)),
                json!({ "project": project_value(&removed) }),
            ))
        }
        ProjectCommand::Delete { yes } => {
            let project = resolve_project(&service, selector.as_deref())?;
            if !yes {
                return Err(CliError::Refused {
                    kind: "confirmation_required",
                    message: format!(
                        "refusing to delete project {} at '{}'; pass --yes to confirm",
                        project.id,
                        project.root.display()
                    ),
                    override_flag: Some("--yes"),
                });
            }
            if matches!(project.source, ProjectSource::Local) {
                return Err(CliError::Refused {
                    kind: "local_project_requires_forget",
                    message: format!(
                        "project {} is a local directory; use 'project forget' to preserve its files",
                        project.id
                    ),
                    override_flag: None,
                });
            }
            let removed = service.remove_managed(project.id)?;
            Ok(CommandResult::new(
                format!("deleted {}", project_line(&removed)),
                json!({ "project": project_value(&removed) }),
            ))
        }
    }
}

fn run_worktree(
    command: WorktreeCommand,
    data_dir: Option<&Path>,
) -> Result<CommandResult, CliError> {
    let cancellation = Cancellation::default();
    let mut service = load_service(data_dir)?;
    match command {
        WorktreeCommand::List { parent_id } => {
            let worktrees = service.worktrees(parent_id, &cancellation)?;
            let human = worktrees
                .iter()
                .map(worktree_line)
                .collect::<Vec<_>>()
                .join("\n");
            Ok(CommandResult::new(
                human,
                json!({
                    "worktrees": worktrees.iter().map(worktree_value).collect::<Vec<_>>()
                }),
            ))
        }
        WorktreeCommand::Create(arguments) => {
            let base = if let Some(name) = arguments.new {
                WorktreeBase::NewBranch {
                    name,
                    start_point: arguments.start,
                }
            } else if let Some(name) = arguments.existing {
                WorktreeBase::ExistingBranch { name }
            } else {
                WorktreeBase::Detached {
                    commit: arguments
                        .detached
                        .expect("clap requires exactly one worktree base"),
                }
            };
            let project = service.create_worktree(arguments.parent_id, &base, &cancellation)?;
            Ok(CommandResult::new(
                format!(
                    "created {}\t{}\t{}",
                    project.id,
                    project.display_name,
                    project.root.display()
                ),
                json!({ "project": project_value(&project) }),
            ))
        }
        WorktreeCommand::Remove { worktree_id, force } => {
            let project = service.remove_worktree(worktree_id, force, &cancellation)?;
            Ok(CommandResult::new(
                format!("removed {}", project.display_name),
                json!({ "project": project_value(&project) }),
            ))
        }
        WorktreeCommand::Reconcile { parent_id } => {
            let removed = service.reconcile_worktrees(parent_id, &cancellation)?;
            Ok(CommandResult::new(
                format!("reconciled {} stale worktree entries", removed.len()),
                json!({
                    "removed": removed.iter().map(project_value).collect::<Vec<_>>()
                }),
            ))
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

fn reject_selector(selector: &Option<String>, command: &str) -> Result<(), CliError> {
    if selector.is_some() {
        Err(CliError::Usage(format!(
            "--project cannot be used with '{command}'"
        )))
    } else {
        Ok(())
    }
}

fn project_line(project: &Project) -> String {
    let availability = if project.available {
        "available"
    } else {
        "missing"
    };
    format!(
        "{}\t{}\t{}\t{}\t{availability}",
        project.id,
        project.display_name,
        project.root.display(),
        project_source_name(&project.source)
    )
}

fn project_value(project: &Project) -> Value {
    let mut value = Map::new();
    value.insert("id".to_owned(), json!(project.id.to_string()));
    value.insert("display_name".to_owned(), json!(project.display_name));
    value.insert(
        "root".to_owned(),
        json!(project.root.to_string_lossy().into_owned()),
    );
    value.insert(
        "source".to_owned(),
        json!(project_source_name(&project.source)),
    );
    match &project.source {
        ProjectSource::Local => {}
        ProjectSource::ManagedRepository { remote } => {
            value.insert("remote".to_owned(), json!(remote));
        }
        ProjectSource::Worktree {
            parent,
            worktree_branch,
        } => {
            value.insert("parent".to_owned(), json!(parent.to_string()));
            if let Some(branch) = worktree_branch {
                value.insert("worktree_branch".to_owned(), json!(branch));
            }
        }
    }
    value.insert("available".to_owned(), json!(project.available));
    value.insert(
        "last_opened".to_owned(),
        serde_json::to_value(project.last_opened).expect("timestamps serialize"),
    );
    value.insert(
        "git".to_owned(),
        serde_json::to_value(&project.git).expect("Git status serializes"),
    );
    Value::Object(value)
}

fn project_source_name(source: &ProjectSource) -> &'static str {
    match source {
        ProjectSource::Local => "local",
        ProjectSource::ManagedRepository { .. } => "managed_repository",
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

fn emit_progress(json_output: bool, message: &str) {
    if json_output {
        let mut stderr = io::stderr().lock();
        let _ = serde_json::to_writer(
            &mut stderr,
            &ProgressEnvelope {
                r#type: "progress",
                message,
            },
        );
        let _ = writeln!(stderr);
    } else {
        eprintln!("{message}");
    }
}

fn emit_error(kind: &str, message: &str, details: &Value) {
    println!(
        "{}",
        serde_json::to_string(&ErrorEnvelope {
            ok: false,
            error: ErrorBody {
                kind,
                message,
                details,
            },
        })
        .expect("JSON values always serialize")
    );
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
        GitError::NoSuchBranch { .. } | GitError::NoRemote { .. } => EXIT_NOT_FOUND,
        GitError::RepositoryBusy { .. }
        | GitError::BranchAlreadyExists { .. }
        | GitError::BranchCheckedOutInWorktree { .. }
        | GitError::WorktreeLocked { .. }
        | GitError::NonFastForward { .. }
        | GitError::OperationInProgress { .. }
        | GitError::Interrupted { .. } => EXIT_CONFLICT,
        GitError::PathOutsideRepository { .. }
        | GitError::EmptyCommitMessage
        | GitError::NothingStaged
        | GitError::AmendUnbornBranch
        | GitError::CurrentBranchDeletion { .. }
        | GitError::DefaultBranchDeletion { .. }
        | GitError::UnmergedBranchDeletion { .. }
        | GitError::LocalUpstreamUnsupported { .. }
        | GitError::DefaultBranchPush { .. }
        | GitError::DefaultBranchUnknown { .. }
        | GitError::DetachedHead { .. } => EXIT_REFUSED,
        _ => EXIT_OPERATION_FAILED,
    }
}

fn project_error_details(error: &ProjectError) -> Value {
    match error {
        ProjectError::AmbiguousProjectSelector { candidates, .. } => json!({
            "candidates": candidates.iter().map(project_value).collect::<Vec<_>>()
        }),
        ProjectError::DirtyWorktreeRemoval { .. } => json!({ "override_flag": "--force" }),
        ProjectError::Git(error) => git_error_details(error),
        _ => json!({}),
    }
}

fn git_error_details(error: &GitError) -> Value {
    match error {
        GitError::NothingStaged => json!({ "override_flag": "--allow-empty" }),
        GitError::UnmergedBranchDeletion { .. } => json!({ "override_flag": "--force" }),
        GitError::DefaultBranchPush { .. } | GitError::DefaultBranchUnknown { .. } => {
            json!({ "override_flag": "--allow-default-branch" })
        }
        _ => json!({}),
    }
}

#[cfg(test)]
mod tests {
    use super::{CliError, EXIT_OPERATION_FAILED, EXIT_REFUSED};
    use harkness_core::ProjectError;

    #[test]
    fn guardrail_and_operation_failures_have_distinct_exit_codes() {
        let refusal = CliError::Refused {
            kind: "confirmation_required",
            message: "confirmation required".to_owned(),
            override_flag: Some("--yes"),
        };
        assert_eq!(refusal.exit_code(), EXIT_REFUSED);
        assert_eq!(refusal.details()["override_flag"], "--yes");

        let failure = CliError::Project(ProjectError::CloneFailed {
            stderr: "network failed".to_owned(),
        });
        assert_eq!(failure.exit_code(), EXIT_OPERATION_FAILED);
    }
}
