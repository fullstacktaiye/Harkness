//! Bounded structured Git diff with artifact spill for large JSON payloads.

use std::path::Path;

use harkness_git::{
    DiffLineKind, DiffOmission, DiffOptions, DiffTarget, FileDiff, GitService, Hunk,
    IntraLineDegradation,
};
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize};

use crate::tool::{
    ArtifactRef, ExecutionContext, RequestEffects, RiskLevel, Tool, ToolError, ToolIdentity,
    ToolMetadata,
};
use crate::trust::{PathAccess, PathBoundary};

use super::fs_read::ContentEncoding;
use super::git_status::{GitChange, map_git_error, project_change, project_path};

/// Default maximum serialized inline result size.
pub const DEFAULT_DIFF_INLINE_BYTES: usize = 48 * 1024;
/// Largest inline result callers may request.
pub const MAX_DIFF_INLINE_BYTES: usize = 60 * 1024;
/// Largest individual file budget exposed by the tool contract.
pub const MAX_TOOL_DIFF_FILE_SIZE: u64 = 16 * 1024 * 1024;
/// Largest combined hunk-content budget exposed by the tool contract.
pub const MAX_TOOL_DIFF_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
/// Largest file-content count exposed by the tool contract.
pub const MAX_TOOL_DIFF_FILES: usize = 10_000;
/// Largest context radius exposed by the tool contract.
pub const MAX_TOOL_DIFF_CONTEXT_LINES: u32 = 100;

/// Comparison requested from `git.diff@1.0.0`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum GitDiffTarget {
    Staged,
    Unstaged,
    Commit {
        revision: String,
        parent: Option<String>,
    },
    Revisions {
        old_revision: String,
        new_revision: String,
    },
    RevisionAgainstWorktree {
        revision: String,
    },
    BranchAgainstBase {
        branch: String,
        base_branch: String,
    },
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ShortGitDiffTarget {
    Staged,
    Unstaged,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
enum TaggedGitDiffTarget {
    Staged,
    Unstaged,
    Commit {
        revision: String,
        parent: Option<String>,
    },
    Revisions {
        old_revision: String,
        new_revision: String,
    },
    RevisionAgainstWorktree {
        revision: String,
    },
    BranchAgainstBase {
        branch: String,
        base_branch: String,
    },
}

#[derive(Deserialize, JsonSchema)]
#[serde(untagged)]
enum GitDiffTargetWire {
    Short(ShortGitDiffTarget),
    Tagged(TaggedGitDiffTarget),
}

impl From<GitDiffTargetWire> for GitDiffTarget {
    fn from(wire: GitDiffTargetWire) -> Self {
        match wire {
            GitDiffTargetWire::Short(ShortGitDiffTarget::Staged)
            | GitDiffTargetWire::Tagged(TaggedGitDiffTarget::Staged) => Self::Staged,
            GitDiffTargetWire::Short(ShortGitDiffTarget::Unstaged)
            | GitDiffTargetWire::Tagged(TaggedGitDiffTarget::Unstaged) => Self::Unstaged,
            GitDiffTargetWire::Tagged(TaggedGitDiffTarget::Commit { revision, parent }) => {
                Self::Commit { revision, parent }
            }
            GitDiffTargetWire::Tagged(TaggedGitDiffTarget::Revisions {
                old_revision,
                new_revision,
            }) => Self::Revisions {
                old_revision,
                new_revision,
            },
            GitDiffTargetWire::Tagged(TaggedGitDiffTarget::RevisionAgainstWorktree {
                revision,
            }) => Self::RevisionAgainstWorktree { revision },
            GitDiffTargetWire::Tagged(TaggedGitDiffTarget::BranchAgainstBase {
                branch,
                base_branch,
            }) => Self::BranchAgainstBase {
                branch,
                base_branch,
            },
        }
    }
}

impl<'de> Deserialize<'de> for GitDiffTarget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        GitDiffTargetWire::deserialize(deserializer).map(Into::into)
    }
}

