//! Production tools shipped with the Harkness runtime.
//!
//! A tool stays independently invocable: registration needs no agent or
//! coordinator, and execution needs only the ordinary registry and execution
//! context. Policy and scheduling consume the descriptors but remain outside
//! the tool bodies.

mod fs_apply_patch;
mod fs_read;
mod git_diff;
mod git_status;
mod process_exec;
mod safe_read;
mod workspace_inspect;
mod workspace_search;

pub use fs_apply_patch::{
    ApplyPatchInput, ApplyPatchOutput, FileBase, FileChangeKind, FileChangeSummary, FsApplyPatch,
};
pub use fs_read::{
    ContentEncoding, DEFAULT_FS_READ_MAX_BYTES, FsRead, FsReadInput, FsReadOutput,
    MAX_FS_READ_BYTES, ReadTruncation,
};
pub use git_diff::{
    DEFAULT_DIFF_INLINE_BYTES, DiffWhitespace, GitDiff, GitDiffHunk, GitDiffInput, GitDiffLine,
    GitDiffOmission, GitDiffOutput, GitDiffPayload, GitDiffSummary, GitDiffTarget,
    GitDiffTargetKind, GitFileDiff, MAX_DIFF_INLINE_BYTES, MAX_TOOL_DIFF_CONTEXT_LINES,
    MAX_TOOL_DIFF_FILE_SIZE, MAX_TOOL_DIFF_FILES, MAX_TOOL_DIFF_TOTAL_BYTES,
};
pub use git_status::{
    GitChange, GitHead, GitStatus, GitStatusEntry, GitStatusInput, GitStatusOmission,
    GitStatusOutput, GitUpstream,
};
pub use process_exec::{
    BoundedText, ProcessExec, ProcessExecInput, ProcessExecOutput, TestRun, TestRunInput,
    TestRunOutput,
};
pub use workspace_inspect::{
    DEFAULT_INSPECT_MAX_ENTRIES, InspectOmission, InspectedProject, MAX_INSPECT_ENTRIES,
    ProjectSourceKind, WorkspaceEntry, WorkspaceGitSummary, WorkspaceInspect,
    WorkspaceInspectInput, WorkspaceInspectOutput,
};
pub use workspace_search::{
    DEFAULT_SEARCH_MAX_MATCHES, DEFAULT_SEARCH_MAX_PER_FILE, DEFAULT_SEARCH_TOTAL_BYTES,
    MAX_SEARCH_FILES, MAX_SEARCH_PATTERN_BYTES, MAX_SEARCH_SCANNED_BYTES, SearchMatch,
    SearchOmission, WorkspaceSearch, WorkspaceSearchInput, WorkspaceSearchOutput,
};

use crate::tool::{RegistryError, ToolRegistry};

/// Registers the five read-only observation tools from issue 94.
///
/// # Errors
///
/// Returns the first schema, metadata, or duplicate-registration refusal.
pub fn register_read_only_tools(registry: &mut ToolRegistry) -> Result<(), RegistryError> {
    registry.register(WorkspaceInspect)?;
    registry.register(FsRead)?;
    registry.register(WorkspaceSearch)?;
    registry.register(GitStatus)?;
    registry.register(GitDiff)?;
    Ok(())
}

/// Registers the workspace-mutating and process tools from issue 95.
///
/// All three identities are published at `1.0.0`. The function is intentionally
/// separate from the read-only tool set so front ends can assemble a registry
/// explicitly while the two issue tracks land independently.
///
/// # Errors
///
/// Returns the first schema, metadata, or duplicate-registration refusal.
pub fn register_mutating_tools(registry: &mut ToolRegistry) -> Result<(), RegistryError> {
    registry.register(FsApplyPatch)?;
    registry.register(ProcessExec)?;
    registry.register(TestRun)?;
    Ok(())
}

#[cfg(test)]
mod read_tests;
#[cfg(test)]
mod tests;
