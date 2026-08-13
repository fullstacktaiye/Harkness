//! Stale-safe unified-diff application within one workspace.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, Permissions};
use std::io::Write;
use std::path::{Path, PathBuf};

use git2::{Delta, Diff, DiffFormat, DiffLineType, DiffOptions, Patch, Repository};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::tool::{
    ArtifactRef, Capability, ExecutionContext, RiskLevel, Tool, ToolError, ToolIdentity,
    ToolMetadata,
};
use crate::trust::ContainedPath;

/// Input to `fs.apply_patch@1.0.0`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ApplyPatchInput {
    /// Git-style unified diff to apply.
    pub patch: String,
    /// Exact precondition for every target file. A null hash means new file.
    #[schemars(length(min = 1))]
    pub bases: Vec<FileBase>,
}

/// Approved base identity for one patch target.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FileBase {
    /// Workspace-relative target path.
    pub path: String,
    /// Lowercase SHA-256 of current bytes, or null to assert the file is absent.
    pub base_sha256: Option<String>,
}

/// Kind of workspace-visible change a patch produced.
#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    /// The target did not exist before this call.
    Created,
    /// The target existed and was replaced atomically.
    Modified,
}

/// Result for one patched file.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileChangeSummary {
    /// Workspace-relative path from the patch.
    pub path: String,
    /// Whether the file was created or modified.
    pub change: FileChangeKind,
    /// Number of unified-diff hunks applied.
    pub hunks_applied: u64,
    /// Signed difference between resulting and base byte lengths.
    pub byte_delta: i64,
}

/// Result of `fs.apply_patch@1.0.0`.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyPatchOutput {
    /// Per-file results in lexicographic path order.
    pub files: Vec<FileChangeSummary>,
    /// Resulting worktree diff for exactly the touched paths.
    pub diff_artifact: ArtifactRef,
}

/// The production `fs.apply_patch@1.0.0` tool.
#[derive(Clone, Copy, Debug, Default)]
pub struct FsApplyPatch;

impl Tool for FsApplyPatch {
    type Input = ApplyPatchInput;
    type Output = ApplyPatchOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse("fs.apply_patch", "1.0.0").expect("a built-in tool identity"),
            "Apply a workspace patch",
            "Applies a unified diff only when every target still matches its approved SHA-256, validating every hunk before any atomic file replacement.",
            RiskLevel::WorkspaceWrite,
        )
        .with_capabilities([
            Capability::new("fs.write").expect("a built-in capability"),
        ])
    }

    fn execute(
        &self,
        input: Self::Input,
        context: &mut ExecutionContext,
    ) -> Result<Self::Output, ToolError> {
        context.check_still_permitted()?;
        let repository = Repository::open(context.workspace_root()).map_err(|error| {
            ToolError::execution_failed(format!(
                "the workspace is not an available Git repository: {error}"
            ))
        })?;
        let parsed = parse_patch(input.patch.as_bytes())?;
        let bases = resolve_bases(input.bases, context)?;
        let mut prepared = validate_all(parsed, &bases, context)?;
        prepared.sort_by(|left, right| left.relative.cmp(&right.relative));

        for file in &prepared {
            context.check_still_permitted()?;
            write_atomically(file)?;
        }

        let touched = prepared
            .iter()
            .map(|file| file.relative.clone())
            .collect::<Vec<_>>();
        let diff = resulting_diff(&repository, &touched)?;
        let diff_artifact = context.write_artifact("applied.patch", "text/x-diff", &diff)?;
        Ok(ApplyPatchOutput {
            files: prepared.iter().map(PreparedFile::summary).collect(),
            diff_artifact,
        })
    }
}

struct ParsedFile {
    relative: PathBuf,
    hunks: Vec<ParsedHunk>,
    created: bool,
}

struct ParsedHunk {
    old_start: usize,
    old_lines: usize,
    lines: Vec<ParsedLine>,
}

enum ParsedLine {
    Context(Vec<u8>),
    Addition(Vec<u8>),
    Deletion(Vec<u8>),
}

struct ResolvedBase {
    relative: PathBuf,
    path: ContainedPath,
    original: Option<Vec<u8>>,
    permissions: Option<Permissions>,
}

struct PreparedFile {
    relative: PathBuf,
    path: ContainedPath,
    original_len: usize,
    resulting: Vec<u8>,
    hunks: usize,
    created: bool,
    permissions: Option<Permissions>,
}

impl PreparedFile {
    fn summary(&self) -> FileChangeSummary {
        FileChangeSummary {
            path: display_path(&self.relative),
            change: if self.created {
                FileChangeKind::Created
            } else {
                FileChangeKind::Modified
            },
            hunks_applied: u64::try_from(self.hunks).unwrap_or(u64::MAX),
            byte_delta: signed_delta(self.resulting.len(), self.original_len),
        }
    }
}

