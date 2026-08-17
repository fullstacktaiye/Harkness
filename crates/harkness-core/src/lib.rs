//! Shared catalog and cross-domain application behavior for Harkness front ends.
//!
//! Public project workflows use types from the lower-level [`harkness_git`]
//! crate. It is re-exported here so an application that wants the core facade
//! can name those types without declaring and version-aligning a second direct
//! dependency.

mod catalog;
mod check;
mod editor;
mod listing;
mod paths;
mod project;
mod remote;
#[cfg(test)]
mod testing;

pub use catalog::entry::{Project, ProjectId, ProjectSource};
pub use check::{
    CheckConfiguration, CheckParser, MAX_CHECK_ARGUMENTS, MAX_CHECK_ENVIRONMENT_ENTRIES,
    MAX_CHECK_SERIALIZED_BYTES, MAX_CHECK_TEXT_BYTES, MAX_PROJECT_CHECKS, default_checks,
};
pub use editor::{
    EditorConfiguration, EditorError, EditorLaunch, EditorLaunchContext, EditorPosition,
    EditorPreset,
};
pub use harkness_git;
pub use harkness_git::{
    Cancellation, GitError, GitService, GitStatus, InspectionSource, WorktreeBase,
};
pub use listing::{
    DirEntry, compare_directory_entries, directory_entry_is_visible, list_directory,
};
pub use paths::data_directory;
pub use project::{
    ProjectError, ProjectSelector, ProjectService, Worktree, WorktreeReconciliation,
    WorktreeReconciliationSkip,
};
pub use remote::normalize_remote;
