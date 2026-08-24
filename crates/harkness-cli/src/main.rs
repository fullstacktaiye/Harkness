use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    fs,
    io::{self, Read, Write},
    num::NonZeroU32,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD as URL_BASE64},
};
use clap::{
    ArgGroup, Args, Parser, Subcommand, ValueEnum,
    error::{Error as ClapError, ErrorKind},
};
use harkness_core::{
    CheckConfiguration, EditorConfiguration, EditorError, EditorLaunchContext, EditorPosition,
    EditorPreset, Project, ProjectError, ProjectSelector, ProjectService, ProjectSource, Worktree,
};
use harkness_git::{
    Branch, BranchCheckout, BranchKind, BranchListOptions, Cancellation, ChangeProvenance,
    CommitAttribution, CommitInfo, CommitOptions, CommitOutcome, CommitSignature,
    CreateBranchOptions, DEFAULT_DIFF_CONTEXT_LINES, DEFAULT_MAX_DIFF_FILE_SIZE,
    DEFAULT_MAX_DIFF_FILES, DEFAULT_MAX_DIFF_TOTAL_BYTES, DEFAULT_MAX_PROVENANCE_COMMITS,
    DetailedStatus, DiffLine, DiffLineKind, DiffOmission, DiffOptions, DiffTarget, FetchOptions,
    FetchOutcome, FileChange, FileContextOmission, FileContextRange, FileContextRequest,
    FileContextResponse, FileDiff, FileProvenance, FileSide, GitError, GitService, GitStatus,
    HeadState, Hunk, HunkSelection, IntraLineDegradation, LineSelection, LogCursor, LogOptions,
    LogRange, PendingOperation, Producer, ProvenanceGap, ProvenanceOptions, ProvenanceRange,
    ProvenanceTruncation, PullOptions, PullOutcome, PullStrategy, PushOptions, PushOutcome,
    RefUpdate, StageOutcome, StagePathResult, StatusEntry, StatusRefreshOutcome,
    TrackedRestoreSource, UpstreamStatus, Whitespace, WhitespaceMode, WorktreeBase,
};
use harkness_runtime::{
    approval::DecidedVia,
    canonical_json,
    check::{CheckOutcome, CheckSummary, check_coordinator, project_checks, run_configured_check},
    coordinator::RuntimeError,
    observe,
    policy::EXTERNAL_POLICY_DENIAL_KINDS,
    store::{Store, StoreError},
    tool::InvocationError,
    trust::{TrustState, WorkspaceTrust},
};
use serde::Serialize;
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

mod agent_commands;
mod approval_commands;
mod run_commands;
mod runtime_support;
mod tool_commands;

use runtime_support::{
    RUNTIME_KIND_EXIT_CODES, TOOL_KIND_EXIT_CODES, runtime_details, runtime_exit_code,
    store_details, store_exit_code,
};

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
    "check_operation_failed",
    "check_failed",
    "check_cancelled",
    "confirmation_required",
    "managed_project_requires_delete",
    "local_project_requires_forget",
    "worktree_requires_remove",
    // Runtime outcomes the CLI itself concludes. Each one is a fact about a
    // recorded run or call rather than a failure the runtime returned, so none
    // of them belongs in the coordinator's own namespace.
    "approval_required_noninteractive",
    "policy_denied",
    "approval_denied",
    "tool_call_denied",
    "tool_call_failed",
    "tool_call_cancelled",
    "tool_call_interrupted",
    "run_failed",
    "run_cancelled",
    "run_interrupted",
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

    /// Mirror the diagnostic log to standard error, one JSON object per line.
    ///
    /// The same lines the log file receives, in the same rendering, so what this
    /// shows is exactly what was recorded. `HARKNESS_LOG` chooses the level and
    /// `HARKNESS_LOG_STDERR` turns the mirror on without a flag.
    #[arg(long, global = true)]
    verbose: bool,

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
    /// Inspect and change Git repositories through the dedicated Git service.
    Git {
        #[command(subcommand)]
        command: Box<GitCommand>,
    },
    /// Manage linked Git worktree workspaces.
    Worktree {
        #[command(subcommand)]
        command: WorktreeCommand,
    },
    /// Run and inspect state-bound project checks.
    Check {
        #[command(subcommand)]
        command: CheckCommand,
    },
    /// Configure and launch an external editor without invoking a shell.
    Editor {
        #[command(subcommand)]
        command: EditorCommand,
    },
    /// Inspect, cancel, and re-attempt recorded runs.
    Run {
        #[command(subcommand)]
        command: run_commands::RunCommand,
    },
    /// List and answer approval requests.
    Approvals {
        #[command(subcommand)]
        command: approval_commands::ApprovalsCommand,
    },
    /// Publish the typed tool contract and invoke one tool.
    Tool {
        #[command(subcommand)]
        command: tool_commands::ToolCommand,
    },
    /// Replay a deterministic agent scenario through the runtime.
    Agent {
        #[command(subcommand)]
        command: agent_commands::AgentCommand,
    },
    /// Describe the versioned machine-readable CLI contract.
    Contract,
}

#[derive(Debug, Subcommand)]
enum CheckCommand {
    /// List configured checks and their newest recorded results.
    List(ProjectSelection),
    /// Replace this project's explicit checks from a JSON document.
    Configure(CheckConfigureArguments),
    /// Discard explicit checks and restore the workspace defaults.
    Clear(CheckClearArguments),
    /// Run one configured check through the durable runtime.
    Run(CheckRunArguments),
}

#[derive(Debug, Args)]
struct CheckConfigureArguments {
    #[command(flatten)]
    selection: ProjectSelection,
    /// JSON array of check definitions, or `-` to read standard input.
    ///
    /// A document is used rather than flags because a check is argv, a working
    /// directory, an environment map, a parser and a timeout together, and a
    /// half-specified one is not a thing this catalog can hold.
    #[arg(long, value_name = "PATH")]
    from: PathBuf,
}

#[derive(Debug, Args)]
struct CheckClearArguments {
    #[command(flatten)]
    selection: ProjectSelection,
    /// Confirm discarding the project's explicit check configuration.
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct CheckRunArguments {
    #[command(flatten)]
    selection: ProjectSelection,
    /// Stable configured check identifier.
    #[arg(value_name = "CHECK_ID")]
    check_id: String,
    /// Confirm execution of the exact configured command.
    #[arg(long)]
    yes: bool,
    /// Explicitly trust this project identity and canonical root before running.
    #[arg(long, requires = "yes")]
    trust_workspace: bool,
}

#[derive(Debug, Subcommand)]
enum EditorCommand {
    /// Show the configured argv template and fallback behavior.
    Show,
    /// List the built-in convenience templates.
    Presets,
    /// Store a preset or custom argv template globally.
    Set(EditorSetArguments),
    /// Remove the configured template and restore automatic fallback.
    Clear,
    /// Open a repository-relative file at a one-based source position.
    Open(EditorOpenArguments),
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum EditorPresetArgument {
    Kate,
    Code,
    Zed,
}

impl From<EditorPresetArgument> for EditorPreset {
    fn from(value: EditorPresetArgument) -> Self {
        match value {
            EditorPresetArgument::Kate => Self::Kate,
            EditorPresetArgument::Code => Self::VisualStudioCode,
            EditorPresetArgument::Zed => Self::Zed,
        }
    }
}

#[derive(Debug, Args)]
struct EditorSetArguments {
    /// Use a built-in Kate, VS Code, or Zed template.
    #[arg(
        long,
        value_enum,
        conflicts_with = "command",
        required_unless_present = "command"
    )]
    preset: Option<EditorPresetArgument>,
    /// Custom argv template. Use `--` before the executable; `{file}` is required.
    #[arg(
        value_name = "COMMAND",
        num_args = 1..,
        trailing_var_arg = true,
        required_unless_present = "preset"
    )]
    command: Vec<String>,
}

#[derive(Debug, Args)]
struct EditorOpenArguments {
    #[command(flatten)]
    selection: ProjectSelection,
    /// Repository-relative file path.
    #[arg(value_name = "PATH", allow_hyphen_values = true)]
    path: PathBuf,
    /// One-based line number.
    #[arg(long, default_value = "1")]
    line: NonZeroU32,
    /// One-based column number.
    #[arg(long, default_value = "1")]
    column: NonZeroU32,
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
        /// Discard uncommitted changes. This never bypasses a worktree lock.
        #[arg(long)]
        force: bool,
    },
    /// Relocate a Harkness-managed linked worktree.
    Move {
        #[command(flatten)]
        selection: ProjectSelection,
        /// New absolute path. The destination itself must not exist.
        #[arg(value_name = "DESTINATION")]
        destination: PathBuf,
    },
    /// Protect a worktree from move, removal, and pruning; there is no force bypass.
    Lock {
        #[command(flatten)]
        selection: ProjectSelection,
        /// Why the worktree is protected. Git trims and stores this text.
        #[arg(long, value_name = "TEXT")]
        reason: String,
        /// Replace an existing lock reason instead of refusing.
        #[arg(long)]
        replace: bool,
    },
    /// Clear the lock on a Harkness-managed linked worktree.
    Unlock(ProjectSelection),
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
    /// Walk bounded commit history using Git-style revision ranges.
    Log(LogArguments),
    /// Inspect structured, byte-preserving changes; raw patch text is never emitted.
    Diff(DiffArguments),
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
    Unstage(UnstageArguments),
    /// Discard working-tree content through an explicit, confirmed boundary.
    Discard(DiscardArguments),
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
    #[arg(
        required_unless_present_any = ["all", "hunk", "hunk_selection", "line_selection"],
        value_name = "PATH"
    )]
    paths: Vec<PathBuf>,
    /// Stage every change, including deletions.
    #[arg(
        long,
        conflicts_with_all = ["paths", "hunk", "hunk_selection", "line_selection"]
    )]
    all: bool,
    #[command(flatten)]
    hunk: HunkArguments,
}

#[derive(Debug, Args)]
struct UnstageArguments {
    #[command(flatten)]
    selection: ProjectSelection,
    /// Repository-relative or absolute paths to unstage.
    #[arg(
        required_unless_present_any = ["hunk", "hunk_selection", "line_selection"],
        value_name = "PATH"
    )]
    paths: Vec<PathBuf>,
    #[command(flatten)]
    hunk: HunkArguments,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum DiscardSourceArgument {
    /// Restore the working tree from the index and preserve staged changes.
    Index,
    /// Restore both the index and working tree from HEAD.
    Head,
}

impl From<DiscardSourceArgument> for TrackedRestoreSource {
    fn from(value: DiscardSourceArgument) -> Self {
        match value {
            DiscardSourceArgument::Index => Self::Index,
            DiscardSourceArgument::Head => Self::Head,
        }
    }
}

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("discard_kind")
        .required(true)
        .multiple(false)
        .args(["from", "delete_untracked"])
))]
struct DiscardArguments {
    #[command(flatten)]
    selection: ProjectSelection,
    /// Repository-relative or absolute paths to discard.
    #[arg(
        required_unless_present_any = ["hunk", "hunk_selection", "line_selection"],
        value_name = "PATH"
    )]
    paths: Vec<PathBuf>,
    /// Restore tracked content from this explicit Git boundary.
    #[arg(long, value_enum, value_name = "BOUNDARY")]
    from: Option<DiscardSourceArgument>,
    /// Permanently delete explicitly named untracked files.
    #[arg(
        long,
        conflicts_with_all = ["hunk", "hunk_selection", "line_selection"]
    )]
    delete_untracked: bool,
    /// Confirm the destructive operation after reviewing its description.
    #[arg(long)]
    yes: bool,
    #[command(flatten)]
    hunk: HunkArguments,
}

/// The largest context setting the CLI will ask libgit2 to render.
///
/// Context multiplies every hunk in every file, and each rendered line costs
/// far more as a structured record than as bytes, so an unbounded setting turns
/// a modest repository into a response nobody can hold. A hundred lines either
/// side already covers "show me the whole neighbourhood"; wanting the rest of
/// the file is a request to read the file, not to widen a diff.
const MAX_DIFF_CONTEXT_LINES: u32 = 100;

/// The CLI's finite upper bound for one history page.
const DEFAULT_LOG_LIMIT: usize = 50;
const MAX_LOG_LIMIT: usize = 1_000;

#[derive(Debug, Args)]
#[command(
    after_help = "Range grammar:\n  REVISION        commits reachable from one revision (default: HEAD)\n  OLD..NEW        commits reachable from NEW but not OLD\n  BASE...BRANCH   commits on BRANCH after its merge-base with BASE\n\nExamples:\n  harkness --json git log HEAD --limit 25\n  harkness --json git log main..feature\n  harkness --json git log main...feature --cursor <token>"
)]
struct LogArguments {
    #[command(flatten)]
    selection: ProjectSelection,
    /// Revision or range to walk; see RANGE GRAMMAR below.
    #[arg(value_name = "RANGE", default_value = "HEAD")]
    range: String,
    /// Maximum number of commits in this page.
    #[arg(
        long,
        value_name = "COUNT",
        default_value_t = DEFAULT_LOG_LIMIT,
        value_parser = parse_log_limit,
    )]
    limit: usize,
    /// Opaque continuation token returned as `next_cursor` by an earlier page.
    #[arg(long, value_name = "TOKEN")]
    cursor: Option<String>,
}

impl LogArguments {
    fn options(&self) -> Result<LogOptions, CliError> {
        let range = parse_log_range(&self.range)?;
        let cursor = self.cursor.as_deref().map(decode_log_cursor).transpose()?;
        let options = match range {
            LogRange::Revision { revision } => LogOptions::new(revision, self.limit),
            LogRange::Excluding {
                reachable_from,
                not_from,
            } => LogOptions::excluding(reachable_from, not_from, self.limit),
            LogRange::BranchAgainstBase {
                branch,
                base_branch,
            } => LogOptions::branch_against_base(branch, base_branch, self.limit),
            _ => {
                return Err(CliError::Usage("unsupported log range kind".to_owned()));
            }
        };
        Ok(match cursor {
            Some(cursor) => options.with_cursor(cursor),
            None => options,
        })
    }
}

/// The `--whitespace` spellings, kept in step with [`WhitespaceMode`].
///
/// Clap renders these kebab-cased, which is the house style for a flag value.
/// Each also accepts the snake-cased [`WhitespaceMode::name`] spelling as an
/// alias, because `git stage --hunk` asks a caller to copy `whitespace.mode`
/// straight off a diff record: a flag that rejected the exact bytes the
/// envelope published would be a translation step, and a translation step is
/// somewhere to get it wrong.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
enum WhitespaceArgument {
    /// Compare bytes; every whitespace difference is a change.
    #[default]
    Exact,
    /// Ignore whitespace at end of line, which is what a CRLF-to-LF pass changes.
    #[value(alias = "ignore_eol")]
    IgnoreEol,
    /// Ignore changes in the amount of whitespace, such as a re-indent.
    #[value(alias = "ignore_change")]
    IgnoreChange,
    /// Ignore whitespace everywhere, including whitespace added to a line.
    #[value(alias = "ignore_all")]
    IgnoreAll,
}

impl From<WhitespaceArgument> for WhitespaceMode {
    fn from(value: WhitespaceArgument) -> Self {
        match value {
            WhitespaceArgument::Exact => Self::Exact,
            WhitespaceArgument::IgnoreEol => Self::IgnoreEol,
            WhitespaceArgument::IgnoreChange => Self::IgnoreChange,
            WhitespaceArgument::IgnoreAll => Self::IgnoreAll,
        }
    }
}

impl std::fmt::Display for WhitespaceArgument {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Exact => "exact",
            Self::IgnoreEol => "ignore-eol",
            Self::IgnoreChange => "ignore-change",
            Self::IgnoreAll => "ignore-all",
        })
    }
}

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("diff_target")
        .multiple(false)
        .args(["staged", "unstaged", "commit", "revisions", "worktree", "branch"])
))]
struct DiffArguments {
    #[command(flatten)]
    selection: ProjectSelection,
    /// Return only changes between HEAD and the index.
    #[arg(long)]
    staged: bool,
    /// Return only changes between the index and working tree.
    #[arg(long)]
    unstaged: bool,
    /// Compare a commit with its first parent, or with --parent.
    #[arg(long, value_name = "REVISION")]
    commit: Option<String>,
    /// Parent revision to compare with --commit; it must be a recorded parent.
    #[arg(long, value_name = "REVISION", requires = "commit")]
    parent: Option<String>,
    /// Compare two revisions, written OLD..NEW.
    #[arg(long, value_name = "OLD..NEW")]
    revisions: Option<String>,
    /// Compare this revision with the index and working tree combined.
    #[arg(long, value_name = "REVISION")]
    worktree: Option<String>,
    /// Compare a branch with its merge-base, written BASE...BRANCH.
    #[arg(long, value_name = "BASE...BRANCH")]
    branch: Option<String>,
    /// Number of unchanged lines surrounding each hunk.
    #[arg(
        long,
        value_name = "LINES",
        default_value_t = DEFAULT_DIFF_CONTEXT_LINES,
        value_parser = clap::value_parser!(u32).range(0..=i64::from(MAX_DIFF_CONTEXT_LINES)),
    )]
    context_lines: u32,
    /// How whitespace-only differences are treated. Anything but `exact`
    /// produces a view-only diff: its hunks omit lines that differ on disk, so
    /// `git stage --hunk-selection` refuses a selection taken from it.
    #[arg(long, value_name = "MODE", default_value_t = WhitespaceArgument::Exact, value_enum)]
    whitespace: WhitespaceArgument,
    /// Leave lines that are blank on both sides out of the comparison. This is
    /// view-only for the same reason `--whitespace` is.
    #[arg(long)]
    ignore_blank_lines: bool,
    /// Retrieve each hunk with this many additional lines before and after,
    /// addressed by the diff's recorded blob IDs rather than by recomputing a
    /// wider diff.
    #[arg(long, value_name = "LINES", conflicts_with = "full_file_context")]
    expand_context: Option<u32>,
    /// Retrieve complete old and new file content alongside each diff record.
    #[arg(long, visible_alias = "full-file", conflicts_with = "expand_context")]
    full_file_context: bool,
    /// Expand records from a prior `git diff` JSON document instead of
    /// recomputing the diff. Accepts a file path or "-" for standard input.
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with_all = [
            "staged",
            "unstaged",
            "commit",
            "parent",
            "revisions",
            "worktree",
            "branch",
            "intra_line",
            "context_lines",
            "whitespace",
            "ignore_blank_lines",
            "max_files",
            "paths",
            "provenance",
            "provenance_max_commits",
            "checks"
        ]
    )]
    context_from: Option<PathBuf>,
    /// Add deterministic paired-line byte ranges and named degradations.
    #[arg(long, visible_alias = "intra-line-ranges")]
    intra_line: bool,
    /// Attribute each file to the commits in the diff's own range, and name
    /// the identities those commits record. It is off by default because it
    /// walks the range, and it is advisory: nothing may act on what it says.
    #[arg(long)]
    provenance: bool,
    /// The most commits one attribution walks before reporting a named
    /// truncation.
    #[arg(
        long,
        value_name = "COUNT",
        default_value_t = DEFAULT_MAX_PROVENANCE_COMMITS,
        requires = "provenance",
    )]
    provenance_max_commits: usize,
    /// Include recorded project checks and their current staleness beside the diff.
    #[arg(long)]
    checks: bool,
    /// Largest old or new file whose content is included.
    #[arg(long, value_name = "BYTES", default_value_t = DEFAULT_MAX_DIFF_FILE_SIZE)]
    max_file_size: u64,
    /// Combined budget for hunk content across every file in the response.
    #[arg(long, value_name = "BYTES", default_value_t = DEFAULT_MAX_DIFF_TOTAL_BYTES)]
    max_total_bytes: u64,
    /// Number of files that carry content before the rest are named only.
    #[arg(long, value_name = "COUNT", default_value_t = DEFAULT_MAX_DIFF_FILES)]
    max_files: usize,
    /// Restrict the diff to these repository-relative or absolute paths. A
    /// directory selects everything beneath it. Put a path that begins with a
    /// hyphen after a `--` separator, as Git itself requires.
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,
}

impl DiffArguments {
    /// The sides of the index to inspect, in response order.
    ///
    /// Both sides are read from one snapshot, so the absence of a narrowing
    /// flag is expressed as two targets rather than as two separate diffs.
    fn targets(&self) -> Result<Vec<DiffTarget>, CliError> {
        if let Some(revision) = &self.commit {
            return Ok(vec![DiffTarget::Commit {
                revision: revision.clone(),
                parent: self.parent.clone(),
            }]);
        }
        if let Some(range) = &self.revisions {
            let (old_revision, new_revision) = parse_revision_pair(range)?;
            return Ok(vec![DiffTarget::Revisions {
                old_revision,
                new_revision,
            }]);
        }
        if let Some(revision) = &self.worktree {
            return Ok(vec![DiffTarget::RevisionAgainstWorktree {
                revision: revision.clone(),
            }]);
        }
        if let Some(range) = &self.branch {
            let (base_branch, branch) = parse_branch_range(range)?;
            return Ok(vec![DiffTarget::BranchAgainstBase {
                branch,
                base_branch,
            }]);
        }
        Ok(match (self.staged, self.unstaged) {
            (true, false) => vec![DiffTarget::Staged],
            (false, true) => vec![DiffTarget::Unstaged],
            _ => vec![DiffTarget::Staged, DiffTarget::Unstaged],
        })
    }

