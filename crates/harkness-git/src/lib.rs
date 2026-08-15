//! System Git integration for Harkness front ends.
//!
//! Everything Harkness does with Git goes through this crate: one command
//! runner, one per-repository lock, and the two status tiers. [`GitService`] is
//! the front door, addressed by filesystem path so that nothing here needs the
//! project catalog — and therefore nothing here can take the catalog lock out
//! of order.

mod branch;
mod clone;
mod commit;
mod context;
mod diff;
mod discard;
mod history;
mod hunk;
mod intra_line;
mod lock;
mod patch;
mod path;
mod provenance;
mod runner;
mod status;
mod sync;
#[cfg(test)]
mod testing;
mod worktree;

use std::{
    fmt, io,
    path::{Path, PathBuf},
    time::Duration,
};

use git2::{ErrorCode, Repository};
use thiserror::Error;

/// The libgit2 identity and classification types used by this crate's public
/// API. Re-exporting the exact dependency keeps downstream versions aligned.
pub use git2;

pub use branch::{Branch, BranchCheckout, BranchKind, BranchListOptions, CreateBranchOptions};
pub use commit::{
    CommitOptions, CommitOutcome, CommitScope, StageOptions, StageOutcome, StagePathOutcome,
    StagePathResult, StatusRefreshOutcome,
};
pub use context::{
    FileContextOmission, FileContextRange, FileContextRequest, FileContextResponse,
    FileContextSource, FileSide,
};
pub use diff::{
    DEFAULT_DIFF_CONTEXT_LINES, DEFAULT_MAX_DIFF_FILE_SIZE, DEFAULT_MAX_DIFF_FILES,
    DEFAULT_MAX_DIFF_TOTAL_BYTES, DiffLine, DiffLineKind, DiffOmission, DiffOptions, DiffTarget,
    ExactFileDiff, FileDiff, Hunk, IntraLineDegradation, IntraLineRange, MAX_INTRA_LINE_BYTES,
    MAX_INTRA_LINE_COMPARISONS, Whitespace, WhitespaceMode,
};
pub use discard::{
    DiscardDescription, DiscardOperation, DiscardOutcome, DiscardRecoverability, DiscardSnapshot,
    TrackedRestoreSource,
};
pub use history::{CommitInfo, CommitSignature, LogCursor, LogOptions, LogPage, LogRange};
pub use hunk::{HunkSelection, HunkStageOutcome, LineSelection, LineStageOutcome, remap_to_exact};
use lock::RepositoryLock;
pub use patch::{
    PatchFileMode, UnifiedPatch, UnifiedPatchError, UnifiedPatchFile, UnifiedPatchHunk,
    UnifiedPatchLine, parse_unified_patch, resulting_worktree_patch,
};
#[doc(hidden)]
pub use path::canonicalize_with_missing_tail;
pub use provenance::{
    AGENT_BRANCH_PREFIX, ChangeProvenance, CommitAttribution, DEFAULT_MAX_PROVENANCE_COMMITS,
    FileProvenance, MAX_CO_AUTHORS_PER_COMMIT, Producer, ProducerKind, ProvenanceGap,
    ProvenanceOptions, ProvenancePaths, ProvenanceRange, ProvenanceTruncation,
};
pub use runner::{Cancellation, CloneCancellation};
pub use status::{
    DetailedStatus, FileChange, GitStatus, HeadState, PendingOperation, StatusEntry, UpstreamStatus,
};
pub use sync::{
    FetchOptions, FetchOutcome, PullOptions, PullOutcome, PullStrategy, PushOptions, PushOutcome,
    RefUpdate,
};
pub use worktree::{AddedWorktree, GitWorktree, WorktreeBase};

pub(crate) const DEFAULT_REMOTE: &str = "origin";
pub(crate) const LOCAL_REMOTE: &str = ".";

/// Returns a stable opaque identity for a repository's shared mutation domain.
///
/// Main checkouts, linked worktrees, and canonical filesystem aliases of the
/// same Git repository return the same value because the identity is derived
/// from Git's common directory. Front ends can use it to reject conflicting
/// work before a second operation waits on the repository lock.
pub fn repository_identity(repository: impl AsRef<Path>) -> Result<String, GitError> {
    lock::repository_identity(repository.as_ref()).map(|identity| identity.to_string())
}

/// Returns the URL of the repository's preferred configured remote.
///
/// `origin` wins when present, otherwise a sole configured remote is used.
/// Repositories with no remote or with several equally plausible non-origin
/// remotes return `None`; inspection never guesses which forge owns them.
/// This is local, read-only configuration inspection and spawns no process.
pub fn repository_remote_url(repository: impl AsRef<Path>) -> Result<Option<String>, GitError> {
    let root = repository.as_ref();
    let repository = Repository::open(root).map_err(|source| inspection(root, source))?;
    let remotes = repository
        .remotes()
        .map_err(|source| inspection(root, source))?;
    let configured = remotes
        .iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| inspection(root, source))?;
    let configured = configured.into_iter().flatten().collect::<Vec<_>>();
    let selected = if configured.contains(&DEFAULT_REMOTE) {
        Some(DEFAULT_REMOTE)
    } else if configured.len() == 1 {
        configured.first().copied()
    } else {
        None
    };
    let Some(selected) = selected else {
        return Ok(None);
    };
    let remote = repository
        .find_remote(selected)
        .map_err(|source| inspection(root, source))?;
    let url = remote.url().map_err(|source| inspection(root, source))?;
    Ok(Some(url.to_owned()))
}

/// A Git inspection diagnostic without exposing the underlying libgit2 type
/// across crate boundaries.
#[derive(Debug)]
pub struct InspectionSource(git2::Error);

impl InspectionSource {
    /// Creates an inspection diagnostic with the supplied human-readable text.
    #[must_use]
    pub fn from_message(message: &str) -> Self {
        Self(git2::Error::from_str(message))
    }

    /// The libgit2 error code, for callers that need more than the stable
    /// Harkness error kind.
    #[must_use]
    pub fn code(&self) -> git2::ErrorCode {
        self.0.code()
    }

    /// The libgit2 subsystem that raised the diagnostic.
    #[must_use]
    pub fn class(&self) -> git2::ErrorClass {
        self.0.class()
    }
}

impl fmt::Display for InspectionSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for InspectionSource {}

impl From<git2::Error> for InspectionSource {
    fn from(source: git2::Error) -> Self {
        Self(source)
    }
}

