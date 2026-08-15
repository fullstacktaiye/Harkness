//! Bounded overview of one workspace root.

use harkness_core::list_directory;
use harkness_git::GitService;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::tool::{
    ExecutionContext, RiskLevel, Tool, ToolError, ToolIdentity, ToolMetadata, WorkspaceSourceKind,
};

use super::git_status::{
    GitHead, GitUpstream, map_git_error, project_head, project_path, project_upstream,
};

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
        omitted_entries: usize,
    },
    /// Directory entries did not fit the inline result budget.
    OutputBudgetExhausted {
        limit: usize,
        omitted_entries: usize,
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
        let mut listed =
            list_directory(context.workspace_root()).map_err(ToolError::execution_failed)?;
        context.check_still_permitted()?;
        let mut omission =
            (listed.len() > maximum).then(|| InspectOmission::EntryBudgetExhausted {
                limit: maximum,
                omitted_entries: listed.len() - maximum,
            });
        listed.truncate(maximum);
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
                name: entry.name,
                path,
                path_is_lossy,
                path_base64,
                is_dir: entry.is_dir,
                expandable: entry.expandable,
            };
            let bytes = serde_json::to_vec(&projected)
                .map_err(ToolError::execution_failed)?
                .len();
            if entry_bytes.saturating_add(bytes) > output_budget {
                omission = Some(InspectOmission::OutputBudgetExhausted {
                    limit: output_budget,
                    omitted_entries: listed_count - entries.len(),
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