fn parse_patch(bytes: &[u8]) -> Result<Vec<ParsedFile>, ToolError> {
    if bytes.is_empty() {
        return Err(patch_conflict("<patch>", "the patch is empty"));
    }
    let diff = Diff::from_buffer(bytes)
        .map_err(|error| patch_conflict("<patch>", format!("invalid unified diff: {error}")))?;
    if diff.deltas().len() == 0 {
        return Err(patch_conflict(
            "<patch>",
            "the patch contains no file changes",
        ));
    }

    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    for index in 0..diff.deltas().len() {
        let delta = diff
            .get_delta(index)
            .ok_or_else(|| patch_conflict("<patch>", "a parsed file delta disappeared"))?;
        let created = match delta.status() {
            Delta::Added => true,
            Delta::Modified => false,
            status => {
                return Err(patch_conflict(
                    delta
                        .new_file()
                        .path()
                        .or_else(|| delta.old_file().path())
                        .unwrap_or_else(|| Path::new("<patch>")),
                    format!(
                        "unsupported {status:?} delta; only file creation and modification are allowed"
                    ),
                ));
            }
        };
        let relative = delta
            .new_file()
            .path()
            .ok_or_else(|| patch_conflict("<patch>", "a target path is missing"))?
            .to_path_buf();
        ensure_relative_normal_path(&relative)?;
        if !seen.insert(relative.clone()) {
            return Err(patch_conflict(
                &relative,
                "the patch targets this path more than once",
            ));
        }
        if delta.old_file().is_binary() || delta.new_file().is_binary() {
            return Err(patch_conflict(
                &relative,
                "binary patches are not supported",
            ));
        }
        let patch = Patch::from_diff(&diff, index)
            .map_err(|error| patch_conflict(&relative, error))?
            .ok_or_else(|| patch_conflict(&relative, "the patch has no textual hunks"))?;
        let mut hunks = Vec::new();
        for hunk_index in 0..patch.num_hunks() {
            let (hunk, line_count) = patch
                .hunk(hunk_index)
                .map_err(|error| patch_conflict(&relative, error))?;
            let mut lines = Vec::new();
            for line_index in 0..line_count {
                let line = patch
                    .line_in_hunk(hunk_index, line_index)
                    .map_err(|error| patch_conflict(&relative, error))?;
                match line.origin_value() {
                    DiffLineType::Context => {
                        lines.push(ParsedLine::Context(line.content().to_vec()))
                    }
                    DiffLineType::Addition => {
                        lines.push(ParsedLine::Addition(line.content().to_vec()))
                    }
                    DiffLineType::Deletion => {
                        lines.push(ParsedLine::Deletion(line.content().to_vec()))
                    }
                    DiffLineType::ContextEOFNL
                    | DiffLineType::AddEOFNL
                    | DiffLineType::DeleteEOFNL => {
                        let Some(previous) = lines.last_mut() else {
                            return Err(patch_conflict(
                                &relative,
                                "a no-newline marker appears before any hunk line",
                            ));
                        };
                        let bytes = match previous {
                            ParsedLine::Context(bytes)
                            | ParsedLine::Addition(bytes)
                            | ParsedLine::Deletion(bytes) => bytes,
                        };
                        if bytes.last() == Some(&b'\n') {
                            bytes.pop();
                        }
                    }
                    kind => {
                        return Err(patch_conflict(
                            &relative,
                            format!("unsupported line type {kind:?}"),
                        ));
                    }
                }
            }
            hunks.push(ParsedHunk {
                old_start: hunk.old_start() as usize,
                old_lines: hunk.old_lines() as usize,
                lines,
            });
        }
        files.push(ParsedFile {
            relative,
            hunks,
            created,
        });
    }
    Ok(files)
}

