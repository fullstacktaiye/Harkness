//! Shared application behavior for Harkness front ends.

mod catalog;
mod listing;
mod paths;
mod project;
mod remote;
#[cfg(test)]
mod testing;

pub use catalog::entry::{Project, ProjectId, ProjectSource};
pub use listing::{DirEntry, list_directory};
pub use project::{
    ProjectError, ProjectSelector, ProjectService, Worktree, WorktreeReconciliation,
    WorktreeReconciliationSkip,
};
pub use remote::normalize_remote;