impl JsonSchema for GitDiffTarget {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "GitDiffTarget".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        GitDiffTargetWire::json_schema(generator)
    }
}

/// Input to `git.diff@1.0.0`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GitDiffInput {
    /// Staged, unstaged, or revision comparison.
    pub target: GitDiffTarget,
    /// Optional literal workspace-relative paths.
    #[schemars(length(max = 1024))]
    pub paths: Option<Vec<String>>,
    /// Largest old or new file whose hunks may be returned.
    #[schemars(range(min = 1, max = 16777216))]
    pub max_file_size: Option<u64>,
    /// Combined hunk-content byte budget.
    #[schemars(range(min = 1, max = 67108864))]
    pub max_total_bytes: Option<u64>,
    /// Number of file records allowed to carry content.
    #[schemars(range(min = 1, max = 10000))]
    pub max_files: Option<usize>,
    /// Unchanged lines around each hunk.
    #[schemars(range(max = 100))]
    pub context_lines: Option<u32>,
    /// Serialized result threshold before the full payload becomes an artifact.
    #[schemars(range(min = 1024, max = 61440))]
    pub inline_max_bytes: Option<usize>,
}

/// Stable target spelling on each file record.
#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GitDiffTargetKind {
    Staged,
    Unstaged,
    Commit,
    Revisions,
    RevisionAgainstWorktree,
    BranchAgainstBase,
    Unknown,
}

/// Whitespace settings that produced hunk coordinates.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiffWhitespace {
    pub mode: String,
    pub ignore_blank_lines: bool,
}

/// Named reason one changed file carries no hunks.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum GitDiffOmission {
    FileTooLarge { limit: u64 },
    Unmerged,
    ContentBudgetExhausted { limit: u64 },
    FileBudgetExhausted { limit: usize },
    Unrepresentable { detail: String },
    Unknown,
}

/// One byte-preserving diff line.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitDiffLine {
    pub kind: String,
    pub old_line_number: Option<u32>,
    pub new_line_number: Option<u32>,
    pub content: String,
    pub content_encoding: ContentEncoding,
}

/// One structured diff hunk.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitDiffHunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub header: String,
    pub header_encoding: ContentEncoding,
    pub intra_line_degradation: Option<String>,
    pub lines: Vec<GitDiffLine>,
}

/// One CLI-compatible structured file diff.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitFileDiff {
    pub target: GitDiffTargetKind,
    pub target_details: Option<GitDiffTarget>,
    pub change: GitChange,
    pub old_path: Option<String>,
    pub old_path_is_lossy: Option<bool>,
    pub old_path_base64: Option<String>,
    pub new_path: Option<String>,
    pub new_path_is_lossy: Option<bool>,
    pub new_path_base64: Option<String>,
    pub old_blob_id: String,
    pub new_blob_id: String,
    pub old_mode: u32,
    pub new_mode: u32,
    pub context_lines: u32,
    pub whitespace: DiffWhitespace,
    pub old_size: u64,
    pub new_size: u64,
    pub binary: bool,
    pub omission: Option<GitDiffOmission>,
    pub hunks: Vec<GitDiffHunk>,
}

/// Complete diff payload, stored verbatim as JSON when it does not fit inline.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitDiffPayload {
    pub files: Vec<GitFileDiff>,
}

/// Bounded summary retained inline in both output modes.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitDiffSummary {
    pub changed_files: u64,
    pub omitted_files: u64,
    pub binary_files: u64,
    pub hunks: u64,
}

/// Result of `git.diff@1.0.0`.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitDiffOutput {
    pub summary: GitDiffSummary,
    /// Full file records when the result fits inline.
    pub files: Option<Vec<GitFileDiff>>,
    /// Full JSON payload when `files` is absent.
    pub artifact: Option<ArtifactRef>,
}