/// Failures raised by Git operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum GitError {
    /// The system Git executable could not be launched.
    #[error("failed to start system Git: {source}")]
    Launch {
        #[source]
        source: io::Error,
    },

    /// Git ran and reported failure. Its diagnostic output is retained.
    #[error("git {command} failed: {stderr}")]
    Failed { command: String, stderr: String },

    /// The operation was cancelled through its [`Cancellation`].
    #[error("the Git operation was cancelled")]
    Cancelled,

    /// Git ran past the timeout for its kind of access and was killed.
    #[error("git {command} did not finish within {} seconds", timeout.as_secs())]
    TimedOut { command: String, timeout: Duration },

    /// Another operation holds the repository lock.
    #[error("another operation is already running on the repository at '{}'", path.display())]
    RepositoryBusy { path: PathBuf },

    /// The repository lock file could not be created or locked.
    #[error("failed to lock repository '{}': {source}", path.display())]
    Lock {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// The path is not the working directory of a Git repository.
    #[error("'{}' is not a Git repository", path.display())]
    NotARepository { path: PathBuf },

    /// A revision expression names no object in this repository.
    #[error("revision '{revision}' does not exist")]
    RevisionNotFound { revision: String },

    /// An abbreviated object ID names more than one object.
    #[error("revision '{revision}' is ambiguous")]
    AmbiguousRevision { revision: String },

    /// A history or diff operation requires a commit, but the revision names
    /// another kind of Git object.
    #[error("revision '{revision}' resolves to {id}, which is not a commit")]
    RevisionNotCommit { revision: String, id: git2::Oid },

    /// A commit diff's explicit comparison revision is not one of the
    /// commit's recorded parents.
    #[error("revision '{parent}' is not a parent of commit '{revision}'")]
    RevisionNotParent { revision: String, parent: String },

    /// Two revisions have no common ancestor.
    #[error("revisions '{one}' and '{two}' have no merge base")]
    NoMergeBase { one: String, two: String },

    /// A zero-sized page cannot make progress or produce a continuation.
    #[error("a commit log page limit must be greater than zero")]
    InvalidLogLimit,

    /// A continuation was copied to another range or no longer names its
    /// recorded ancestry frontier.
    #[error("commit log cursor {cursor} is not valid for the requested range")]
    InvalidLogCursor { cursor: git2::Oid },

    /// An explicit path resolves beyond the repository working tree.
    #[error(
        "path '{}' resolves outside the repository at '{}'",
        path.display(),
        repository.display()
    )]
    PathOutsideRepository { path: PathBuf, repository: PathBuf },

    /// A commit message must contain something other than whitespace.
    #[error("refusing to create a commit with an empty message")]
    EmptyCommitMessage,

    /// The index has no changes to commit.
    #[error("nothing is staged; set CommitOptions::allow_empty to create an empty commit")]
    NothingStaged,

    /// There is no existing commit for an amend operation to replace.
    #[error("cannot amend because the checked-out branch has no commits")]
    AmendUnbornBranch,

    /// A branch name fails Git's `check-ref-format --branch` rules.
    #[error("'{name}' is not a valid branch name")]
    InvalidBranchName { name: String },

    /// A branch or remote-tracking ref required by the operation does not exist.
    #[error("branch '{branch}' does not exist")]
    NoSuchBranch { branch: String },

    /// Creating or renaming a branch would overwrite an existing local branch.
    #[error("local branch '{branch}' already exists")]
    BranchAlreadyExists { branch: String },

    /// A branch creation start point does not resolve to a commit.
    #[error("'{start_point}' is not a valid branch start point")]
    InvalidStartPoint { start_point: String },

    /// The branch is checked out in the working tree being addressed.
    #[error("refusing to delete '{branch}', the currently checked-out branch")]
    CurrentBranchDeletion { branch: String },

    /// The branch is the locally recorded default branch of the repository.
    #[error("refusing to delete '{branch}', the repository's default branch")]
    DefaultBranchDeletion { branch: String },

    /// Another worktree has the branch checked out.
    #[error(
        "refusing to delete '{branch}', which is checked out in the worktree at '{}'",
        worktree.display()
    )]
    BranchCheckedOutInWorktree { branch: String, worktree: PathBuf },

    /// Worktree creation never reuses a filesystem entry, including a
    /// dangling symlink that [`Path::exists`] would overlook.
    #[error("worktree destination '{}' already exists", path.display())]
    WorktreeAddDestinationExists { path: PathBuf },

    /// The worktree destination could not be inspected safely before Git ran.
    #[error(
        "worktree destination '{}' cannot be inspected: {source}",
        path.display()
    )]
    WorktreeAddDestinationUnavailable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// A failed worktree add left either its checkout or its administrative
    /// record behind after the complete targeted cleanup sequence.
    #[error("failed to clean worktree destination '{}': {detail}", path.display())]
    WorktreeAddCleanup { path: PathBuf, detail: String },

    /// A worktree was explicitly locked by Git against protected mutations.
    ///
    /// The message names no single operation because one lock refuses index,
    /// history, discard, and lifecycle mutations alike, and `--force`
    /// overrides none of them.
    #[error(
        "worktree at '{}' is locked{}; run 'worktree unlock' before changing it",
        path.display(),
        reason.as_deref().map(|reason| format!(": {reason}")).unwrap_or_default()
    )]
    WorktreeLocked {
        path: PathBuf,
        reason: Option<String>,
    },

    /// Harkness requires every new worktree lock to explain its purpose.
    #[error("a worktree lock reason cannot be empty")]
    EmptyWorktreeLockReason,

    /// A caller tried to replace an existing worktree lock implicitly.
    #[error(
        "worktree at '{}' is already locked{}",
        path.display(),
        reason.as_deref().map(|reason| format!(": {reason}")).unwrap_or_default()
    )]
    WorktreeAlreadyLocked {
        path: PathBuf,
        reason: Option<String>,
    },

    /// A caller tried to unlock a worktree that has no lock.
    #[error("worktree at '{}' is not locked", path.display())]
    WorktreeNotLocked { path: PathBuf },

    /// Git cannot relocate a linked checkout with a filesystem rename.
    #[error(
        "worktree at '{}' cannot move across filesystems to '{}': {stderr}",
        worktree.display(),
        destination.display()
    )]
    WorktreeMoveAcrossDevices {
        worktree: PathBuf,
        destination: PathBuf,
        stderr: String,
    },

    /// The branch contains commits not merged into its upstream or HEAD.
    #[error(
        "refusing to delete unmerged branch '{branch}'; explicitly force the deletion to continue"
    )]
    UnmergedBranchDeletion { branch: String },

    /// The remote rejected the update because it holds commits this repository
    /// does not have.
    #[error("git {command} was rejected: the remote has commits this branch does not: {stderr}")]
    NonFastForward { command: String, stderr: String },

    /// Git could not prove who it was to the remote.
    ///
    /// Recognized from Git's own diagnostic, once and here, so that no caller
    /// has to match on the text of standard error to tell a credential problem
    /// from a missing repository.
    #[error("git {command} could not authenticate to the remote: {stderr}")]
    AuthenticationFailed { command: String, stderr: String },

    /// The branch tracks nothing, so there is no branch to reconcile with or
    /// publish to.
    #[error("branch '{branch}' has no upstream branch configured")]
    NoUpstream { branch: String },

    /// The branch exists but has no commit yet, so a commit-based operation
    /// cannot proceed.
    ///
    /// Distinct from [`NoUpstream`]: asking for an upstream would not help, and
    /// Git's own diagnostic for it names a refspec the caller never wrote.
    ///
    /// [`NoUpstream`]: GitError::NoUpstream
    #[error("branch '{branch}' in '{}' has no commits", path.display())]
    UnbornBranch { path: PathBuf, branch: String },

    /// The branch tracks `.`, which is Git's name for the repository itself
    /// rather than a remote.
    ///
    /// A perfectly ordinary upstream for a branch built on another local
    /// branch, and one a pull honors. Reported distinctly because the
    /// alternative was to claim the remote was not configured, which sent the
    /// caller looking for a configuration mistake that was never made.
    #[error(
        "branch '{branch}' tracks '.', the repository itself, which is a valid \
         upstream to pull from but not something to push to"
    )]
    LocalUpstreamUnsupported { branch: String },

    /// The repository is already in the middle of an operation, so another
    /// guarded operation cannot safely begin.
    ///
    /// Refused before Git is spawned. Left to Git, the failure would be
    /// indistinguishable from one this operation had caused.
    #[error(
        "'{}' is in the middle of a {pending}; finish or abort it before continuing",
        path.display()
    )]
    OperationInProgress {
        path: PathBuf,
        pending: PendingOperation,
    },

    /// An operation failed part-way and left the repository mid-operation.
    ///
    /// The failure that caused it is the [`source`]; the point of the variant
    /// is everything around it. A caller that treats an error as "nothing
    /// happened" is wrong here: the index and the working tree have changed,
    /// [`status`] describes them as they now are, and nothing else will run
    /// against this repository until the [`pending`] operation is finished or
    /// aborted.
    ///
    /// [`source`]: std::error::Error::source
    /// [`status`]: GitError::Interrupted::status
    /// [`pending`]: GitError::Interrupted::pending
    #[error(
        "git {command} left '{}' in the middle of a {pending} that has to be \
         resolved: {source}",
        path.display()
    )]
    Interrupted {
        command: String,
        path: PathBuf,
        pending: PendingOperation,
        /// Boxed because this is the only variant carrying a whole status, and
        /// every `Result<_, GitError>` in the crate would otherwise be as wide
        /// as the rarest failure in it.
        status: Option<Box<GitStatus>>,
        #[source]
        source: Box<GitError>,
    },

    /// The named remote does not exist, or none could be chosen.
    #[error("{}", match remote {
        Some(remote) => format!("remote '{remote}' is not configured for this repository"),
        None => "no remote was named, and this repository has neither an 'origin' \
                 nor a single remote to fall back to".to_owned(),
    })]
    NoRemote { remote: Option<String> },

    /// Pushing to the remote's default branch was refused.
    ///
    /// The branch is the one the remote names as its default *now*, asked of
    /// the remote itself rather than read from what a clone recorded once.
    #[error(
        "refusing to push to '{branch}', the default branch of '{remote}'; \
         set PushOptions::allow_default_branch to push to it anyway"
    )]
    DefaultBranchPush { remote: String, branch: String },

    /// The remote's default branch could not be determined, so the refusal
    /// above cannot be evaluated. Deliberately distinct: assuming `main` would
    /// let a push through on exactly the repositories that cannot be checked.
    #[error(
        "the default branch of '{remote}' is neither advertised by the remote nor \
         recorded in refs/remotes/{remote}/HEAD; run 'git remote set-head {remote} --auto', \
         or set PushOptions::allow_default_branch"
    )]
    DefaultBranchUnknown { remote: String },

    /// A commit is checked out rather than a branch, and the operation is about
    /// a branch.
    #[error("'{}' has no branch checked out: {detail}", path.display())]
    DetachedHead { path: PathBuf, detail: String },

    /// Git metadata could not be inspected for a readable directory.
    #[error("failed to inspect Git metadata for '{}': {source}", path.display())]
    Inspection {
        path: PathBuf,
        #[source]
        source: InspectionSource,
    },

    /// Working-tree content could not be read while computing its blob ID.
    #[error("failed to read diff content for '{}': {source}", path.display())]
    DiffContent {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// The paths, index, or `HEAD` changed after destructive confirmation.
    #[error("the selected changes are stale; refresh and confirm the discard again")]
    StaleDiscardSelection,

    /// A tracked-content restore was asked to delete a path Git does not track.
    #[error(
        "'{}' is untracked; delete it only through the explicit untracked-file operation",
        path.display()
    )]
    UntrackedDiscardRequiresDelete { path: PathBuf },

    /// An untracked-file deletion was asked to remove a path Git tracks.
    #[error(
        "'{}' is tracked by Git; restore it through a tracked-content operation instead",
        path.display()
    )]
    TrackedDiscardRequiresRestore { path: PathBuf },

    /// A conflicted index has no single baseline from which content can be restored.
    #[error("'{}' is unmerged; resolve the conflict before discarding it", path.display())]
    UnmergedDiscard { path: PathBuf },

    /// The selected path has no change at the requested discard boundary.
    #[error("'{}' has no changes to discard at the requested boundary", path.display())]
    NothingToDiscard { path: PathBuf },

    /// Untracked deletion is deliberately file-only and never recurses.
    #[error("'{}' is not an untracked file; directories are never deleted recursively", path.display())]
    UntrackedDiscardNotFile { path: PathBuf },

    /// A validated untracked file could not be removed from the filesystem.
    #[error("failed to delete untracked file '{}': {source}", path.display())]
    UntrackedDiscardIo {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// A file-context request did not contain a complete object ID valid for
    /// its selected source.
    #[error("'{blob_id}' is not a valid full blob object ID for this context source")]
    InvalidBlobId { blob_id: String },

    /// No blob object exists at the requested object ID.
    #[error("blob '{blob_id}' was not found in the repository")]
    BlobNotFound { blob_id: String },

    /// Libgit2 returned a diff record that violates Harkness's model contract.
    #[error("Git produced a diff that could not be represented: {detail}")]
    MalformedDiff { detail: String },

    /// A hunk selection no longer names the blobs on the requested side of the index.
    #[error("the selected hunks for '{}' are stale; refresh the diff before retrying", path.display())]
    StaleHunkSelection { path: PathBuf },

    /// A selection was taken from a diff that does not describe the file.
    ///
    /// A whitespace-insensitive comparison omits lines that genuinely differ on
    /// disk, so a patch rendered from its hunks would write the wrong bytes.
    /// Refused before the repository lock is taken and before anything is
    /// recomputed, because an exact recomputation could match these coordinates
    /// by coincidence on a hunk whose interior differs.
    #[error(
        "the selection for '{}' was taken from a diff whose whitespace handling was \
         {whitespace}, which omits lines that differ on disk; recompute it with exact \
         whitespace and blank lines included, and select from that",
        path.display()
    )]
    WhitespaceInsensitiveSelection {
        path: PathBuf,
        whitespace: Whitespace,
    },

    /// An exact recomputation found changes the whitespace-insensitive view hid.
    ///
    /// Raised only by [`remap_to_exact`]. The region a caller picked in a
    /// relaxed view maps onto exact hunks that carry additional changed lines,
    /// so applying them would act on content the user was never shown.
    #[error(
        "the region selected in '{}' also contains whitespace-only differences that this view \
         hides; switch the diff to exact whitespace to see and select them",
        path.display()
    )]
    HiddenWhitespaceChanges { path: PathBuf },

    /// Binary content cannot be staged below path granularity.
    #[error("binary file '{}' does not support hunk staging", path.display())]
    BinaryHunkSelection { path: PathBuf },

    /// A rename without content changes has no hunk that can be staged.
    #[error(
        "rename from '{}' to '{}' has no content hunk; stage the paths instead",
        old_path.display(),
        new_path.display()
    )]
    RenameOnlyHunkSelection {
        old_path: PathBuf,
        new_path: PathBuf,
    },

    /// A change to file metadata alone has no hunk that can be staged.
    #[error(
        "'{}' changes only its file mode ({old_mode:o} to {new_mode:o}); stage the path instead",
        path.display()
    )]
    MetadataOnlyHunkSelection {
        path: PathBuf,
        old_mode: u32,
        new_mode: u32,
    },

    /// The kind of change cannot be expressed as an index-only hunk apply.
    #[error(
        "'{}' is a {change} record, which does not support hunk staging; stage the path instead",
        path.display()
    )]
    UnsupportedHunkChange { path: PathBuf, change: FileChange },

    /// Content Git would rewrite through an external filter driver.
    #[error(
        "'{}' is filtered by the '{driver}' driver, which hunk staging cannot run; stage the path instead",
        path.display()
    )]
    FilteredHunkSelection { path: PathBuf, driver: String },

    /// Two selections for one path cover the same lines.
    #[error(
        "two selected hunks for '{}' cover the same lines; stage them one batch at a time",
        path.display()
    )]
    OverlappingHunkSelection { path: PathBuf },

    /// The selected coordinates are not present in the current diff.
    #[error(
        "the diff for '{}' does not contain hunk -{},{} +{},{}",
        path.display(), old_start, old_lines, new_start, new_lines
    )]
    HunkNotFound {
        path: PathBuf,
        old_start: u32,
        old_lines: u32,
        new_start: u32,
        new_lines: u32,
    },

    /// The selected changed-line coordinates are not present in the named hunk.
    #[error(
        "the diff for '{}' does not contain changed line old={:?} new={:?}",
        path.display(), old_line_number, new_line_number
    )]
    LineNotFound {
        path: PathBuf,
        old_line_number: Option<u32>,
        new_line_number: Option<u32>,
    },

    /// The selected lines cannot stand alone as a patch.
    ///
    /// Retaining an unselected change puts a line that ends the file without a
    /// final newline ahead of a line that must follow it. No patch can express
    /// that, so the selection is refused rather than applied approximately.
    #[error(
        "the selected lines for '{}' cannot be applied on their own: a line with no final newline would not be last; select the rest of the change too",
        path.display()
    )]
    UnrepresentableLineSelection { path: PathBuf },

    /// Libgit2 could not parse or atomically apply a rebuilt hunk patch.
    ///
    /// The batch is atomic, so the failure names every path it covered rather
    /// than pretending libgit2's line-oriented message identifies one of them.
    #[error("failed to apply the selected hunks for {}: {source}", describe_paths(.paths))]
    HunkApplication {
        paths: Vec<PathBuf>,
        #[source]
        source: git2::Error,
    },

    /// Git reported a status Harkness cannot parse.
    #[error("Git reported a status that could not be parsed: {detail}")]
    MalformedStatus { detail: String },
}