    const fn context_mode(&self) -> DiffContextMode {
        match (self.expand_context, self.full_file_context) {
            (Some(lines), false) => DiffContextMode::Expanded(lines),
            (None, true) => DiffContextMode::FullFile,
            _ => DiffContextMode::None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DiffContextMode {
    None,
    Expanded(u32),
    FullFile,
}

/// The high mode bits distinguish blob-backed entries from trees and gitlinks.
/// Missing sides use mode zero and still have a valid empty context response.
const GIT_MODE_TYPE_MASK: u32 = 0o170_000;
const GIT_MODE_REGULAR_FILE: u32 = 0o100_000;
const GIT_MODE_SYMBOLIC_LINK: u32 = 0o120_000;

const fn mode_has_file_context(mode: u32) -> bool {
    mode == 0
        || matches!(
            mode & GIT_MODE_TYPE_MASK,
            GIT_MODE_REGULAR_FILE | GIT_MODE_SYMBOLIC_LINK
        )
}

fn parse_log_range(range: &str) -> Result<LogRange, CliError> {
    if range.contains("...") {
        let (base_branch, branch) = parse_branch_range(range)?;
        return Ok(LogRange::BranchAgainstBase {
            branch,
            base_branch,
        });
    }
    if range.contains("..") {
        let (not_from, reachable_from) = parse_revision_pair(range)?;
        return Ok(LogRange::Excluding {
            reachable_from,
            not_from,
        });
    }
    if range.is_empty() {
        return Err(CliError::Usage(
            "log range must be REVISION, OLD..NEW, or BASE...BRANCH".to_owned(),
        ));
    }
    Ok(LogRange::Revision {
        revision: range.to_owned(),
    })
}

fn parse_log_limit(value: &str) -> Result<usize, String> {
    let limit = value
        .parse::<usize>()
        .map_err(|_| format!("'{value}' is not a positive history-page size"))?;
    if !(1..=MAX_LOG_LIMIT).contains(&limit) {
        return Err(format!(
            "history-page size must be between 1 and {MAX_LOG_LIMIT}"
        ));
    }
    Ok(limit)
}

fn parse_revision_pair(range: &str) -> Result<(String, String), CliError> {
    parse_range_pair(range, "..", "OLD..NEW")
}

fn parse_branch_range(range: &str) -> Result<(String, String), CliError> {
    parse_range_pair(range, "...", "BASE...BRANCH")
}

fn parse_range_pair(
    range: &str,
    separator: &str,
    expected: &'static str,
) -> Result<(String, String), CliError> {
    let Some((left, right)) = range.split_once(separator) else {
        return Err(CliError::Usage(format!(
            "range '{range}' must use the {expected} form"
        )));
    };
    if left.is_empty()
        || right.is_empty()
        || left.ends_with('.')
        || right.starts_with('.')
        || left.contains("..")
        || right.contains("..")
    {
        return Err(CliError::Usage(format!(
            "range '{range}' must contain exactly two non-empty revisions in the {expected} form"
        )));
    }
    Ok((left.to_owned(), right.to_owned()))
}

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("hunk_path")
        .multiple(true)
        .args(["old_path", "new_path", "old_path_base64", "new_path_base64"])
        .requires("hunk")
))]
struct HunkArguments {
    /// Stage or unstage one named hunk; a stale identity is refused before any
    /// mutation. Use --hunk-selection for more than one hunk at a time.
    #[arg(
        long,
        conflicts_with_all = ["paths", "hunk_selection", "line_selection"],
        requires_all = [
            "hunk_path",
            "old_blob_id",
            "new_blob_id",
            "context_lines",
            "whitespace",
            "ignore_blank_lines",
            "old_start",
            "old_lines",
            "new_start",
            "new_lines"
        ]
    )]
    hunk: bool,
    /// Apply every hunk named by a JSON selection document, atomically. Accepts
    /// a file path, or "-" to read standard input. The document is either
    /// {"files": [...]} holding whole `git diff` file records whose "hunks"
    /// arrays have been narrowed to the wanted hunks, or {"selections": [...]}
    /// holding flat records. One invocation is one atomic index write, so the
    /// coordinate shift that invalidates a second single-hunk call cannot occur.
    ///
    /// A flat record carries the file's "old_path", "new_path", "old_blob_id",
    /// "new_blob_id", "context_lines" and "whitespace" alongside the hunk's four
    /// coordinates. Copy "whitespace" across with the rest: a record built from
    /// a whitespace-insensitive diff names coordinates that do not describe the
    /// file, and omitting the field claims they came from an exact one.
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with_all = ["paths", "line_selection"]
    )]
    hunk_selection: Option<PathBuf>,
    /// Apply changed lines named by a JSON selection document, atomically.
    /// The shape matches --hunk-selection, but each retained hunk's "lines"
    /// array is narrowed to the additions and deletions to apply. Context and
    /// EOF-marker records may remain and are ignored.
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with_all = ["paths", "hunk", "hunk_selection"]
    )]
    line_selection: Option<PathBuf>,
    /// Old-side path from the diff file record; omit for an addition.
    #[arg(
        long,
        value_name = "PATH",
        requires = "hunk",
        allow_hyphen_values = true
    )]
    old_path: Option<PathBuf>,
    /// New-side path from the diff file record; omit for a deletion.
    #[arg(
        long,
        value_name = "PATH",
        requires = "hunk",
        allow_hyphen_values = true
    )]
    new_path: Option<PathBuf>,
    /// Old-side path as the Base64 of its exact bytes, from `old_path_base64`.
    /// Required instead of --old-path when the diff marked the path lossy.
    #[arg(
        long,
        value_name = "BASE64",
        requires = "hunk",
        conflicts_with = "old_path"
    )]
    old_path_base64: Option<String>,
    /// New-side path as the Base64 of its exact bytes, from `new_path_base64`.
    /// Required instead of --new-path when the diff marked the path lossy.
    #[arg(
        long,
        value_name = "BASE64",
        requires = "hunk",
        conflicts_with = "new_path"
    )]
    new_path_base64: Option<String>,
    /// Old-side blob ID from the diff file record.
    #[arg(long, value_name = "OID", requires = "hunk")]
    old_blob_id: Option<String>,
    /// New-side blob ID from the diff file record.
    #[arg(long, value_name = "OID", requires = "hunk")]
    new_blob_id: Option<String>,
    /// Context-line count from the diff file record.
    #[arg(long, value_name = "LINES", requires = "hunk")]
    context_lines: Option<u32>,
    /// Whitespace mode from the diff file record's "whitespace.mode", which for
    /// an ordinary diff is `exact`.
    ///
    /// Required rather than defaulted, and for the same reason the coordinates
    /// themselves are: it describes where these numbers came from. Hunk
    /// revalidation matches on blob IDs and coordinates, and a
    /// whitespace-insensitive hunk can carry exactly the coordinates of an
    /// exact hunk that also contains the whitespace change the relaxed view was
    /// hiding. Naming the mode is what turns that into a refusal instead of an
    /// apply nobody asked for.
    #[arg(long, value_name = "MODE", requires = "hunk", value_enum)]
    whitespace: Option<WhitespaceArgument>,
    /// Blank-line handling from the diff file record's
    /// "whitespace.ignore_blank_lines", written `true` or `false`.
    ///
    /// A value rather than the bare switch `git diff` takes, and required for
    /// the same reason `--whitespace` is. On `git diff` the flag is a request,
    /// and its absence means "do not". Here it states where the coordinates
    /// came from, and its absence would mean "the caller did not say" — which a
    /// bare switch cannot distinguish from `false`. Suppressing blank lines
    /// hides changed lines exactly as a relaxed mode does, so reading an
    /// unstated flag as `false` would restore the silent apply that requiring
    /// `--whitespace` closes.
    #[arg(long, value_name = "BOOL", requires = "hunk")]
    ignore_blank_lines: Option<bool>,
    /// Old-side start line from the selected hunk.
    #[arg(long, value_name = "LINE", requires = "hunk")]
    old_start: Option<u32>,
    /// Old-side line count from the selected hunk.
    #[arg(long, value_name = "COUNT", requires = "hunk")]
    old_lines: Option<u32>,
    /// New-side start line from the selected hunk.
    #[arg(long, value_name = "LINE", requires = "hunk")]
    new_start: Option<u32>,
    /// New-side line count from the selected hunk.
    #[arg(long, value_name = "COUNT", requires = "hunk")]
    new_lines: Option<u32>,
}

enum GranularSelections {
    Hunks(Vec<HunkSelection>),
    Lines(Vec<LineSelection>),
}

impl HunkArguments {
    /// The batch this invocation names, or `None` when it is a path operation.
    ///
    /// A returned batch is never empty: an empty selection document is a usage
    /// error rather than a silent no-op that would look like a successful
    /// stage. Clap already enforces the flag form's requirements, so the checks
    /// here exist for the document form, whose contents it cannot see.
    fn into_selections(self, consumes: &str) -> Result<Option<GranularSelections>, CliError> {
        if let Some(source) = self.line_selection {
            let document = read_selection_document(&source)?;
            let selections = parse_line_selection_document(&document, consumes)?;
            if selections.is_empty() {
                return Err(CliError::Usage(
                    "the line-selection document names no changed lines".to_owned(),
                ));
            }
            return Ok(Some(GranularSelections::Lines(selections)));
        }
        if let Some(source) = self.hunk_selection {
            let document = read_selection_document(&source)?;
            let selections = parse_selection_document(&document, consumes)?;
            if selections.is_empty() {
                return Err(CliError::Usage(
                    "the selection document names no hunks".to_owned(),
                ));
            }
            return Ok(Some(GranularSelections::Hunks(selections)));
        }
        if !self.hunk {
            return Ok(None);
        }
        let missing =
            |field: &str| CliError::Usage(format!("--hunk requires --{}", field.replace('_', "-")));
        let old_path = flag_path(self.old_path, self.old_path_base64, "old-path-base64")?;
        let new_path = flag_path(self.new_path, self.new_path_base64, "new-path-base64")?;
        if old_path.is_none() && new_path.is_none() {
            return Err(CliError::Usage(
                "--hunk requires at least one of --old-path and --new-path".to_owned(),
            ));
        }
        Ok(Some(GranularSelections::Hunks(vec![
            HunkSelection::from_parts(
                old_path,
                new_path,
                self.old_blob_id.ok_or_else(|| missing("old_blob_id"))?,
                self.new_blob_id.ok_or_else(|| missing("new_blob_id"))?,
                self.context_lines.ok_or_else(|| missing("context_lines"))?,
                Whitespace {
                    mode: self.whitespace.ok_or_else(|| missing("whitespace"))?.into(),
                    ignore_blank_lines: self
                        .ignore_blank_lines
                        .ok_or_else(|| missing("ignore_blank_lines"))?,
                },
                (
                    self.old_start.ok_or_else(|| missing("old_start"))?,
                    self.old_lines.ok_or_else(|| missing("old_lines"))?,
                ),
                (
                    self.new_start.ok_or_else(|| missing("new_start"))?,
                    self.new_lines.ok_or_else(|| missing("new_lines"))?,
                ),
            ),
        ])))
    }
}

/// Resolves one side's path from its plain and Base64 spellings.
fn flag_path(
    plain: Option<PathBuf>,
    encoded: Option<String>,
    flag: &str,
) -> Result<Option<PathBuf>, CliError> {
    match encoded {
        Some(encoded) => decode_path(&encoded, flag).map(Some),
        None => Ok(plain),
    }
}

fn decode_path(encoded: &str, field: &str) -> Result<PathBuf, CliError> {
    let bytes = BASE64
        .decode(encoded)
        .map_err(|error| CliError::Usage(format!("{field} is not valid Base64: {error}")))?;
    path_from_bytes(bytes, field)
}

#[cfg(unix)]
fn path_from_bytes(bytes: Vec<u8>, _field: &str) -> Result<PathBuf, CliError> {
    use std::os::unix::ffi::OsStringExt;

    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

/// Where a path is UTF-16 rather than bytes, arbitrary bytes name no file.
///
/// Refusing is the only honest answer: Git cannot create such a path here
/// either, so decoding it lossily would hand back a name pointing somewhere
/// else, and a mutation would then be revalidated against the wrong file.
#[cfg(not(unix))]
fn path_from_bytes(bytes: Vec<u8>, field: &str) -> Result<PathBuf, CliError> {
    String::from_utf8(bytes).map(PathBuf::from).map_err(|_| {
        CliError::Usage(format!(
            "{field} decodes to bytes that are not a valid path on this platform"
        ))
    })
}

/// The exact bytes of a path, or `None` when this platform cannot supply them.
///
/// A Unix path is already bytes. A Windows path is UTF-16, and one holding an
/// unpaired surrogate has no faithful byte spelling, so the field is withheld
/// rather than filled with a lossy conversion. A caller then sees a lossy path
/// with no exact alternative and is refused, which is the truth, instead of
/// receiving an encoding that silently decodes to a different name.
#[cfg(unix)]
fn path_bytes(path: &Path) -> Option<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;

    Some(path.as_os_str().as_bytes().to_vec())
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Option<Vec<u8>> {
    path.to_str().map(|path| path.as_bytes().to_vec())
}

fn read_selection_document(source: &Path) -> Result<String, CliError> {
    if source == Path::new("-") {
        let mut document = String::new();
        return io::stdin()
            .read_to_string(&mut document)
            .map(|_| document)
            .map_err(|error| {
                CliError::Usage(format!("the selection document could not be read: {error}"))
            });
    }
    fs::read_to_string(source).map_err(|error| {
        CliError::Usage(format!(
            "the selection document '{}' could not be read: {error}",
            source.display()
        ))
    })
}

/// Reads a selection batch from either accepted document shape.
///
/// The `files` shape is `git diff` output with unwanted hunks removed, so the
/// round trip needs no reshaping by the caller. The `selections` shape is the
/// flat form for callers that assemble coordinates themselves.
///
/// `consumes` names the side of the index this command reads. A record from the
/// other side would fail revalidation on its blob IDs and be reported as stale,
/// which is true of the identities but useless as a diagnosis: nothing has gone
/// out of date, the wrong half of a combined diff was piped in.
fn parse_selection_document(
    document: &str,
    consumes: &str,
) -> Result<Vec<HunkSelection>, CliError> {
    let value: Value = serde_json::from_str(document)
        .map_err(|error| CliError::Usage(format!("the selection document is not JSON: {error}")))?;
    let value = value
        .get("data")
        .filter(|data| data.get("files").is_some())
        .unwrap_or(&value);
    if let Some(files) = value.get("files") {
        let files = array(files, "files")?;
        let mut selections = Vec::new();
        for (index, file) in files.iter().enumerate() {
            let at = format!("files[{index}]");
            check_target(file, consumes, &at)?;
            selections.extend(file_selections(file, &at)?);
        }
        return Ok(selections);
    }
    let selections = match value.get("selections") {
        Some(selections) => array(selections, "selections")?,
        None => array(
            value,
            "the document root; expected an object with \"files\" or \"selections\"",
        )?,
    };
    selections
        .iter()
        .enumerate()
        .map(|(index, selection)| {
            let at = format!("selections[{index}]");
            check_target(selection, consumes, &at)?;
            flat_selection(selection, &at)
        })
        .collect()
}

/// Reads the same two document shapes as hunk selection, with each selected
/// hunk retaining only the changed lines the caller wants to apply. Context and
/// EOF-marker records may remain in a diff projection and are ignored.
fn parse_line_selection_document(
    document: &str,
    consumes: &str,
) -> Result<Vec<LineSelection>, CliError> {
    let value: Value = serde_json::from_str(document).map_err(|error| {
        CliError::Usage(format!("the line-selection document is not JSON: {error}"))
    })?;
    let value = value
        .get("data")
        .filter(|data| data.get("files").is_some())
        .unwrap_or(&value);
    if let Some(files) = value.get("files") {
        let files = array(files, "files")?;
        let mut selections = Vec::new();
        for (index, file) in files.iter().enumerate() {
            let at = format!("files[{index}]");
            check_target(file, consumes, &at)?;
            selections.extend(file_line_selections(file, &at)?);
        }
        return Ok(selections);
    }
    let selections = match value.get("selections") {
        Some(selections) => array(selections, "selections")?,
        None => array(
            value,
            "the document root; expected an object with \"files\" or \"selections\"",
        )?,
    };
    selections
        .iter()
        .enumerate()
        .map(|(index, selection)| {
            let at = format!("selections[{index}]");
            check_target(selection, consumes, &at)?;
            flat_line_selection(selection, &at)
        })
        .collect()
}

/// Refuses a record taken from the side of the index this command cannot use.
///
/// A record with no `target` is accepted: the flat form is not required to
/// carry one, and revalidation still refuses anything that does not match.
fn check_target(record: &Value, consumes: &str, at: &str) -> Result<(), CliError> {
    let Some(target) = record.get("target").and_then(Value::as_str) else {
        return Ok(());
    };
    if target == consumes {
        return Ok(());
    }
    let other = if consumes == "unstaged" {
        "unstage"
    } else {
        "stage"
    };
    Err(CliError::Usage(format!(
        "{at}.target is \"{target}\" but this command consumes {consumes} records; \
         narrow the diff with --{consumes}, or pass the document to 'git {other}'"
    )))
}

fn file_selections(file: &Value, at: &str) -> Result<Vec<HunkSelection>, CliError> {
    let old_path = record_path(file, "old_path", at)?;
    let new_path = record_path(file, "new_path", at)?;
    if old_path.is_none() && new_path.is_none() {
        return Err(CliError::Usage(format!(
            "{at} has neither an old_path nor a new_path"
        )));
    }
    let old_blob_id = record_string(file, "old_blob_id", at)?;
    let new_blob_id = record_string(file, "new_blob_id", at)?;
    let context_lines = record_u32(file, "context_lines", at)?;
    let whitespace = record_whitespace(file, at)?;
    let hunks = array(
        file.get("hunks")
            .ok_or_else(|| CliError::Usage(format!("{at} has no \"hunks\"")))?,
        &format!("{at}.hunks"),
    )?;
    hunks
        .iter()
        .enumerate()
        .map(|(hunk_index, hunk)| {
            let at = format!("{at}.hunks[{hunk_index}]");
            Ok(HunkSelection::from_parts(
                old_path.clone(),
                new_path.clone(),
                old_blob_id.clone(),
                new_blob_id.clone(),
                context_lines,
                whitespace,
                (
                    record_u32(hunk, "old_start", &at)?,
                    record_u32(hunk, "old_lines", &at)?,
                ),
                (
                    record_u32(hunk, "new_start", &at)?,
                    record_u32(hunk, "new_lines", &at)?,
                ),
            ))
        })
        .collect()
}

fn file_line_selections(file: &Value, at: &str) -> Result<Vec<LineSelection>, CliError> {
    let old_path = record_path(file, "old_path", at)?;
    let new_path = record_path(file, "new_path", at)?;
    if old_path.is_none() && new_path.is_none() {
        return Err(CliError::Usage(format!(
            "{at} has neither an old_path nor a new_path"
        )));
    }
    let old_blob_id = record_string(file, "old_blob_id", at)?;
    let new_blob_id = record_string(file, "new_blob_id", at)?;
    let context_lines = record_u32(file, "context_lines", at)?;
    let whitespace = record_whitespace(file, at)?;
    let hunks = array(
        file.get("hunks")
            .ok_or_else(|| CliError::Usage(format!("{at} has no \"hunks\"")))?,
        &format!("{at}.hunks"),
    )?;
    let mut selections = Vec::new();
    for (hunk_index, hunk) in hunks.iter().enumerate() {
        let hunk_at = format!("{at}.hunks[{hunk_index}]");
        let old_range = (
            record_u32(hunk, "old_start", &hunk_at)?,
            record_u32(hunk, "old_lines", &hunk_at)?,
        );
        let new_range = (
            record_u32(hunk, "new_start", &hunk_at)?,
            record_u32(hunk, "new_lines", &hunk_at)?,
        );
        let lines = array(
            hunk.get("lines")
                .ok_or_else(|| CliError::Usage(format!("{hunk_at} has no \"lines\"")))?,
            &format!("{hunk_at}.lines"),
        )?;
        for (line_index, line) in lines.iter().enumerate() {
            let line_at = format!("{hunk_at}.lines[{line_index}]");
            let kind = record_string(line, "kind", &line_at)?;
            if !matches!(kind.as_str(), "addition" | "deletion") {
                if matches!(
                    kind.as_str(),
                    "context" | "both_eof_no_newline" | "old_eof_no_newline" | "new_eof_no_newline"
                ) {
                    continue;
                }
                return Err(CliError::Usage(format!(
                    "{line_at}.kind is not a selectable diff-line kind"
                )));
            }
            let (old_line_number, new_line_number) = line_coordinates(line, Some(&kind), &line_at)?;
            selections.push(LineSelection::from_parts(
                old_path.clone(),
                new_path.clone(),
                old_blob_id.clone(),
                new_blob_id.clone(),
                context_lines,
                whitespace,
                old_range,
                new_range,
                old_line_number,
                new_line_number,
            ));
        }
    }
    Ok(selections)
}

fn flat_selection(selection: &Value, at: &str) -> Result<HunkSelection, CliError> {
    let old_path = record_path(selection, "old_path", at)?;
    let new_path = record_path(selection, "new_path", at)?;
    if old_path.is_none() && new_path.is_none() {
        return Err(CliError::Usage(format!(
            "{at} has neither an old_path nor a new_path"
        )));
    }
    Ok(HunkSelection::from_parts(
        old_path,
        new_path,
        record_string(selection, "old_blob_id", at)?,
        record_string(selection, "new_blob_id", at)?,
        record_u32(selection, "context_lines", at)?,
        record_whitespace(selection, at)?,
        (
            record_u32(selection, "old_start", at)?,
            record_u32(selection, "old_lines", at)?,
        ),
        (
            record_u32(selection, "new_start", at)?,
            record_u32(selection, "new_lines", at)?,
        ),
    ))
}

fn flat_line_selection(selection: &Value, at: &str) -> Result<LineSelection, CliError> {
    let old_path = record_path(selection, "old_path", at)?;
    let new_path = record_path(selection, "new_path", at)?;
    if old_path.is_none() && new_path.is_none() {
        return Err(CliError::Usage(format!(
            "{at} has neither an old_path nor a new_path"
        )));
    }
    let kind = selection.get("kind").and_then(Value::as_str);
    let (old_line_number, new_line_number) = line_coordinates(selection, kind, at)?;
    Ok(LineSelection::from_parts(
        old_path,
        new_path,
        record_string(selection, "old_blob_id", at)?,
        record_string(selection, "new_blob_id", at)?,
        record_u32(selection, "context_lines", at)?,
        record_whitespace(selection, at)?,
        (
            record_u32(selection, "old_start", at)?,
            record_u32(selection, "old_lines", at)?,
        ),
        (
            record_u32(selection, "new_start", at)?,
            record_u32(selection, "new_lines", at)?,
        ),
        old_line_number,
        new_line_number,
    ))
}

fn line_coordinates(
    line: &Value,
    kind: Option<&str>,
    at: &str,
) -> Result<(Option<u32>, Option<u32>), CliError> {
    let old = record_optional_u32(line, "old_line_number", at)?;
    let new = record_optional_u32(line, "new_line_number", at)?;
    let expected = match kind {
        Some("addition") => old.is_none() && new.is_some(),
        Some("deletion") => old.is_some() && new.is_none(),
        Some(other) => {
            return Err(CliError::Usage(format!(
                "{at}.kind \"{other}\" is not selectable"
            )));
        }
        None => old.is_some() ^ new.is_some(),
    };
    if !expected {
        return Err(CliError::Usage(format!(
            "{at} must identify one addition or deletion by its old/new line numbers"
        )));
    }
    Ok((old, new))
}

fn array<'a>(value: &'a Value, at: &str) -> Result<&'a Vec<Value>, CliError> {
    value
        .as_array()
        .ok_or_else(|| CliError::Usage(format!("{at} is not an array")))
}

/// Reads one side's path, preferring the Base64 spelling of its exact bytes.
///
/// A path the diff marked lossy is refused rather than replayed: the lossy
/// string names a different file, so accepting it would report a stale
/// selection for a path that is merely unspellable in JSON.
fn record_path(record: &Value, field: &str, at: &str) -> Result<Option<PathBuf>, CliError> {
    let encoded_field = format!("{field}_base64");
    match record.get(&encoded_field) {
        Some(Value::String(encoded)) => {
            return decode_path(encoded, &format!("{at}.{encoded_field}")).map(Some);
        }
        Some(Value::Null) | None => {}
        Some(_) => {
            return Err(CliError::Usage(format!(
                "{at}.{encoded_field} is not a string"
            )));
        }
    }
    match record.get(field) {
        Some(Value::String(path)) => {
            if record.get(format!("{field}_is_lossy")) == Some(&Value::Bool(true)) {
                return Err(CliError::Usage(format!(
                    "{at}.{field} is lossy; supply {encoded_field} to name its exact bytes"
                )));
            }
            Ok(Some(PathBuf::from(path)))
        }
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(CliError::Usage(format!("{at}.{field} is not a string"))),
    }
}

fn record_string(record: &Value, field: &str, at: &str) -> Result<String, CliError> {
    record
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| CliError::Usage(format!("{at}.{field} is missing or not a string")))
}

fn record_u32(record: &Value, field: &str, at: &str) -> Result<u32, CliError> {
    record
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| {
            CliError::Usage(format!(
                "{at}.{field} is missing or not a 32-bit unsigned integer"
            ))
        })
}