/// The production `git.diff@1.0.0` tool.
#[derive(Clone, Copy, Debug, Default)]
pub struct GitDiff;

impl Tool for GitDiff {
    type Input = GitDiffInput;
    type Output = GitDiffOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse("git.diff", "1.0.0").expect("a built-in tool identity"),
            "Inspect a Git diff",
            "Returns the existing bounded structured diff model and spills oversized JSON to a redacted artifact without taking a repository lock.",
            RiskLevel::Observe,
        )
    }

    fn request_effects(
        &self,
        input: &Self::Input,
        boundary: &PathBoundary,
    ) -> Result<RequestEffects, ToolError> {
        input
            .paths
            .as_deref()
            .unwrap_or_default()
            .iter()
            .try_fold(RequestEffects::default(), |effects, path| {
                Ok(effects.with_path(boundary.contain(path)?, PathAccess::Read))
            })
    }

    fn execute(
        &self,
        input: Self::Input,
        context: &mut ExecutionContext,
    ) -> Result<Self::Output, ToolError> {
        context.check_still_permitted()?;
        debug_assert!(input.max_file_size.unwrap_or(1) <= MAX_TOOL_DIFF_FILE_SIZE);
        debug_assert!(input.max_total_bytes.unwrap_or(1) <= MAX_TOOL_DIFF_TOTAL_BYTES);
        debug_assert!(input.max_files.unwrap_or(1) <= MAX_TOOL_DIFF_FILES);
        debug_assert!(input.context_lines.unwrap_or(0) <= MAX_TOOL_DIFF_CONTEXT_LINES);
        let target = to_git_target(&input.target);
        let mut options = DiffOptions::default();
        if let Some(value) = input.max_file_size {
            options = options.with_max_file_size(value);
        }
        if let Some(value) = input.max_total_bytes {
            options = options.with_max_total_bytes(value);
        }
        if let Some(value) = input.max_files {
            options = options.with_max_files(value);
        }
        if let Some(value) = input.context_lines {
            options = options.with_context_lines(value);
        }
        if let Some(paths) = &input.paths {
            let mut relative = Vec::with_capacity(paths.len());
            for supplied in paths {
                context.check_still_permitted()?;
                let resolved = context.resolve(Path::new(supplied))?;
                relative.push(
                    resolved
                        .as_path()
                        .strip_prefix(context.workspace_root())
                        .map_err(ToolError::execution_failed)?
                        .to_path_buf(),
                );
            }
            options = options.with_paths(relative);
        }
        let service = GitService::new(context.workspace_root(), context.workspace_root());
        // libgit2's single diff construction is indivisible, so cancellation is
        // gated immediately before and after it; no lock or process is involved.
        context.check_still_permitted()?;
        let files = service.diff(target, &options).map_err(map_git_error)?;
        context.check_still_permitted()?;
        let mut projected = Vec::with_capacity(files.len());
        for file in &files {
            context.check_still_permitted()?;
            projected.push(project_file(file, context)?);
        }
        let summary = summarize(&projected);
        let tentative = GitDiffOutput {
            summary: summary.clone(),
            files: Some(projected.clone()),
            artifact: None,
        };
        let serialized = serde_json::to_vec(&tentative).map_err(ToolError::execution_failed)?;
        let inline_limit = input.inline_max_bytes.unwrap_or(DEFAULT_DIFF_INLINE_BYTES);
        debug_assert!(inline_limit <= MAX_DIFF_INLINE_BYTES);
        if serialized.len() <= inline_limit {
            return Ok(tentative);
        }
        let payload = serde_json::to_vec(&GitDiffPayload { files: projected })
            .map_err(ToolError::execution_failed)?;
        let artifact = context.write_artifact("git-diff.json", "application/json", &payload)?;
        Ok(GitDiffOutput {
            summary,
            files: None,
            artifact: Some(artifact),
        })
    }
}

