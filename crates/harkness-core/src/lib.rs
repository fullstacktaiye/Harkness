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
    CommitOptions, CommitOutcome, CreateBranchOptions, DetailedStatus, DiffLine, DiffLineKind,
    DiffOmission, DiffOptions, DiffTarget, FetchOptions, FetchOutcome, FileChange, FileDiff,
    GitAccess, GitCommand, GitError, GitOutput, GitService, HeadState, Hunk, PendingOperation,
    PullOptions, PullOutcome, PullStrategy, PushOptions, PushOutcome, RefUpdate, RepositoryLock,
    StageOptions, StageOutcome, StagePathOutcome, StagePathResult, StatusEntry,
    StatusRefreshOutcome,
};
pub use listing::{DirEntry, list_directory};
pub use project::{ProjectError, ProjectSelector, ProjectService, Worktree, WorktreeBase};
pub use remote::normalize_remote;