impl GitError {
    /// Every stable discriminant this error namespace can emit.
    pub const KINDS: &'static [&'static str] = &[
        "launch",
        "failed",
        "cancelled",
        "timed_out",
        "repository_busy",
        "lock",
        "not_a_repository",
        "revision_not_found",
        "ambiguous_revision",
        "revision_not_commit",
        "revision_not_parent",
        "no_merge_base",
        "invalid_log_limit",
        "invalid_log_cursor",
        "path_outside_repository",
        "empty_commit_message",
        "nothing_staged",
        "amend_unborn_branch",
        "invalid_branch_name",
        "no_such_branch",
        "branch_already_exists",
        "invalid_start_point",
        "current_branch_deletion",
        "default_branch_deletion",
        "branch_checked_out_in_worktree",
        "worktree_add_destination_exists",
        "worktree_add_destination_unavailable",
        "worktree_add_cleanup",
        "worktree_locked",
        "empty_worktree_lock_reason",
        "worktree_already_locked",
        "worktree_not_locked",
        "worktree_move_across_devices",
        "unmerged_branch_deletion",
        "non_fast_forward",
        "authentication_failed",
        "no_upstream",
        "unborn_branch",
        "local_upstream_unsupported",
        "operation_in_progress",
        "interrupted",
        "no_remote",
        "default_branch_push",
        "default_branch_unknown",
        "detached_head",
        "inspection",
        "diff_content",
        "stale_discard_selection",
        "untracked_discard_requires_delete",
        "tracked_discard_requires_restore",
        "unmerged_discard",
        "nothing_to_discard",
        "untracked_discard_not_file",
        "untracked_discard_io",
        "invalid_blob_id",
        "blob_not_found",
        "malformed_diff",
        "stale_hunk_selection",
        "whitespace_insensitive_selection",
        "hidden_whitespace_changes",
        "binary_hunk_selection",
        "rename_only_hunk_selection",
        "metadata_only_hunk_selection",
        "unsupported_hunk_change",
        "filtered_hunk_selection",
        "overlapping_hunk_selection",
        "hunk_not_found",
        "line_not_found",
        "unrepresentable_line_selection",
        "hunk_application",
        "malformed_status",
    ];

    /// Stable machine-readable discriminant for agent-facing error handling.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Launch { .. } => "launch",
            Self::Failed { .. } => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut { .. } => "timed_out",
            Self::RepositoryBusy { .. } => "repository_busy",
            Self::Lock { .. } => "lock",
            Self::NotARepository { .. } => "not_a_repository",
            Self::RevisionNotFound { .. } => "revision_not_found",
            Self::AmbiguousRevision { .. } => "ambiguous_revision",
            Self::RevisionNotCommit { .. } => "revision_not_commit",
            Self::RevisionNotParent { .. } => "revision_not_parent",
            Self::NoMergeBase { .. } => "no_merge_base",
            Self::InvalidLogLimit => "invalid_log_limit",
            Self::InvalidLogCursor { .. } => "invalid_log_cursor",
            Self::PathOutsideRepository { .. } => "path_outside_repository",
            Self::EmptyCommitMessage => "empty_commit_message",
            Self::NothingStaged => "nothing_staged",
            Self::AmendUnbornBranch => "amend_unborn_branch",
            Self::InvalidBranchName { .. } => "invalid_branch_name",
            Self::NoSuchBranch { .. } => "no_such_branch",
            Self::BranchAlreadyExists { .. } => "branch_already_exists",
            Self::InvalidStartPoint { .. } => "invalid_start_point",
            Self::CurrentBranchDeletion { .. } => "current_branch_deletion",
            Self::DefaultBranchDeletion { .. } => "default_branch_deletion",
            Self::BranchCheckedOutInWorktree { .. } => "branch_checked_out_in_worktree",
            Self::WorktreeAddDestinationExists { .. } => "worktree_add_destination_exists",
            Self::WorktreeAddDestinationUnavailable { .. } => {
                "worktree_add_destination_unavailable"
            }
            Self::WorktreeAddCleanup { .. } => "worktree_add_cleanup",
            Self::WorktreeLocked { .. } => "worktree_locked",
            Self::EmptyWorktreeLockReason => "empty_worktree_lock_reason",
            Self::WorktreeAlreadyLocked { .. } => "worktree_already_locked",
            Self::WorktreeNotLocked { .. } => "worktree_not_locked",
            Self::WorktreeMoveAcrossDevices { .. } => "worktree_move_across_devices",
            Self::UnmergedBranchDeletion { .. } => "unmerged_branch_deletion",
            Self::NonFastForward { .. } => "non_fast_forward",
            Self::AuthenticationFailed { .. } => "authentication_failed",
            Self::NoUpstream { .. } => "no_upstream",
            Self::UnbornBranch { .. } => "unborn_branch",
            Self::LocalUpstreamUnsupported { .. } => "local_upstream_unsupported",
            Self::OperationInProgress { .. } => "operation_in_progress",
            Self::Interrupted { .. } => "interrupted",
            Self::NoRemote { .. } => "no_remote",
            Self::DefaultBranchPush { .. } => "default_branch_push",
            Self::DefaultBranchUnknown { .. } => "default_branch_unknown",
            Self::DetachedHead { .. } => "detached_head",
            Self::Inspection { .. } => "inspection",
            Self::DiffContent { .. } => "diff_content",
            Self::StaleDiscardSelection => "stale_discard_selection",
            Self::UntrackedDiscardRequiresDelete { .. } => "untracked_discard_requires_delete",
            Self::TrackedDiscardRequiresRestore { .. } => "tracked_discard_requires_restore",
            Self::UnmergedDiscard { .. } => "unmerged_discard",
            Self::NothingToDiscard { .. } => "nothing_to_discard",
            Self::UntrackedDiscardNotFile { .. } => "untracked_discard_not_file",
            Self::UntrackedDiscardIo { .. } => "untracked_discard_io",
            Self::InvalidBlobId { .. } => "invalid_blob_id",
            Self::BlobNotFound { .. } => "blob_not_found",
            Self::MalformedDiff { .. } => "malformed_diff",
            Self::StaleHunkSelection { .. } => "stale_hunk_selection",
            Self::WhitespaceInsensitiveSelection { .. } => "whitespace_insensitive_selection",
            Self::HiddenWhitespaceChanges { .. } => "hidden_whitespace_changes",
            Self::BinaryHunkSelection { .. } => "binary_hunk_selection",
            Self::RenameOnlyHunkSelection { .. } => "rename_only_hunk_selection",
            Self::MetadataOnlyHunkSelection { .. } => "metadata_only_hunk_selection",
            Self::UnsupportedHunkChange { .. } => "unsupported_hunk_change",
            Self::FilteredHunkSelection { .. } => "filtered_hunk_selection",
            Self::OverlappingHunkSelection { .. } => "overlapping_hunk_selection",
            Self::HunkNotFound { .. } => "hunk_not_found",
            Self::LineNotFound { .. } => "line_not_found",
            Self::UnrepresentableLineSelection { .. } => "unrepresentable_line_selection",
            Self::HunkApplication { .. } => "hunk_application",
            Self::MalformedStatus { .. } => "malformed_status",
        }
    }
}

/// Names the paths one atomic hunk batch covered, for a failure message.
fn describe_paths(paths: &[PathBuf]) -> String {
    match paths {
        [] => "the requested hunks".to_owned(),
        [path] => format!("'{}'", path.display()),
        [path, rest @ ..] => format!("'{}' and {} more", path.display(), rest.len()),
    }
}