fn to_git_target(target: &GitDiffTarget) -> DiffTarget {
    match target {
        GitDiffTarget::Staged => DiffTarget::Staged,
        GitDiffTarget::Unstaged => DiffTarget::Unstaged,
        GitDiffTarget::Commit { revision, parent } => DiffTarget::Commit {
            revision: revision.clone(),
            parent: parent.clone(),
        },
        GitDiffTarget::Revisions {
            old_revision,
            new_revision,
        } => DiffTarget::Revisions {
            old_revision: old_revision.clone(),
            new_revision: new_revision.clone(),
        },
        GitDiffTarget::RevisionAgainstWorktree { revision } => {
            DiffTarget::RevisionAgainstWorktree {
                revision: revision.clone(),
            }
        }
        GitDiffTarget::BranchAgainstBase {
            branch,
            base_branch,
        } => DiffTarget::BranchAgainstBase {
            branch: branch.clone(),
            base_branch: base_branch.clone(),
        },
    }
}

fn project_file(file: &FileDiff, context: &ExecutionContext) -> Result<GitFileDiff, ToolError> {
    let (old_path, old_path_is_lossy, old_path_base64) = optional_path(file.old_path.as_deref());
    let (new_path, new_path_is_lossy, new_path_base64) = optional_path(file.new_path.as_deref());
    let mut hunks = Vec::with_capacity(file.hunks.len());
    for hunk in &file.hunks {
        context.check_still_permitted()?;
        hunks.push(project_hunk(hunk, context)?);
    }
    Ok(GitFileDiff {
        target: target_kind(&file.target),
        target_details: target_details(&file.target),
        change: project_change(file.change),
        old_path,
        old_path_is_lossy,
        old_path_base64,
        new_path,
        new_path_is_lossy,
        new_path_base64,
        old_blob_id: file.old_blob_id.clone(),
        new_blob_id: file.new_blob_id.clone(),
        old_mode: file.old_mode,
        new_mode: file.new_mode,
        context_lines: file.context_lines,
        whitespace: DiffWhitespace {
            mode: file.whitespace.mode.name().to_owned(),
            ignore_blank_lines: file.whitespace.ignore_blank_lines,
        },
        old_size: file.old_size,
        new_size: file.new_size,
        binary: file.binary,
        omission: file.omission.as_ref().map(project_omission),
        hunks,
    })
}

fn optional_path(path: Option<&Path>) -> (Option<String>, Option<bool>, Option<String>) {
    path.map(project_path)
        .map_or((None, None, None), |(path, lossy, encoded)| {
            (Some(path), Some(lossy), encoded)
        })
}

fn target_kind(target: &DiffTarget) -> GitDiffTargetKind {
    match target {
        DiffTarget::Staged => GitDiffTargetKind::Staged,
        DiffTarget::Unstaged => GitDiffTargetKind::Unstaged,
        DiffTarget::Commit { .. } => GitDiffTargetKind::Commit,
        DiffTarget::Revisions { .. } => GitDiffTargetKind::Revisions,
        DiffTarget::RevisionAgainstWorktree { .. } => GitDiffTargetKind::RevisionAgainstWorktree,
        DiffTarget::BranchAgainstBase { .. } => GitDiffTargetKind::BranchAgainstBase,
        _ => GitDiffTargetKind::Unknown,
    }
}

fn target_details(target: &DiffTarget) -> Option<GitDiffTarget> {
    match target {
        DiffTarget::Staged | DiffTarget::Unstaged => None,
        DiffTarget::Commit { revision, parent } => Some(GitDiffTarget::Commit {
            revision: revision.clone(),
            parent: parent.clone(),
        }),
        DiffTarget::Revisions {
            old_revision,
            new_revision,
        } => Some(GitDiffTarget::Revisions {
            old_revision: old_revision.clone(),
            new_revision: new_revision.clone(),
        }),
        DiffTarget::RevisionAgainstWorktree { revision } => {
            Some(GitDiffTarget::RevisionAgainstWorktree {
                revision: revision.clone(),
            })
        }
        DiffTarget::BranchAgainstBase {
            branch,
            base_branch,
        } => Some(GitDiffTarget::BranchAgainstBase {
            branch: branch.clone(),
            base_branch: base_branch.clone(),
        }),
        _ => None,
    }
}

