//! Typed, in-process projection of detailed repository status.

use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use harkness_git::{
    FileChange, GitError, GitService, HeadState, PendingOperation, StatusEntry, UpstreamStatus,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::tool::{ExecutionContext, RiskLevel, Tool, ToolError, ToolIdentity, ToolMetadata};

/// Input to `git.status@1.0.0`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GitStatusInput {}

/// Checked-out head projection.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum GitHead {
    /// A branch with no commit yet.
    Unborn { branch: Option<String> },
    /// A named branch.
    Branch { name: String },
    /// A detached commit.
    Detached { commit: String },
}

/// Locally known upstream divergence.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitUpstream {
    /// Tracked branch name.
    pub name: String,
    /// Local-only commit count.
    pub ahead: u64,
    /// Upstream-only commit count.
    pub behind: u64,
}

/// Stable change spelling shared with the CLI status projection.
#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GitChange {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
    Unmerged,
}

/// One path in detailed status.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitStatusEntry {
    /// Lossy display spelling of the path.
    pub path: String,
    /// Whether `path` lost platform-native information.
    pub path_is_lossy: bool,
    /// Base64 over exact path bytes where the platform exposes them.
    pub path_base64: Option<String>,
    /// Index-to-HEAD change.
    pub staged: Option<GitChange>,
    /// Worktree-to-index change.
    pub unstaged: Option<GitChange>,
    /// Lossy display spelling of a rename or copy source.
    pub rename_source: Option<String>,
    /// Whether `rename_source` lost platform-native information.
    pub rename_source_is_lossy: Option<bool>,
    /// Base64 over exact source bytes where available.
    pub rename_source_base64: Option<String>,
    /// Whether the path is unresolved.
    pub conflicted: bool,
}

/// Result of `git.status@1.0.0`.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitStatusOutput {
    /// Checked-out head.
    pub head: GitHead,
    /// Local view of the tracked branch.
    pub upstream: Option<GitUpstream>,
    /// Stable pending-operation spelling.
    pub pending: Option<String>,
    /// Every changed, untracked, or conflicted path.
    pub entries: Vec<GitStatusEntry>,
}

/// The production `git.status@1.0.0` tool.
#[derive(Clone, Copy, Debug, Default)]
pub struct GitStatus;

impl Tool for GitStatus {
    type Input = GitStatusInput;
    type Output = GitStatusOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse("git.status", "1.0.0").expect("a built-in tool identity"),
            "Inspect Git status",
            "Returns the checked-out head, upstream divergence, pending operation, and every changed path without taking a repository lock or spawning Git.",
            RiskLevel::Observe,
        )
    }

    fn execute(
        &self,
        _input: Self::Input,
        context: &mut ExecutionContext,
    ) -> Result<Self::Output, ToolError> {
        context.check_still_permitted()?;
        let service = GitService::new(context.workspace_root(), context.workspace_root());
        let status = service
            .detailed_status_in_process(context.cancellation())
            .map_err(map_git_error)?;
        context.check_still_permitted()?;
        let output = GitStatusOutput {
            head: project_head(&status.head),
            upstream: status.upstream.as_ref().map(project_upstream),
            pending: status.pending.map(pending_name).map(str::to_owned),
            entries: status.entries.iter().map(project_entry).collect(),
        };
        let serialized = serde_json::to_vec(&output).map_err(ToolError::execution_failed)?;
        if serialized.len() > 60 * 1024 {
            return Err(ToolError::OutputBudgetExhausted { limit: 60 * 1024 });
        }
        Ok(output)
    }
}

pub(super) fn map_git_error(error: GitError) -> ToolError {
    if matches!(error, GitError::Cancelled) {
        ToolError::Cancelled
    } else {
        ToolError::execution_failed(error)
    }
}

pub(super) fn project_head(head: &HeadState) -> GitHead {
    match head {
        HeadState::Unborn { branch } => GitHead::Unborn {
            branch: branch.clone(),
        },
        HeadState::Branch { name } => GitHead::Branch { name: name.clone() },
        HeadState::Detached { commit } => GitHead::Detached {
            commit: commit.clone(),
        },
    }
}

pub(super) fn project_upstream(upstream: &UpstreamStatus) -> GitUpstream {
    GitUpstream {
        name: upstream.name.clone(),
        ahead: u64::try_from(upstream.ahead).unwrap_or(u64::MAX),
        behind: u64::try_from(upstream.behind).unwrap_or(u64::MAX),
    }
}

fn project_entry(entry: &StatusEntry) -> GitStatusEntry {
    let (path, path_is_lossy, path_base64) = project_path(&entry.path);
    let (rename_source, rename_source_is_lossy, rename_source_base64) = entry
        .rename_source
        .as_deref()
        .map(project_path)
        .map_or((None, None, None), |(path, lossy, encoded)| {
            (Some(path), Some(lossy), encoded)
        });
    GitStatusEntry {
        path,
        path_is_lossy,
        path_base64,
        staged: entry.staged.map(project_change),
        unstaged: entry.unstaged.map(project_change),
        rename_source,
        rename_source_is_lossy,
        rename_source_base64,
        conflicted: entry.conflicted,
    }
}

pub(super) fn project_change(change: FileChange) -> GitChange {
    match change {
        FileChange::Added => GitChange::Added,
        FileChange::Modified => GitChange::Modified,
        FileChange::Deleted => GitChange::Deleted,
        FileChange::Renamed => GitChange::Renamed,
        FileChange::Copied => GitChange::Copied,
        FileChange::TypeChanged => GitChange::TypeChanged,
        FileChange::Untracked => GitChange::Untracked,
        FileChange::Unmerged => GitChange::Unmerged,
    }
}

pub(super) fn project_path(path: &Path) -> (String, bool, Option<String>) {
    let display = path.to_string_lossy().into_owned();
    let lossy = path.as_os_str().to_str().is_none();
    (
        display,
        lossy,
        path_bytes(path).map(|bytes| BASE64.encode(bytes)),
    )
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Option<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    Some(path.as_os_str().as_bytes().to_vec())
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Option<Vec<u8>> {
    path.to_str().map(|path| path.as_bytes().to_vec())
}

fn pending_name(pending: PendingOperation) -> &'static str {
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