/// Git operations on one repository.
///
/// Stateless and addressed by path. It deliberately has no project-catalog
/// dependency: the embedding layer resolves project identities and releases
/// its catalog lock before constructing this service. That keeps repository
/// locking independent of, and ordered before, any later catalog lock.
///
/// # What a front end has to provide
///
/// The synchronizing verbs are blocking, and the contract they need from a
/// caller with an event loop is the same for all three:
///
/// - **Worker thread.** [`fetch`], [`pull`] and [`push`] block until Git exits.
///   Calling one on a UI thread freezes the window for the length of a network
///   operation, which has no upper bound.
/// - **Pending state.** The repository lock is exclusive, so a second operation
///   on the same repository fails with [`GitError::RepositoryBusy`] rather than
///   queueing. A front end that lets a user press *Pull* twice has to disable
///   the control itself; this service will not serialize on its behalf.
/// - **Cancellation.** Every verb takes a [`Cancellation`], and cancelling
///   kills Git's whole process group. A cancelled [`pull`] is the one case that
///   can leave work behind: see the recovery rule below.
/// - **Refresh after failure, not just after success.** Every outcome carries
///   the status the repository had once the operation finished, so the success
///   path needs no second inspection. Failures carry it only where it exists —
///   [`GitError::Interrupted`] — so any other error should be followed by a
///   [`status`] or [`detailed_status`] call rather than by the assumption that
///   nothing moved.
/// - **Conflict recovery.** [`GitError::Interrupted`] means the working tree
///   changed and Git is waiting: the front end has to offer the user a way to
///   resolve or abort, because every later operation on that repository will
///   refuse with [`GitError::OperationInProgress`] until it does.
///
/// [`fetch`]: GitService::fetch
/// [`pull`]: GitService::pull
/// [`push`]: GitService::push
/// [`status`]: GitService::status
/// [`detailed_status`]: GitService::detailed_status
#[derive(Clone, Debug)]
pub struct GitService {
    root: PathBuf,
    data_dir: PathBuf,
    git_executable: PathBuf,
}

