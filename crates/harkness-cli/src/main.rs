use std::{
    env,
    ffi::OsString,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use clap::{
    ArgGroup, Args, Parser, Subcommand,
    error::{Error as ClapError, ErrorKind},
};
use harkness_core::{
    Branch, BranchCheckout, BranchKind, BranchListOptions, Cancellation, CommitOptions,
    CommitOutcome, CreateBranchOptions, DEFAULT_DIFF_CONTEXT_LINES, DEFAULT_MAX_DIFF_FILE_SIZE,
    DEFAULT_MAX_DIFF_FILES, DEFAULT_MAX_DIFF_TOTAL_BYTES, DetailedStatus, DiffLine, DiffLineKind,
    DiffOmission, DiffOptions, DiffTarget, FetchOptions, FetchOutcome, FileChange, FileDiff,
    GitError, GitStatus, HeadState, Hunk, HunkSelection, PendingOperation, Project, ProjectError,
    ProjectSelector, ProjectService, ProjectSource, PullOptions, PullOutcome, PullStrategy,
    PushOptions, PushOutcome, RefUpdate, StageOutcome, StagePathResult, StatusRefreshOutcome,
    UpstreamStatus, Worktree, WorktreeBase,
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
    #[arg(required_unless_present_any = ["all", "hunk", "hunk_selection"], value_name = "PATH")]
    paths: Vec<PathBuf>,
    /// Stage every change, including deletions.
    #[arg(long, conflicts_with_all = ["paths", "hunk", "hunk_selection"])]
    all: bool,
    #[command(flatten)]
    hunk: HunkArguments,
}

#[derive(Debug, Args)]
struct UnstageArguments {
    #[command(flatten)]
    selection: ProjectSelection,
    /// Repository-relative or absolute paths to unstage.
    #[arg(required_unless_present_any = ["hunk", "hunk_selection"], value_name = "PATH")]
    paths: Vec<PathBuf>,
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

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("diff_target")
        .multiple(false)
        .args(["staged", "unstaged"])
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
    /// Number of unchanged lines surrounding each hunk.
    #[arg(
        long,
        value_name = "LINES",
        default_value_t = DEFAULT_DIFF_CONTEXT_LINES,
        value_parser = clap::value_parser!(u32).range(0..=i64::from(MAX_DIFF_CONTEXT_LINES)),
    )]
    context_lines: u32,
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
    fn targets(&self) -> Vec<DiffTarget> {
        match (self.staged, self.unstaged) {
            (true, false) => vec![DiffTarget::Staged],
            (false, true) => vec![DiffTarget::Unstaged],
            _ => vec![DiffTarget::Staged, DiffTarget::Unstaged],
        }
    }
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
        conflicts_with_all = ["paths", "hunk_selection"],
        requires_all = [
            "hunk_path",
            "old_blob_id",
            "new_blob_id",
            "context_lines",
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
    #[arg(long, value_name = "PATH", conflicts_with = "paths")]
    hunk_selection: Option<PathBuf>,
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

impl HunkArguments {
    /// The batch this invocation names, or `None` when it is a path operation.
    ///
    /// A returned batch is never empty: an empty selection document is a usage
    /// error rather than a silent no-op that would look like a successful
    /// stage. Clap already enforces the flag form's requirements, so the checks
    /// here exist for the document form, whose contents it cannot see.
    fn into_selections(self, consumes: &str) -> Result<Option<Vec<HunkSelection>>, CliError> {
        if let Some(source) = self.hunk_selection {
            let document = read_selection_document(&source)?;
            let selections = parse_selection_document(&document, consumes)?;
            if selections.is_empty() {
                return Err(CliError::Usage(
                    "the selection document names no hunks".to_owned(),
                ));
            }
            return Ok(Some(selections));
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
        Ok(Some(vec![HunkSelection::from_parts(
            old_path,
            new_path,
            self.old_blob_id.ok_or_else(|| missing("old_blob_id"))?,
            self.new_blob_id.ok_or_else(|| missing("new_blob_id"))?,
            self.context_lines.ok_or_else(|| missing("context_lines"))?,
            (
                self.old_start.ok_or_else(|| missing("old_start"))?,
                self.old_lines.ok_or_else(|| missing("old_lines"))?,
            ),
            (
                self.new_start.ok_or_else(|| missing("new_start"))?,
                self.new_lines.ok_or_else(|| missing("new_lines"))?,
            ),
        )]))
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
            Self::Usage(_) => "usage_error",
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
        }
    }

    fn exit_code(&self) -> u8 {
        match self {
            Self::Project(error) => project_exit_code(error),
            Self::Usage(_) => EXIT_USAGE,
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
            Self::Usage(_)
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
                    &clap_error_details(&error),
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
        GitCommand::Diff(arguments) => {
            let targets = arguments.targets();
            let git = selected_git(&service, arguments.selection)?;
            let options = DiffOptions::default()
                .with_context_lines(arguments.context_lines)
                .with_max_file_size(arguments.max_file_size)
                .with_max_total_bytes(arguments.max_total_bytes)
                .with_max_files(arguments.max_files)
                .with_paths(arguments.paths);
            if cancellation.is_cancelled() {
                return Err(GitError::Cancelled.into());
            }
            let files = git.diff_snapshot(&targets, &options)?;
            if cancellation.is_cancelled() {
                return Err(GitError::Cancelled.into());
            }
            command_result(
                json_output,
                || diff_summary_line(&files),
                || Ok(json!({ "files": files.iter().map(file_diff_value).collect::<Vec<_>>() })),
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

fn file_diff_value(file: &FileDiff) -> Value {
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
        "old_size": file.old_size,
        "new_size": file.new_size,
        "binary": file.binary,
        "omission": file.omission.as_ref().map_or(Value::Null, diff_omission_value),
        "hunks": file.hunks.iter().map(hunk_value).collect::<Vec<_>>(),
    })
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

fn hunk_value(hunk: &Hunk) -> Value {
    let (header, header_encoding) = encoded_bytes(&hunk.header);
    json!({
        "old_start": hunk.old_start,
        "old_lines": hunk.old_lines,
        "new_start": hunk.new_start,
        "new_lines": hunk.new_lines,
        "header": header,
        "header_encoding": header_encoding,
        "lines": hunk.lines.iter().map(diff_line_value).collect::<Vec<_>>(),
    })
}

fn diff_line_value(line: &DiffLine) -> Value {
    let (content, content_encoding) = encoded_bytes(&line.content);
    json!({
        "kind": diff_line_kind_name(line.kind),
        "old_line_number": line.old_line_number,
        "new_line_number": line.new_line_number,
        "content": content,
        "content_encoding": content_encoding,
    })
}

fn encoded_bytes(bytes: &[u8]) -> (String, &'static str) {
    match std::str::from_utf8(bytes) {
        Ok(text) => (text.to_owned(), "utf8"),
        Err(_) => (BASE64.encode(bytes), "base64"),
    }
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
fn diff_summary_line(files: &[FileDiff]) -> String {
    let staged = files
        .iter()
        .filter(|file| matches!(file.target, DiffTarget::Staged))
        .count();
    let mut lines = vec![format!(
        "{staged} staged, {} unstaged",
        files.len() - staged
    )];
    lines.extend(files.iter().map(|file| {
        let path = display_diff_path(file);
        let content = match (&file.omission, file.binary) {
            (Some(omission), _) => format!("\tno content ({})", omission_reason(omission)),
            (None, true) => "\tno content (binary)".to_owned(),
            (None, false) => format!("\t{} hunks", file.hunks.len()),
        };
        format!(
            "{}\t{}\t{path}{content}",
            diff_target_name(&file.target),
            file_change_name(file.change),
        )
    }));
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
        _ => "unknown",
    }
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
        },
        // The category map above names the codes; this names which code each
        // error kind actually reports. Without it a caller has to hardcode the
        // mapping, and a deliberate reclassification looks to that caller like
        // an unannounced break rather than a contract change it can observe.
        "exit_code_by_kind": {
            "cli": kind_exit_codes(CLI_KIND_EXIT_CODES),
            "project": kind_exit_codes(PROJECT_KIND_EXIT_CODES),
            "git": kind_exit_codes(GIT_KIND_EXIT_CODES),
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
        | ProjectError::WorktreeDestinationInsideDataDirectory { .. } => EXIT_REFUSED,
        ProjectError::Git(error) => git_exit_code(error),
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

fn git_exit_code(error: &GitError) -> u8 {
    match error {
        GitError::Cancelled => EXIT_CANCELLED,
        GitError::NoSuchBranch { .. }
        | GitError::NotARepository { .. }
        | GitError::RevisionNotFound { .. } => EXIT_NOT_FOUND,
        GitError::RepositoryBusy { .. }
        | GitError::AmbiguousRevision { .. }
        | GitError::NoMergeBase { .. }
        | GitError::BranchAlreadyExists { .. }
        | GitError::BranchCheckedOutInWorktree { .. }
        | GitError::WorktreeAlreadyLocked { .. }
        | GitError::WorktreeNotLocked { .. } => EXIT_CONFLICT,
        GitError::PathOutsideRepository { .. }
        | GitError::RevisionNotCommit { .. }
        | GitError::InvalidLogLimit
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
        | GitError::StaleHunkSelection { .. }
        | GitError::BinaryHunkSelection { .. }
        | GitError::RenameOnlyHunkSelection { .. }
        | GitError::MetadataOnlyHunkSelection { .. }
        | GitError::UnsupportedHunkChange { .. }
        | GitError::FilteredHunkSelection { .. }
        | GitError::OverlappingHunkSelection { .. }
        | GitError::HunkNotFound { .. } => EXIT_REFUSED,
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
        | GitError::InvalidBranchName { .. }
        | GitError::InvalidStartPoint { .. }
        | GitError::NonFastForward { .. }
        | GitError::AuthenticationFailed { .. }
        | GitError::Interrupted { .. }
        | GitError::NoRemote { .. }
        | GitError::Inspection { .. }
        | GitError::DiffContent { .. }
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
    ("ambiguous_revision", EXIT_CONFLICT),
    ("revision_not_commit", EXIT_REFUSED),
    ("no_merge_base", EXIT_CONFLICT),
    ("invalid_log_limit", EXIT_REFUSED),
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
    ("malformed_diff", EXIT_OPERATION_FAILED),
    ("stale_hunk_selection", EXIT_REFUSED),
    ("binary_hunk_selection", EXIT_REFUSED),
    ("rename_only_hunk_selection", EXIT_REFUSED),
    ("metadata_only_hunk_selection", EXIT_REFUSED),
    ("unsupported_hunk_change", EXIT_REFUSED),
    ("filtered_hunk_selection", EXIT_REFUSED),
    ("overlapping_hunk_selection", EXIT_REFUSED),
    ("hunk_not_found", EXIT_REFUSED),
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

/// The exit code every CLI-originated error kind reports, in
/// `CLI_ERROR_KINDS` order.
const CLI_KIND_EXIT_CODES: &[(&str, u8)] = &[
    ("usage_error", EXIT_USAGE),
    ("current_directory_unavailable", EXIT_OPERATION_FAILED),
    ("interrupt_handler_unavailable", EXIT_OPERATION_FAILED),
    ("wire_projection_failed", EXIT_OPERATION_FAILED),
    ("path_operation_failed", EXIT_OPERATION_FAILED),
    ("confirmation_required", EXIT_REFUSED),
    ("managed_project_requires_delete", EXIT_REFUSED),
    ("local_project_requires_forget", EXIT_REFUSED),
    ("worktree_requires_remove", EXIT_REFUSED),
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
        _ => json!({}),
    }
}

fn git_error_details(error: &GitError) -> Value {
    match error {
        GitError::RevisionNotFound { revision } | GitError::AmbiguousRevision { revision } => {
            json!({ "revision": revision })
        }
        GitError::RevisionNotCommit { revision, id } => {
            json!({ "revision": revision, "object_id": id.to_string() })
        }
        GitError::NoMergeBase { one, two } => json!({
            "one": one,
            "two": two,
        }),
        GitError::InvalidLogLimit => json!({ "minimum": 1 }),
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
        | GitError::StaleHunkSelection { path }
        | GitError::BinaryHunkSelection { path }
        | GitError::OverlappingHunkSelection { path }
        | GitError::RepositoryBusy { path }
        | GitError::NotARepository { path }
        | GitError::DiffContent { path, .. } => {
            let (path, path_is_lossy) = wire_path(path);
            json!({ "path": path, "path_is_lossy": path_is_lossy })
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
        _ => json!({}),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        io,
        path::{Path, PathBuf},
    };

    use super::{
        CLI_ERROR_KINDS, CLI_KIND_EXIT_CODES, CliError, EXIT_CANCELLED, EXIT_CONFLICT,
        EXIT_NOT_FOUND, EXIT_OPERATION_FAILED, EXIT_REFUSED, EXIT_USAGE, GIT_KIND_EXIT_CODES,
        GitError, PROJECT_KIND_EXIT_CODES, Project, ProjectError, RefusalKind, git_error_details,
        git_exit_code, parse_selection_document, project_exit_code, project_value, requested_json,
        single_line,
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
    }

    #[test]
    fn history_errors_keep_agent_facing_classification_and_details() {
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
                EXIT_CONFLICT,
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
            CliError::Usage("fixture".to_owned()),
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
        assert_eq!(kinds, CLI_ERROR_KINDS);
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