fn resolve_bases(
    bases: Vec<FileBase>,
    context: &ExecutionContext,
) -> Result<BTreeMap<PathBuf, ResolvedBase>, ToolError> {
    let mut resolved = BTreeMap::new();
    for base in bases {
        let relative = PathBuf::from(&base.path);
        ensure_relative_normal_path(&relative)?;
        if resolved.contains_key(&relative) {
            return Err(patch_conflict(
                &relative,
                "the base is declared more than once",
            ));
        }
        let path = context
            .resolve(&relative)
            .map_err(|error| forbidden_patch_path(&relative, error))?;
        let metadata = match fs::symlink_metadata(path.as_path()) {
            Ok(metadata) if metadata.file_type().is_file() => Some(metadata),
            Ok(_) => {
                return Err(patch_conflict(
                    &relative,
                    "the target is not a regular file",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(io_failure(&relative, "could not inspect", error)),
        };
        let original = metadata
            .as_ref()
            .map(|_| fs::read(path.as_path()))
            .transpose()
            .map_err(|error| io_failure(&relative, "could not read", error))?;
        validate_hash(&relative, base.base_sha256.as_deref(), original.as_deref())?;
        resolved.insert(
            relative.clone(),
            ResolvedBase {
                relative,
                path,
                original,
                permissions: metadata.map(|metadata| metadata.permissions()),
            },
        );
    }
    Ok(resolved)
}

fn validate_all(
    parsed: Vec<ParsedFile>,
    bases: &BTreeMap<PathBuf, ResolvedBase>,
    context: &ExecutionContext,
) -> Result<Vec<PreparedFile>, ToolError> {
    let patch_paths = parsed
        .iter()
        .map(|file| file.relative.clone())
        .collect::<BTreeSet<_>>();
    let base_paths = bases.keys().cloned().collect::<BTreeSet<_>>();
    if patch_paths != base_paths {
        let missing = patch_paths.difference(&base_paths).next();
        let extra = base_paths.difference(&patch_paths).next();
        let detail = match (missing, extra) {
            (Some(path), _) => format!("no base precondition was supplied for {}", path.display()),
            (_, Some(path)) => format!("a base was supplied for untouched path {}", path.display()),
            _ => "the patch repeats a target path".to_owned(),
        };
        return Err(patch_conflict("<patch>", detail));
    }

    let mut prepared = Vec::new();
    for file in parsed {
        context.check_still_permitted()?;
        let base = bases.get(&file.relative).expect("path sets were compared");
        if file.created != base.original.is_none() {
            return Err(patch_conflict(
                &file.relative,
                if file.created {
                    "the patch declares a new file but the base exists"
                } else {
                    "the patch modifies a file whose base declares it new"
                },
            ));
        }
        let original = base.original.as_deref().unwrap_or_default();
        let resulting = apply_hunks(&file.relative, original, &file.hunks)?;
        let parent =
            base.path.as_path().parent().ok_or_else(|| {
                patch_conflict(&file.relative, "the target has no parent directory")
            })?;
        if !parent.is_dir() {
            return Err(patch_conflict(
                &file.relative,
                "the target parent directory does not exist",
            ));
        }
        prepared.push(PreparedFile {
            relative: base.relative.clone(),
            path: base.path.clone(),
            original_len: original.len(),
            resulting,
            hunks: file.hunks.len(),
            created: file.created,
            permissions: base.permissions.clone(),
        });
    }
    Ok(prepared)
}

fn apply_hunks(path: &Path, original: &[u8], hunks: &[ParsedHunk]) -> Result<Vec<u8>, ToolError> {
    let source = split_lines(original);
    let mut output = Vec::with_capacity(original.len());
    let mut cursor = 0usize;
    for (number, hunk) in hunks.iter().enumerate() {
        let start = if hunk.old_start == 0 {
            0
        } else {
            hunk.old_start - 1
        };
        if start < cursor || start > source.len() {
            return Err(patch_conflict(
                path,
                format!("hunk {} starts outside the remaining base", number + 1),
            ));
        }
        for line in &source[cursor..start] {
            output.extend_from_slice(line);
        }
        cursor = start;
        let mut consumed = 0usize;
        for line in &hunk.lines {
            match line {
                ParsedLine::Addition(bytes) => output.extend_from_slice(bytes),
                ParsedLine::Context(bytes) | ParsedLine::Deletion(bytes) => {
                    let Some(actual) = source.get(cursor) else {
                        return Err(patch_conflict(
                            path,
                            format!("hunk {} extends past end of file", number + 1),
                        ));
                    };
                    if *actual != bytes.as_slice() {
                        return Err(patch_conflict(
                            path,
                            format!("hunk {} does not match line {}", number + 1, cursor + 1),
                        ));
                    }
                    if matches!(line, ParsedLine::Context(_)) {
                        output.extend_from_slice(actual);
                    }
                    cursor += 1;
                    consumed += 1;
                }
            }
        }
        if consumed != hunk.old_lines {
            return Err(patch_conflict(
                path,
                format!(
                    "hunk {} header declares {} old lines but contains {consumed}",
                    number + 1,
                    hunk.old_lines
                ),
            ));
        }
    }
    for line in &source[cursor..] {
        output.extend_from_slice(line);
    }
    Ok(output)
}

fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            lines.push(&bytes[start..=index]);
            start = index + 1;
        }
    }
    if start < bytes.len() {
        lines.push(&bytes[start..]);
    }
    lines
}