impl GitService {
    /// Addresses the repository whose working directory is `root`.
    ///
    /// `data_dir` is the embedding application's data directory. Repository
    /// locks always live below its stable `locks/` child; callers cannot choose
    /// a second lock namespace accidentally. Nothing is created there until a
    /// mutation lock is taken.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            data_dir: data_dir.into(),
            git_executable: PathBuf::from("git"),
        }
    }

    /// Runs a different Git executable than the one on `PATH`.
    #[must_use]
    pub fn with_git_executable(mut self, git_executable: impl Into<PathBuf>) -> Self {
        self.git_executable = git_executable.into();
        self
    }

    /// The working directory every command runs in.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Describes the repository cheaply and in process.
    ///
    /// `None` means the root is not the working tree of a repository. Spawns
    /// nothing, because this runs for every catalog entry on every read.
    pub fn status(&self) -> Result<Option<GitStatus>, GitError> {
        status::inspect(&self.root)
    }

    /// Reads the checked-out branch, unborn branch, or detached commit in process.
    pub fn head_state(&self) -> Result<Option<HeadState>, GitError> {
        status::head(&self.root)
    }

    /// Classifies workspace-relative paths through Git's ignore rules in process.
    ///
    /// A plain non-repository workspace returns `false` for every candidate.
    pub fn ignored_paths(&self, candidates: &[PathBuf]) -> Result<Vec<bool>, GitError> {
        status::ignored(&self.root, candidates)
    }

    /// Describes every changed path in the repository.
    ///
    /// One `git status` spawn, so it is computed only for a project a caller
    /// names rather than for a whole listing.
    pub fn detailed_status(&self, cancellation: &Cancellation) -> Result<DetailedStatus, GitError> {
        status::detailed(&self.git_executable, &self.root, cancellation)
    }

    /// Describes every changed path in process, without a lock or child process.
    pub fn detailed_status_in_process(
        &self,
        cancellation: &Cancellation,
    ) -> Result<DetailedStatus, GitError> {
        status::detailed_in_process(&self.root, cancellation)
    }

    /// Clones `remote` to an explicit destination using this service's working
    /// directory and Git executable.
    ///
    /// The caller must validate `remote` before passing it here; every Git URL
    /// form, local path, and transport helper reaches Git unchanged. A relative
    /// destination resolves beneath [`GitService::root`], while an absolute
    /// destination is used as written.
    ///
    /// Clone creates a repository, so there is no repository lock to take yet.
    pub fn clone_to(
        &self,
        remote: &str,
        destination: impl AsRef<Path>,
        cancellation: &Cancellation,
        mut on_progress: impl FnMut(String),
    ) -> Result<(), GitError> {
        clone::run(
            &self.git_executable,
            &self.root,
            remote,
            destination.as_ref(),
            cancellation,
            &mut on_progress,
        )
    }

    /// Lists the repository's main and linked worktrees without taking its
    /// mutation lock. Use [`LockedRepository::worktrees_while_locked`] when a
    /// later lifecycle mutation must retain the same mutation capability.
    pub fn worktrees(&self, cancellation: &Cancellation) -> Result<Vec<GitWorktree>, GitError> {
        worktree::list(&self.git_executable, &self.root, cancellation)
    }

    /// Lists one bounded page of commits, newest first.
    ///
    /// History inspection runs entirely through libgit2: it takes no
    /// repository lock, spawns no process and never contacts a remote. A
    /// continuation cursor is anchored at the first commit of the next page
    /// and retains every pending ancestry path, so later commits added above it
    /// cannot move that page and merges cannot strand an unvisited parent.
    pub fn log(
        &self,
        options: &LogOptions,
        cancellation: &Cancellation,
    ) -> Result<LogPage, GitError> {
        history::log(&self.root, options, cancellation)
    }

    /// Resolves a Git revision expression to the object it names.
    ///
    /// Branches, tags, full and abbreviated object IDs are accepted using
    /// libgit2's revision syntax. Missing and ambiguous expressions remain
    /// distinct typed errors for front ends.
    pub fn resolve_revision(&self, revision: &str) -> Result<git2::Oid, GitError> {
        history::resolve_revision(&self.root, revision)
    }

    /// Finds the best common ancestor of two commit-ish revisions.
    ///
    /// This is local, read-only inspection and therefore takes no repository
    /// lock and spawns no process.
    pub fn merge_base(&self, one: &str, two: &str) -> Result<git2::Oid, GitError> {
        history::merge_base(&self.root, one, two)
    }

    /// Computes structured content differences for one target.
    ///
    /// [`DiffTarget::Staged`] compares `HEAD` (or the empty tree before the
    /// first commit) with the index. [`DiffTarget::Unstaged`] compares the
    /// index with the working tree, including untracked files. Revision targets
    /// compare commit trees, a commit with its parent, a revision with the
    /// working tree, or a branch with its merge-base. Every revision expression
    /// uses the same resolver as [`Self::resolve_revision`]. The operation runs
    /// entirely in process, takes no repository lock, and never mutates the
    /// index while inspecting it.
    pub fn diff(
        &self,
        target: DiffTarget,
        options: &DiffOptions,
    ) -> Result<Vec<FileDiff>, GitError> {
        diff::compute(&self.root, target, options)
    }

    /// Computes several targets against one repository and index snapshot.
    ///
    /// Records are returned in the order the targets are given. Every target is
    /// resolved through one open repository, and every index-backed target uses
    /// the same open index. Prefer this to separate [`Self::diff`] calls when a
    /// coherent multi-target view is needed. [`DiffOptions`] budgets apply to
    /// the combined model rather than to each target, so the whole response
    /// stays bounded.
    pub fn diff_snapshot(
        &self,
        targets: &[DiffTarget],
        options: &DiffOptions,
    ) -> Result<Vec<FileDiff>, GitError> {
        diff::compute_targets(&self.root, targets, options)
    }

    /// Attributes each path of one comparison to the commits that produced it.
    ///
    /// This answers "what produced this file" beside the diff's "what changed",
    /// and it answers it from the repository alone: the commits between the two
    /// sides of the comparison, the identities they record, and the
    /// `agent/<slug>` branch convention. Pass one target's own file list
    /// through [`ProvenanceOptions::for_files`]: the result carries one entry
    /// per record, in the same order, so the two pair by index. A file list
    /// that came back empty is fine to pass — [`ProvenancePaths::Only`] with
    /// nothing in it asks about nothing, and is answered without a walk.
    ///
    /// The range is walked once and each commit compared with its first parent
    /// once, so cost follows the size of the range and never the number of
    /// files being reviewed; nothing here walks history per path. Like every
    /// other read on this service it runs entirely in process, takes no
    /// repository lock, and spawns no process.
    ///
    /// The result is total and advisory. Every requested path is reported, a
    /// path nothing could be attributed to carries a
    /// [`ProvenanceGap`] naming why, and nothing about staging, discarding or
    /// diffing may change behaviour because of what it says.
    pub fn provenance(
        &self,
        target: &DiffTarget,
        options: &ProvenanceOptions,
        cancellation: &Cancellation,
    ) -> Result<ChangeProvenance, GitError> {
        provenance::resolve(&self.root, target, options, cancellation)
    }

    /// Retrieves bounded source context without recomputing a diff.
    ///
    /// Immutable sides are addressed by the blob IDs recorded in [`FileDiff`],
    /// so later index or working-tree changes cannot move their content. A
    /// working-tree source is guarded by its recorded hash and refuses with
    /// [`GitError::StaleHunkSelection`] when the path no longer produces the
    /// same raw or clean-filtered representation.
    /// The operation runs entirely in process, takes no repository lock and
    /// never spawns system Git.
    pub fn file_context(
        &self,
        request: &FileContextRequest,
    ) -> Result<FileContextResponse, GitError> {
        context::load(&self.root, request)
    }

    /// Restores tracked paths from an explicit Git boundary.
    ///
    /// [`TrackedRestoreSource::Index`] changes only the working tree, preserving
    /// staged content. [`TrackedRestoreSource::Head`] restores both the index
    /// and working tree. Untracked paths and unmerged paths are refused before
    /// Git is spawned, and a linked-worktree lock is never bypassed.
    pub fn restore_tracked<I, P>(
        &self,
        paths: I,
        source: TrackedRestoreSource,
        cancellation: &Cancellation,
    ) -> Result<DiscardOutcome, GitError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let paths = paths
            .into_iter()
            .map(|path| path.as_ref().to_path_buf())
            .collect::<Vec<_>>();
        let lock = self.acquire_lock(cancellation)?;
        discard::restore_tracked(
            &self.git_executable,
            &self.root,
            &lock,
            &paths,
            source,
            None,
            cancellation,
        )
    }

    /// Captures the exact worktree, index, and `HEAD` state a discard confirmation describes.
    pub fn discard_snapshot<I, P>(&self, paths: I) -> Result<DiscardSnapshot, GitError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let paths = paths
            .into_iter()
            .map(|path| path.as_ref().to_path_buf())
            .collect::<Vec<_>>();
        discard::snapshot(&self.root, &paths)
    }

    /// Restores tracked paths only if they still match a previously captured snapshot.
    pub fn restore_tracked_if_unchanged<I, P>(
        &self,
        paths: I,
        source: TrackedRestoreSource,
        expected: &DiscardSnapshot,
        cancellation: &Cancellation,
    ) -> Result<DiscardOutcome, GitError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let paths = paths
            .into_iter()
            .map(|path| path.as_ref().to_path_buf())
            .collect::<Vec<_>>();
        let lock = self.acquire_lock(cancellation)?;
        discard::restore_tracked(
            &self.git_executable,
            &self.root,
            &lock,
            &paths,
            source,
            Some(expected),
            cancellation,
        )
    }

    /// Deletes explicit untracked files without ever recursing into a directory.
    ///
    /// Every path is validated as an untracked file before the first one is
    /// removed. Tracked and unmerged paths are typed refusals, and a worktree
    /// lock cannot be overridden.
    pub fn delete_untracked<I, P>(
        &self,
        paths: I,
        cancellation: &Cancellation,
    ) -> Result<DiscardOutcome, GitError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let paths = paths
            .into_iter()
            .map(|path| path.as_ref().to_path_buf())
            .collect::<Vec<_>>();
        let lock = self.acquire_lock(cancellation)?;
        discard::delete_untracked(
            &self.git_executable,
            &self.root,
            &lock,
            &paths,
            None,
            cancellation,
        )
    }

    /// Deletes untracked files only if they still match a previously captured snapshot.
    pub fn delete_untracked_if_unchanged<I, P>(
        &self,
        paths: I,
        expected: &DiscardSnapshot,
        cancellation: &Cancellation,
    ) -> Result<DiscardOutcome, GitError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let paths = paths
            .into_iter()
            .map(|path| path.as_ref().to_path_buf())
            .collect::<Vec<_>>();
        let lock = self.acquire_lock(cancellation)?;
        discard::delete_untracked(
            &self.git_executable,
            &self.root,
            &lock,
            &paths,
            Some(expected),
            cancellation,
        )
    }

    /// Discards selected tracked working-tree hunks while leaving the index alone.
    ///
    /// Selections are recomputed and revalidated under the repository lock,
    /// then the trusted reverse patch is applied to the working tree. Untracked
    /// and unmerged content is refused rather than being deleted implicitly.
    /// A selection taken from a whitespace-insensitive diff is refused with
    /// [`GitError::WhitespaceInsensitiveSelection`] before the lock is taken.
    pub fn discard_hunks(
        &self,
        selections: &[HunkSelection],
        cancellation: &Cancellation,
    ) -> Result<DiscardOutcome, GitError> {
        hunk::refuse_whitespace_insensitive(selections)?;
        let lock = self.acquire_lock(cancellation)?;
        discard::discard_hunks(
            &self.git_executable,
            &self.root,
            &lock,
            selections,
            cancellation,
        )
    }

    /// Discards selected tracked working-tree lines while leaving the index alone.
    ///
    /// Each line retains its enclosing hunk identity, so the same stale-safe
    /// recomputation used for line staging protects this destructive direction.
    /// A selection taken from a whitespace-insensitive diff is refused with
    /// [`GitError::WhitespaceInsensitiveSelection`] before the lock is taken.
    pub fn discard_lines(
        &self,
        selections: &[LineSelection],
        cancellation: &Cancellation,
    ) -> Result<DiscardOutcome, GitError> {
        hunk::refuse_whitespace_insensitive(selections)?;
        let lock = self.acquire_lock(cancellation)?;
        discard::discard_lines(
            &self.git_executable,
            &self.root,
            &lock,
            selections,
            cancellation,
        )
    }

    /// Stages selected working-tree hunks without writing the working tree.
    ///
    /// Selections must come from an unstaged [`Self::diff`] result. Their blob
    /// IDs and coordinates are revalidated under the repository lock before a
    /// byte-preserving patch is rebuilt and applied atomically to the index:
    /// the whole batch lands or the index is left exactly as it was.
    /// A selection taken from a whitespace-insensitive diff is refused with
    /// [`GitError::WhitespaceInsensitiveSelection`] before the lock is taken.
    pub fn stage_hunks(
        &self,
        selections: &[HunkSelection],
        cancellation: &Cancellation,
    ) -> Result<HunkStageOutcome, GitError> {
        self.stage_hunks_with_options(selections, &StageOptions::default(), cancellation)
    }

    /// Stages selected hunks with control over the final status refresh.
    pub fn stage_hunks_with_options(
        &self,
        selections: &[HunkSelection],
        options: &StageOptions,
        cancellation: &Cancellation,
    ) -> Result<HunkStageOutcome, GitError> {
        hunk::refuse_whitespace_insensitive(selections)?;
        let lock = self.acquire_lock(cancellation)?;
        hunk::stage(
            &self.git_executable,
            &self.root,
            &lock,
            selections,
            options,
            cancellation,
        )
    }

    /// Unstages selected index hunks without writing the working tree.
    ///
    /// Selections must come from a staged [`Self::diff`] result. Before the
    /// first commit, reverse application uses the empty HEAD tree in exactly
    /// the same way as the staged diff model.
    /// A selection taken from a whitespace-insensitive diff is refused with
    /// [`GitError::WhitespaceInsensitiveSelection`] before the lock is taken.
    pub fn unstage_hunks(
        &self,
        selections: &[HunkSelection],
        cancellation: &Cancellation,
    ) -> Result<HunkStageOutcome, GitError> {
        self.unstage_hunks_with_options(selections, &StageOptions::default(), cancellation)
    }

    /// Unstages selected hunks with control over the final status refresh.
    pub fn unstage_hunks_with_options(
        &self,
        selections: &[HunkSelection],
        options: &StageOptions,
        cancellation: &Cancellation,
    ) -> Result<HunkStageOutcome, GitError> {
        hunk::refuse_whitespace_insensitive(selections)?;
        let lock = self.acquire_lock(cancellation)?;
        hunk::unstage(
            &self.git_executable,
            &self.root,
            &lock,
            selections,
            options,
            cancellation,
        )
    }

    /// Stages selected changed lines without writing the working tree.
    ///
    /// Each selection retains its enclosing hunk identity so the diff can be
    /// recomputed and checked under the repository lock. Lines from the same
    /// fresh hunk are merged into one internally recounted patch hunk before
    /// the batch is applied atomically.
    /// A selection taken from a whitespace-insensitive diff is refused with
    /// [`GitError::WhitespaceInsensitiveSelection`] before the lock is taken.
    pub fn stage_lines(
        &self,
        selections: &[LineSelection],
        cancellation: &Cancellation,
    ) -> Result<LineStageOutcome, GitError> {
        self.stage_lines_with_options(selections, &StageOptions::default(), cancellation)
    }

    /// Stages selected lines with control over the final status refresh.
    pub fn stage_lines_with_options(
        &self,
        selections: &[LineSelection],
        options: &StageOptions,
        cancellation: &Cancellation,
    ) -> Result<LineStageOutcome, GitError> {
        hunk::refuse_whitespace_insensitive(selections)?;
        let lock = self.acquire_lock(cancellation)?;
        hunk::stage_lines(
            &self.git_executable,
            &self.root,
            &lock,
            selections,
            options,
            cancellation,
        )
    }

    /// Unstages selected changed lines without writing the working tree.
    /// A selection taken from a whitespace-insensitive diff is refused with
    /// [`GitError::WhitespaceInsensitiveSelection`] before the lock is taken.
    pub fn unstage_lines(
        &self,
        selections: &[LineSelection],
        cancellation: &Cancellation,
    ) -> Result<LineStageOutcome, GitError> {
        self.unstage_lines_with_options(selections, &StageOptions::default(), cancellation)
    }

    /// Unstages selected lines with control over the final status refresh.
    pub fn unstage_lines_with_options(
        &self,
        selections: &[LineSelection],
        options: &StageOptions,
        cancellation: &Cancellation,
    ) -> Result<LineStageOutcome, GitError> {
        hunk::refuse_whitespace_insensitive(selections)?;
        let lock = self.acquire_lock(cancellation)?;
        hunk::unstage_lines(
            &self.git_executable,
            &self.root,
            &lock,
            selections,
            options,
            cancellation,
        )
    }

    /// Stages every change to each explicit path and refreshes repository
    /// status afterward.
    ///
    /// Every path is resolved and checked against the working tree before Git
    /// is spawned. The path arguments retain their platform-native encoding
    /// and follow a `--` separator, so neither a non-UTF-8 name nor a name that
    /// resembles an option is reinterpreted.
    ///
    /// Pass every path from one user action in the same call. The iterator is
    /// intentionally a batch boundary: all paths share one repository lock and
    /// one full-repository status refresh. Use [`Self::stage_with_options`] to
    /// opt out of that refresh when the caller will perform one separately.
    /// Inspect every [`StagePathOutcome`]: one path's ordinary Git rejection is
    /// retained there while later paths continue.
    pub fn stage<I, P>(
        &self,
        paths: I,
        cancellation: &Cancellation,
    ) -> Result<StageOutcome, GitError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        self.stage_with_options(paths, &StageOptions::default(), cancellation)
    }

    /// Stages explicit paths with control over the final status refresh.
    pub fn stage_with_options<I, P>(
        &self,
        paths: I,
        options: &StageOptions,
        cancellation: &Cancellation,
    ) -> Result<StageOutcome, GitError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let paths = paths
            .into_iter()
            .map(|path| path.as_ref().to_path_buf())
            .collect::<Vec<_>>();
        let lock = self.acquire_lock(cancellation)?;
        commit::stage(
            &self.git_executable,
            &self.root,
            &lock,
            &paths,
            options,
            cancellation,
        )
    }

    /// Stages every change in the working tree, including deletions.
    pub fn stage_all(&self, cancellation: &Cancellation) -> Result<DetailedStatus, GitError> {
        match self.stage_all_with_options(&StageOptions::default(), cancellation)? {
            StatusRefreshOutcome::Refreshed(status) => Ok(status),
            StatusRefreshOutcome::Failed(error) => Err(error),
            StatusRefreshOutcome::Skipped => unreachable!("default stage options refresh status"),
        }
    }

    /// Stages the whole working tree with control over the final status
    /// refresh.
    pub fn stage_all_with_options(
        &self,
        options: &StageOptions,
        cancellation: &Cancellation,
    ) -> Result<StatusRefreshOutcome, GitError> {
        let lock = self.acquire_lock(cancellation)?;
        commit::stage_all(
            &self.git_executable,
            &self.root,
            &lock,
            options,
            cancellation,
        )
    }

    /// Removes each explicit path from the staged snapshot without changing
    /// the working tree, then refreshes repository status.
    ///
    /// On an unborn branch this uses `git rm --cached`, because there is no
    /// `HEAD` tree for `git restore --staged` to restore.
    ///
    /// Batch paths into one call to share one repository lock and one final
    /// full-repository status refresh. Use [`Self::unstage_with_options`] when
    /// the caller will refresh separately. Inspect every [`StagePathOutcome`]
    /// because an ordinary Git rejection is reported per path rather than as
    /// the outer error.
    pub fn unstage<I, P>(
        &self,
        paths: I,
        cancellation: &Cancellation,
    ) -> Result<StageOutcome, GitError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        self.unstage_with_options(paths, &StageOptions::default(), cancellation)
    }

    /// Unstages explicit paths with control over the final status refresh.
    pub fn unstage_with_options<I, P>(
        &self,
        paths: I,
        options: &StageOptions,
        cancellation: &Cancellation,
    ) -> Result<StageOutcome, GitError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let paths = paths
            .into_iter()
            .map(|path| path.as_ref().to_path_buf())
            .collect::<Vec<_>>();
        let lock = self.acquire_lock(cancellation)?;
        commit::unstage(
            &self.git_executable,
            &self.root,
            &lock,
            &paths,
            options,
            cancellation,
        )
    }

    /// Creates a commit.
    ///
    /// What the commit records is [`CommitOptions::scope`]: the staged
    /// snapshot by default, or a working tree or path selection that this call
    /// stages itself, under the one repository lock it already holds.
    ///
    /// Empty messages, a selection that would record nothing, and amending an
    /// unborn branch are refused in process before Git is spawned. Amending is
    /// exempt from the second of those, because rewriting an existing commit
    /// with an unchanged tree is a message edit rather than an empty commit. A
    /// successful commit remains an `Ok` [`CommitOutcome`] if only the
    /// follow-up refresh fails; inspect [`CommitOutcome::status`] for it.
    pub fn commit(
        &self,
        message: &str,
        options: &CommitOptions,
        cancellation: &Cancellation,
    ) -> Result<CommitOutcome, GitError> {
        let lock = self.acquire_lock(cancellation)?;
        commit::commit(
            &self.git_executable,
            &self.root,
            &lock,
            message,
            options,
            cancellation,
        )
    }

    /// Lists local branches and optionally the remote-tracking refs already
    /// present in this repository.
    ///
    /// Runs entirely in process and takes no repository lock. Ahead/behind
    /// calculation can walk substantial history, so event-loop callers must
    /// use a worker thread; [`BranchListOptions::calculate_divergence`] can
    /// disable those walks for a branch picker.
    pub fn branches(
        &self,
        options: &BranchListOptions,
        cancellation: &Cancellation,
    ) -> Result<Vec<Branch>, GitError> {
        branch::branches(&self.root, options, cancellation)
    }

    /// Creates a local branch, optionally checking it out in the same call.
    pub fn create_branch(
        &self,
        name: &str,
        options: &CreateBranchOptions,
        cancellation: &Cancellation,
    ) -> Result<(), GitError> {
        branch::validate_name(name)?;
        let lock = self.acquire_lock(cancellation)?;
        branch::create(
            &self.git_executable,
            &self.root,
            lock,
            name,
            options,
            cancellation,
        )
    }

    /// Checks out an existing local branch without discarding local changes.
    pub fn checkout_branch(&self, name: &str, cancellation: &Cancellation) -> Result<(), GitError> {
        branch::validate_name(name)?;
        let lock = self.acquire_lock(cancellation)?;
        branch::checkout(&self.git_executable, &self.root, lock, name, cancellation)
    }

    /// Deletes a local branch after applying the branch-safety guardrails.
    ///
    /// `force` overrides only the unmerged-commit refusal. It cannot delete a
    /// current, recorded-default or other-worktree branch. The default-branch
    /// guard is intentionally fail-open when no `refs/remotes/<remote>/HEAD`
    /// exists: local deletion is reflog-recoverable, and repositories assembled
    /// without `git clone` ordinarily have no recorded remote HEAD.
    pub fn delete_branch(
        &self,
        name: &str,
        force: bool,
        cancellation: &Cancellation,
    ) -> Result<(), GitError> {
        branch::validate_name(name)?;
        let lock = self.acquire_lock(cancellation)?;
        branch::delete(
            &self.git_executable,
            &self.root,
            lock,
            name,
            force,
            cancellation,
        )
    }

    /// Sets a local branch's upstream, or clears it when `upstream` is `None`.
    pub fn set_upstream(
        &self,
        branch: &str,
        upstream: Option<&str>,
        cancellation: &Cancellation,
    ) -> Result<(), GitError> {
        branch::validate_name(branch)?;
        let lock = self.acquire_lock(cancellation)?;
        branch::set_upstream(
            &self.git_executable,
            &self.root,
            lock,
            branch,
            upstream,
            cancellation,
        )
    }

    /// Renames an existing local branch without overwriting another branch.
    pub fn rename_branch(
        &self,
        old_name: &str,
        new_name: &str,
        cancellation: &Cancellation,
    ) -> Result<(), GitError> {
        branch::validate_name(old_name)?;
        branch::validate_name(new_name)?;
        let lock = self.acquire_lock(cancellation)?;
        branch::rename(
            &self.git_executable,
            &self.root,
            lock,
            old_name,
            new_name,
            cancellation,
        )
    }

    /// Updates the remote-tracking refs of one remote.
    ///
    /// Blocks until Git exits, so a front end with an event loop must call it
    /// on a worker thread; `on_progress` receives Git's transfer counters as
    /// they arrive. The repository lock is held for the whole operation,
    /// including the inspection either side of it that decides what moved.
    pub fn fetch(
        &self,
        options: &FetchOptions,
        cancellation: &Cancellation,
        on_progress: impl FnMut(String),
    ) -> Result<FetchOutcome, GitError> {
        let lock = self.acquire_lock(cancellation)?;
        sync::fetch(
            &self.git_executable,
            &self.root,
            &lock,
            options,
            cancellation,
            on_progress,
        )
    }

    /// Fetches the checked-out branch's upstream and reconciles the branch with
    /// it.
    ///
    /// Fast-forward only unless [`PullOptions::strategy`] says otherwise, so
    /// the default can neither rewrite history nor invent a merge commit.
    ///
    /// This is the one verb here that writes to the working tree, and therefore
    /// the one whose failures are not all equivalent to "nothing happened". A
    /// merge or rebase that stops at a conflict, and a cancellation that lands
    /// mid-reconciliation, both report [`GitError::Interrupted`] carrying the
    /// state the repository was left in; every other failure leaves the branch
    /// where it was.
    pub fn pull(
        &self,
        options: &PullOptions,
        cancellation: &Cancellation,
        on_progress: impl FnMut(String),
    ) -> Result<PullOutcome, GitError> {
        let lock = self.acquire_lock(cancellation)?;
        sync::pull(
            &self.git_executable,
            &self.root,
            &lock,
            options,
            cancellation,
            on_progress,
        )
    }

    /// Publishes the checked-out branch to a remote, under the same name.
    ///
    /// Every refusal is decided before anything is written, so a refused push
    /// never changes the remote. [`GitError::NoUpstream`],
    /// [`GitError::UnbornBranch`] and [`GitError::LocalUpstreamUnsupported`]
    /// are settled without contacting it at all;
    /// [`GitError::DefaultBranchPush`] costs one read-only query, because the
    /// only trustworthy answer to "which branch is the default" comes from the
    /// remote rather than from what a clone recorded.
    pub fn push(
        &self,
        options: &PushOptions,
        cancellation: &Cancellation,
        on_progress: impl FnMut(String),
    ) -> Result<PushOutcome, GitError> {
        let lock = self.acquire_lock(cancellation)?;
        sync::push(
            &self.git_executable,
            &self.root,
            &lock,
            options,
            cancellation,
            on_progress,
        )
    }

    /// Takes the exclusive lock covering this repository and its worktrees.
    ///
    /// Every Git mutation holds it; no read takes it at all. The caller must
    /// acquire it before any catalog lock.
    ///
    /// While the returned session is alive, do not call a mutating method on
    /// this or another [`GitService`] for the same repository. Repository
    /// locks are deliberately non-reentrant, so that call waits for this
    /// session and then returns [`GitError::RepositoryBusy`].
    pub fn lock(&self, cancellation: &Cancellation) -> Result<LockedRepository, GitError> {
        Ok(LockedRepository {
            root: self.root.clone(),
            git_executable: self.git_executable.clone(),
            lock: self.acquire_lock(cancellation)?,
        })
    }

    fn acquire_lock(&self, cancellation: &Cancellation) -> Result<RepositoryLock, GitError> {
        RepositoryLock::acquire(&self.data_dir, &self.root, cancellation)
    }
}

