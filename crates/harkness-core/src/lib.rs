//! Shared application behavior for Harkness front ends.

mod catalog;
mod git;
mod listing;
mod paths;
mod project;
mod remote;
#[cfg(test)]
mod testing;

pub use catalog::entry::{GitStatus, Project, ProjectId, ProjectSource, UpstreamStatus};
pub use git::{
    Branch, BranchCheckout, BranchKind, BranchListOptions, Cancellation, CloneCancellation,
    CommitInfo, CommitOptions, CommitOutcome, CommitSignature, CreateBranchOptions,
    DEFAULT_DIFF_CONTEXT_LINES, DEFAULT_MAX_DIFF_FILE_SIZE, DEFAULT_MAX_DIFF_FILES,
    DEFAULT_MAX_DIFF_TOTAL_BYTES, DetailedStatus, DiffLine, DiffLineKind, DiffOmission,
    DiffOptions, DiffTarget, FetchOptions, FetchOutcome, FileChange, FileContextOmission,
    FileContextRange, FileContextRequest, FileContextResponse, FileContextSource, FileDiff,
    FileSide, GitAccess, GitCommand, GitError, GitOutput, GitService, HeadState, Hunk,
    HunkSelection, LogCursor, LogOptions, LogPage, LogRange, PendingOperation, PullOptions,
    PullOutcome, PullStrategy, PushOptions, PushOutcome, RefUpdate, RepositoryLock, StageOptions,
    StageOutcome, StagePathOutcome, StagePathResult, StatusEntry, StatusRefreshOutcome,
};
pub use listing::{DirEntry, list_directory};
pub use project::{
    ProjectError, ProjectSelector, ProjectService, Worktree, WorktreeBase, WorktreeReconciliation,
    WorktreeReconciliationSkip,
};
pub use remote::normalize_remote;