fn record_optional_u32(record: &Value, field: &str, at: &str) -> Result<Option<u32>, CliError> {
    match record.get(field) {
        Some(Value::Null) | None => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| {
                CliError::Usage(format!(
                    "{at}.{field} is not a 32-bit unsigned integer or null"
                ))
            }),
    }
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
    Check(String),
    /// A check ran correctly and did not pass.
    ///
    /// Reported through the error envelope rather than as a success carrying a
    /// negative verdict, so a caller can act on the exit status without parsing
    /// stdout — the reason `cargo test` and `gh pr checks` both exit non-zero.
    /// The whole recorded result travels in `details`, so nothing a success
    /// envelope carried is lost.
    CheckVerdict {
        kind: &'static str,
        message: String,
        details: Value,
    },
    Usage(String),
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
    /// A coordinator or run-store failure, reported under the runtime namespace.
    ///
    /// The discriminant is `RuntimeError::kind()` verbatim, including the store
    /// spellings it delegates to. In particular an unknown run, approval, or
    /// task is the runtime's own `not_found` rather than a CLI-invented
    /// `run_not_found`: the record kind and identifier travel in `details`, so
    /// nothing is lost, and a caller reading a discriminant sees the same word
    /// the application service produced.
    Runtime(RuntimeError),
    /// A run-store failure reached directly, without a coordinator verb.
    Store(StoreError),
    /// A recorded run or tool call that did not succeed.
    ///
    /// Distinct from [`Runtime`](Self::Runtime): nothing failed to *execute*
    /// here. The runtime did exactly what it was asked and recorded an outcome,
    /// and this is the CLI reporting that outcome through the error envelope so
    /// a caller can act on the exit status without parsing standard output —
    /// the same reason a project check reports its verdict this way.
    RuntimeOutcome {
        kind: &'static str,
        code: u8,
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
            Self::Check(_) => "check_operation_failed",
            Self::CheckVerdict { kind, .. } => kind,
            Self::Usage(_) => "usage_error",
            Self::CurrentDirectory(_) => "current_directory_unavailable",
            Self::InterruptHandler(_) => "interrupt_handler_unavailable",
            Self::WireProjection(_) => "wire_projection_failed",
            Self::PathOperation { .. } => "path_operation_failed",
            Self::Refused { kind, .. } => kind.as_str(),
            Self::Runtime(error) => error.kind(),
            Self::Store(error) => error.kind(),
            Self::RuntimeOutcome { kind, .. } => kind,
        }
    }

    fn message(&self) -> String {
        match self {
            Self::Project(error) => error.to_string(),
            Self::Check(message) | Self::CheckVerdict { message, .. } => message.clone(),
            Self::Usage(message) => message.clone(),
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
            Self::Runtime(error) => error.to_string(),
            Self::Store(error) => error.to_string(),
            Self::RuntimeOutcome { message, .. } => message.clone(),
        }
    }

    fn exit_code(&self) -> u8 {
        match self {
            Self::Project(error) => project_exit_code(error),
            Self::Usage(_) => EXIT_USAGE,
            Self::CurrentDirectory(_)
            | Self::InterruptHandler(_)
            | Self::WireProjection(_)
            | Self::PathOperation { .. }
            | Self::Check(_) => EXIT_OPERATION_FAILED,
            Self::CheckVerdict { kind, .. } => {
                if *kind == "check_cancelled" {
                    EXIT_CANCELLED
                } else {
                    EXIT_OPERATION_FAILED
                }
            }
            Self::Refused { .. } => EXIT_REFUSED,
            Self::Runtime(error) => runtime_exit_code(error),
            Self::Store(error) => store_exit_code(error),
            Self::RuntimeOutcome { code, .. } => *code,
        }
    }

    fn details(&self) -> Value {
        match self {
            Self::Project(error) => project_error_details(error),
            Self::Refused { details, .. } => details.clone(),
            Self::PathOperation { details, .. } => details.clone(),
            Self::CheckVerdict { details, .. } => details.clone(),
            Self::RuntimeOutcome { details, .. } => details.clone(),
            Self::Runtime(error) => runtime_details(error),
            Self::Store(error) => store_details(error),
            Self::Check(_)
            | Self::Usage(_)
            | Self::CurrentDirectory(_)
            | Self::InterruptHandler(_)
            | Self::WireProjection(_) => {
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
    /// One persisted run event, when the progress line is a timeline entry.
    ///
    /// Additive within envelope version 1: every progress line that existed
    /// before — clone, fetch, pull, and push — omits the field entirely, so a
    /// consumer that never looked for it reads exactly the bytes it always
    /// read. It exists because a run timeline is machine-readable evidence and
    /// `message` alone would force a consumer to parse prose or re-read the
    /// whole log with `run show` to recover what it just watched go past.
    #[serde(skip_serializing_if = "Option::is_none")]
    event: Option<&'a Value>,
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
                emit_error(
                    "usage_error",
                    &clap_error_message(&error),
                    clap_error_details(&error),
                )
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

/// The rest of a clap diagnostic, as data rather than as discarded prose.
///
/// The message is only the first line, and for an argument that requires eight
/// companions — `--hunk` — that line is the useless half: it says arguments are
/// missing and names none of them. The list lives in the lines that follow, so
/// they are carried through as `missing` instead of being dropped.
fn clap_error_details(error: &ClapError) -> Value {
    let rendered = error.to_string();
    let missing = rendered
        .lines()
        .skip(1)
        .map(str::trim)
        .take_while(|line| !line.is_empty())
        .filter(|line| !line.starts_with("Usage:") && !line.starts_with("For more information"))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return json!({});
    }
    json!({ "missing": missing })
}

fn finish_result(result: CommandResult) -> ExitCode {
    let output = match result {
        CommandResult::Human(human) if human.is_empty() => Ok(()),
        CommandResult::Human(human) => write_line(&mut io::stdout().lock(), human.as_bytes()),
        CommandResult::Json(data) => emit_success(data),
    };
    finish_output(output, 0)
}

fn finish_error(json_output: bool, error: CliError) -> ExitCode {
    let code = error.exit_code();
    let output = if json_output {
        emit_error(error.kind(), &error.message(), error.details())
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
        verbose,
        command,
    } = cli;
    // Installed before any command body, so a span opened deep in the runtime
    // has somewhere to go. The log file itself is created by the first line
    // rather than by this call, which is what keeps `harkness project list`
    // against a data directory that does not exist from bringing one into
    // being — the same promise `Store::open_existing` makes about `runtime.db`.
    let diagnostics = observe::init(
        data_dir
            .clone()
            .or_else(harkness_core::data_directory)
            .as_deref(),
        observe::Options::default().mirror_to_stderr(verbose),
    );
    // Reported rather than discarded, because "where did my logs go" is a
    // question with an answer, and `--verbose` — which installs the stderr
    // mirror — is how a user asks it. Emitted as an event rather than printed,
    // so the answer arrives as one JSON object per line like everything else on
    // this stream, and so the file records its own location.
    tracing::info!(
        arrangement = %diagnostics.describe(),
        "diagnostics initialized"
    );
    match command {
        Command::Project { command } => {
            run_project(command, data_dir.as_deref(), json, cancellation)
        }
        Command::Git { command } => run_git(*command, data_dir.as_deref(), json, cancellation),
        Command::Worktree { command } => {
            run_worktree(command, data_dir.as_deref(), json, cancellation)
        }
        Command::Check { command } => run_check(command, data_dir.as_deref(), json, cancellation),
        Command::Editor { command } => run_editor(command, data_dir.as_deref(), json),
        Command::Run { command } => {
            run_commands::run_run(command, data_dir.as_deref(), json, cancellation)
        }
        Command::Approvals { command } => {
            approval_commands::run_approvals(command, data_dir.as_deref(), json, cancellation)
        }
        Command::Tool { command } => {
            tool_commands::run_tool(command, data_dir.as_deref(), json, cancellation)
        }
        Command::Agent { command } => {
            agent_commands::run_agent(command, data_dir.as_deref(), json, cancellation)
        }
        Command::Contract => Ok(contract_result(json)),
    }
}

fn run_check(
    command: CheckCommand,
    data_dir: Option<&Path>,
    json_output: bool,
    cancellation: &Cancellation,
) -> Result<CommandResult, CliError> {
    let mut service = load_service(data_dir)?;
    match command {
        CheckCommand::List(selection) => {
            let project = resolve_project(&service, selection.project.as_deref())?;
            let checks = project.effective_checks();
            let results = recorded_checks_without_creating_a_store(&service, &project)?;
            check_list_result(json_output, &checks, &results)
        }
        CheckCommand::Configure(arguments) => {
            let project = resolve_project(&service, arguments.selection.project.as_deref())?;
            let document = read_selection_document(&arguments.from)?;
            let checks =
                serde_json::from_str::<Vec<CheckConfiguration>>(&document).map_err(|error| {
                    CliError::Usage(format!(
                        "the check document must be a JSON array of check definitions: {error}"
                    ))
                })?;
            let configured = service.configure_checks(project.id, Some(checks))?;
            let stored = configured.checks.clone().unwrap_or_default();
            command_result(
                json_output,
                || {
                    format!(
                        "configured {} check{} for {}",
                        stored.len(),
                        if stored.len() == 1 { "" } else { "s" },
                        single_line(&configured.display_name)
                    )
                },
                || {
                    Ok(json!({
                        "kind": "check_configure",
                        "project_id": configured.id,
                        "checks": stored,
                    }))
                },
            )
        }
        CheckCommand::Clear(arguments) => {
            if !arguments.yes {
                return Err(CliError::Refused {
                    kind: RefusalKind::ConfirmationRequired,
                    message: "clearing project checks discards their explicit configuration; retry with --yes"
                        .to_owned(),
                    details: json!({}),
                });
            }
            let project = resolve_project(&service, arguments.selection.project.as_deref())?;
            // `None`, not an empty list. The distinction is the whole point of
            // the field: no explicit configuration falls back to the workspace
            // defaults, while an empty list is a configured "run nothing".
            let cleared = service.configure_checks(project.id, None)?;
            command_result(
                json_output,
                || {
                    format!(
                        "cleared explicit checks for {}",
                        single_line(&cleared.display_name)
                    )
                },
                || {
                    Ok(json!({
                        "kind": "check_clear",
                        "project_id": cleared.id,
                        "checks": Value::Null,
                        "effective_checks": cleared.effective_checks(),
                    }))
                },
            )
        }
        CheckCommand::Run(arguments) => {
            if !arguments.yes {
                return Err(CliError::Refused {
                    kind: RefusalKind::ConfirmationRequired,
                    message: "running a project check requires --yes after reviewing its configured command"
                        .to_owned(),
                    details: json!({ "check_id": arguments.check_id }),
                });
            }
            let project = resolve_project(&service, arguments.selection.project.as_deref())?;
            let checks = project.effective_checks();
            let check = checks
                .iter()
                .find(|check| check.id == arguments.check_id)
                .ok_or_else(|| {
                    CliError::Usage(format!(
                        "project {} has no configured check {:?}",
                        project.display_name, arguments.check_id
                    ))
                })?;
            let store = Arc::new(
                Store::open(service.data_dir())
                    .map_err(|error| CliError::Check(error.to_string()))?,
            );
            if arguments.trust_workspace {
                let trust = WorkspaceTrust::decide(
                    project.id,
                    &project.root,
                    TrustState::Trusted,
                    OffsetDateTime::now_utc(),
                )
                .map_err(|error| CliError::Check(error.to_string()))?;
                store
                    .put_workspace_trust(&trust)
                    .map_err(|error| CliError::Check(error.to_string()))?;
            }
            if store
                .resolve_workspace_trust(project.id, &project.root)
                .map_err(|error| CliError::Check(error.to_string()))?
                != TrustState::Trusted
            {
                return Err(CliError::Refused {
                    kind: RefusalKind::ConfirmationRequired,
                    message: "the selected workspace is untrusted; review the project root and retry with --trust-workspace --yes"
                        .to_owned(),
                    details: json!({
                        "project_id": project.id,
                        "root": project.root,
                        "check_id": check.id,
                        "command": check.command,
                    }),
                });
            }
            // One coordinator, and therefore one scheduler, for the process.
            // This process runs exactly one check, so building it here is also
            // building it once.
            let coordinator = check_coordinator(Arc::clone(&store))
                .map_err(|error| CliError::Check(error.to_string()))?;
            let run_id =
                run_configured_check(&coordinator, &project, check, DecidedVia::Cli, cancellation)
                    .map_err(|error| match error {
                        // A configuration this build cannot execute is the caller's
                        // input, not a failed operation.
                        harkness_runtime::check::CheckLaunchError::UndeclaredEnvironment {
                            ..
                        } => CliError::Usage(error.to_string()),
                        other => CliError::Check(other.to_string()),
                    })?;
            let results = project_checks(&store, &project)
                .map_err(|error| CliError::Check(error.to_string()))?;
            let result = results
                .iter()
                .find(|result| result.run_id == run_id.to_string());
            let data = json!({
                "kind": "check_run",
                "run_id": run_id,
                "check": check,
                "result": result,
            });
            // The verdict decides the exit status. Reporting a check that did not
            // pass as a plain success left the only usable signal inside the JSON,
            // so the CI-shaped caller this command exists for could not tell pass
            // from fail without parsing it — and a run cancelled by Ctrl-C never
            // produced the documented 130 at all.
            if let Some(verdict) = check_verdict(check, result, &data) {
                return Err(verdict);
            }
            command_result(
                json_output,
                || {
                    result.map_or_else(
                        || format!("check {} recorded as run {run_id}", check.label),
                        check_summary_line,
                    )
                },
                || Ok(data),
            )
        }
    }
}

fn check_list_result(
    json_output: bool,
    checks: &[CheckConfiguration],
    results: &[CheckSummary],
) -> Result<CommandResult, CliError> {
    command_result(
        json_output,
        || {
            if checks.is_empty() {
                return "no checks configured".to_owned();
            }
            checks
                .iter()
                .map(|check| {
                    results
                        .iter()
                        .find(|result| result.check_id == check.id)
                        .map_or_else(
                            || format!("{}\tnever run\t{}", check.id, single_line(&check.label)),
                            check_summary_line,
                        )
                })
                .collect::<Vec<_>>()
                .join("\n")
        },
        || Ok(json!({ "kind": "check_list", "checks": checks, "results": results })),
    )
}

/// Recorded check results for a read-only projection.
///
/// A read must not bring a run store into existence. `Store::open` creates
/// `runtime.db`, its WAL sidecars and every migration as a side effect, so
/// `check list` and `git diff --checks` — both of which only report — used to
/// write to a data directory a caller had asked them to read. No database means
/// nothing has been recorded, which is the empty projection.
fn recorded_checks_without_creating_a_store(
    service: &ProjectService,
    project: &Project,
) -> Result<Vec<CheckSummary>, CliError> {
    let Some(store) = Store::open_existing(service.data_dir())
        .map_err(|error| CliError::Check(error.to_string()))?
    else {
        return Ok(Vec::new());
    };
    project_checks(&store, project).map_err(|error| CliError::Check(error.to_string()))
}

/// The error a non-passing run reports, or `None` when the check passed.
///
/// An absent result is a verdict too: the command was asked to produce one and
/// there is nothing recorded to read, which no caller should see as a pass.
fn check_verdict(
    check: &CheckConfiguration,
    result: Option<&CheckSummary>,
    data: &Value,
) -> Option<CliError> {
    let Some(result) = result else {
        return Some(CliError::CheckVerdict {
            kind: "check_failed",
            message: format!(
                "check {} recorded no result to report",
                single_line(&check.label)
            ),
            details: data.clone(),
        });
    };
    let (kind, verdict) = match result.outcome {
        CheckOutcome::Passed => return None,
        CheckOutcome::Cancelled => ("check_cancelled", "was cancelled"),
        CheckOutcome::Interrupted => ("check_cancelled", "was interrupted"),
        CheckOutcome::TimedOut => ("check_failed", "timed out"),
        CheckOutcome::Denied => ("check_failed", "was denied"),
        CheckOutcome::Failed => ("check_failed", "failed"),
        // Neither is reachable from here: this runs after the supervising loop
        // has seen the run reach a terminal state. Named rather than caught by a
        // wildcard so a new outcome has to be classified here on purpose.
        CheckOutcome::Queued | CheckOutcome::WaitingForApproval | CheckOutcome::Running => {
            ("check_failed", "did not reach a verdict")
        }
    };
    Some(CliError::CheckVerdict {
        kind,
        message: format!("check {} {verdict}", single_line(&result.label)),
        details: data.clone(),
    })
}

fn check_summary_line(summary: &CheckSummary) -> String {
    let outcome = serde_json::to_value(summary.outcome)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned());
    let freshness = match &summary.freshness {
        harkness_runtime::check::CheckFreshness::Current => "current",
        harkness_runtime::check::CheckFreshness::Stale { .. } => "stale",
        harkness_runtime::check::CheckFreshness::Unverifiable { .. } => "unverifiable",
    };
    format!(
        "{}\t{}\t{}\t{}",
        summary.check_id,
        outcome,
        freshness,
        single_line(&summary.label)
    )
}

fn run_editor(
    command: EditorCommand,
    data_dir: Option<&Path>,
    json_output: bool,
) -> Result<CommandResult, CliError> {
    let mut service = load_service(data_dir)?;
    match command {
        EditorCommand::Show => {
            let configuration = service.editor_configuration()?;
            command_result(
                json_output,
                || {
                    configuration.as_ref().map_or_else(
                        || "automatic ($VISUAL, $EDITOR, desktop default)".to_owned(),
                        display_editor_command,
                    )
                },
                || Ok(json!({ "editor": configuration.as_ref().map(editor_configuration_value) })),
            )
        }
        EditorCommand::Presets => command_result(
            json_output,
            || {
                EditorPreset::ALL
                    .iter()
                    .map(|preset| {
                        format!(
                            "{}\t{}\t{}",
                            preset.id(),
                            preset.name(),
                            display_editor_command(&preset.configuration())
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            },
            || {
                Ok(json!({
                    "presets": EditorPreset::ALL.iter().map(|preset| json!({
                        "id": preset.id(),
                        "name": preset.name(),
                        "command": preset.configuration().command(),
                    })).collect::<Vec<_>>()
                }))
            },
        ),
        EditorCommand::Set(arguments) => {
            let configuration = match arguments.preset {
                Some(preset) => EditorPreset::from(preset).configuration(),
                None => EditorConfiguration::new(arguments.command).map_err(ProjectError::from)?,
            };
            service.set_editor_configuration(Some(configuration.clone()))?;
            command_result(
                json_output,
                || format!("editor set to {}", display_editor_command(&configuration)),
                || Ok(json!({ "editor": editor_configuration_value(&configuration) })),
            )
        }
        EditorCommand::Clear => {
            service.set_editor_configuration(None)?;
            command_result(
                json_output,
                || "editor configuration cleared".to_owned(),
                || Ok(json!({ "editor": Value::Null })),
            )
        }
        EditorCommand::Open(arguments) => {
            let project = resolve_project(&service, arguments.selection.project.as_deref())?;
            let launch = service.open_in_editor(
                project.id,
                &arguments.path,
                EditorPosition::new(arguments.line, arguments.column),
                EditorLaunchContext::CommandLine,
            )?;
            command_result(
                json_output,
                || {
                    format!(
                        "opened {} at {}:{} with {}",
                        arguments.path.display(),
                        launch.position.line(),
                        launch.position.column(),
                        launch.command
                    )
                },
                || {
                    let (file, path_is_lossy) = wire_path(&launch.file);
                    Ok(json!({
                        "kind": "editor_open",
                        "command": launch.command,
                        "file": file,
                        "path_is_lossy": path_is_lossy,
                        "line": launch.position.line(),
                        "column": launch.position.column(),
                    }))
                },
            )
        }
    }
}

fn editor_configuration_value(configuration: &EditorConfiguration) -> Value {
    json!({ "command": configuration.command() })
}

fn display_editor_command(configuration: &EditorConfiguration) -> String {
    Value::Array(
        configuration
            .command()
            .iter()
            .cloned()
            .map(Value::String)
            .collect(),
    )
    .to_string()
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
        GitCommand::Log(arguments) => {
            let options = arguments.options()?;
            let git = selected_git(&service, arguments.selection)?;
            let page = git.log(&options, cancellation)?;
            command_result(
                json_output,
                || log_page_line(&page.commits, page.next_cursor.is_some()),
                || {
                    Ok(json!({
                        "kind": "git_log",
                        "range": log_range_value(&options.range),
                        "limit": options.limit,
                        "commits": page.commits.iter().map(commit_value).collect::<Vec<_>>(),
                        "next_cursor": page
                            .next_cursor
                            .as_ref()
                            .map(encode_log_cursor)
                            .transpose()?,
                    }))
                },
            )
        }
        GitCommand::Diff(arguments) => {
            let context_mode = arguments.context_mode();
            let max_file_size = arguments.max_file_size;
            let max_total_bytes = arguments.max_total_bytes;
            if let Some(source) = arguments.context_from.as_deref() {
                if context_mode == DiffContextMode::None {
                    return Err(CliError::Usage(
                        "--context-from requires --expand-context or --full-file-context"
                            .to_owned(),
                    ));
                }
                let document = read_selection_document(source)?;
                let source_files = parse_context_files(&document)?;
                let git = selected_git(&service, arguments.selection)?;
                let projected = context_values_from_document(
                    &git,
                    &source_files,
                    context_mode,
                    max_file_size,
                    max_total_bytes,
                    cancellation,
                )?;
                return command_result(
                    json_output,
                    || {
                        format!(
                            "expanded context for {} file{}",
                            projected.len(),
                            if projected.len() == 1 { "" } else { "s" }
                        )
                    },
                    || Ok(json!({ "kind": "git_diff_context", "files": projected })),
                );
            }
            let targets = arguments.targets()?;
            let include_intra_line = arguments.intra_line;
            let whitespace = Whitespace {
                mode: arguments.whitespace.into(),
                ignore_blank_lines: arguments.ignore_blank_lines,
            };
            // Resolved once, whether or not the checks projection was asked for:
            // `selected_git` resolves the same project internally and throws it
            // away, so keeping it costs nothing and the two paths stop being two
            // copies of one lookup.
            let (project, git) = selected_project_git(&service, arguments.selection)?;
            let check_project = arguments.checks.then_some(project);
            let options = DiffOptions::default()
                .with_context_lines(arguments.context_lines)
                .with_whitespace(whitespace)
                .with_intra_line_ranges(include_intra_line)
                .with_max_file_size(max_file_size)
                .with_max_total_bytes(max_total_bytes)
                .with_max_files(arguments.max_files)
                .with_paths(arguments.paths);
            if cancellation.is_cancelled() {
                return Err(GitError::Cancelled.into());
            }
            let files = git.diff_snapshot(&targets, &options)?;
            if cancellation.is_cancelled() {
                return Err(GitError::Cancelled.into());
            }
            let mut projected = diff_values(
                &git,
                &files,
                context_mode,
                include_intra_line,
                max_file_size,
                max_total_bytes,
                cancellation,
            )?;
            let attribution = if arguments.provenance {
                let records = resolve_diff_provenance(
                    &git,
                    &targets,
                    &files,
                    arguments.provenance_max_commits,
                    cancellation,
                )?;
                for (value, entry) in projected
                    .iter_mut()
                    .zip(file_provenance_values(&targets, &files, &records))
                {
                    value
                        .as_object_mut()
                        .expect("a file diff projection is an object")
                        .insert("provenance".to_owned(), entry);
                }
                Some(records)
            } else {
                None
            };
            let recorded_checks = check_project
                .as_ref()
                .map(|project| recorded_checks_without_creating_a_store(&service, project))
                .transpose()?;
            let (checks, checks_excluded_for_target) =
                recorded_checks.map_or(Ok::<_, CliError>((None, 0usize)), |recorded| {
                    let total = recorded.len();
                    let mut covering = Vec::new();
                    // One resolver for the whole pass, and the first target that
                    // is not covered ends this check. Resolving inside the target
                    // loop asked Git for the same two or three revisions once per
                    // recorded check.
                    let mut revisions = RevisionCache::new(&git);
                    for check in recorded {
                        let mut covers_every_target = true;
                        for target in &targets {
                            if !check_covers_diff_target(&mut revisions, &check, target)? {
                                covers_every_target = false;
                                break;
                            }
                        }
                        if covers_every_target {
                            covering.push(check);
                        }
                    }
                    let excluded = total.saturating_sub(covering.len());
                    Ok((Some(covering), excluded))
                })?;
            command_result(
                json_output,
                || diff_summary_line(&files, &targets, attribution.as_deref()),
                || {
                    Ok(json!({
                        "kind": "git_diff",
                        "targets": targets.iter().map(diff_target_value).collect::<Vec<_>>(),
                        // Repeated on every file record as well. A consumer
                        // that keeps one file and drops the response still
                        // knows how that file's hunks were computed, and a
                        // consumer reading the response as a whole does not
                        // have to open a file record to find out.
                        "whitespace": whitespace_value(whitespace),
                        // Null means attribution was not asked for. An empty
                        // block means it was asked for and there was none,
                        // which is a different answer and the common one.
                        "provenance": attribution.as_ref().map_or(Value::Null, |records| {
                            json!(
                                records
                                    .iter()
                                    .zip(&targets)
                                    .map(|(record, target)| change_provenance_value(
                                        record, target
                                    ))
                                    .collect::<Vec<_>>()
                            )
                        }),
                        // Null means the projection was not requested; an empty
                        // list means the project has no recorded checks.
                        "checks": checks,
                        // Results are excluded unless their recorded state and
                        // definition cover every target in this envelope.
                        "checks_excluded_for_target": checks_excluded_for_target,
                        "files": projected,
                    }))
                },
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
            let selections = arguments.hunk.into_selections("unstaged")?;
            let git = selected_git(&service, arguments.selection)?;
            if let Some(selections) = selections {
                match selections {
                    GranularSelections::Hunks(selections) => {
                        let outcome = git.stage_hunks(&selections, cancellation)?;
                        // The applied count, not the supplied one: a batch deduplicates
                        // selections that resolve to the same hunk.
                        let count = outcome.hunks;
                        command_result(
                            json_output,
                            || hunk_outcome_line("staged", count),
                            || {
                                Ok(json!({
                                    "hunks": count,
                                    "status": status_refresh_value(&outcome.status),
                                }))
                            },
                        )
                    }
                    GranularSelections::Lines(selections) => {
                        let outcome = git.stage_lines(&selections, cancellation)?;
                        command_result(
                            json_output,
                            || line_outcome_line("staged", outcome.lines),
                            || {
                                Ok(json!({
                                    "lines": outcome.lines,
                                    "hunks": outcome.hunks,
                                    "status": status_refresh_value(&outcome.status),
                                }))
                            },
                        )
                    }
                }
            } else if arguments.all {
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
        GitCommand::Unstage(arguments) => {
            let selections = arguments.hunk.into_selections("staged")?;
            let git = selected_git(&service, arguments.selection)?;
            if let Some(selections) = selections {
                match selections {
                    GranularSelections::Hunks(selections) => {
                        let outcome = git.unstage_hunks(&selections, cancellation)?;
                        let count = outcome.hunks;
                        command_result(
                            json_output,
                            || hunk_outcome_line("unstaged", count),
                            || {
                                Ok(json!({
                                    "hunks": count,
                                    "status": status_refresh_value(&outcome.status),
                                }))
                            },
                        )
                    }
                    GranularSelections::Lines(selections) => {
                        let outcome = git.unstage_lines(&selections, cancellation)?;
                        command_result(
                            json_output,
                            || line_outcome_line("unstaged", outcome.lines),
                            || {
                                Ok(json!({
                                    "lines": outcome.lines,
                                    "hunks": outcome.hunks,
                                    "status": status_refresh_value(&outcome.status),
                                }))
                            },
                        )
                    }
                }
            } else {
                let outcome = git.unstage(arguments.paths, cancellation)?;
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
        }
        GitCommand::Discard(arguments) => {
            let selections = arguments.hunk.into_selections("unstaged")?;
            let git = selected_git(&service, arguments.selection)?;
            let source = arguments.from.map(TrackedRestoreSource::from);
            let description = match (&selections, source, arguments.delete_untracked) {
                (
                    Some(GranularSelections::Hunks(selections)),
                    Some(TrackedRestoreSource::Index),
                    false,
                ) => {
                    let hunks = distinct_hunk_selection_count(selections);
                    harkness_git::DiscardDescription::restore_hunks(
                        selections.iter().filter_map(HunkSelection::path),
                        hunks,
                    )
                }
                (
                    Some(GranularSelections::Lines(selections)),
                    Some(TrackedRestoreSource::Index),
                    false,
                ) => {
                    let (lines, hunks) = distinct_line_selection_counts(selections);
                    harkness_git::DiscardDescription::restore_lines(
                        selections.iter().filter_map(LineSelection::path),
                        lines,
                        hunks,
                    )
                }
                (Some(_), Some(TrackedRestoreSource::Head), false) => {
                    return Err(CliError::Usage(
                        "hunk and line discard restore from the index; use --from index".to_owned(),
                    ));
                }
                (None, Some(source), false) => {
                    harkness_git::DiscardDescription::restore_tracked(&arguments.paths, source)
                }
                (None, None, true) => {
                    harkness_git::DiscardDescription::delete_untracked(&arguments.paths)
                }
                _ => {
                    return Err(CliError::Usage(
                        "choose exactly one tracked restore boundary or --delete-untracked"
                            .to_owned(),
                    ));
                }
            };
            if !arguments.yes {
                return Err(refusal(
                    RefusalKind::ConfirmationRequired,
                    discard_confirmation_message(&description),
                    json!({
                        "override_flag": "--yes",
                        "discard": discard_description_value(&description),
                    }),
                ));
            }
            let outcome = match selections {
                Some(GranularSelections::Hunks(selections)) => {
                    git.discard_hunks(&selections, cancellation)?
                }
                Some(GranularSelections::Lines(selections)) => {
                    git.discard_lines(&selections, cancellation)?
                }
                None if arguments.delete_untracked => {
                    git.delete_untracked(arguments.paths, cancellation)?
                }
                None => git.restore_tracked(
                    arguments.paths,
                    source.expect("discard kind group requires --from"),
                    cancellation,
                )?,
            };
            command_result(
                json_output,
                || discard_outcome_line(&outcome.description),
                || {
                    Ok(json!({
                        "discard": discard_description_value(&outcome.description),
                        "status": status_refresh_value(&outcome.status),
                    }))
                },
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
        WorktreeCommand::Move {
            selection,
            destination,
        } => {
            let worktree = resolve_project(&service, selection.project.as_deref())?;
            let project = service.move_worktree(worktree.id, &destination, cancellation)?;
            command_result(
                json_output,
                || {
                    format!(
                        "moved {}\t{}\t{}",
                        project.id,
                        project.display_name,
                        project.root.display()
                    )
                },
                || Ok(json!({ "project": project_value(&project, true)? })),
            )
        }
        WorktreeCommand::Lock {
            selection,
            reason,
            replace,
        } => {
            let worktree = resolve_project(&service, selection.project.as_deref())?;
            if replace {
                service.relock_worktree(worktree.id, &reason, cancellation)?;
            } else {
                service.lock_worktree(worktree.id, &reason, cancellation)?;
            }
            // Report the text Git actually stored, not the text as typed.
            let stored = reason.trim().to_owned();
            command_result(
                json_output,
                || format!("locked {}\t{stored}", worktree.display_name),
                || {
                    Ok(json!({
                        "project": project_value(&worktree, false)?,
                        "lock_reason": stored,
                    }))
                },
            )
        }
        WorktreeCommand::Unlock(selection) => {
            let worktree = resolve_project(&service, selection.project.as_deref())?;
            service.unlock_worktree(worktree.id, cancellation)?;
            command_result(
                json_output,
                || format!("unlocked {}", worktree.display_name),
                || Ok(json!({ "project": project_value(&worktree, false)? })),
            )
        }
        WorktreeCommand::Prune(selection) => {
            let parent = resolve_project(&service, selection.project.as_deref())?;
            let outcome = service.prune_worktrees(parent.id, cancellation)?;
            command_result(
                json_output,
                || {
                    format!(
                        "reconciled worktrees: removed {}, repaired {}, skipped {}",
                        outcome.removed.len(),
                        outcome.repaired.len(),
                        outcome.skipped.len()
                    )
                },
                || {
                    Ok(json!({
                        "removed": project_values(&outcome.removed, false)?,
                        "repaired": project_values(&outcome.repaired, false)?,
                        "skipped": outcome.skipped.iter().map(|skip| {
                            Ok(json!({
                                "project": project_value(&skip.project, false)?,
                                "reason": skip.reason,
                            }))
                        }).collect::<Result<Vec<Value>, CliError>>()?,
                    }))
                },
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

/// Resolves the selected project and opens its Git service.
///
/// The project is worth keeping: the `--checks` projection needs it, and
/// `selected_git` resolved one only to drop it, so the two call sites were two
/// copies of this lookup waiting to disagree.
fn selected_project_git(
    service: &ProjectService,
    selection: ProjectSelection,
) -> Result<(Project, GitService), CliError> {
    let project = resolve_project(service, selection.project.as_deref())?;
    let git = service.git(project.id)?;
    Ok((project, git))
}

fn selected_git(
    service: &ProjectService,
    selection: ProjectSelection,
) -> Result<GitService, CliError> {
    selected_project_git(service, selection).map(|(_, git)| git)
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
        // Null distinguishes "no explicit configuration, so the workspace
        // defaults apply" from a configured empty list meaning "run nothing" —
        // the same distinction the catalog stores, and the reason a caller
        // cannot infer this field from `effective_checks` alone.
        "checks": project.checks,
        "effective_checks": project.effective_checks(),
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

fn diff_values(
    git: &GitService,
    files: &[FileDiff],
    context_mode: DiffContextMode,
    include_intra_line: bool,
    max_file_size: u64,
    max_total_bytes: u64,
    cancellation: &Cancellation,
) -> Result<Vec<Value>, CliError> {
    let mut context_budget = max_total_bytes;
    files
        .iter()
        .map(|file| {
            let mut value = file_diff_value(file, include_intra_line);
            if let Some(details) = diff_target_details(&file.target) {
                value
                    .as_object_mut()
                    .expect("a file diff projection is an object")
                    .insert("target_details".to_owned(), details);
            }
            match context_mode {
                DiffContextMode::None => {}
                DiffContextMode::FullFile => {
                    let context = if file.omission.is_some() {
                        Value::Null
                    } else {
                        json!({
                            "kind": "full_file_context",
                            "old": load_diff_context_value(
                                git,
                                file,
                                FileContextRequest::full_file(file, FileSide::Old),
                                max_file_size,
                                &mut context_budget,
                                cancellation,
                            )?,
                            "new": load_diff_context_value(
                                git,
                                file,
                                FileContextRequest::full_file(file, FileSide::New),
                                max_file_size,
                                &mut context_budget,
                                cancellation,
                            )?,
                        })
                    };
                    value
                        .as_object_mut()
                        .expect("a file diff projection is an object")
                        .insert("context".to_owned(), context);
                }
                DiffContextMode::Expanded(lines) => {
                    let projected_hunks = value["hunks"]
                        .as_array_mut()
                        .expect("a file diff hunk projection is an array");
                    for (hunk, projected) in file.hunks.iter().zip(projected_hunks) {
                        let context = json!({
                            "kind": "hunk_context",
                            "lines_before": lines,
                            "lines_after": lines,
                            "old": load_diff_context_value(
                                git,
                                file,
                                FileContextRequest::for_hunk(
                                    file,
                                    hunk,
                                    FileSide::Old,
                                    lines,
                                    lines,
                                ),
                                max_file_size,
                                &mut context_budget,
                                cancellation,
                            )?,
                            "new": load_diff_context_value(
                                git,
                                file,
                                FileContextRequest::for_hunk(
                                    file,
                                    hunk,
                                    FileSide::New,
                                    lines,
                                    lines,
                                ),
                                max_file_size,
                                &mut context_budget,
                                cancellation,
                            )?,
                        });
                        projected
                            .as_object_mut()
                            .expect("a hunk projection is an object")
                            .insert("context".to_owned(), context);
                    }
                }
            }
            Ok(value)
        })
        .collect()
}

fn parse_context_files(document: &str) -> Result<Vec<Value>, CliError> {
    let value: Value = serde_json::from_str(document)
        .map_err(|error| CliError::Usage(format!("the context document is not JSON: {error}")))?;
    let value = value
        .get("data")
        .filter(|data| data.get("files").is_some())
        .unwrap_or(&value);
    let files = value
        .get("files")
        .ok_or_else(|| CliError::Usage("the context document has no \"files\" array".to_owned()))?;
    Ok(array(files, "files")?.clone())
}

fn context_values_from_document(
    git: &GitService,
    files: &[Value],
    context_mode: DiffContextMode,
    max_file_size: u64,
    max_total_bytes: u64,
    cancellation: &Cancellation,
) -> Result<Vec<Value>, CliError> {
    let mut context_budget = max_total_bytes;
    files
        .iter()
        .enumerate()
        .map(|(file_index, source_file)| {
            let at = format!("files[{file_index}]");
            let _ = context_target_uses_worktree(source_file, &at)?;
            let mut projected = source_file.clone();
            match context_mode {
                DiffContextMode::None => unreachable!("context mode was validated by the caller"),
                DiffContextMode::FullFile => {
                    let unavailable = matches!(
                        source_file
                            .get("omission")
                            .and_then(|omission| omission.get("kind"))
                            .and_then(Value::as_str),
                        Some("unmerged" | "unrepresentable")
                    );
                    let context = if unavailable {
                        Value::Null
                    } else {
                        json!({
                            "kind": "full_file_context",
                            "old": load_record_context_value(
                                git,
                                source_file,
                                FileSide::Old,
                                FileContextRange::FullFile,
                                &at,
                                max_file_size,
                                &mut context_budget,
                                cancellation,
                            )?,
                            "new": load_record_context_value(
                                git,
                                source_file,
                                FileSide::New,
                                FileContextRange::FullFile,
                                &at,
                                max_file_size,
                                &mut context_budget,
                                cancellation,
                            )?,
                        })
                    };
                    projected
                        .as_object_mut()
                        .ok_or_else(|| CliError::Usage(format!("{at} is not an object")))?
                        .insert("context".to_owned(), context);
                }
                DiffContextMode::Expanded(lines) => {
                    let source_hunks = array(
                        source_file
                            .get("hunks")
                            .ok_or_else(|| CliError::Usage(format!("{at} has no \"hunks\"")))?,
                        &format!("{at}.hunks"),
                    )?;
                    let projected_hunks = projected
                        .get_mut("hunks")
                        .and_then(Value::as_array_mut)
                        .ok_or_else(|| CliError::Usage(format!("{at}.hunks is not an array")))?;
                    for (hunk_index, (source_hunk, projected_hunk)) in
                        source_hunks.iter().zip(projected_hunks).enumerate()
                    {
                        let hunk_at = format!("{at}.hunks[{hunk_index}]");
                        let old_range =
                            context_range_from_hunk(source_hunk, FileSide::Old, lines, &hunk_at)?;
                        let new_range =
                            context_range_from_hunk(source_hunk, FileSide::New, lines, &hunk_at)?;
                        let context = json!({
                            "kind": "hunk_context",
                            "lines_before": lines,
                            "lines_after": lines,
                            "old": load_record_context_value(
                                git,
                                source_file,
                                FileSide::Old,
                                old_range,
                                &hunk_at,
                                max_file_size,
                                &mut context_budget,
                                cancellation,
                            )?,
                            "new": load_record_context_value(
                                git,
                                source_file,
                                FileSide::New,
                                new_range,
                                &hunk_at,
                                max_file_size,
                                &mut context_budget,
                                cancellation,
                            )?,
                        });
                        projected_hunk
                            .as_object_mut()
                            .ok_or_else(|| CliError::Usage(format!("{hunk_at} is not an object")))?
                            .insert("context".to_owned(), context);
                    }
                }
            }
            Ok(projected)
        })
        .collect()
}

fn context_range_from_hunk(
    hunk: &Value,
    side: FileSide,
    lines: u32,
    at: &str,
) -> Result<FileContextRange, CliError> {
    let (start, count) = match side {
        FileSide::Old => ("old_start", "old_lines"),
        FileSide::New => ("new_start", "new_lines"),
        _ => return Err(CliError::Usage("unsupported context side".to_owned())),
    };
    Ok(FileContextRange::Hunk {
        start_line: record_u32(hunk, start, at)?,
        line_count: record_u32(hunk, count, at)?,
        lines_before: lines,
        lines_after: lines,
    })
}

#[allow(clippy::too_many_arguments)]
fn load_record_context_value(
    git: &GitService,
    file: &Value,
    side: FileSide,
    range: FileContextRange,
    at: &str,
    max_file_size: u64,
    remaining_bytes: &mut u64,
    cancellation: &Cancellation,
) -> Result<Value, CliError> {
    let target_uses_worktree = context_target_uses_worktree(file, at)?;
    let (path_field, blob_field, mode_field) = match side {
        FileSide::Old => ("old_path", "old_blob_id", "old_mode"),
        FileSide::New => ("new_path", "new_blob_id", "new_mode"),
        _ => return Err(CliError::Usage("unsupported context side".to_owned())),
    };
    let blob_id = record_string(file, blob_field, at)?;
    if !mode_has_file_context(record_u32(file, mode_field, at)?) {
        return Ok(Value::Null);
    }
    let encoded_path_field = format!("{path_field}_base64");
    let path_is_absent = file.get(path_field).is_none_or(Value::is_null)
        && file.get(&encoded_path_field).is_none_or(Value::is_null);
    let request =
        if path_is_absent && !blob_id.is_empty() && blob_id.bytes().all(|byte| byte == b'0') {
            FileContextRequest::absent(blob_id, side, range)
        } else if side == FileSide::New && target_uses_worktree {
            let path = record_path(file, path_field, at)?.ok_or_else(|| {
                CliError::Usage(format!(
                    "{at}.{path_field} is required for a working-tree context"
                ))
            })?;
            FileContextRequest::worktree(path, blob_id, side, range)
        } else {
            FileContextRequest::blob(blob_id, side, range)
        };
    load_context_value(git, request, max_file_size, remaining_bytes, cancellation)
}

fn context_target_uses_worktree(file: &Value, at: &str) -> Result<bool, CliError> {
    let target = record_string(file, "target", at)?;
    match target.as_str() {
        "unstaged" | "revision_against_worktree" => Ok(true),
        "staged" | "commit" | "revisions" | "branch_against_base" => Ok(false),
        _ => Err(CliError::Usage(format!(
            "{at}.target is an unsupported diff target \"{target}\""
        ))),
    }
}

fn load_diff_context_value(
    git: &GitService,
    file: &FileDiff,
    request: FileContextRequest,
    max_file_size: u64,
    remaining_bytes: &mut u64,
    cancellation: &Cancellation,
) -> Result<Value, CliError> {
    let mode = match request.side {
        FileSide::Old => file.old_mode,
        FileSide::New => file.new_mode,
        _ => return Err(CliError::Usage("unsupported context side".to_owned())),
    };
    if !mode_has_file_context(mode) {
        return Ok(Value::Null);
    }
    load_context_value(git, request, max_file_size, remaining_bytes, cancellation)
}

fn load_context_value(
    git: &GitService,
    mut request: FileContextRequest,
    max_file_size: u64,
    remaining_bytes: &mut u64,
    cancellation: &Cancellation,
) -> Result<Value, CliError> {
    if cancellation.is_cancelled() {
        return Err(GitError::Cancelled.into());
    }
    request.max_file_size = max_file_size;
    request.max_total_bytes = *remaining_bytes;
    let response = git.file_context(&request)?;
    if cancellation.is_cancelled() {
        return Err(GitError::Cancelled.into());
    }
    let returned_bytes = response
        .lines
        .iter()
        .map(|line| line.content.len() as u64)
        .sum::<u64>();
    *remaining_bytes = remaining_bytes.saturating_sub(returned_bytes);
    Ok(file_context_value(&response))
}

fn file_diff_value(file: &FileDiff, include_intra_line: bool) -> Value {
    let (old_path, old_path_is_lossy) = optional_wire_path(file.old_path.as_deref());
    let (new_path, new_path_is_lossy) = optional_wire_path(file.new_path.as_deref());
    json!({
        "target": diff_target_name(&file.target),
        "change": file_change_name(file.change),
        "old_path": old_path,
        "old_path_is_lossy": old_path_is_lossy,
        "old_path_base64": encoded_path(file.old_path.as_deref()),
        "new_path": new_path,
        "new_path_is_lossy": new_path_is_lossy,
        "new_path_base64": encoded_path(file.new_path.as_deref()),
        "old_blob_id": file.old_blob_id,
        "new_blob_id": file.new_blob_id,
        "old_mode": file.old_mode,
        "new_mode": file.new_mode,
        "context_lines": file.context_lines,
        "whitespace": whitespace_value(file.whitespace),
        "old_size": file.old_size,
        "new_size": file.new_size,
        "binary": file.binary,
        "omission": file.omission.as_ref().map_or(Value::Null, diff_omission_value),
        // Replaced with this file's attribution when `--provenance` was given.
        // Null means it was not asked for, never that nothing produced the
        // file: that answer is a named gap inside the object.
        "provenance": Value::Null,
        "hunks": file
            .hunks
            .iter()
            .map(|hunk| hunk_value(hunk, include_intra_line))
            .collect::<Vec<_>>(),
    })
}

/// Attributes one diff response, one record per requested target.
///
/// Each target is asked only about the paths its own file records name, so the
/// walk is bounded by the review in front of the caller rather than by
/// everything the range touched.
/// A failed attribution is degraded to a named reason rather than propagated:
/// the diff beside it already succeeded, and losing it over an advisory
/// annotation would make provenance authoritative for whether `git diff`
/// works. A branch force-updated between the diff and this walk is the ordinary
/// way that happens. Cancellation is the exception and still ends the command,
/// because a cancelled read is the caller's own decision about all of it.
fn resolve_diff_provenance(
    git: &GitService,
    targets: &[DiffTarget],
    files: &[FileDiff],
    max_commits: usize,
    cancellation: &Cancellation,
) -> Result<Vec<Result<ChangeProvenance, GitError>>, CliError> {
    targets
        .iter()
        .map(|target| {
            let mut seen = std::collections::HashSet::new();
            let mut paths: Vec<PathBuf> = Vec::new();
            for file in files.iter().filter(|file| file.target == *target) {
                let Some(path) = file.new_path.as_ref().or(file.old_path.as_ref()) else {
                    continue;
                };
                if seen.insert(path.as_path()) {
                    paths.push(path.clone());
                }
            }
            let options = ProvenanceOptions::default()
                .with_paths(paths)
                .with_max_commits(max_commits);
            match git.provenance(target, &options, cancellation) {
                Err(GitError::Cancelled) => Err(CliError::from(GitError::Cancelled)),
                outcome => Ok(outcome),
            }
        })
        .collect()
}

/// One attribution entry per file record, in the same order.
///
/// The entry indexes into the response's `provenance` array rather than
/// repeating a commit per file: a forty-file review of one commit carries that
/// commit once.
fn file_provenance_values(
    targets: &[DiffTarget],
    files: &[FileDiff],
    records: &[Result<ChangeProvenance, GitError>],
) -> Vec<Value> {
    let by_path = records
        .iter()
        .map(|record| {
            record
                .iter()
                .flat_map(|record| &record.files)
                .map(|file| (file.path.as_path(), file))
                .collect::<std::collections::HashMap<_, _>>()
        })
        .collect::<Vec<_>>();

    files
        .iter()
        .map(|file| {
            let Some(index) = targets.iter().position(|target| *target == file.target) else {
                return Value::Null;
            };
            let path = file.new_path.as_deref().or(file.old_path.as_deref());
            path.and_then(|path| by_path[index].get(path))
                .map_or(Value::Null, |entry| file_provenance_value(index, entry))
        })
        .collect()
}

fn file_provenance_value(target_index: usize, file: &FileProvenance) -> Value {
    json!({
        "target_index": target_index,
        "commits": file.commits,
        "producers": file.producers,
        // Present exactly when `commits` is empty, and it is the answer rather
        // than the absence of one.
        "gap": file.gap.map_or(Value::Null, provenance_gap_value),
    })
}

/// One target's attribution, or the named reason it has none.
///
/// `unavailable` is null on every ordinary answer, so a consumer that ignores it
/// reads an empty block rather than a wrong one.
fn change_provenance_value(
    record: &Result<ChangeProvenance, GitError>,
    target: &DiffTarget,
) -> Value {
    let provenance = match record {
        Ok(provenance) => provenance,
        Err(error) => {
            return json!({
                "target": diff_target_value(target),
                "range": Value::Null,
                "producers": Vec::<Value>::new(),
                "commits": Vec::<Value>::new(),
                "walked_commits": 0,
                "skipped_merges": 0,
                "truncation": Value::Null,
                "unavailable": {
                    "kind": error.kind(),
                    "message": single_line(&error.to_string()),
                },
            });
        }
    };
    json!({
        "target": diff_target_value(&provenance.target),
        "range": provenance
            .range
            .as_ref()
            .map_or(Value::Null, provenance_range_value),
        "producers": provenance
            .producers
            .iter()
            .map(producer_value)
            .collect::<Vec<_>>(),
        "commits": provenance
            .commits
            .iter()
            .map(commit_attribution_value)
            .collect::<Vec<_>>(),
        "walked_commits": provenance.walked_commits,
        "skipped_merges": provenance.skipped_merges,
        "truncation": provenance
            .truncation
            .map_or(Value::Null, provenance_truncation_value),
        "unavailable": Value::Null,
    })
}

fn provenance_range_value(range: &ProvenanceRange) -> Value {
    json!({
        "head": range.head.to_string(),
        "base": range.base.map(|id| id.to_string()),
        "head_revision": range.head_revision,
        // Null here, and populated by a front end that resolved a branch to an
        // object id itself and still wants the name read for its conventions.
        "head_reference": range.head_reference,
        // Null unless the head reference follows the `agent/<slug>`
        // convention. It describes the branch, never one commit.
        "agent_slug": range.agent_slug,
    })
}

fn producer_value(producer: &Producer) -> Value {
    let (name, name_encoding) = encoded_bytes(&producer.name);
    let (email, email_encoding) = encoded_bytes(&producer.email);
    json!({
        "kind": producer.kind.name(),
        "name": name,
        "name_encoding": name_encoding,
        "email": email,
        "email_encoding": email_encoding,
    })
}

fn commit_attribution_value(commit: &CommitAttribution) -> Value {
    let (summary, summary_encoding) = encoded_bytes(&commit.summary);
    json!({
        "id": commit.id.to_string(),
        "author": commit_signature_value(&commit.author),
        "committer": commit_signature_value(&commit.committer),
        "summary": summary,
        "summary_encoding": summary_encoding,
        "producers": commit.producers,
    })
}

fn provenance_gap_value(gap: ProvenanceGap) -> Value {
    match gap {
        ProvenanceGap::CommitBudgetExhausted { limit } => {
            json!({ "kind": gap.name(), "limit": limit })
        }
        _ => json!({ "kind": gap.name() }),
    }
}

fn provenance_truncation_value(truncation: ProvenanceTruncation) -> Value {
    match truncation {
        ProvenanceTruncation::CommitBudgetExhausted { limit } => {
            json!({ "kind": truncation.name(), "limit": limit })
        }
        _ => json!({ "kind": truncation.name() }),
    }
}

/// A path and its lossy flag, both null when the side has no path at all.
///
/// The flag is null rather than false so the two questions stay separable: a
/// consumer can tell "there is no path here" from "there is a path and it
/// survived the wire intact" without consulting a second field.
fn optional_wire_path(path: Option<&Path>) -> (Value, Value) {
    path.map_or((Value::Null, Value::Null), |path| {
        let (path, is_lossy) = wire_path(path);
        (json!(path), json!(is_lossy))
    })
}

/// The exact path bytes, so a lossy wire string is never the only spelling.
///
/// Hunk content is Base64 whenever it is not valid UTF-8, and a path deserves
/// the same treatment: without this a file whose name is not UTF-8 could be
/// listed by `git diff` and then never named back to `git stage --hunk`.
fn encoded_path(path: Option<&Path>) -> Value {
    path.and_then(path_bytes)
        .map_or(Value::Null, |bytes| json!(BASE64.encode(bytes)))
}

/// The whitespace handling a diff was computed under.
///
/// An object rather than one string because the two settings answer different
/// questions and a consumer may care about only one of them. `mode` uses the
/// same spellings [`WhitespaceMode::name`] publishes, so the value a file
/// record carries is exactly the value `git stage --hunk-selection` reads back.
fn whitespace_value(whitespace: Whitespace) -> Value {
    json!({
        "mode": whitespace.mode.name(),
        "ignore_blank_lines": whitespace.ignore_blank_lines,
    })
}

/// Reads the whitespace record a diff file or flat selection carries.
///
/// A record without one predates the setting and means exact: this is the
/// additive rule every optional wire field here follows. An unknown mode is
/// refused rather than folded into exact, because a build that cannot tell how
/// a hunk was computed must not stage from it.
fn record_whitespace(record: &Value, at: &str) -> Result<Whitespace, CliError> {
    let Some(value) = record.get("whitespace") else {
        return Ok(Whitespace::EXACT);
    };
    if value.is_null() {
        return Ok(Whitespace::EXACT);
    }
    let object = value
        .as_object()
        .ok_or_else(|| CliError::Usage(format!("{at}.whitespace is not an object")))?;
    // A mistyped `mode` is refused rather than read through `as_str`, which
    // would collapse "absent" and "present but not a string" into the same arm
    // and hand a wrong-typed record the exact default. Absent means an older
    // producer; a JSON array means a producer that is wrong about something,
    // and staging on its say-so is what this whole guard exists to avoid.
    let mode = match object.get("mode") {
        None | Some(Value::Null) => WhitespaceMode::Exact,
        Some(Value::String(mode)) => match mode.as_str() {
            "exact" => WhitespaceMode::Exact,
            "ignore_eol" => WhitespaceMode::IgnoreEol,
            "ignore_change" => WhitespaceMode::IgnoreChange,
            "ignore_all" => WhitespaceMode::IgnoreAll,
            other => {
                return Err(CliError::Usage(format!(
                    "{at}.whitespace.mode \"{other}\" is not a whitespace mode this build knows"
                )));
            }
        },
        Some(_) => {
            return Err(CliError::Usage(format!(
                "{at}.whitespace.mode is not a string"
            )));
        }
    };
    let ignore_blank_lines = match object.get("ignore_blank_lines") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => {
            return Err(CliError::Usage(format!(
                "{at}.whitespace.ignore_blank_lines is not a boolean"
            )));
        }
    };
    Ok(Whitespace {
        mode,
        ignore_blank_lines,
    })
}

fn diff_omission_value(omission: &DiffOmission) -> Value {
    match omission {
        DiffOmission::FileTooLarge { limit } => json!({
            "kind": "file_too_large",
            "limit": limit,
        }),
        DiffOmission::Unmerged => json!({ "kind": "unmerged" }),
        DiffOmission::ContentBudgetExhausted { limit } => json!({
            "kind": "content_budget_exhausted",
            "limit": limit,
        }),
        DiffOmission::FileBudgetExhausted { limit } => json!({
            "kind": "file_budget_exhausted",
            "limit": limit,
        }),
        DiffOmission::Unrepresentable { detail } => json!({
            "kind": "unrepresentable",
            "detail": detail,
        }),
        _ => json!({ "kind": "unknown" }),
    }
}

fn hunk_value(hunk: &Hunk, include_intra_line: bool) -> Value {
    let (header, header_encoding) = encoded_bytes(&hunk.header);
    let mut value = json!({
        "old_start": hunk.old_start,
        "old_lines": hunk.old_lines,
        "new_start": hunk.new_start,
        "new_lines": hunk.new_lines,
        "header": header,
        "header_encoding": header_encoding,
        "lines": hunk
            .lines
            .iter()
            .map(|line| diff_line_value(line, include_intra_line))
            .collect::<Vec<_>>(),
    });
    if include_intra_line {
        value
            .as_object_mut()
            .expect("a hunk projection is an object")
            .insert(
                "intra_line_degradation".to_owned(),
                hunk.intra_line_degradation
                    .as_ref()
                    .map_or(Value::Null, intra_line_degradation_value),
            );
    }
    value
}

fn diff_line_value(line: &DiffLine, include_intra_line: bool) -> Value {
    let (content, content_encoding) = encoded_bytes(&line.content);
    let mut value = json!({
        "kind": diff_line_kind_name(line.kind),
        "old_line_number": line.old_line_number,
        "new_line_number": line.new_line_number,
        "content": content,
        "content_encoding": content_encoding,
    });
    if include_intra_line {
        let ranges = line.intra_line_ranges.as_ref().map(|ranges| {
            ranges
                .iter()
                .map(|range| json!({ "start": range.start, "end": range.end }))
                .collect::<Vec<_>>()
        });
        let object = value
            .as_object_mut()
            .expect("a diff line projection is an object");
        object.insert(
            "paired_line_index".to_owned(),
            json!(line.paired_line_index),
        );
        object.insert("intra_line_ranges".to_owned(), json!(ranges));
    }
    value
}

fn intra_line_degradation_value(degradation: &IntraLineDegradation) -> Value {
    match degradation {
        IntraLineDegradation::LineTooLong { limit } => {
            json!({ "kind": "line_too_long", "limit": limit })
        }
        IntraLineDegradation::PairingTooLarge { limit } => {
            json!({ "kind": "pairing_too_large", "limit": limit })
        }
        _ => json!({ "kind": "unknown" }),
    }
}

fn file_context_value(response: &FileContextResponse) -> Value {
    json!({
        "kind": "file_context",
        "blob_id": response.blob_id,
        "side": file_side_name(response.side),
        "range": file_context_range_value(&response.range),
        "byte_size": response.byte_size,
        "total_lines": response.total_lines,
        "start_line": response.start_line,
        "lines": response
            .lines
            .iter()
            .map(|line| diff_line_value(line, false))
            .collect::<Vec<_>>(),
        "omission": response
            .omission
            .as_ref()
            .map_or(Value::Null, file_context_omission_value),
    })
}

const fn file_side_name(side: FileSide) -> &'static str {
    match side {
        FileSide::Old => "old",
        FileSide::New => "new",
        _ => "unknown",
    }
}

fn file_context_range_value(range: &FileContextRange) -> Value {
    match range {
        FileContextRange::FullFile => json!({ "kind": "full_file" }),
        FileContextRange::Hunk {
            start_line,
            line_count,
            lines_before,
            lines_after,
        } => json!({
            "kind": "hunk",
            "start_line": start_line,
            "line_count": line_count,
            "lines_before": lines_before,
            "lines_after": lines_after,
        }),
        _ => json!({ "kind": "unknown" }),
    }
}

fn file_context_omission_value(omission: &FileContextOmission) -> Value {
    match omission {
        FileContextOmission::FileTooLarge { limit } => {
            json!({ "kind": "file_too_large", "limit": limit })
        }
        FileContextOmission::ContentBudgetExhausted { limit } => {
            json!({ "kind": "content_budget_exhausted", "limit": limit })
        }
        _ => json!({ "kind": "unknown" }),
    }
}

fn encoded_bytes(bytes: &[u8]) -> (String, &'static str) {
    match std::str::from_utf8(bytes) {
        Ok(text) => (text.to_owned(), "utf8"),
        Err(_) => (BASE64.encode(bytes), "base64"),
    }
}

fn encode_log_cursor(cursor: &LogCursor) -> Result<String, CliError> {
    serde_json::to_vec(cursor)
        .map(|bytes| URL_BASE64.encode(bytes))
        .map_err(|error| CliError::WireProjection(error.to_string()))
}

fn decode_log_cursor(token: &str) -> Result<LogCursor, CliError> {
    let bytes = URL_BASE64.decode(token).map_err(|_| {
        CliError::Usage("--cursor is not a valid Harkness log cursor token".to_owned())
    })?;
    serde_json::from_slice(&bytes).map_err(|_| {
        CliError::Usage("--cursor is not a valid Harkness log cursor token".to_owned())
    })
}

fn commit_value(commit: &CommitInfo) -> Value {
    let (summary, summary_encoding) = encoded_bytes(&commit.summary);
    let (message, message_encoding) = encoded_bytes(&commit.message);
    json!({
        "kind": "commit",
        "id": commit.id.to_string(),
        "parent_ids": commit
            .parent_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        "author": commit_signature_value(&commit.author),
        "committer": commit_signature_value(&commit.committer),
        "summary": summary,
        "summary_encoding": summary_encoding,
        "message": message,
        "message_encoding": message_encoding,
    })
}

fn log_range_value(range: &LogRange) -> Value {
    match range {
        LogRange::Revision { revision } => json!({
            "kind": "revision",
            "revision": revision,
        }),
        LogRange::Excluding {
            reachable_from,
            not_from,
        } => json!({
            "kind": "excluding",
            "reachable_from": reachable_from,
            "not_from": not_from,
        }),
        LogRange::BranchAgainstBase {
            branch,
            base_branch,
        } => json!({
            "kind": "branch_against_base",
            "branch": branch,
            "base_branch": base_branch,
        }),
        _ => json!({ "kind": "unknown" }),
    }
}

fn commit_signature_value(signature: &CommitSignature) -> Value {
    let (name, name_encoding) = encoded_bytes(&signature.name);
    let (email, email_encoding) = encoded_bytes(&signature.email);
    json!({
        "name": name,
        "name_encoding": name_encoding,
        "email": email,
        "email_encoding": email_encoding,
        "time": {
            "seconds": signature.time.seconds(),
            "offset_minutes": signature.time.offset_minutes(),
            "sign": signature.time.sign().to_string(),
        },
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

fn status_entry_value(entry: &StatusEntry) -> Value {
    let (path, path_is_lossy) = wire_path(&entry.path);
    let (rename_source, rename_source_is_lossy) =
        optional_wire_path(entry.rename_source.as_deref());
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

/// One line per changed file, plus a counted header.
///
/// The JSON projection is the contract, but a human running this still needs to
/// see which paths changed and which of them came back without content; a pair
/// of totals alone answers no question worth asking.
fn diff_summary_line(
    files: &[FileDiff],
    targets: &[DiffTarget],
    provenance: Option<&[Result<ChangeProvenance, GitError>]>,
) -> String {
    let index_targets = targets
        .iter()
        .all(|target| matches!(target, DiffTarget::Staged | DiffTarget::Unstaged));
    let mut lines = if index_targets {
        let staged = files
            .iter()
            .filter(|file| matches!(file.target, DiffTarget::Staged))
            .count();
        vec![format!(
            "{staged} staged, {} unstaged",
            files.len() - staged
        )]
    } else {
        vec![format!(
            "{} changed file{}",
            files.len(),
            if files.len() == 1 { "" } else { "s" }
        )]
    };
    lines.extend(files.iter().map(|file| {
        let path = display_diff_path(file);
        let content = match (&file.omission, file.binary) {
            (Some(omission), _) => format!("\tno content ({})", omission_reason(omission)),
            (None, true) => "\tno content (binary)".to_owned(),
            (None, false) => format!("\t{} hunks", file.hunks.len()),
        };
        let attribution = provenance.map_or_else(String::new, |records| {
            format!("\t{}", provenance_summary(targets, records, file))
        });
        format!(
            "{}\t{}\t{path}{content}{attribution}",
            diff_target_name(&file.target),
            file_change_name(file.change),
        )
    }));
    lines.join("\n")
}

/// One file's attribution as a single column.
///
/// Producer names come out of commit objects, which is repository content and
/// therefore untrusted text: every one goes through [`single_line`] so a name
/// carrying a tab or a newline cannot forge a column in this table. An
/// unattributed file says so by name and is never left blank.
fn provenance_summary(
    targets: &[DiffTarget],
    records: &[Result<ChangeProvenance, GitError>],
    file: &FileDiff,
) -> String {
    let Some(index) = targets.iter().position(|target| *target == file.target) else {
        return "unknown".to_owned();
    };
    let Ok(record) = &records[index] else {
        return "unknown (unavailable)".to_owned();
    };
    let path = file.new_path.as_deref().or(file.old_path.as_deref());
    let Some(entry) = path.and_then(|path| {
        record
            .files
            .iter()
            .find(|candidate| candidate.path.as_path() == path)
    }) else {
        return "unknown".to_owned();
    };
    if !entry.is_attributed() {
        return format!(
            "unknown ({})",
            entry.gap.map_or("unknown", ProvenanceGap::name)
        );
    }
    let names = entry
        .producers
        .iter()
        .map(|producer| producer_display_name(&record.producers[*producer]))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{} commit{} by {names}",
        entry.commits.len(),
        if entry.commits.len() == 1 { "" } else { "s" },
    )
}

/// What to call one producer in the human table.
///
/// A `Co-Authored-By` trailer may carry an address and no name, which would
/// otherwise print as a dangling separator between two commas. Both halves go
/// through [`single_line`] because both are repository content.
fn producer_display_name(producer: &Producer) -> String {
    let name = single_line(&String::from_utf8_lossy(&producer.name));
    if name.is_empty() {
        single_line(&String::from_utf8_lossy(&producer.email))
    } else {
        name
    }
}

fn log_page_line(commits: &[CommitInfo], has_more: bool) -> String {
    let mut lines = commits
        .iter()
        .map(|commit| {
            let id = commit.id.to_string();
            format!(
                "{}\t{}",
                &id[..12],
                single_line(&String::from_utf8_lossy(&commit.summary))
            )
        })
        .collect::<Vec<_>>();
    if has_more {
        lines.push("more commits available; use --json to obtain next_cursor".to_owned());
    }
    lines.join("\n")
}

fn display_diff_path(file: &FileDiff) -> String {
    match (file.old_path.as_deref(), file.new_path.as_deref()) {
        (Some(old), Some(new)) if old != new => {
            format!("{} -> {}", wire_path(old).0, wire_path(new).0)
        }
        (_, Some(path)) | (Some(path), None) => wire_path(path).0,
        (None, None) => "(unnamed)".to_owned(),
    }
}

const fn omission_reason(omission: &DiffOmission) -> &'static str {
    match omission {
        DiffOmission::FileTooLarge { .. } => "too large",
        DiffOmission::Unmerged => "unmerged",
        DiffOmission::ContentBudgetExhausted { .. } => "content budget spent",
        DiffOmission::FileBudgetExhausted { .. } => "file budget spent",
        DiffOmission::Unrepresentable { .. } => "unrepresentable",
        _ => "omitted",
    }
}

fn hunk_outcome_line(verb: &str, count: usize) -> String {
    format!("{verb} {count} hunk{}", if count == 1 { "" } else { "s" })
}

/// Counts the hunk identities the execution pipeline will retain after
/// duplicate selections are merged.
fn distinct_hunk_selection_count(selections: &[HunkSelection]) -> usize {
    selections
        .iter()
        .enumerate()
        .filter(|(index, selection)| {
            !selections[..*index]
                .iter()
                .any(|other| hunk_selections_share_hunk(other, selection))
        })
        .count()
}

fn hunk_selections_share_hunk(left: &HunkSelection, right: &HunkSelection) -> bool {
    left.old_path == right.old_path
        && left.new_path == right.new_path
        && left.old_blob_id == right.old_blob_id
        && left.new_blob_id == right.new_blob_id
        && left.old_start == right.old_start
        && left.old_lines == right.old_lines
        && left.new_start == right.new_start
        && left.new_lines == right.new_lines
}

/// Counts distinct selected lines and the distinct enclosing hunks that will
/// contain them after execution merges the batch.
fn distinct_line_selection_counts(selections: &[LineSelection]) -> (usize, usize) {
    let lines = selections
        .iter()
        .enumerate()
        .filter(|(index, selection)| {
            !selections[..*index]
                .iter()
                .any(|other| line_selections_share_line(other, selection))
        })
        .count();
    let hunks = selections
        .iter()
        .enumerate()
        .filter(|(index, selection)| {
            !selections[..*index]
                .iter()
                .any(|other| line_selections_share_hunk(other, selection))
        })
        .count();
    (lines, hunks)
}

fn line_selections_share_line(left: &LineSelection, right: &LineSelection) -> bool {
    line_selections_share_hunk(left, right)
        && left.old_line_number == right.old_line_number
        && left.new_line_number == right.new_line_number
}

fn line_selections_share_hunk(left: &LineSelection, right: &LineSelection) -> bool {
    left.old_path == right.old_path
        && left.new_path == right.new_path
        && left.old_blob_id == right.old_blob_id
        && left.new_blob_id == right.new_blob_id
        && left.old_start == right.old_start
        && left.old_lines == right.old_lines
        && left.new_start == right.new_start
        && left.new_lines == right.new_lines
}

fn discard_description_value(description: &harkness_git::DiscardDescription) -> Value {
    let (operation, source, hunks, lines) = match description.operation() {
        harkness_git::DiscardOperation::RestoreTracked { source } => (
            "restore_tracked",
            match source {
                TrackedRestoreSource::Index => "index",
                TrackedRestoreSource::Head => "head",
                _ => "unknown",
            },
            Value::Null,
            Value::Null,
        ),
        harkness_git::DiscardOperation::RestoreTrackedHunks { hunks } => {
            ("restore_tracked_hunks", "index", json!(hunks), Value::Null)
        }
        harkness_git::DiscardOperation::RestoreTrackedLines { lines, hunks } => {
            ("restore_tracked_lines", "index", json!(hunks), json!(lines))
        }
        harkness_git::DiscardOperation::DeleteUntracked => {
            ("delete_untracked", "none", Value::Null, Value::Null)
        }
        _ => ("unknown", "unknown", Value::Null, Value::Null),
    };
    let paths = description
        .paths()
        .iter()
        .map(|path| wire_path(path).0)
        .collect::<Vec<_>>();
    json!({
        "operation": operation,
        "source": source,
        "tracked_files": description.tracked_files(),
        "untracked_files": description.untracked_files(),
        "hunks": hunks,
        "lines": lines,
        "paths": paths,
        "paths_are_lossy": description.paths().iter().any(|path| wire_path(path).1),
        "recoverability": match description.recoverability() {
            harkness_git::DiscardRecoverability::GitRecordedBaseline => "git_recorded_baseline",
            harkness_git::DiscardRecoverability::Unrecoverable => "unrecoverable",
            _ => "unknown",
        },
    })
}

fn discard_confirmation_message(description: &harkness_git::DiscardDescription) -> String {
    let paths = description
        .paths()
        .iter()
        .map(|path| format!("'{}'", path.display()))
        .collect::<Vec<_>>()
        .join(", ");
    let action = match description.operation() {
        harkness_git::DiscardOperation::RestoreTracked {
            source: TrackedRestoreSource::Index,
        } => format!(
            "restore {} tracked file(s) from the index: {paths}; the discarded unstaged edits cannot be recovered, while the staged baseline remains in Git",
            description.tracked_files()
        ),
        harkness_git::DiscardOperation::RestoreTracked {
            source: TrackedRestoreSource::Head,
        } => format!(
            "restore {} tracked file(s) from HEAD: {paths}; staged and unstaged edits will be lost, while HEAD remains in Git",
            description.tracked_files()
        ),
        harkness_git::DiscardOperation::RestoreTrackedHunks { hunks } => format!(
            "discard {hunks} tracked hunk(s) in {paths}; those uncommitted edits cannot be recovered, while the index baseline remains in Git"
        ),
        harkness_git::DiscardOperation::RestoreTrackedLines { lines, .. } => format!(
            "discard {lines} tracked line(s) in {paths}; those uncommitted edits cannot be recovered, while the index baseline remains in Git"
        ),
        harkness_git::DiscardOperation::DeleteUntracked => format!(
            "permanently delete {} untracked file(s): {paths}; Git has no copy and this cannot be undone",
            description.untracked_files()
        ),
        _ => format!("discard changes in {paths}"),
    };
    format!("refusing to {action}; pass --yes to confirm")
}

fn discard_outcome_line(description: &harkness_git::DiscardDescription) -> String {
    match description.operation() {
        harkness_git::DiscardOperation::RestoreTracked { source } => format!(
            "restored {} tracked file(s) from {}",
            description.tracked_files(),
            match source {
                TrackedRestoreSource::Index => "the index",
                TrackedRestoreSource::Head => "HEAD",
                _ => "the selected boundary",
            }
        ),
        harkness_git::DiscardOperation::RestoreTrackedHunks { hunks } => {
            format!("discarded {hunks} tracked hunk(s)")
        }
        harkness_git::DiscardOperation::RestoreTrackedLines { lines, .. } => {
            format!("discarded {lines} tracked line(s)")
        }
        harkness_git::DiscardOperation::DeleteUntracked => format!(
            "deleted {} untracked file(s) permanently",
            description.untracked_files()
        ),
        _ => "discarded changes".to_owned(),
    }
}

fn line_outcome_line(verb: &str, count: usize) -> String {
    format!("{verb} {count} line{}", if count == 1 { "" } else { "s" })
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

const fn diff_target_name(target: &DiffTarget) -> &'static str {
    match target {
        DiffTarget::Staged => "staged",
        DiffTarget::Unstaged => "unstaged",
        DiffTarget::Commit { .. } => "commit",
        DiffTarget::Revisions { .. } => "revisions",
        DiffTarget::RevisionAgainstWorktree { .. } => "revision_against_worktree",
        DiffTarget::BranchAgainstBase { .. } => "branch_against_base",
        _ => "unknown",
    }
}

fn diff_target_details(target: &DiffTarget) -> Option<Value> {
    match target {
        DiffTarget::Staged | DiffTarget::Unstaged => None,
        DiffTarget::Commit { revision, parent } => Some(json!({
            "kind": "commit",
            "revision": revision,
            "parent": parent,
        })),
        DiffTarget::Revisions {
            old_revision,
            new_revision,
        } => Some(json!({
            "kind": "revisions",
            "old_revision": old_revision,
            "new_revision": new_revision,
        })),
        DiffTarget::RevisionAgainstWorktree { revision } => Some(json!({
            "kind": "revision_against_worktree",
            "revision": revision,
        })),
        DiffTarget::BranchAgainstBase {
            branch,
            base_branch,
        } => Some(json!({
            "kind": "branch_against_base",
            "branch": branch,
            "base_branch": base_branch,
        })),
        _ => Some(json!({ "kind": "unknown" })),
    }
}

fn diff_target_value(target: &DiffTarget) -> Value {
    diff_target_details(target).unwrap_or_else(|| json!({ "kind": diff_target_name(target) }))
}

/// Resolves each named revision once for a whole coverage pass.
///
/// A recorded check is compared against the 40-hex commit a target names, and
/// every recorded check asks about the same handful of targets. Resolving per
/// (check, target) pair meant up to `checks x targets` Git invocations for two
/// or three distinct revisions.
struct RevisionCache<'a> {
    git: &'a GitService,
    resolved: HashMap<String, String>,
}

impl<'a> RevisionCache<'a> {
    fn new(git: &'a GitService) -> Self {
        Self {
            git,
            resolved: HashMap::new(),
        }
    }

    fn resolve(&mut self, revision: &str) -> Result<String, CliError> {
        if let Some(resolved) = self.resolved.get(revision) {
            return Ok(resolved.clone());
        }
        let resolved = self.git.resolve_revision(revision)?.to_string();
        self.resolved.insert(revision.to_owned(), resolved.clone());
        Ok(resolved)
    }
}

fn check_covers_diff_target(
    revisions: &mut RevisionCache<'_>,
    check: &CheckSummary,
    target: &DiffTarget,
) -> Result<bool, CliError> {
    if !check.definition_current {
        return Ok(false);
    }
    let live_state_is_current = matches!(
        check.freshness,
        harkness_runtime::check::CheckFreshness::Current
    );
    match target {
        DiffTarget::Unstaged | DiffTarget::RevisionAgainstWorktree { .. } => {
            Ok(live_state_is_current)
        }
        DiffTarget::Staged => {
            Ok(live_state_is_current && check.workspace_matches_index == Some(true))
        }
        DiffTarget::Commit { revision, .. } => check_covers_commit(revisions, check, revision),
        DiffTarget::Revisions { new_revision, .. } => {
            check_covers_commit(revisions, check, new_revision)
        }
        DiffTarget::BranchAgainstBase { branch, .. } => {
            check_covers_commit(revisions, check, branch)
        }
        _ => Ok(false),
    }
}

fn check_covers_commit(
    revisions: &mut RevisionCache<'_>,
    check: &CheckSummary,
    revision: &str,
) -> Result<bool, CliError> {
    let resolved = revisions.resolve(revision)?;
    Ok(check.workspace_clean == Some(true)
        && check.state_head.as_deref() == Some(resolved.as_str()))
}

const fn diff_line_kind_name(kind: DiffLineKind) -> &'static str {
    match kind {
        DiffLineKind::Context => "context",
        DiffLineKind::Addition => "addition",
        DiffLineKind::Deletion => "deletion",
        DiffLineKind::BothEofNoNewline => "both_eof_no_newline",
        DiffLineKind::OldEofNoNewline => "old_eof_no_newline",
        DiffLineKind::NewEofNoNewline => "new_eof_no_newline",
        _ => "unknown",
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
    // A lock reason is caller-supplied text that Git stores verbatim apart
    // from trimming, so collapse its whitespace before it reaches a
    // tab-separated line that a reader splits on.
    let state = if worktree.locked {
        worktree.lock_reason.as_deref().map_or_else(
            || "locked".to_owned(),
            |reason| format!("locked: {}", single_line(reason)),
        )
    } else if worktree.prunable {
        "prunable".to_owned()
    } else {
        "active".to_owned()
    };
    format!(
        "{id}\t{branch}\t{}\t{owner}\t{state}",
        worktree.root.display()
    )
}

/// Collapses every run of whitespace so untrusted text cannot forge a column
/// break or a new row in tab-separated output.
fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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
        // Null when the worktree is unlocked and when Git recorded a lock
        // without a reason; `locked` remains the authoritative state.
        "lock_reason": worktree.lock_reason,
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
            "editor": EditorError::KINDS,
            "policy": EXTERNAL_POLICY_DENIAL_KINDS,
            // The coordinator namespace is its own table followed by the
            // store's, because `RuntimeError::Store` delegates its
            // discriminant rather than spelling one of its own.
            "runtime": RuntimeError::KINDS
                .iter()
                .chain(StoreError::KINDS)
                .collect::<Vec<_>>(),
            "tool": InvocationError::kinds(),
        },
        // The category map above names the codes; this names which code each
        // error kind actually reports. Without it a caller has to hardcode the
        // mapping, and a deliberate reclassification looks to that caller like
        // an unannounced break rather than a contract change it can observe.
        "exit_code_by_kind": {
            "cli": kind_exit_codes(CLI_KIND_EXIT_CODES),
            "project": kind_exit_codes(PROJECT_KIND_EXIT_CODES),
            "git": kind_exit_codes(GIT_KIND_EXIT_CODES),
            "editor": kind_exit_codes(EDITOR_KIND_EXIT_CODES),
            "policy": kind_exit_codes(POLICY_KIND_EXIT_CODES),
            "runtime": kind_exit_codes(RUNTIME_KIND_EXIT_CODES),
            "tool": kind_exit_codes(TOOL_KIND_EXIT_CODES),
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
            serde_json::to_string_pretty(&canonical_json(data))
                .unwrap_or_else(|_| "contract unavailable".to_owned()),
        )
    }
}

/// Writes the success envelope, with the payload's object keys in one fixed
/// order.
///
/// The envelope's own fields come from a struct and are in declaration order
/// whatever happens; the payload is a hand-built `Value`, and its key order used
/// to come from `serde_json::Map` being a `BTreeMap`. That map type is a cargo
/// feature any crate in the workspace can turn on for every other one — one
/// does, through `agent-client-protocol-schema` (ADR-0010) — so the order a
/// released `harkness --json` has always emitted is sorted here rather than
/// inherited. Nothing about the contract changes: JSON objects are unordered to
/// a parser, and this is what keeps the bytes the same for anyone who did not
/// read them that way.
///
/// Takes the payload by value because every caller already owns one, and
/// sorting a borrowed tree would mean cloning the whole thing first.
fn emit_success(data: Value) -> io::Result<()> {
    write_json_line(
        &mut io::stdout().lock(),
        &SuccessEnvelope {
            v: ENVELOPE_VERSION,
            r#type: "success",
            ok: true,
            data: &canonical_json(data),
        },
    )
}

fn emit_progress(json_output: bool, message: &str) {
    emit_progress_line(json_output, message, None);
}

/// Emits one persisted run event as a progress line.
///
/// The payload is sorted for the reason [`emit_success`] sorts: a `Value` built
/// by hand no longer inherits a key order from the map type.
fn emit_event_progress(json_output: bool, message: &str, event: Value) {
    let event = canonical_json(event);
    emit_progress_line(json_output, message, Some(&event));
}

fn emit_progress_line(json_output: bool, message: &str, event: Option<&Value>) {
    let output = if json_output {
        write_json_line(
            &mut io::stderr().lock(),
            &ProgressEnvelope {
                v: ENVELOPE_VERSION,
                r#type: "progress",
                message,
                event,
            },
        )
    } else {
        write_line(&mut io::stderr().lock(), message.as_bytes())
    };
    let _ = output;
}

/// Writes the error envelope, sorted for the reason [`emit_success`] is.
fn emit_error(kind: &str, message: &str, details: Value) -> io::Result<()> {
    write_json_line(
        &mut io::stdout().lock(),
        &ErrorEnvelope {
            v: ENVELOPE_VERSION,
            r#type: "error",
            ok: false,
            error: ErrorBody {
                kind,
                message,
                details: &canonical_json(details),
            },
        },
    )
}

/// Writes one envelope and a newline.
///
/// Does no key ordering of its own: a payload's canonical order is applied by
/// whoever built it, in `emit_success`, `emit_error`, and `contract_result`. A
/// new envelope writer owes that call too.
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

/// Classifies a project failure for the published exit-code contract.
///
/// The worktree-destination checks deliberately split across two classes. An
/// occupied destination is a conflict, in the same family as an existing branch
/// name: nothing is wrong with the request, the world is simply already using
/// that place, and the same command succeeds once it is free. Every other
/// destination check is a refusal, because a relative path, a destination
/// inside the project, or one inside the data directory is a request Harkness
/// will never accept no matter what the filesystem does next. Keep that split
/// when adding a variant, and add it to `PROJECT_KIND_EXIT_CODES` too.
fn project_exit_code(error: &ProjectError) -> u8 {
    match error {
        ProjectError::CloneCancelled => EXIT_CANCELLED,
        ProjectError::ProjectSelectorNotFound { .. } | ProjectError::ProjectNotFound(_) => {
            EXIT_NOT_FOUND
        }
        ProjectError::AmbiguousProjectSelector { .. }
        | ProjectError::ParentHasWorktrees { .. }
        | ProjectError::WorktreeDestinationExists { .. } => EXIT_CONFLICT,
        ProjectError::UnsafeManagedRemoval { .. }
        | ProjectError::WorktreeRemovalRequired { .. }
        | ProjectError::UnsafeWorktreeRemoval { .. }
        | ProjectError::WorktreeParentUnsupported { .. }
        | ProjectError::DirtyWorktreeRemoval { .. }
        | ProjectError::UnsafeWorktreeLock { .. }
        | ProjectError::UnsafeWorktreeMove { .. }
        | ProjectError::WorktreeDestinationNotAbsolute { .. }
        | ProjectError::WorktreeDestinationParentUnavailable { .. }
        | ProjectError::WorktreeDestinationInsideProject { .. }
        | ProjectError::WorktreeDestinationContainsProject { .. }
        | ProjectError::WorktreeDestinationInsideDataDirectory { .. }
        | ProjectError::InvalidCheckConfiguration { .. } => EXIT_REFUSED,
        ProjectError::Git(error) => git_exit_code(error),
        ProjectError::Editor(error) => editor_exit_code(error),
        ProjectError::DataDirectoryUnavailable
        | ProjectError::InvalidDirectory { .. }
        | ProjectError::UnreadableDirectory { .. }
        | ProjectError::CatalogRead { .. }
        | ProjectError::CatalogLock { .. }
        | ProjectError::MalformedCatalog { .. }
        | ProjectError::CatalogVersionTooOld { .. }
        | ProjectError::CatalogVersionTooNew { .. }
        | ProjectError::InvalidCatalog { .. }
        | ProjectError::ProjectUnavailable { .. }
        | ProjectError::GitInspection { .. }
        | ProjectError::Persistence { .. }
        | ProjectError::InvalidRemote { .. }
        | ProjectError::GitLaunch { .. }
        | ProjectError::CloneFailed { .. }
        | ProjectError::ManagedRemoval { .. }
        | ProjectError::ManagedRepositoryLock { .. }
        | ProjectError::ManagedRepositoryReconciliation { .. }
        | ProjectError::WorktreeMovedCatalogStale { .. } => EXIT_OPERATION_FAILED,
    }
}

fn editor_exit_code(error: &EditorError) -> u8 {
    match error {
        EditorError::PathOutsideProject { .. } | EditorError::InvalidTemplate { .. } => {
            EXIT_REFUSED
        }
        EditorError::FileUnavailable { .. } => EXIT_NOT_FOUND,
        EditorError::Launch { .. } => EXIT_OPERATION_FAILED,
    }
}

fn git_exit_code(error: &GitError) -> u8 {
    match error {
        GitError::Cancelled => EXIT_CANCELLED,
        GitError::NoSuchBranch { .. }
        | GitError::NotARepository { .. }
        | GitError::RevisionNotFound { .. }
        | GitError::AmbiguousRevision { .. }
        | GitError::BlobNotFound { .. } => EXIT_NOT_FOUND,
        GitError::RepositoryBusy { .. }
        | GitError::NoMergeBase { .. }
        | GitError::BranchAlreadyExists { .. }
        | GitError::BranchCheckedOutInWorktree { .. }
        | GitError::WorktreeAddDestinationExists { .. }
        | GitError::WorktreeAlreadyLocked { .. }
        | GitError::WorktreeNotLocked { .. } => EXIT_CONFLICT,
        GitError::PathOutsideRepository { .. }
        | GitError::RevisionNotCommit { .. }
        | GitError::RevisionNotParent { .. }
        | GitError::InvalidBlobId { .. }
        | GitError::InvalidLogLimit
        | GitError::InvalidLogCursor { .. }
        | GitError::EmptyCommitMessage
        | GitError::EmptyWorktreeLockReason
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
        | GitError::DetachedHead { .. }
        | GitError::WorktreeLocked { .. }
        | GitError::WorktreeMoveAcrossDevices { .. }
        | GitError::UntrackedDiscardRequiresDelete { .. }
        | GitError::TrackedDiscardRequiresRestore { .. }
        | GitError::UnmergedDiscard { .. }
        | GitError::NothingToDiscard { .. }
        | GitError::UntrackedDiscardNotFile { .. }
        | GitError::StaleDiscardSelection
        | GitError::StaleHunkSelection { .. }
        | GitError::WhitespaceInsensitiveSelection { .. }
        | GitError::HiddenWhitespaceChanges { .. }
        | GitError::BinaryHunkSelection { .. }
        | GitError::RenameOnlyHunkSelection { .. }
        | GitError::MetadataOnlyHunkSelection { .. }
        | GitError::UnsupportedHunkChange { .. }
        | GitError::FilteredHunkSelection { .. }
        | GitError::OverlappingHunkSelection { .. }
        | GitError::HunkNotFound { .. }
        | GitError::LineNotFound { .. }
        | GitError::UnrepresentableLineSelection { .. } => EXIT_REFUSED,
        // Enumerated rather than left to the wildcard so the classification of
        // every kind in `GitError::KINDS` is stated here, and a reader adding a
        // variant sees that the published exit-code contract needs a decision.
        // `GitError` is `#[non_exhaustive]`, so the wildcard below cannot be
        // removed; `git_error_kinds_are_classified_for_the_exit_code_contract`
        // is what actually keeps a new kind from defaulting silently.
        GitError::Launch { .. }
        | GitError::Failed { .. }
        | GitError::TimedOut { .. }
        | GitError::Lock { .. }
        | GitError::WorktreeAddDestinationUnavailable { .. }
        | GitError::WorktreeAddCleanup { .. }
        | GitError::InvalidBranchName { .. }
        | GitError::InvalidStartPoint { .. }
        | GitError::NonFastForward { .. }
        | GitError::AuthenticationFailed { .. }
        | GitError::Interrupted { .. }
        | GitError::NoRemote { .. }
        | GitError::Inspection { .. }
        | GitError::DiffContent { .. }
        | GitError::UntrackedDiscardIo { .. }
        | GitError::MalformedDiff { .. }
        | GitError::HunkApplication { .. }
        | GitError::MalformedStatus { .. } => EXIT_OPERATION_FAILED,
        _ => EXIT_OPERATION_FAILED,
    }
}

/// The exit code every stable Git error kind is expected to report, in
/// `GitError::KINDS` order.
///
/// `GitError` is `#[non_exhaustive]`, so [`git_exit_code`] must keep a wildcard
/// arm and a newly added variant cannot fail to compile here. This table is
/// what refuses that silence instead: adding a kind upstream breaks
/// `git_error_kinds_are_classified_for_the_exit_code_contract` until its
/// exit code is stated.
///
/// It is also published by `harkness contract`, so a caller never has to
/// hardcode the mapping and a reclassification is observable rather than a
/// silent change in what an existing script sees.
const GIT_KIND_EXIT_CODES: &[(&str, u8)] = &[
    ("launch", EXIT_OPERATION_FAILED),
    ("failed", EXIT_OPERATION_FAILED),
    ("cancelled", EXIT_CANCELLED),
    ("timed_out", EXIT_OPERATION_FAILED),
    ("repository_busy", EXIT_CONFLICT),
    ("lock", EXIT_OPERATION_FAILED),
    ("not_a_repository", EXIT_NOT_FOUND),
    ("revision_not_found", EXIT_NOT_FOUND),
    ("ambiguous_revision", EXIT_NOT_FOUND),
    ("revision_not_commit", EXIT_REFUSED),
    ("revision_not_parent", EXIT_REFUSED),
    ("no_merge_base", EXIT_CONFLICT),
    ("invalid_log_limit", EXIT_REFUSED),
    ("invalid_log_cursor", EXIT_REFUSED),
    ("path_outside_repository", EXIT_REFUSED),
    ("empty_commit_message", EXIT_REFUSED),
    ("nothing_staged", EXIT_REFUSED),
    ("amend_unborn_branch", EXIT_REFUSED),
    ("invalid_branch_name", EXIT_OPERATION_FAILED),
    ("no_such_branch", EXIT_NOT_FOUND),
    ("branch_already_exists", EXIT_CONFLICT),
    ("invalid_start_point", EXIT_OPERATION_FAILED),
    ("current_branch_deletion", EXIT_REFUSED),
    ("default_branch_deletion", EXIT_REFUSED),
    ("branch_checked_out_in_worktree", EXIT_CONFLICT),
    ("worktree_add_destination_exists", EXIT_CONFLICT),
    (
        "worktree_add_destination_unavailable",
        EXIT_OPERATION_FAILED,
    ),
    ("worktree_add_cleanup", EXIT_OPERATION_FAILED),
    ("worktree_locked", EXIT_REFUSED),
    ("empty_worktree_lock_reason", EXIT_REFUSED),
    ("worktree_already_locked", EXIT_CONFLICT),
    ("worktree_not_locked", EXIT_CONFLICT),
    ("worktree_move_across_devices", EXIT_REFUSED),
    ("unmerged_branch_deletion", EXIT_REFUSED),
    ("non_fast_forward", EXIT_OPERATION_FAILED),
    ("authentication_failed", EXIT_OPERATION_FAILED),
    ("no_upstream", EXIT_REFUSED),
    ("unborn_branch", EXIT_REFUSED),
    ("local_upstream_unsupported", EXIT_REFUSED),
    ("operation_in_progress", EXIT_REFUSED),
    ("interrupted", EXIT_OPERATION_FAILED),
    ("no_remote", EXIT_OPERATION_FAILED),
    ("default_branch_push", EXIT_REFUSED),
    ("default_branch_unknown", EXIT_REFUSED),
    ("detached_head", EXIT_REFUSED),
    ("inspection", EXIT_OPERATION_FAILED),
    ("diff_content", EXIT_OPERATION_FAILED),
    ("stale_discard_selection", EXIT_REFUSED),
    ("untracked_discard_requires_delete", EXIT_REFUSED),
    ("tracked_discard_requires_restore", EXIT_REFUSED),
    ("unmerged_discard", EXIT_REFUSED),
    ("nothing_to_discard", EXIT_REFUSED),
    ("untracked_discard_not_file", EXIT_REFUSED),
    ("untracked_discard_io", EXIT_OPERATION_FAILED),
    ("invalid_blob_id", EXIT_REFUSED),
    ("blob_not_found", EXIT_NOT_FOUND),
    ("malformed_diff", EXIT_OPERATION_FAILED),
    ("stale_hunk_selection", EXIT_REFUSED),
    ("whitespace_insensitive_selection", EXIT_REFUSED),
    ("hidden_whitespace_changes", EXIT_REFUSED),
    ("binary_hunk_selection", EXIT_REFUSED),
    ("rename_only_hunk_selection", EXIT_REFUSED),
    ("metadata_only_hunk_selection", EXIT_REFUSED),
    ("unsupported_hunk_change", EXIT_REFUSED),
    ("filtered_hunk_selection", EXIT_REFUSED),
    ("overlapping_hunk_selection", EXIT_REFUSED),
    ("hunk_not_found", EXIT_REFUSED),
    ("line_not_found", EXIT_REFUSED),
    ("unrepresentable_line_selection", EXIT_REFUSED),
    ("hunk_application", EXIT_OPERATION_FAILED),
    ("malformed_status", EXIT_OPERATION_FAILED),
];

/// The exit code every project error kind reports, in
/// `ProjectError::DIRECT_KINDS` order.
///
/// [`project_exit_code`] is exhaustive today, so a new variant does fail to
/// compile there. That is not enough on its own: the compiler will happily
/// accept a variant grouped under the wrong existing arm, which is exactly how
/// a classification drifts. This table states the intended answer separately,
/// and `project_error_kinds_are_classified_for_the_exit_code_contract` holds
/// the two in agreement.
const PROJECT_KIND_EXIT_CODES: &[(&str, u8)] = &[
    ("data_directory_unavailable", EXIT_OPERATION_FAILED),
    ("invalid_directory", EXIT_OPERATION_FAILED),
    ("unreadable_directory", EXIT_OPERATION_FAILED),
    ("catalog_read", EXIT_OPERATION_FAILED),
    ("catalog_lock", EXIT_OPERATION_FAILED),
    ("malformed_catalog", EXIT_OPERATION_FAILED),
    ("catalog_version_too_old", EXIT_OPERATION_FAILED),
    ("catalog_version_too_new", EXIT_OPERATION_FAILED),
    ("invalid_catalog", EXIT_OPERATION_FAILED),
    ("project_selector_not_found", EXIT_NOT_FOUND),
    ("ambiguous_project_selector", EXIT_CONFLICT),
    ("project_not_found", EXIT_NOT_FOUND),
    ("invalid_check_configuration", EXIT_REFUSED),
    ("project_unavailable", EXIT_OPERATION_FAILED),
    ("git_inspection", EXIT_OPERATION_FAILED),
    ("persistence", EXIT_OPERATION_FAILED),
    ("invalid_remote", EXIT_OPERATION_FAILED),
    ("git_launch", EXIT_OPERATION_FAILED),
    ("clone_failed", EXIT_OPERATION_FAILED),
    ("clone_cancelled", EXIT_CANCELLED),
    ("unsafe_managed_removal", EXIT_REFUSED),
    ("managed_removal", EXIT_OPERATION_FAILED),
    ("managed_repository_lock", EXIT_OPERATION_FAILED),
    ("managed_repository_reconciliation", EXIT_OPERATION_FAILED),
    ("parent_has_worktrees", EXIT_CONFLICT),
    ("worktree_removal_required", EXIT_REFUSED),
    ("unsafe_worktree_removal", EXIT_REFUSED),
    ("worktree_parent_unsupported", EXIT_REFUSED),
    ("dirty_worktree_removal", EXIT_REFUSED),
    ("unsafe_worktree_lock", EXIT_REFUSED),
    ("unsafe_worktree_move", EXIT_REFUSED),
    ("worktree_destination_exists", EXIT_CONFLICT),
    ("worktree_destination_not_absolute", EXIT_REFUSED),
    ("worktree_destination_parent_unavailable", EXIT_REFUSED),
    ("worktree_destination_inside_project", EXIT_REFUSED),
    ("worktree_destination_contains_project", EXIT_REFUSED),
    ("worktree_destination_inside_data_directory", EXIT_REFUSED),
    ("worktree_moved_catalog_stale", EXIT_OPERATION_FAILED),
];

const EDITOR_KIND_EXIT_CODES: &[(&str, u8)] = &[
    ("invalid_editor_template", EXIT_REFUSED),
    ("editor_path_outside_project", EXIT_REFUSED),
    ("editor_file_unavailable", EXIT_NOT_FOUND),
    ("editor_launch", EXIT_OPERATION_FAILED),
];

/// Every external-policy refusal is a guardrail denial (exit code 3).
const POLICY_KIND_EXIT_CODES: &[(&str, u8)] = &[
    ("noninteractive_external_agent_launch_denied", EXIT_REFUSED),
    ("noninteractive_mcp_server_connect_denied", EXIT_REFUSED),
    ("noninteractive_mcp_tool_invoke_denied", EXIT_REFUSED),
    ("noninteractive_forge_resource_read_denied", EXIT_REFUSED),
    ("noninteractive_remote_branch_push_denied", EXIT_REFUSED),
    ("noninteractive_pull_request_create_denied", EXIT_REFUSED),
    ("noninteractive_forge_resource_modify_denied", EXIT_REFUSED),
    (
        "noninteractive_workflow_recipe_execute_denied",
        EXIT_REFUSED,
    ),
    ("agent_executable_identity_required", EXIT_REFUSED),
    ("mcp_tool_schema_identity_required", EXIT_REFUSED),
    ("recipe_content_identity_required", EXIT_REFUSED),
    ("external_identity_context_invalid", EXIT_REFUSED),
];

/// The exit code every CLI-originated error kind reports, in
/// `CLI_ERROR_KINDS` order.
const CLI_KIND_EXIT_CODES: &[(&str, u8)] = &[
    ("usage_error", EXIT_USAGE),
    ("current_directory_unavailable", EXIT_OPERATION_FAILED),
    ("interrupt_handler_unavailable", EXIT_OPERATION_FAILED),
    ("wire_projection_failed", EXIT_OPERATION_FAILED),
    ("path_operation_failed", EXIT_OPERATION_FAILED),
    ("check_operation_failed", EXIT_OPERATION_FAILED),
    ("check_failed", EXIT_OPERATION_FAILED),
    ("check_cancelled", EXIT_CANCELLED),
    ("confirmation_required", EXIT_REFUSED),
    ("managed_project_requires_delete", EXIT_REFUSED),
    ("local_project_requires_forget", EXIT_REFUSED),
    ("worktree_requires_remove", EXIT_REFUSED),
    ("approval_required_noninteractive", EXIT_REFUSED),
    ("policy_denied", EXIT_REFUSED),
    ("approval_denied", EXIT_REFUSED),
    ("tool_call_denied", EXIT_REFUSED),
    ("tool_call_failed", EXIT_OPERATION_FAILED),
    ("tool_call_cancelled", EXIT_CANCELLED),
    ("tool_call_interrupted", EXIT_OPERATION_FAILED),
    ("run_failed", EXIT_OPERATION_FAILED),
    ("run_cancelled", EXIT_CANCELLED),
    ("run_interrupted", EXIT_OPERATION_FAILED),
];

fn kind_exit_codes(table: &[(&str, u8)]) -> Value {
    Value::Object(
        table
            .iter()
            .map(|(kind, code)| ((*kind).to_owned(), json!(code)))
            .collect(),
    )
}

fn project_error_details(error: &ProjectError) -> Value {
    match error {
        ProjectError::AmbiguousProjectSelector { candidates, .. } => json!({
            "candidates": candidates.iter().map(candidate_value).collect::<Vec<_>>()
        }),
        ProjectError::DirtyWorktreeRemoval { .. } => {
            json!({ "override_flag": "--force" })
        }
        ProjectError::UnsafeWorktreeRemoval { id, path, reason }
        | ProjectError::UnsafeWorktreeLock { id, path, reason }
        | ProjectError::UnsafeWorktreeMove { id, path, reason } => {
            let (path, path_is_lossy) = wire_path(path);
            json!({
                "project_id": id.to_string(),
                "path": path,
                "path_is_lossy": path_is_lossy,
                "reason": reason,
            })
        }
        ProjectError::WorktreeDestinationExists { path }
        | ProjectError::WorktreeDestinationNotAbsolute { path } => {
            let (path, path_is_lossy) = wire_path(path);
            json!({ "path": path, "path_is_lossy": path_is_lossy })
        }
        ProjectError::WorktreeDestinationParentUnavailable { path, .. } => {
            let (path, path_is_lossy) = wire_path(path);
            json!({ "path": path, "path_is_lossy": path_is_lossy })
        }
        ProjectError::WorktreeDestinationInsideProject {
            path,
            project_id,
            project_root,
        }
        | ProjectError::WorktreeDestinationContainsProject {
            path,
            project_id,
            project_root,
        } => {
            let (path, path_is_lossy) = wire_path(path);
            let (project_root, project_root_is_lossy) = wire_path(project_root);
            json!({
                "path": path,
                "path_is_lossy": path_is_lossy,
                "project_id": project_id.to_string(),
                "project_root": project_root,
                "project_root_is_lossy": project_root_is_lossy,
            })
        }
        ProjectError::WorktreeDestinationInsideDataDirectory { path, data_dir } => {
            let (path, path_is_lossy) = wire_path(path);
            let (data_dir, data_dir_is_lossy) = wire_path(data_dir);
            json!({
                "path": path,
                "path_is_lossy": path_is_lossy,
                "data_dir": data_dir,
                "data_dir_is_lossy": data_dir_is_lossy,
            })
        }
        ProjectError::WorktreeMovedCatalogStale {
            id,
            stale_root,
            destination,
            reason,
        } => {
            let (stale_root, stale_root_is_lossy) = wire_path(stale_root);
            let (destination, destination_is_lossy) = wire_path(destination);
            json!({
                "project_id": id.to_string(),
                "stale_root": stale_root,
                "stale_root_is_lossy": stale_root_is_lossy,
                "destination": destination,
                "destination_is_lossy": destination_is_lossy,
                "reason": reason,
            })
        }
        ProjectError::Git(error) => git_error_details(error),
        ProjectError::Editor(error) => editor_error_details(error),
        _ => json!({}),
    }
}

fn editor_error_details(error: &EditorError) -> Value {
    match error {
        EditorError::PathOutsideProject { path } | EditorError::FileUnavailable { path } => {
            let (path, path_is_lossy) = wire_path(path);
            json!({ "path": path, "path_is_lossy": path_is_lossy })
        }
        EditorError::Launch { command, .. } => json!({ "command": command }),
        EditorError::InvalidTemplate { command, .. } => json!({ "command": command }),
    }
}

fn git_error_details(error: &GitError) -> Value {
    match error {
        GitError::RevisionNotFound { revision } | GitError::AmbiguousRevision { revision } => {
            json!({ "revision": revision })
        }
        GitError::InvalidBlobId { blob_id } | GitError::BlobNotFound { blob_id } => {
            json!({ "blob_id": blob_id })
        }
        GitError::RevisionNotCommit { revision, id } => {
            json!({ "revision": revision, "object_id": id.to_string() })
        }
        GitError::RevisionNotParent { revision, parent } => {
            json!({ "revision": revision, "parent": parent })
        }
        GitError::NoMergeBase { one, two } => json!({
            "one": one,
            "two": two,
        }),
        GitError::InvalidLogLimit => json!({ "minimum": 1 }),
        GitError::InvalidLogCursor { cursor } => json!({ "cursor": cursor.to_string() }),
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
        GitError::WorktreeMoveAcrossDevices {
            worktree,
            destination,
            ..
        } => {
            let (worktree, worktree_is_lossy) = wire_path(worktree);
            let (destination, destination_is_lossy) = wire_path(destination);
            json!({
                "worktree": worktree,
                "worktree_is_lossy": worktree_is_lossy,
                "destination": destination,
                "destination_is_lossy": destination_is_lossy,
            })
        }
        GitError::WorktreeLocked { path, reason }
        | GitError::WorktreeAlreadyLocked { path, reason } => {
            let (path, path_is_lossy) = wire_path(path);
            json!({
                "path": path,
                "path_is_lossy": path_is_lossy,
                "reason": reason,
            })
        }
        GitError::WorktreeNotLocked { path }
        | GitError::WorktreeAddDestinationExists { path }
        | GitError::WorktreeAddDestinationUnavailable { path, .. }
        | GitError::UntrackedDiscardRequiresDelete { path }
        | GitError::TrackedDiscardRequiresRestore { path }
        | GitError::UnmergedDiscard { path }
        | GitError::NothingToDiscard { path }
        | GitError::UntrackedDiscardNotFile { path }
        | GitError::UntrackedDiscardIo { path, .. }
        | GitError::StaleHunkSelection { path }
        | GitError::HiddenWhitespaceChanges { path }
        | GitError::BinaryHunkSelection { path }
        | GitError::OverlappingHunkSelection { path }
        | GitError::UnrepresentableLineSelection { path }
        | GitError::RepositoryBusy { path }
        | GitError::NotARepository { path }
        | GitError::DiffContent { path, .. } => {
            let (path, path_is_lossy) = wire_path(path);
            json!({ "path": path, "path_is_lossy": path_is_lossy })
        }
        GitError::WorktreeAddCleanup { path, detail } => {
            let (path, path_is_lossy) = wire_path(path);
            json!({
                "path": path,
                "path_is_lossy": path_is_lossy,
                "detail": detail,
            })
        }
        // The whitespace record travels back out with the path, so a consumer
        // that built the selection from a document can name the setting it has
        // to change without parsing the message.
        GitError::WhitespaceInsensitiveSelection { path, whitespace } => {
            let (path, path_is_lossy) = wire_path(path);
            json!({
                "path": path,
                "path_is_lossy": path_is_lossy,
                "whitespace": whitespace_value(*whitespace),
            })
        }
        // Every hunk refusal below names an alternative in its message, and the
        // path that alternative applies to has to reach the caller as data. A
        // machine consumer choosing to fall back to path staging cannot be made
        // to recover the path by matching prose.
        GitError::RenameOnlyHunkSelection { old_path, new_path } => {
            let (old_path, old_path_is_lossy) = wire_path(old_path);
            let (new_path, new_path_is_lossy) = wire_path(new_path);
            json!({
                "old_path": old_path,
                "old_path_is_lossy": old_path_is_lossy,
                "new_path": new_path,
                "new_path_is_lossy": new_path_is_lossy,
            })
        }
        GitError::MetadataOnlyHunkSelection {
            path,
            old_mode,
            new_mode,
        } => {
            let (path, path_is_lossy) = wire_path(path);
            json!({
                "path": path,
                "path_is_lossy": path_is_lossy,
                "old_mode": old_mode,
                "new_mode": new_mode,
            })
        }
        GitError::UnsupportedHunkChange { path, change } => {
            let (path, path_is_lossy) = wire_path(path);
            json!({
                "path": path,
                "path_is_lossy": path_is_lossy,
                "change": file_change_name(*change),
            })
        }
        GitError::FilteredHunkSelection { path, driver } => {
            let (path, path_is_lossy) = wire_path(path);
            json!({
                "path": path,
                "path_is_lossy": path_is_lossy,
                "driver": driver,
            })
        }
        GitError::HunkApplication { paths, .. } => json!({
            "paths": paths.iter().map(|path| wire_path(path).0).collect::<Vec<_>>(),
            "paths_are_lossy": paths.iter().any(|path| wire_path(path).1),
        }),
        GitError::PathOutsideRepository { path, repository } => {
            let (path, path_is_lossy) = wire_path(path);
            let (repository, repository_is_lossy) = wire_path(repository);
            json!({
                "path": path,
                "path_is_lossy": path_is_lossy,
                "repository": repository,
                "repository_is_lossy": repository_is_lossy,
            })
        }
        GitError::MalformedDiff { detail } | GitError::MalformedStatus { detail } => {
            json!({ "detail": detail })
        }
        GitError::OperationInProgress { path, pending } => {
            let (path, path_is_lossy) = wire_path(path);
            json!({
                "path": path,
                "path_is_lossy": path_is_lossy,
                "pending": pending_name(*pending),
            })
        }
        GitError::BranchCheckedOutInWorktree { branch, worktree } => {
            let (worktree, worktree_is_lossy) = wire_path(worktree);
            json!({
                "branch": branch,
                "worktree": worktree,
                "worktree_is_lossy": worktree_is_lossy,
            })
        }
        GitError::UnbornBranch { path, branch } => {
            let (path, path_is_lossy) = wire_path(path);
            json!({
                "path": path,
                "path_is_lossy": path_is_lossy,
                "branch": branch,
            })
        }
        GitError::DetachedHead { path, detail } => {
            let (path, path_is_lossy) = wire_path(path);
            json!({
                "path": path,
                "path_is_lossy": path_is_lossy,
                "detail": detail,
            })
        }
        GitError::HunkNotFound {
            path,
            old_start,
            old_lines,
            new_start,
            new_lines,
        } => {
            let (path, path_is_lossy) = wire_path(path);
            json!({
                "path": path,
                "path_is_lossy": path_is_lossy,
                "old_start": old_start,
                "old_lines": old_lines,
                "new_start": new_start,
                "new_lines": new_lines,
            })
        }
        GitError::LineNotFound {
            path,
            old_line_number,
            new_line_number,
        } => {
            let (path, path_is_lossy) = wire_path(path);
            json!({
                "path": path,
                "path_is_lossy": path_is_lossy,
                "old_line_number": old_line_number,
                "new_line_number": new_line_number,
            })
        }
        _ => json!({}),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, HashSet},
        fs, io,
        path::{Path, PathBuf},
    };

    use super::{
        CLI_ERROR_KINDS, CLI_KIND_EXIT_CODES, CliError, CommandResult, EDITOR_KIND_EXIT_CODES,
        EXIT_CANCELLED, EXIT_CONFLICT, EXIT_NOT_FOUND, EXIT_OPERATION_FAILED, EXIT_REFUSED,
        EXIT_USAGE, EXTERNAL_POLICY_DENIAL_KINDS, EditorError, GIT_KIND_EXIT_CODES, GitError,
        HunkSelection, InvocationError, LineSelection, POLICY_KIND_EXIT_CODES,
        PROJECT_KIND_EXIT_CODES, Project, ProjectError, RUNTIME_KIND_EXIT_CODES, RefusalKind,
        RuntimeError, StoreError, TOOL_KIND_EXIT_CODES, Whitespace, WhitespaceMode,
        change_provenance_value, check_covers_diff_target, contract_result,
        distinct_hunk_selection_count, distinct_line_selection_counts, editor_exit_code,
        git_error_details, git_exit_code, parse_line_selection_document, parse_selection_document,
        project_exit_code, project_value, requested_json, single_line,
    };

    use crate::runtime_support::OUTCOME_KIND_EXIT_CODES;
    use tempfile::tempdir;

    /// Attribution is advisory: a walk that could not be made degrades to a
    /// named reason inside the block, so the diff beside it still reaches the
    /// caller. A consumer distinguishes that from an ordinary empty answer by
    /// `unavailable` rather than by counting what is missing.
    #[test]
    fn an_attribution_that_failed_degrades_to_a_named_block() {
        let target = super::DiffTarget::BranchAgainstBase {
            branch: "agent/demo".to_owned(),
            base_branch: "main".to_owned(),
        };
        let value = change_provenance_value(
            &Err(GitError::RevisionNotFound {
                revision: "agent/demo".to_owned(),
            }),
            &target,
        );

        assert_eq!(value["target"]["kind"], "branch_against_base");
        assert_eq!(value["unavailable"]["kind"], "revision_not_found");
        assert!(
            value["unavailable"]["message"]
                .as_str()
                .unwrap()
                .contains("agent/demo")
        );
        // Nothing is claimed about a range that was never walked.
        assert_eq!(value["range"], serde_json::Value::Null);
        assert_eq!(value["commits"].as_array().unwrap().len(), 0);
        assert_eq!(value["producers"].as_array().unwrap().len(), 0);
        assert_eq!(value["walked_commits"], 0);
        assert_eq!(value["truncation"], serde_json::Value::Null);
    }

    #[test]
    fn a_check_on_the_current_head_does_not_cover_an_older_commit_review() {
        let workspace = tempdir().unwrap();
        let data = tempdir().unwrap();
        let repository = git2::Repository::init(workspace.path()).unwrap();
        let first = commit_file(&repository, "first");
        let second = commit_file(&repository, "second");
        let git = super::GitService::new(workspace.path(), data.path());
        let summary = harkness_runtime::check::CheckSummary {
            run_id: "run".to_owned(),
            check_id: "test".to_owned(),
            label: "Test".to_owned(),
            command: vec!["true".to_owned()],
            recorded_cwd: None,
            recorded_env: BTreeMap::new(),
            recorded_timeout: None,
            recorded_parser: "plain".to_owned(),
            definition_current: true,
            outcome: harkness_runtime::check::CheckOutcome::Passed,
            evidence_class: harkness_runtime::check::ActivityClass::HarknessObserved,
            created_at: "2026-08-17T00:00:00Z".to_owned(),
            finished_at: Some("2026-08-17T00:00:01Z".to_owned()),
            duration_ms: Some(1),
            state_digest: Some("digest".to_owned()),
            state_head: Some(second.to_string()),
            workspace_clean: Some(true),
            workspace_matches_index: Some(true),
            freshness: harkness_runtime::check::CheckFreshness::Current,
            diagnostics: Vec::new(),
            diagnostics_omitted: 0,
            diagnostics_scan_truncated: false,
            diagnostics_unavailable: None,
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            artifact_byte_limit: 8 * 1024 * 1024,
            stdout_artifact_truncated: false,
            stderr_artifact_truncated: false,
        };

        let mut revisions = super::RevisionCache::new(&git);
        assert!(
            !check_covers_diff_target(
                &mut revisions,
                &summary,
                &super::DiffTarget::Commit {
                    revision: first.to_string(),
                    parent: None,
                },
            )
            .unwrap()
        );
        assert!(
            check_covers_diff_target(
                &mut revisions,
                &summary,
                &super::DiffTarget::Commit {
                    revision: second.to_string(),
                    parent: None,
                },
            )
            .unwrap()
        );
    }

    fn commit_file(repository: &git2::Repository, contents: &str) -> git2::Oid {
        fs::write(repository.workdir().unwrap().join("file.txt"), contents).unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(Path::new("file.txt")).unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repository.find_tree(tree_id).unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.com").unwrap();
        let parents = repository
            .head()
            .ok()
            .and_then(|head| head.peel_to_commit().ok())
            .into_iter()
            .collect::<Vec<_>>();
        let parent_refs = parents.iter().collect::<Vec<_>>();
        repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                contents,
                &tree,
                &parent_refs,
            )
            .unwrap()
    }

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

    /// The exit-code table and `GitError::KINDS` must stay in lockstep. A new
    /// Git error kind reaches the published contract through `git_exit_code`'s
    /// mandatory wildcard, which cannot fail to compile because `GitError` is
    /// `#[non_exhaustive]`. This test is what refuses the default instead.
    #[test]
    fn git_error_kinds_are_classified_for_the_exit_code_contract() {
        let declared = GIT_KIND_EXIT_CODES
            .iter()
            .map(|(kind, _)| *kind)
            .collect::<Vec<_>>();
        assert_eq!(
            declared,
            GitError::KINDS,
            "GIT_KIND_EXIT_CODES must classify every Git error kind, in order"
        );
        assert_eq!(
            git_exit_code(&GitError::StaleDiscardSelection),
            EXIT_REFUSED,
            "a stale destructive confirmation is a refusal, not an operation failure"
        );
    }

    #[test]
    fn discard_confirmation_counts_distinct_hunks_and_lines() {
        let hunk = HunkSelection::from_parts(
            Some(PathBuf::from("tracked.txt")),
            Some(PathBuf::from("tracked.txt")),
            "old",
            "new",
            3,
            Whitespace::EXACT,
            (10, 4),
            (10, 5),
        );
        let same_hunk_with_different_context = HunkSelection::from_parts(
            Some(PathBuf::from("tracked.txt")),
            Some(PathBuf::from("tracked.txt")),
            "old",
            "new",
            8,
            Whitespace::EXACT,
            (10, 4),
            (10, 5),
        );
        assert_eq!(
            distinct_hunk_selection_count(&[hunk.clone(), hunk, same_hunk_with_different_context,]),
            1,
            "a repeated hunk must be confirmed once"
        );

        let first = LineSelection::from_parts(
            Some(PathBuf::from("tracked.txt")),
            Some(PathBuf::from("tracked.txt")),
            "old",
            "new",
            3,
            Whitespace::EXACT,
            (10, 4),
            (10, 5),
            None,
            Some(11),
        );
        let second = LineSelection::from_parts(
            Some(PathBuf::from("tracked.txt")),
            Some(PathBuf::from("tracked.txt")),
            "old",
            "new",
            3,
            Whitespace::EXACT,
            (10, 4),
            (10, 5),
            None,
            Some(12),
        );
        let repeated_with_different_context = LineSelection::from_parts(
            Some(PathBuf::from("tracked.txt")),
            Some(PathBuf::from("tracked.txt")),
            "old",
            "new",
            8,
            Whitespace::EXACT,
            (10, 4),
            (10, 5),
            None,
            Some(11),
        );
        assert_eq!(
            distinct_line_selection_counts(&[
                first.clone(),
                first,
                second,
                repeated_with_different_context,
            ]),
            (2, 1),
            "repeated lines are deduplicated and lines in one hunk share its count"
        );
    }

    #[test]
    fn editor_error_kinds_are_classified_for_the_exit_code_contract() {
        let declared = EDITOR_KIND_EXIT_CODES
            .iter()
            .map(|(kind, _)| *kind)
            .collect::<Vec<_>>();
        assert_eq!(declared, EditorError::KINDS);
        assert_eq!(
            editor_exit_code(&EditorError::InvalidTemplate {
                command: "fixture-editor".to_owned(),
                reason: "fixture".to_owned(),
            }),
            EXIT_REFUSED
        );
        assert_eq!(
            editor_exit_code(&EditorError::Launch {
                command: "missing-editor".to_owned(),
                source: io::Error::new(io::ErrorKind::NotFound, "fixture"),
            }),
            EXIT_OPERATION_FAILED
        );
    }

    #[test]
    fn history_and_context_errors_keep_agent_facing_classification_and_details() {
        let object_id = git2::Oid::ZERO_SHA1;
        let cases = vec![
            (
                GitError::RevisionNotFound {
                    revision: "missing".to_owned(),
                },
                EXIT_NOT_FOUND,
                serde_json::json!({ "revision": "missing" }),
            ),
            (
                GitError::AmbiguousRevision {
                    revision: "abcd".to_owned(),
                },
                EXIT_NOT_FOUND,
                serde_json::json!({ "revision": "abcd" }),
            ),
            (
                GitError::RevisionNotCommit {
                    revision: "blob".to_owned(),
                    id: object_id,
                },
                EXIT_REFUSED,
                serde_json::json!({
                    "revision": "blob",
                    "object_id": object_id.to_string(),
                }),
            ),
            (
                GitError::RevisionNotParent {
                    revision: "commit".to_owned(),
                    parent: "unrelated".to_owned(),
                },
                EXIT_REFUSED,
                serde_json::json!({
                    "revision": "commit",
                    "parent": "unrelated",
                }),
            ),
            (
                GitError::NoMergeBase {
                    one: "main".to_owned(),
                    two: "orphan".to_owned(),
                },
                EXIT_CONFLICT,
                serde_json::json!({ "one": "main", "two": "orphan" }),
            ),
            (
                GitError::InvalidLogLimit,
                EXIT_REFUSED,
                serde_json::json!({ "minimum": 1 }),
            ),
            (
                GitError::InvalidLogCursor { cursor: object_id },
                EXIT_REFUSED,
                serde_json::json!({ "cursor": object_id.to_string() }),
            ),
            (
                GitError::InvalidBlobId {
                    blob_id: "invalid".to_owned(),
                },
                EXIT_REFUSED,
                serde_json::json!({ "blob_id": "invalid" }),
            ),
            (
                GitError::BlobNotFound {
                    blob_id: "1".repeat(40),
                },
                EXIT_NOT_FOUND,
                serde_json::json!({ "blob_id": "1".repeat(40) }),
            ),
        ];

        for (error, exit_code, details) in cases {
            assert_eq!(git_exit_code(&error), exit_code, "for {error:?}");
            assert_eq!(git_error_details(&error), details, "for {error:?}");
            assert_eq!(
                GIT_KIND_EXIT_CODES
                    .iter()
                    .find(|(kind, _)| *kind == error.kind())
                    .map(|(_, declared)| *declared),
                Some(exit_code),
                "published table disagrees for {error:?}"
            );
        }
    }

    /// The project table has no wildcard to defend against, but it does have to
    /// agree with `project_exit_code`. A new variant grouped under the wrong
    /// existing arm compiles perfectly; this is what notices.
    #[test]
    fn project_error_kinds_are_classified_for_the_exit_code_contract() {
        let declared = PROJECT_KIND_EXIT_CODES
            .iter()
            .map(|(kind, _)| *kind)
            .collect::<Vec<_>>();
        assert_eq!(
            declared,
            ProjectError::DIRECT_KINDS,
            "PROJECT_KIND_EXIT_CODES must classify every project error kind, in order"
        );
    }

    /// The published tables must describe what the classifiers actually return,
    /// or `harkness contract` documents a mapping the binary does not honour.
    #[test]
    fn published_exit_codes_match_the_classifiers() {
        let cases: [(ProjectError, u8); 4] = [
            (
                ProjectError::WorktreeDestinationExists {
                    path: PathBuf::from("/tmp/occupied"),
                },
                EXIT_CONFLICT,
            ),
            (
                ProjectError::WorktreeDestinationNotAbsolute {
                    path: PathBuf::from("relative"),
                },
                EXIT_REFUSED,
            ),
            (
                ProjectError::CloneFailed {
                    stderr: "fixture".to_owned(),
                },
                EXIT_OPERATION_FAILED,
            ),
            (ProjectError::CloneCancelled, EXIT_CANCELLED),
        ];
        for (error, expected) in cases {
            assert_eq!(project_exit_code(&error), expected, "for {error:?}");
            let declared = PROJECT_KIND_EXIT_CODES
                .iter()
                .find(|(kind, _)| *kind == error.kind())
                .map(|(_, code)| *code);
            assert_eq!(declared, Some(expected), "table disagrees for {error:?}");
        }
        let cli_kinds = CLI_KIND_EXIT_CODES
            .iter()
            .map(|(kind, _)| *kind)
            .collect::<Vec<_>>();
        assert_eq!(cli_kinds, CLI_ERROR_KINDS);
        assert_eq!(
            CLI_KIND_EXIT_CODES
                .iter()
                .find(|(kind, _)| *kind == "usage_error")
                .map(|(_, code)| *code),
            Some(EXIT_USAGE)
        );
    }

    /// A `git diff` response is meant to be usable as a selection document with
    /// nothing but its unwanted hunks removed, so the parser is pinned against
    /// the exact shape `file_diff_value` emits, envelope and all.
    #[test]
    fn a_diff_response_round_trips_as_a_selection_document() {
        let document = serde_json::json!({
            "v": 1,
            "type": "success",
            "ok": true,
            "data": {
                "files": [{
                    "old_path": "kept.txt",
                    "old_path_is_lossy": false,
                    "old_path_base64": "a2VwdC50eHQ=",
                    "new_path": "kept.txt",
                    "new_path_is_lossy": false,
                    "new_path_base64": "a2VwdC50eHQ=",
                    "old_blob_id": "aaaa",
                    "new_blob_id": "bbbb",
                    "context_lines": 3,
                    "hunks": [
                        { "old_start": 1, "old_lines": 5, "new_start": 1, "new_lines": 5 },
                        { "old_start": 11, "old_lines": 5, "new_start": 11, "new_lines": 6 },
                    ],
                }],
            },
        })
        .to_string();

        let selections = parse_selection_document(&document, "unstaged").unwrap();

        assert_eq!(selections.len(), 2);
        assert_eq!(
            selections[0].old_path.as_deref(),
            Some(Path::new("kept.txt"))
        );
        assert_eq!(selections[0].context_lines, 3);
        assert_eq!(selections[1].new_start, 11);
        assert_eq!(selections[1].new_lines, 6);
        // No whitespace record at all is what every document written before the
        // setting existed looks like, and it has to mean the one handling a
        // selection may be taken under.
        assert!(
            selections
                .iter()
                .all(|selection| selection.whitespace.is_exact())
        );
    }

    /// The whitespace record travels with a selection at both granularities, so
    /// the refusal lands on the service rather than on a guess made here, and a
    /// spelling this build does not define is refused rather than read as exact.
    #[test]
    fn a_selection_document_carries_its_whitespace_record() {
        let file = |whitespace: serde_json::Value| {
            serde_json::json!({
                "files": [{
                    "new_path": "kept.txt",
                    "old_path": "kept.txt",
                    "old_blob_id": "aaaa",
                    "new_blob_id": "bbbb",
                    "context_lines": 3,
                    "whitespace": whitespace,
                    "hunks": [{
                        "old_start": 1, "old_lines": 2, "new_start": 1, "new_lines": 2,
                        "lines": [
                            { "kind": "deletion", "old_line_number": 1, "new_line_number": null },
                            { "kind": "addition", "old_line_number": null, "new_line_number": 1 },
                        ],
                    }],
                }],
            })
            .to_string()
        };

        let relaxed = file(serde_json::json!({
            "mode": "ignore_all",
            "ignore_blank_lines": true,
        }));
        let hunks = parse_selection_document(&relaxed, "unstaged").unwrap();
        assert_eq!(hunks[0].whitespace.mode, WhitespaceMode::IgnoreAll);
        assert!(hunks[0].whitespace.ignore_blank_lines);
        assert!(!hunks[0].whitespace.is_exact());
        let lines = parse_line_selection_document(&relaxed, "unstaged").unwrap();
        assert_eq!(lines[0].whitespace, hunks[0].whitespace);

        let exact = file(serde_json::json!({ "mode": "exact" }));
        assert_eq!(
            parse_selection_document(&exact, "unstaged").unwrap()[0].whitespace,
            Whitespace::EXACT
        );

        let unknown = file(serde_json::json!({ "mode": "ignore_vibes" }));
        let error = parse_selection_document(&unknown, "unstaged").unwrap_err();
        assert!(
            single_line(&error.message()).contains("is not a whitespace mode this build knows"),
            "unhelpful refusal: {}",
            error.message()
        );

        // A producer that got the field's *type* wrong is refused too. Reading
        // this through `as_str` would fold it into the same arm as "absent" and
        // hand a wrong record the exact default, which is the one value that
        // lets it be staged from.
        let mistyped = file(serde_json::json!({ "mode": ["ignore_all"] }));
        let error = parse_selection_document(&mistyped, "unstaged").unwrap_err();
        assert!(
            single_line(&error.message()).contains("whitespace.mode is not a string"),
            "a mistyped mode was not refused: {}",
            error.message()
        );
        let mistyped_toggle = file(serde_json::json!({
            "mode": "exact",
            "ignore_blank_lines": "yes",
        }));
        assert!(parse_selection_document(&mistyped_toggle, "unstaged").is_err());
    }

    /// A lossy path names a different file than the one on disk, so replaying
    /// it must be refused with an actionable message rather than accepted and
    /// later reported as a stale selection the caller cannot possibly refresh.
    #[test]
    fn a_lossy_path_is_refused_until_its_exact_bytes_are_supplied() {
        let lossy = serde_json::json!({
            "selections": [{
                "new_path": "bad-\u{fffd}.txt",
                "new_path_is_lossy": true,
                "old_blob_id": "aaaa",
                "new_blob_id": "bbbb",
                "context_lines": 3,
                "old_start": 1, "old_lines": 1, "new_start": 1, "new_lines": 1,
            }],
        })
        .to_string();

        let error = parse_selection_document(&lossy, "unstaged").unwrap_err();

        assert_eq!(error.kind(), "usage_error");
        assert!(
            error.message().contains("new_path_base64"),
            "the refusal must name the field that fixes it: {}",
            error.message()
        );

        let exact = serde_json::json!({
            "selections": [{
                "new_path": "bad-\u{fffd}.txt",
                "new_path_is_lossy": true,
                "new_path_base64": "YmFkLf8udHh0",
                "old_blob_id": "aaaa",
                "new_blob_id": "bbbb",
                "context_lines": 3,
                "old_start": 1, "old_lines": 1, "new_start": 1, "new_lines": 1,
            }],
        })
        .to_string();

        let parsed = parse_selection_document(&exact, "unstaged");

        // Whether those bytes name a file at all is a platform question. A
        // Unix path is bytes, so the exact spelling wins; a Windows path is
        // UTF-16 and cannot hold them, so the only honest answer is to say so
        // rather than to substitute a name that points somewhere else.
        #[cfg(unix)]
        {
            let selections = parsed.unwrap();
            assert_eq!(selections.len(), 1);
            assert_ne!(
                selections[0].new_path.as_deref(),
                Some(Path::new("bad-\u{fffd}.txt")),
                "the Base64 spelling must win over the lossy one"
            );
        }
        #[cfg(not(unix))]
        {
            let error = parsed.unwrap_err();
            assert_eq!(error.kind(), "usage_error");
            assert!(
                error
                    .message()
                    .contains("not a valid path on this platform"),
                "the refusal must explain why: {}",
                error.message()
            );
        }
    }

    /// Piping a combined diff into one side's command is the obvious mistake,
    /// and revalidation would call it stale — true of the identities, useless
    /// as a diagnosis. The wrong side has to be named as the wrong side.
    #[test]
    fn a_record_from_the_other_side_of_the_index_is_named_not_called_stale() {
        let document = serde_json::json!({
            "files": [{
                "target": "staged",
                "new_path": "a.txt",
                "old_path": "a.txt",
                "old_blob_id": "aaaa",
                "new_blob_id": "bbbb",
                "context_lines": 3,
                "hunks": [{ "old_start": 1, "old_lines": 1, "new_start": 1, "new_lines": 1 }],
            }],
        })
        .to_string();

        let error = parse_selection_document(&document, "unstaged").unwrap_err();

        assert_eq!(error.kind(), "usage_error");
        let message = error.message();
        assert!(message.contains("\"staged\""), "{message}");
        assert!(message.contains("--unstaged"), "{message}");
        assert!(
            !message.contains("stale"),
            "the wrong side must not be reported as staleness: {message}"
        );

        // The same document is exactly what `git unstage` should accept.
        assert_eq!(
            parse_selection_document(&document, "staged").unwrap().len(),
            1
        );
    }

    #[test]
    fn malformed_selection_documents_are_usage_errors_that_name_the_field() {
        let cases = [
            ("not json", "not JSON"),
            (r#"{"files": {}}"#, "files is not an array"),
            (
                r#"{"selections": [{"new_path": "a"}]}"#,
                "selections[0].old_blob_id",
            ),
            (
                r#"{"files": [{"new_path": "a", "old_blob_id": "x", "new_blob_id": "y", "context_lines": 3}]}"#,
                "has no \"hunks\"",
            ),
            (
                r#"{"selections": [{"old_blob_id": "x", "new_blob_id": "y", "context_lines": 3, "old_start": 1, "old_lines": 1, "new_start": 1, "new_lines": 1}]}"#,
                "neither an old_path nor a new_path",
            ),
        ];
        for (document, expected) in cases {
            let error = parse_selection_document(document, "unstaged").unwrap_err();
            assert_eq!(error.kind(), "usage_error", "for {document}");
            assert!(
                error.message().contains(expected),
                "expected {expected:?} in {:?}",
                error.message()
            );
        }
    }

    /// Proves the table describes what `git_exit_code` actually returns for the
    /// worktree-lock kinds, so a state conflict is never reported as a generic
    /// operation failure.
    #[test]
    fn worktree_lock_errors_report_conflict_and_refusal_exit_codes() {
        let path = PathBuf::from("/tmp/worktree");
        let cases: [(GitError, u8); 4] = [
            (GitError::EmptyWorktreeLockReason, EXIT_REFUSED),
            (
                GitError::WorktreeLocked {
                    path: path.clone(),
                    reason: Some("agent is still working".to_owned()),
                },
                EXIT_REFUSED,
            ),
            (
                GitError::WorktreeAlreadyLocked {
                    path: path.clone(),
                    reason: Some("agent is still working".to_owned()),
                },
                EXIT_CONFLICT,
            ),
            (GitError::WorktreeNotLocked { path }, EXIT_CONFLICT),
        ];
        for (error, expected) in cases {
            assert_eq!(git_exit_code(&error), expected, "for {error:?}");
            assert_ne!(git_exit_code(&error), EXIT_OPERATION_FAILED);
            let declared = GIT_KIND_EXIT_CODES
                .iter()
                .find(|(kind, _)| *kind == error.kind())
                .map(|(_, code)| *code);
            assert_eq!(declared, Some(expected), "table disagrees for {error:?}");
        }
    }

    #[test]
    fn worktree_transaction_errors_keep_their_exit_codes_and_paths() {
        let path = PathBuf::from("/tmp/worktree");
        let cases = [
            (
                GitError::WorktreeAddDestinationExists { path: path.clone() },
                EXIT_CONFLICT,
            ),
            (
                GitError::WorktreeAddDestinationUnavailable {
                    path: path.clone(),
                    source: io::Error::other("fixture"),
                },
                EXIT_OPERATION_FAILED,
            ),
            (
                GitError::WorktreeAddCleanup {
                    path: path.clone(),
                    detail: "cleanup could not be verified".to_owned(),
                },
                EXIT_OPERATION_FAILED,
            ),
        ];
        let expected_path = path.to_string_lossy();

        for (error, expected) in cases {
            assert_eq!(git_exit_code(&error), expected, "for {error:?}");
            assert_eq!(
                git_error_details(&error)["path"].as_str(),
                Some(expected_path.as_ref())
            );
        }
    }

    #[test]
    fn worktree_lock_reasons_cannot_forge_tab_separated_columns() {
        assert_eq!(
            single_line("agent\tis\nstill  working"),
            "agent is still working"
        );
        assert_eq!(single_line("  padded  "), "padded");
    }

    #[test]
    fn error_kind_namespaces_are_unique() {
        let kinds = CLI_ERROR_KINDS
            .iter()
            .chain(ProjectError::DIRECT_KINDS)
            .chain(GitError::KINDS)
            .chain(EditorError::KINDS)
            .chain(EXTERNAL_POLICY_DENIAL_KINDS)
            .copied()
            .collect::<Vec<_>>();
        let unique = kinds.iter().copied().collect::<HashSet<_>>();
        assert_eq!(unique.len(), kinds.len(), "error kind collision: {kinds:?}");
    }

    /// The runtime and tool namespaces are deliberately not in the uniqueness
    /// set above, and cannot be: `harkness-runtime` and `harkness-git` both
    /// spell a stopped operation `cancelled` and an expired one `timed_out`,
    /// and both the store and the tool namespace spell a missing subject
    /// `not_found`. Renaming one of them to keep the tables disjoint would
    /// change a discriminant an application service already publishes, which is
    /// worse than an overlap.
    ///
    /// What must hold instead is agreement: a caller holding one discriminant
    /// and reading `exit_code_by_kind` must reach the same code whichever
    /// namespace it looks under, or the published map is ambiguous for exactly
    /// the callers it exists to serve.
    #[test]
    fn a_kind_published_by_two_namespaces_reports_one_exit_code() {
        let mut published: BTreeMap<&str, (u8, &str)> = BTreeMap::new();
        for (namespace, table) in [
            ("cli", CLI_KIND_EXIT_CODES),
            ("project", PROJECT_KIND_EXIT_CODES),
            ("git", GIT_KIND_EXIT_CODES),
            ("editor", EDITOR_KIND_EXIT_CODES),
            ("policy", POLICY_KIND_EXIT_CODES),
            ("runtime", RUNTIME_KIND_EXIT_CODES),
            ("tool", TOOL_KIND_EXIT_CODES),
        ] {
            for (kind, code) in table {
                if let Some((earlier, from)) = published.insert(kind, (*code, namespace)) {
                    assert_eq!(
                        earlier, *code,
                        "{kind} is {earlier} under {from} and {code} under {namespace}"
                    );
                }
            }
        }
    }

    #[test]
    fn every_external_policy_denial_is_a_guardrail_refusal() {
        assert_eq!(
            POLICY_KIND_EXIT_CODES
                .iter()
                .map(|(kind, _)| *kind)
                .collect::<Vec<_>>(),
            EXTERNAL_POLICY_DENIAL_KINDS
        );
        assert!(
            POLICY_KIND_EXIT_CODES
                .iter()
                .all(|(_, code)| *code == EXIT_REFUSED)
        );
    }

    #[test]
    fn cli_error_kind_contract_is_stable() {
        let refused = |kind| CliError::Refused {
            kind,
            message: "fixture".to_owned(),
            details: serde_json::json!({}),
        };
        let cases = [
            CliError::Usage("fixture".to_owned()),
            CliError::CurrentDirectory(io::Error::other("fixture")),
            CliError::InterruptHandler(io::Error::other("fixture")),
            CliError::WireProjection("fixture".to_owned()),
            CliError::PathOperation {
                operation: "staging",
                details: serde_json::json!({}),
            },
            CliError::Check("fixture".to_owned()),
            CliError::CheckVerdict {
                kind: "check_failed",
                message: "fixture".to_owned(),
                details: serde_json::json!({}),
            },
            CliError::CheckVerdict {
                kind: "check_cancelled",
                message: "fixture".to_owned(),
                details: serde_json::json!({}),
            },
            refused(RefusalKind::ConfirmationRequired),
            refused(RefusalKind::ManagedProjectRequiresDelete),
            refused(RefusalKind::LocalProjectRequiresForget),
            refused(RefusalKind::WorktreeRequiresRemove),
        ];
        // Runtime outcomes are one variant carrying a `'static` discriminant,
        // so their spellings are enumerated rather than constructed: the check
        // that keeps them honest is the exit-code table below, which every
        // outcome kind must also appear in.
        let outcomes = [
            "approval_required_noninteractive",
            "policy_denied",
            "approval_denied",
            "tool_call_denied",
            "tool_call_failed",
            "tool_call_cancelled",
            "tool_call_interrupted",
            "run_failed",
            "run_cancelled",
            "run_interrupted",
        ];
        let kinds = cases
            .iter()
            .map(CliError::kind)
            .chain(outcomes.iter().map(|kind| {
                CliError::RuntimeOutcome {
                    kind,
                    code: EXIT_OPERATION_FAILED,
                    message: "fixture".to_owned(),
                    details: serde_json::json!({}),
                }
                .kind()
            }))
            .collect::<Vec<_>>();
        assert_eq!(kinds, CLI_ERROR_KINDS);
    }

    /// The kinds `run_verdict` and `tool_call_verdict` choose from must carry
    /// the exit code `harkness contract` publishes for them.
    ///
    /// Asserting that a `RuntimeOutcome` built from the table's own code reports
    /// that code proves nothing — the variant stores it verbatim. What can
    /// actually drift is the pair the verdicts pick, so the table they read is
    /// what is compared here, entry for entry, against the published one.
    #[test]
    fn cli_outcome_kinds_are_published_with_the_same_exit_code() {
        for (kind, code) in OUTCOME_KIND_EXIT_CODES {
            let published = CLI_KIND_EXIT_CODES
                .iter()
                .find(|(published, _)| published == kind)
                .unwrap_or_else(|| panic!("{kind} is chosen by a verdict but never published"));
            assert_eq!(
                published.1, *code,
                "{kind} exits {code} but is published as {}",
                published.1
            );
        }
        // And the other way, structurally: the outcome kinds are the tail of
        // the published namespace, so a kind added to one table and forgotten
        // in the other is a failing assertion rather than a contract that
        // advertises a spelling nothing reports.
        let outcomes = OUTCOME_KIND_EXIT_CODES
            .iter()
            .map(|(kind, _)| *kind)
            .collect::<Vec<_>>();
        assert_eq!(
            &CLI_ERROR_KINDS[CLI_ERROR_KINDS.len() - outcomes.len()..],
            outcomes
        );
    }

    /// The two runtime namespaces are published beside the four that came
    /// before them, and `harkness contract` concatenates every one of them for
    /// a caller holding a single discriminant.
    #[test]
    fn the_contract_publishes_both_runtime_namespaces() {
        let CommandResult::Json(contract) = contract_result(true) else {
            panic!("--json contract is a JSON result");
        };
        for kind in RuntimeError::KINDS.iter().chain(StoreError::KINDS) {
            assert!(
                contract["exit_code_by_kind"]["runtime"][kind].is_number(),
                "{kind} has no published exit code"
            );
        }
        for kind in InvocationError::kinds() {
            assert!(
                contract["exit_code_by_kind"]["tool"][kind].is_number(),
                "{kind} has no published exit code"
            );
        }
        assert_eq!(
            contract["error_kinds"]["runtime"]
                .as_array()
                .expect("the runtime namespace is a list")
                .len(),
            RuntimeError::KINDS.len() + StoreError::KINDS.len()
        );
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
            checks: None,
        };

        let value = project_value(&project, true).unwrap();

        assert_eq!(value["path_is_lossy"], true);
        assert!(value["root"].as_str().unwrap().contains('\u{fffd}'));
    }
}