/// A repository session that proves the repository mutation lock is held.
///
/// Its public surface is intentionally limited to the worktree lifecycle used
/// by an embedding catalog layer. Dropping the session releases the lock.
/// Callers must keep their catalog row in sync with every successful lifecycle
/// mutation. Do not invoke another `GitService` mutation for this repository
/// while the session is alive; the lock is not re-entrant.
#[derive(Debug)]
pub struct LockedRepository {
    root: PathBuf,
    git_executable: PathBuf,
    lock: RepositoryLock,
}

impl LockedRepository {
    /// Lists worktrees while retaining the repository mutation capability for
    /// a later catalog-coordinated lifecycle change.
    pub fn worktrees_while_locked(
        &self,
        cancellation: &Cancellation,
    ) -> Result<Vec<GitWorktree>, GitError> {
        worktree::list(&self.git_executable, &self.root, cancellation)
    }

    /// Adds a linked worktree and completes its catalog transaction while the
    /// repository lock remains held.
    ///
    /// The destination must not exist. If Git fails, or `complete` returns an
    /// error, the exact attempted checkout and its administrative record are
    /// cleaned before this returns. Cleanup is verified rather than silently
    /// discarding command or filesystem failures. The nested result keeps a
    /// catalog error `E` distinct from a Git or cleanup failure.
    pub fn add_worktree<T, E>(
        &self,
        destination: impl AsRef<Path>,
        base: &WorktreeBase,
        cancellation: &Cancellation,
        complete: impl FnOnce(&AddedWorktree) -> Result<T, E>,
    ) -> Result<Result<T, E>, GitError> {
        let destination = destination.as_ref();
        worktree::require_missing_destination(destination)?;
        let added = match worktree::add(
            &self.git_executable,
            &self.root,
            &self.lock,
            destination,
            base,
            cancellation,
        ) {
            Ok(added) => added,
            Err(error) => {
                if matches!(
                    &error,
                    GitError::Failed { .. } | GitError::Cancelled | GitError::TimedOut { .. }
                ) {
                    worktree::cleanup_failed_add(
                        &self.git_executable,
                        &self.root,
                        &self.lock,
                        destination,
                    )?;
                }
                return Err(error);
            }
        };
        let completed = complete(&added);
        if completed.is_err() {
            worktree::cleanup_failed_add(
                &self.git_executable,
                &self.root,
                &self.lock,
                destination,
            )?;
        }
        Ok(completed)
    }