fn write_atomically(file: &PreparedFile) -> Result<(), ToolError> {
    let fresh = file
        .path
        .revalidate()
        .map_err(|error| forbidden_patch_path(&file.relative, error))?;
    let parent = fresh
        .as_path()
        .parent()
        .ok_or_else(|| patch_conflict(&file.relative, "the target has no parent directory"))?;
    if !parent.is_dir() {
        return Err(patch_conflict(
            &file.relative,
            "the target parent directory does not exist",
        ));
    }
    let mut temporary = NamedTempFile::new_in(parent).map_err(|error| {
        io_failure(
            &file.relative,
            "could not create an atomic temporary file",
            error,
        )
    })?;
    temporary
        .write_all(&file.resulting)
        .map_err(|error| io_failure(&file.relative, "could not write", error))?;
    if let Some(permissions) = &file.permissions {
        temporary
            .as_file_mut()
            .set_permissions(permissions.clone())
            .map_err(|error| {
                io_failure(&file.relative, "could not preserve permissions for", error)
            })?;
    } else {
        set_new_file_permissions(temporary.as_file_mut(), &file.relative)?;
    }
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|error| io_failure(&file.relative, "could not sync", error))?;
    temporary
        .persist(fresh.as_path())
        .map_err(|error| io_failure(&file.relative, "could not replace", error.error))?;
    sync_directory(parent, &file.relative)
}

#[cfg(unix)]
fn set_new_file_permissions(file: &mut File, path: &Path) -> Result<(), ToolError> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(Permissions::from_mode(0o644))
        .map_err(|error| io_failure(path, "could not set permissions for", error))
}

#[cfg(not(unix))]
fn set_new_file_permissions(_file: &mut File, _path: &Path) -> Result<(), ToolError> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(directory: &Path, relative: &Path) -> Result<(), ToolError> {
    File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|error| io_failure(relative, "could not sync the parent of", error))
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path, _relative: &Path) -> Result<(), ToolError> {
    Ok(())
}

fn resulting_diff(repository: &Repository, paths: &[PathBuf]) -> Result<Vec<u8>, ToolError> {
    let mut options = DiffOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .show_untracked_content(true)
        .disable_pathspec_match(true);
    for path in paths {
        options.pathspec(path);
    }
    let diff = repository
        .diff_index_to_workdir(None, Some(&mut options))
        .map_err(|error| ToolError::execution_failed(format!("resulting diff failed: {error}")))?;
    let mut bytes = Vec::new();
    diff.print(DiffFormat::Patch, |_delta, _hunk, line| {
        if matches!(line.origin(), ' ' | '+' | '-') {
            bytes.push(line.origin() as u8);
        }
        bytes.extend_from_slice(line.content());
        true
    })
    .map_err(|error| ToolError::execution_failed(format!("resulting diff failed: {error}")))?;
    Ok(bytes)
}

fn validate_hash(
    path: &Path,
    expected: Option<&str>,
    actual: Option<&[u8]>,
) -> Result<(), ToolError> {
    let expected = match expected {
        Some(hash)
            if hash.len() == 64
                && hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) =>
        {
            Some(hash)
        }
        Some(_) => {
            return Err(patch_conflict(
                path,
                "base_sha256 must be 64 lowercase hexadecimal characters",
            ));
        }
        None => None,
    };
    let actual_hash = actual.map(sha256);
    if expected == actual_hash.as_deref() {
        return Ok(());
    }
    Err(ToolError::StalePatch {
        path: path.to_path_buf(),
        expected: expected.unwrap_or("new file").to_owned(),
        actual: actual_hash.unwrap_or_else(|| "missing".to_owned()),
    })
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn ensure_relative_normal_path(path: &Path) -> Result<(), ToolError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(ToolError::ForbiddenPath {
            path: path.to_path_buf(),
            reason: "patch paths must be non-empty, relative, and contain no . or .. components"
                .to_owned(),
        });
    }
    Ok(())
}

fn signed_delta(resulting: usize, original: usize) -> i64 {
    let resulting = i128::try_from(resulting).unwrap_or(i128::MAX);
    let original = i128::try_from(original).unwrap_or(i128::MAX);
    i64::try_from(resulting - original).unwrap_or({
        if resulting >= original {
            i64::MAX
        } else {
            i64::MIN
        }
    })
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn patch_conflict(path: impl AsRef<Path>, reason: impl ToString) -> ToolError {
    ToolError::PatchConflict {
        path: path.as_ref().to_path_buf(),
        reason: reason.to_string(),
    }
}

fn forbidden_patch_path(path: &Path, error: impl ToString) -> ToolError {
    ToolError::ForbiddenPath {
        path: path.to_path_buf(),
        reason: error.to_string(),
    }
}

fn io_failure(path: &Path, operation: &str, error: std::io::Error) -> ToolError {
    ToolError::execution_failed(format!("{operation} {}: {error}", path.display()))
}
