//! Bounded overview of one workspace root.

use harkness_git::GitService;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::tool::{
    ExecutionContext, RiskLevel, Tool, ToolError, ToolIdentity, ToolMetadata, WorkspaceSourceKind,
};

use super::git_status::{
    GitHead, GitUpstream, map_git_error, project_head, project_path, project_upstream,
};
use super::safe_read::list_directory;

/// Default top-level entry count returned by inspection.
pub const DEFAULT_INSPECT_MAX_ENTRIES: usize = 256;
/// Hard maximum top-level entry count.
pub const MAX_INSPECT_ENTRIES: usize = 4096;

/// Input to `workspace.inspect@1.0.0`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceInspectInput {
    /// Maximum top-level entries to return. Defaults to 256.
    #[schemars(range(min = 1, max = 4096))]
    pub max_entries: Option<usize>,
    /// Serialized entry budget. Defaults to 48 KiB.
    #[schemars(range(min = 1024, max = 49152))]
    pub max_output_bytes: Option<usize>,
}

/// Catalog source kind, present only when the caller supplied catalog metadata.
#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectSourceKind {
    Local,
    ManagedRepository,
    Worktree,
}

/// Authoritative project-catalog identity attached to the call.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InspectedProject {
    /// Stable catalog identifier.
    pub id: String,
    /// Catalog display name.
    pub display_name: String,
    /// Catalog source kind.
    pub source: ProjectSourceKind,
}

/// Cheap Git summary for the workspace.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceGitSummary {
    /// Checked-out head.
    pub head: GitHead,
    /// Whether any tracked or untracked change exists.
    pub dirty: bool,
    /// Number of paths differing between HEAD and the index.
    pub staged: u64,
    /// Number of tracked paths differing between index and worktree.
    pub unstaged: u64,
    /// Locally known upstream divergence.
    pub upstream: Option<GitUpstream>,
}

/// One top-level workspace entry.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceEntry {
    /// Lossy display name from the directory listing.
    pub name: String,
    /// Lossy workspace-relative path.
    pub path: String,
    /// Whether the relative path lost native path information.
    pub path_is_lossy: bool,
    /// Base64 over exact relative path bytes where available.
    pub path_base64: Option<String>,
    /// Whether this is a real directory rather than a symlink to one.
    pub is_dir: bool,
    /// Whether a caller may list its children.
    pub expandable: bool,
}

/// Why the top-level listing is incomplete.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum InspectOmission {
    /// More direct children existed than the requested bound.
    EntryBudgetExhausted {
        limit: usize,
        at_least_omitted_entries: usize,
    },
    /// Directory entries did not fit the inline result budget.
    OutputBudgetExhausted {
        limit: usize,
        omitted_entries: usize,
        /// Whether the directory walk *also* stopped at its entry sentinel, so
        /// more children exist than `omitted_entries` counts. Only one omission
        /// is reported, and without this the caller could not tell a listing
        /// trimmed for size from one that was never fully walked.
        entry_budget_also_exhausted: bool,
    },
}

/// Result of `workspace.inspect@1.0.0`.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceInspectOutput {
    /// Authoritative catalog metadata, or null for a detached direct invocation.
    pub project: Option<InspectedProject>,
    /// Presentation-only label derived from the root name when no catalog name exists.
    pub display_label: String,
    /// Lossy canonical root spelling.
    pub root: String,
    /// Whether the root spelling is lossy.
    pub root_is_lossy: bool,
    /// Exact root bytes where the platform exposes them.
    pub root_base64: Option<String>,
    /// Git state, or null when the workspace is not a repository.
    pub git: Option<WorkspaceGitSummary>,
    /// Bounded top-level listing. `.git` is never included.
    pub entries: Vec<WorkspaceEntry>,
    /// Named reason entries were withheld.
    pub omission: Option<InspectOmission>,
}

/// The production `workspace.inspect@1.0.0` tool.
#[derive(Clone, Copy, Debug, Default)]
pub struct WorkspaceInspect;