    /// Moves a registered, unlocked worktree. The embedding layer is
    /// responsible for validating and updating its catalog row while this
    /// session keeps concurrent Harkness mutations excluded.
    pub fn move_registered_worktree(
        &self,
        source: impl AsRef<Path>,
        destination: impl AsRef<Path>,
        cancellation: &Cancellation,
    ) -> Result<(), GitError> {
        worktree::move_known_unlocked(
            &self.git_executable,
            &self.root,
            &self.lock,
            source.as_ref(),
            destination.as_ref(),
            cancellation,
        )
    }

    /// Removes a registered, unlocked worktree through Git. The embedding
    /// layer is responsible for dropping its matching catalog row before the
    /// session is released.
    pub fn remove_registered_worktree(
        &self,
        destination: impl AsRef<Path>,
        force: bool,
        cancellation: &Cancellation,
    ) -> Result<(), GitError> {
        worktree::remove_known_unlocked(
            &self.git_executable,
            &self.root,
            &self.lock,
            destination.as_ref(),
            force,
            cancellation,
        )
    }

    /// Locks a registered worktree whose current state the embedding layer
    /// already verified.
    pub fn lock_registered_worktree(
        &self,
        destination: impl AsRef<Path>,
        reason: &str,
        cancellation: &Cancellation,
    ) -> Result<(), GitError> {
        worktree::lock_known_unlocked(
            &self.git_executable,
            &self.root,
            &self.lock,
            destination.as_ref(),
            reason,
            cancellation,
        )
    }

    /// Unlocks a registered worktree whose current state the embedding layer
    /// already verified.
    pub fn unlock_registered_worktree(
        &self,
        destination: impl AsRef<Path>,
        cancellation: &Cancellation,
    ) -> Result<(), GitError> {
        worktree::unlock_known_locked(
            &self.git_executable,
            &self.root,
            &self.lock,
            destination.as_ref(),
            cancellation,
        )
    }
}

/// Validates and normalizes a reason before any worktree command is spawned.
pub fn validate_worktree_lock_reason(reason: &str) -> Result<&str, GitError> {
    worktree::validate_lock_reason(reason)
}

/// Describes a directory as a repository working tree without spawning Git.
///
/// `None` means the directory is not itself a repository working tree.
pub fn inspect_repository(path: impl AsRef<Path>) -> Result<Option<GitStatus>, GitError> {
    status::inspect(path.as_ref())
}

/// Chooses a configured remote using the same precedence for every Git verb.
///
/// An explicit request wins, then the branch's configured upstream remote,
/// then `origin`, then a sole configured remote. A named remote is never
/// silently replaced when its configuration has disappeared.
pub(crate) fn resolve_remote(
    repository: &Repository,
    root: &Path,
    requested: Option<&str>,
    upstream_remote: Option<&str>,
) -> Result<String, GitError> {
    let configured = repository
        .remotes()
        .map_err(|source| inspection(root, source))?;
    let names = || configured.iter().filter_map(|name| name.ok().flatten());
    let known =
        |candidate: &str| candidate == LOCAL_REMOTE || names().any(|name| name == candidate);

    if let Some(requested) = requested {
        return if known(requested) {
            Ok(requested.to_owned())
        } else {
            Err(GitError::NoRemote {
                remote: Some(requested.to_owned()),
            })
        };
    }
    if let Some(upstream_remote) = upstream_remote {
        return if known(upstream_remote) {
            Ok(upstream_remote.to_owned())
        } else {
            Err(GitError::NoRemote {
                remote: Some(upstream_remote.to_owned()),
            })
        };
    }
    if known(DEFAULT_REMOTE) {
        return Ok(DEFAULT_REMOTE.to_owned());
    }
    let mut only = names();
    match (only.next(), only.next()) {
        (Some(only), None) => Ok(only.to_owned()),
        _ => Err(GitError::NoRemote { remote: None }),
    }
}

/// Reads the branch named by a locally recorded remote HEAD.
///
/// Absence is an ordinary answer for repositories assembled with `git init`,
/// `git remote add`, and `git fetch`; only clones normally create this ref.
pub(crate) fn recorded_default_branch(
    repository: &Repository,
    root: &Path,
    remote: &str,
) -> Result<Option<String>, GitError> {
    let head = match repository.find_reference(&format!("refs/remotes/{remote}/HEAD")) {
        Ok(head) => head,
        Err(error) if error.code() == ErrorCode::NotFound => return Ok(None),
        Err(source) => return Err(inspection(root, source)),
    };
    let prefix = format!("refs/remotes/{remote}/");
    Ok(head
        .symbolic_target()
        .map_err(|source| inspection(root, source))?
        .and_then(|target| target.strip_prefix(&prefix))
        .filter(|branch| !branch.is_empty())
        .map(str::to_owned))
}

/// Reads the branch name from a symbolic HEAD, including an unborn HEAD.
pub(crate) fn head_branch(repository: &Repository, root: &Path) -> Result<String, GitError> {
    repository
        .find_reference("HEAD")
        .and_then(|head| {
            head.symbolic_target()
                .map(|target| target.map(str::to_owned))
        })
        .map_err(|source| inspection(root, source))?
        .and_then(|target| {
            target
                .strip_prefix("refs/heads/")
                .filter(|branch| !branch.is_empty())
                .map(str::to_owned)
        })
        .ok_or_else(|| GitError::DetachedHead {
            path: root.to_path_buf(),
            detail: "HEAD names no branch".to_owned(),
        })
}

