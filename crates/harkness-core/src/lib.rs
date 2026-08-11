//! Shared catalog and cross-domain application behavior for Harkness front ends.
//!
//! Public project workflows use types from the lower-level [`harkness_git`]
//! crate. It is re-exported here so an application that wants the core facade
//! can name those types without declaring and version-aligning a second direct
//! dependency.

mod catalog;
mod editor;
mod listing;
mod paths;
mod project;
mod remote;
#[cfg(test)]
mod testing;

pub use catalog::entry::{Project, ProjectId, ProjectSource};
pub use editor::{
    EditorConfiguration, EditorError, EditorLaunch, EditorLaunchContext, EditorPosition,
    EditorPreset,
};
pub use harkness_git;
pub use harkness_git::{
    Cancellation, GitError, GitService, GitStatus, InspectionSource, WorktreeBase,
};
pub use listing::{DirEntry, list_directory};
pub use paths::data_directory;
pub use project::{
    ProjectError, ProjectSelector, ProjectService, Worktree, WorktreeReconciliation,
    WorktreeReconciliationSkip,
};
pub use remote::normalize_remote;