impl Tool for WorkspaceInspect {
    type Input = WorkspaceInspectInput;
    type Output = WorkspaceInspectOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse("workspace.inspect", "1.0.0").expect("a built-in tool identity"),
            "Inspect the workspace",
            "Returns optional authoritative catalog identity, current Git state, and a bounded safe top-level directory listing.",
            RiskLevel::Observe,
        )
    }

    fn execute(
        &self,
        input: Self::Input,
        context: &mut ExecutionContext,
    ) -> Result<Self::Output, ToolError> {
        context.check_still_permitted()?;
        let maximum = input.max_entries.unwrap_or(DEFAULT_INSPECT_MAX_ENTRIES);
        let output_budget = input.max_output_bytes.unwrap_or(48 * 1024);
        debug_assert!(maximum <= MAX_INSPECT_ENTRIES);
        let root = context.resolve(".")?;
        let listing = list_directory(&root, maximum, context)?;
        context.check_still_permitted()?;
        let entry_budget_hit = listing.truncated;
        let mut omission = entry_budget_hit.then_some(InspectOmission::EntryBudgetExhausted {
            limit: maximum,
            at_least_omitted_entries: 1,
        });
        let listed = listing.entries;
        let listed_count = listed.len();
        let mut entry_bytes = 0_usize;
        let mut entries = Vec::new();
        for entry in listed {
            let relative = entry
                .path
                .strip_prefix(context.workspace_root())
                .unwrap_or(entry.path.as_path());
            let (path, path_is_lossy, path_base64) = project_path(relative);
            let projected = WorkspaceEntry {
                name: entry.name.to_string_lossy().into_owned(),
                path,
                path_is_lossy,
                path_base64,
                is_dir: entry.is_dir,
                expandable: entry.is_dir,
            };
            let bytes = serde_json::to_vec(&projected)
                .map_err(ToolError::execution_failed)?
                .len();
            if entry_bytes.saturating_add(bytes) > output_budget {
                // The walk itself may also have stopped early, and this used to
                // overwrite that. `git.status` folds the two the same way: the
                // count reports what the caller did not get, and the entry
                // budget is still named as a cause rather than disappearing
                // because a second budget was reached afterwards.
                omission = Some(InspectOmission::OutputBudgetExhausted {
                    limit: output_budget,
                    omitted_entries: listed_count - entries.len(),
                    entry_budget_also_exhausted: entry_budget_hit,
                });
                break;
            }
            entry_bytes += bytes;
            entries.push(projected);
        }
        let service = GitService::new(context.workspace_root(), context.workspace_root());
        let git = service
            .status()
            .map_err(map_git_error)?
            .map(|status| {
                let head = service
                    .head_state()
                    .map_err(map_git_error)?
                    .ok_or_else(|| ToolError::execution_failed("Git status had no head state"))?;
                Ok::<WorkspaceGitSummary, ToolError>(WorkspaceGitSummary {
                    head: project_head(&head),
                    dirty: status.dirty,
                    staged: u64::try_from(status.staged).unwrap_or(u64::MAX),
                    unstaged: u64::try_from(status.unstaged).unwrap_or(u64::MAX),
                    upstream: status.upstream.as_ref().map(project_upstream),
                })
            })
            .transpose()?;
        let metadata = context.workspace_metadata();
        let project = metadata.map(|metadata| InspectedProject {
            id: metadata.project_id().to_string(),
            display_name: metadata.display_name().to_owned(),
            source: match metadata.source() {
                WorkspaceSourceKind::Local => ProjectSourceKind::Local,
                WorkspaceSourceKind::ManagedRepository => ProjectSourceKind::ManagedRepository,
                WorkspaceSourceKind::Worktree => ProjectSourceKind::Worktree,
            },
        });
        let display_label = metadata.map_or_else(
            || {
                context.workspace_root().file_name().map_or_else(
                    || context.workspace_root().to_string_lossy().into_owned(),
                    |name| name.to_string_lossy().into_owned(),
                )
            },
            |metadata| metadata.display_name().to_owned(),
        );
        let (root, root_is_lossy, root_base64) = project_path(context.workspace_root());
        context.check_still_permitted()?;
        Ok(WorkspaceInspectOutput {
            project,
            display_label,
            root,
            root_is_lossy,
            root_base64,
            git,
            entries,
            omission,
        })
    }
}