fn project_omission(omission: &DiffOmission) -> GitDiffOmission {
    match omission {
        DiffOmission::FileTooLarge { limit } => GitDiffOmission::FileTooLarge { limit: *limit },
        DiffOmission::Unmerged => GitDiffOmission::Unmerged,
        DiffOmission::ContentBudgetExhausted { limit } => {
            GitDiffOmission::ContentBudgetExhausted { limit: *limit }
        }
        DiffOmission::FileBudgetExhausted { limit } => {
            GitDiffOmission::FileBudgetExhausted { limit: *limit }
        }
        DiffOmission::Unrepresentable { detail } => GitDiffOmission::Unrepresentable {
            detail: detail.clone(),
        },
        _ => GitDiffOmission::Unknown,
    }
}

fn project_hunk(hunk: &Hunk, context: &ExecutionContext) -> Result<GitDiffHunk, ToolError> {
    let (header, header_encoding) = encode_bytes(&hunk.header);
    let mut lines = Vec::with_capacity(hunk.lines.len());
    for line in &hunk.lines {
        context.check_still_permitted()?;
        let (content, content_encoding) = encode_bytes(&line.content);
        lines.push(GitDiffLine {
            kind: line_kind(line.kind).to_owned(),
            old_line_number: line.old_line_number,
            new_line_number: line.new_line_number,
            content,
            content_encoding,
        });
    }
    Ok(GitDiffHunk {
        old_start: hunk.old_start,
        old_lines: hunk.old_lines,
        new_start: hunk.new_start,
        new_lines: hunk.new_lines,
        header,
        header_encoding,
        intra_line_degradation: hunk.intra_line_degradation.as_ref().map(degradation_name),
        lines,
    })
}

fn encode_bytes(bytes: &[u8]) -> (String, ContentEncoding) {
    match std::str::from_utf8(bytes) {
        Ok(text) => (text.to_owned(), ContentEncoding::Utf8),
        Err(_) => {
            use base64::Engine as _;
            (
                base64::engine::general_purpose::STANDARD.encode(bytes),
                ContentEncoding::Base64,
            )
        }
    }
}

fn line_kind(kind: DiffLineKind) -> &'static str {
    match kind {
        DiffLineKind::Context => "context",
        DiffLineKind::Addition => "addition",
        DiffLineKind::Deletion => "deletion",
        DiffLineKind::BothEofNoNewline => "both_eof_no_newline",
        DiffLineKind::OldEofNoNewline => "old_eof_no_newline",
        DiffLineKind::NewEofNoNewline => "new_eof_no_newline",
        _ => "unknown",
    }
}

fn degradation_name(degradation: &IntraLineDegradation) -> String {
    match degradation {
        IntraLineDegradation::LineTooLong { limit } => format!("line_too_long:{limit}"),
        IntraLineDegradation::PairingTooLarge { limit } => format!("pairing_too_large:{limit}"),
        _ => "unknown".to_owned(),
    }
}

fn summarize(files: &[GitFileDiff]) -> GitDiffSummary {
    GitDiffSummary {
        changed_files: u64::try_from(files.len()).unwrap_or(u64::MAX),
        omitted_files: u64::try_from(files.iter().filter(|file| file.omission.is_some()).count())
            .unwrap_or(u64::MAX),
        binary_files: u64::try_from(files.iter().filter(|file| file.binary).count())
            .unwrap_or(u64::MAX),
        hunks: u64::try_from(files.iter().map(|file| file.hunks.len()).sum::<usize>())
            .unwrap_or(u64::MAX),
    }
}