fn inspection(path: &Path, source: git2::Error) -> GitError {
    GitError::Inspection {
        path: path.to_path_buf(),
        source: source.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::{io, path::PathBuf, time::Duration};

    use git2::Repository;

    use super::{
        Cancellation, FileChange, GitError, GitService, PendingOperation, WorktreeBase,
        repository_remote_url,
    };
    use crate::testing::{Fixture, initialize_repository};

    #[test]
    fn repository_remote_url_prefers_origin_then_a_sole_remote() {
        let fixture = Fixture::new();
        let root = fixture.directory("remote-url");
        initialize_repository(&root);
        let repository = Repository::open(&root).unwrap();

        assert_eq!(repository_remote_url(&root).unwrap(), None);
        repository
            .remote("upstream", "https://github.com/example/upstream.git")
            .unwrap();
        assert_eq!(
            repository_remote_url(&root).unwrap().as_deref(),
            Some("https://github.com/example/upstream.git")
        );
        repository
            .remote("backup", "https://github.com/example/backup.git")
            .unwrap();
        assert_eq!(repository_remote_url(&root).unwrap(), None);
        repository
            .remote("origin", "git@github.com:example/origin.git")
            .unwrap();
        assert_eq!(
            repository_remote_url(&root).unwrap().as_deref(),
            Some("git@github.com:example/origin.git")
        );
    }

    #[test]
    fn locked_sessions_hold_the_repository_lock_while_they_exist() {
        let fixture = Fixture::new();
        let root = fixture.directory("command-lock");
        initialize_repository(&root);
        let service = GitService::new(&root, &fixture.data_dir);

        let session = service.lock(&Cancellation::default()).unwrap();
        let cancelled = Cancellation::default();
        cancelled.cancel();
        assert!(matches!(service.lock(&cancelled), Err(GitError::Cancelled)));

        drop(session);
        service.lock(&Cancellation::default()).unwrap();
    }

    #[test]
    fn worktree_transactions_never_reuse_or_clean_an_existing_destination() {
        let fixture = Fixture::new();
        let root = fixture.directory("transaction-root");
        initialize_repository(&root);
        let destination = fixture.directory("existing-destination");
        let sentinel = destination.join("keep.txt");
        std::fs::write(&sentinel, "keep\n").unwrap();
        let session = GitService::new(&root, &fixture.data_dir)
            .with_git_executable(fixture.root.path().join("must-not-run"))
            .lock(&Cancellation::default())
            .unwrap();

        let error = session
            .add_worktree(
                &destination,
                &WorktreeBase::Detached {
                    commit: "HEAD".to_owned(),
                },
                &Cancellation::default(),
                |_| Ok::<(), ()>(()),
            )
            .unwrap_err();

        assert!(matches!(
            error,
            GitError::WorktreeAddDestinationExists { path } if path == destination
        ));
        assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "keep\n");
    }

    #[cfg(unix)]
    #[test]
    fn worktree_transactions_surface_cleanup_that_cannot_be_verified() {
        let fixture = Fixture::new();
        let root = fixture.directory("unverifiable-cleanup-root");
        initialize_repository(&root);
        let destination = fixture.root.path().join("partial-worktree");
        let shim = fixture.shim(
            "unverifiable-cleanup-git",
            "#!/bin/sh\n\
             previous=\n\
             operation=other\n\
             destination=\n\
             for argument in \"$@\"; do\n\
               destination=$argument\n\
               if [ \"$previous\" = worktree ]; then operation=$argument; fi\n\
               previous=$argument\n\
             done\n\
             if [ \"$operation\" = add ]; then mkdir -p \"$destination\"; exit 42; fi\n\
             if [ \"$operation\" = remove ]; then exit 43; fi\n\
             if [ \"$operation\" = list ]; then exit 44; fi\n\
             exit 45\n",
        );
        let session = GitService::new(&root, &fixture.data_dir)
            .with_git_executable(shim)
            .lock(&Cancellation::default())
            .unwrap();

        let error = session
            .add_worktree(
                &destination,
                &WorktreeBase::NewBranch {
                    name: "cleanup-failure".to_owned(),
                    start_point: None,
                },
                &Cancellation::default(),
                |_| Ok::<(), ()>(()),
            )
            .unwrap_err();

        assert!(matches!(
            error,
            GitError::WorktreeAddCleanup { path, .. } if path == destination
        ));
        assert!(!destination.exists());
    }

    #[test]
    fn git_error_kind_contract_is_stable() {
        let path = PathBuf::from("fixture");
        let io_error = || io::Error::other("fixture");
        let git_error = || git2::Error::from_str("fixture");
        let cases = vec![
            (GitError::Launch { source: io_error() }, "launch"),
            (
                GitError::Failed {
                    command: "status".to_owned(),
                    stderr: "fixture".to_owned(),
                },
                "failed",
            ),
            (GitError::Cancelled, "cancelled"),
            (
                GitError::TimedOut {
                    command: "status".to_owned(),
                    timeout: Duration::from_secs(1),
                },
                "timed_out",
            ),
            (
                GitError::RepositoryBusy { path: path.clone() },
                "repository_busy",
            ),
            (
                GitError::Lock {
                    path: path.clone(),
                    source: io_error(),
                },
                "lock",
            ),
            (
                GitError::NotARepository { path: path.clone() },
                "not_a_repository",
            ),
            (
                GitError::RevisionNotFound {
                    revision: "fixture".to_owned(),
                },
                "revision_not_found",
            ),
            (
                GitError::AmbiguousRevision {
                    revision: "fixture".to_owned(),
                },
                "ambiguous_revision",
            ),
            (
                GitError::RevisionNotCommit {
                    revision: "fixture".to_owned(),
                    id: git2::Oid::ZERO_SHA1,
                },
                "revision_not_commit",
            ),
            (
                GitError::RevisionNotParent {
                    revision: "commit".to_owned(),
                    parent: "unrelated".to_owned(),
                },
                "revision_not_parent",
            ),
            (
                GitError::NoMergeBase {
                    one: "one".to_owned(),
                    two: "two".to_owned(),
                },
                "no_merge_base",
            ),
            (GitError::InvalidLogLimit, "invalid_log_limit"),
            (
                GitError::InvalidLogCursor {
                    cursor: git2::Oid::ZERO_SHA1,
                },
                "invalid_log_cursor",
            ),
            (
                GitError::PathOutsideRepository {
                    path: path.clone(),
                    repository: path.clone(),
                },
                "path_outside_repository",
            ),
            (GitError::EmptyCommitMessage, "empty_commit_message"),
            (GitError::NothingStaged, "nothing_staged"),
            (GitError::AmendUnbornBranch, "amend_unborn_branch"),
            (
                GitError::InvalidBranchName {
                    name: "fixture".to_owned(),
                },
                "invalid_branch_name",
            ),
            (
                GitError::NoSuchBranch {
                    branch: "fixture".to_owned(),
                },
                "no_such_branch",
            ),
            (
                GitError::BranchAlreadyExists {
                    branch: "fixture".to_owned(),
                },
                "branch_already_exists",
            ),
            (
                GitError::InvalidStartPoint {
                    start_point: "fixture".to_owned(),
                },
                "invalid_start_point",
            ),
            (
                GitError::CurrentBranchDeletion {
                    branch: "fixture".to_owned(),
                },
                "current_branch_deletion",
            ),
            (
                GitError::DefaultBranchDeletion {
                    branch: "fixture".to_owned(),
                },
                "default_branch_deletion",
            ),
            (
                GitError::BranchCheckedOutInWorktree {
                    branch: "fixture".to_owned(),
                    worktree: path.clone(),
                },
                "branch_checked_out_in_worktree",
            ),
            (
                GitError::WorktreeAddDestinationExists { path: path.clone() },
                "worktree_add_destination_exists",
            ),
            (
                GitError::WorktreeAddDestinationUnavailable {
                    path: path.clone(),
                    source: io_error(),
                },
                "worktree_add_destination_unavailable",
            ),
            (
                GitError::WorktreeAddCleanup {
                    path: path.clone(),
                    detail: "fixture".to_owned(),
                },
                "worktree_add_cleanup",
            ),
            (
                GitError::WorktreeLocked {
                    path: path.clone(),
                    reason: None,
                },
                "worktree_locked",
            ),
            (
                GitError::EmptyWorktreeLockReason,
                "empty_worktree_lock_reason",
            ),
            (
                GitError::WorktreeAlreadyLocked {
                    path: path.clone(),
                    reason: None,
                },
                "worktree_already_locked",
            ),
            (
                GitError::WorktreeNotLocked { path: path.clone() },
                "worktree_not_locked",
            ),
            (
                GitError::WorktreeMoveAcrossDevices {
                    worktree: path.clone(),
                    destination: path.clone(),
                    stderr: "fixture".to_owned(),
                },
                "worktree_move_across_devices",
            ),
            (
                GitError::UnmergedBranchDeletion {
                    branch: "fixture".to_owned(),
                },
                "unmerged_branch_deletion",
            ),
            (
                GitError::NonFastForward {
                    command: "push".to_owned(),
                    stderr: "fixture".to_owned(),
                },
                "non_fast_forward",
            ),
            (
                GitError::AuthenticationFailed {
                    command: "push".to_owned(),
                    stderr: "fixture".to_owned(),
                },
                "authentication_failed",
            ),
            (
                GitError::NoUpstream {
                    branch: "fixture".to_owned(),
                },
                "no_upstream",
            ),
            (
                GitError::UnbornBranch {
                    path: path.clone(),
                    branch: "fixture".to_owned(),
                },
                "unborn_branch",
            ),
            (
                GitError::LocalUpstreamUnsupported {
                    branch: "fixture".to_owned(),
                },
                "local_upstream_unsupported",
            ),
            (
                GitError::OperationInProgress {
                    path: path.clone(),
                    pending: PendingOperation::Merge,
                },
                "operation_in_progress",
            ),
            (
                GitError::Interrupted {
                    command: "pull".to_owned(),
                    path: path.clone(),
                    pending: PendingOperation::Merge,
                    status: None,
                    source: Box::new(GitError::Failed {
                        command: "pull".to_owned(),
                        stderr: "fixture".to_owned(),
                    }),
                },
                "interrupted",
            ),
            (GitError::NoRemote { remote: None }, "no_remote"),
            (
                GitError::DefaultBranchPush {
                    remote: "origin".to_owned(),
                    branch: "main".to_owned(),
                },
                "default_branch_push",
            ),
            (
                GitError::DefaultBranchUnknown {
                    remote: "origin".to_owned(),
                },
                "default_branch_unknown",
            ),
            (
                GitError::DetachedHead {
                    path: path.clone(),
                    detail: "fixture".to_owned(),
                },
                "detached_head",
            ),
            (
                GitError::Inspection {
                    path: path.clone(),
                    source: git_error().into(),
                },
                "inspection",
            ),
            (
                GitError::DiffContent {
                    path: path.clone(),
                    source: io_error(),
                },
                "diff_content",
            ),
            (GitError::StaleDiscardSelection, "stale_discard_selection"),
            (
                GitError::UntrackedDiscardRequiresDelete { path: path.clone() },
                "untracked_discard_requires_delete",
            ),
            (
                GitError::TrackedDiscardRequiresRestore { path: path.clone() },
                "tracked_discard_requires_restore",
            ),
            (
                GitError::UnmergedDiscard { path: path.clone() },
                "unmerged_discard",
            ),
            (
                GitError::NothingToDiscard { path: path.clone() },
                "nothing_to_discard",
            ),
            (
                GitError::UntrackedDiscardNotFile { path: path.clone() },
                "untracked_discard_not_file",
            ),
            (
                GitError::UntrackedDiscardIo {
                    path: path.clone(),
                    source: io_error(),
                },
                "untracked_discard_io",
            ),
            (
                GitError::InvalidBlobId {
                    blob_id: "invalid".to_owned(),
                },
                "invalid_blob_id",
            ),
            (
                GitError::BlobNotFound {
                    blob_id: "1".repeat(40),
                },
                "blob_not_found",
            ),
            (
                GitError::MalformedDiff {
                    detail: "fixture".to_owned(),
                },
                "malformed_diff",
            ),
            (
                GitError::StaleHunkSelection { path: path.clone() },
                "stale_hunk_selection",
            ),
            (
                GitError::WhitespaceInsensitiveSelection {
                    path: path.clone(),
                    whitespace: crate::Whitespace::new(crate::WhitespaceMode::IgnoreAll),
                },
                "whitespace_insensitive_selection",
            ),
            (
                GitError::HiddenWhitespaceChanges { path: path.clone() },
                "hidden_whitespace_changes",
            ),
            (
                GitError::BinaryHunkSelection { path: path.clone() },
                "binary_hunk_selection",
            ),
            (
                GitError::RenameOnlyHunkSelection {
                    old_path: path.clone(),
                    new_path: path.clone(),
                },
                "rename_only_hunk_selection",
            ),
            (
                GitError::MetadataOnlyHunkSelection {
                    path: path.clone(),
                    old_mode: 0o100_644,
                    new_mode: 0o100_755,
                },
                "metadata_only_hunk_selection",
            ),
            (
                GitError::UnsupportedHunkChange {
                    path: path.clone(),
                    change: FileChange::TypeChanged,
                },
                "unsupported_hunk_change",
            ),
            (
                GitError::FilteredHunkSelection {
                    path: path.clone(),
                    driver: "lfs".to_owned(),
                },
                "filtered_hunk_selection",
            ),
            (
                GitError::OverlappingHunkSelection { path: path.clone() },
                "overlapping_hunk_selection",
            ),
            (
                GitError::HunkNotFound {
                    path: path.clone(),
                    old_start: 1,
                    old_lines: 2,
                    new_start: 3,
                    new_lines: 4,
                },
                "hunk_not_found",
            ),
            (
                GitError::LineNotFound {
                    path: path.clone(),
                    old_line_number: Some(1),
                    new_line_number: None,
                },
                "line_not_found",
            ),
            (
                GitError::UnrepresentableLineSelection { path: path.clone() },
                "unrepresentable_line_selection",
            ),
            (
                GitError::HunkApplication {
                    paths: vec![path.clone()],
                    source: git_error(),
                },
                "hunk_application",
            ),
            (
                GitError::MalformedStatus {
                    detail: "fixture".to_owned(),
                },
                "malformed_status",
            ),
        ];

        let kinds = cases.iter().map(|(_, kind)| *kind).collect::<Vec<_>>();
        assert_eq!(kinds, GitError::KINDS);
        for (error, expected) in cases {
            assert_eq!(error.kind(), expected, "unexpected kind for {error:?}");
        }
    }
}
