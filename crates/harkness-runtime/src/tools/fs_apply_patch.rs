//! Stale-safe unified-diff application within one workspace.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, Permissions};
use std::io::Write;
use std::path::{Path, PathBuf};

use harkness_git::{
    PatchFileMode, UnifiedPatchLine, parse_unified_patch, resulting_worktree_patch,
};
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
        execute_patch(input, context, |_| {})
    }
}

struct ParsedFile {
    relative: PathBuf,
    hunks: Vec<ParsedHunk>,
    created: bool,
    mode: Option<PatchFileMode>,
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
}

struct PreparedFile {
    relative: PathBuf,
    path: ContainedPath,
    original_len: usize,
    resulting: Vec<u8>,
    hunks: usize,
    created: bool,
    original: Option<Vec<u8>>,
    mode: Option<PatchFileMode>,
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

fn execute_patch(
    input: ApplyPatchInput,
    context: &mut ExecutionContext,
    mut after_write: impl FnMut(&Path),
) -> Result<ApplyPatchOutput, ToolError> {
    context.check_still_permitted()?;
    let parsed = parse_patch(input.patch.as_bytes())?;
    let bases = resolve_bases(input.bases, context)?;
    let mut prepared = validate_all(parsed, &bases, context)?;
    prepared.sort_by(|left, right| left.relative.cmp(&right.relative));

    // Once the first rename commits, finishing this already-validated bounded
    // batch is the only outcome whose terminal record describes the workspace.
    context.check_still_permitted()?;
    for file in &prepared {
        write_atomically(file, context.workspace_root())?;
        after_write(&file.relative);
    }

    let touched = prepared
        .iter()
        .map(|file| file.relative.clone())
        .collect::<Vec<_>>();
    let diff = resulting_worktree_patch(context.workspace_root(), &touched)
        .map_err(|error| ToolError::execution_failed(format!("resulting diff failed: {error}")))?;
    let diff_artifact = context.write_artifact("applied.patch", "text/x-diff", &diff)?;
    Ok(ApplyPatchOutput {
        files: prepared.iter().map(PreparedFile::summary).collect(),
        diff_artifact,
    })
}

#[cfg(test)]
pub(super) fn execute_with_after_write(
    input: ApplyPatchInput,
    context: &mut ExecutionContext,
    after_write: impl FnMut(&Path),
) -> Result<ApplyPatchOutput, ToolError> {
    execute_patch(input, context, after_write)
}

fn parse_patch(bytes: &[u8]) -> Result<Vec<ParsedFile>, ToolError> {
    parse_unified_patch(bytes)
        .map_err(|error| patch_conflict(error.path(), error.detail()))
        .map(|patch| {
            patch
                .into_files()
                .into_iter()
                .map(|file| ParsedFile {
                    relative: file.path,
                    hunks: file
                        .hunks
                        .into_iter()
                        .map(|hunk| ParsedHunk {
                            old_start: hunk.old_start,
                            old_lines: hunk.old_lines,
                            lines: hunk
                                .lines
                                .into_iter()
                                .map(|line| match line {
                                    UnifiedPatchLine::Context(bytes) => ParsedLine::Context(bytes),
                                    UnifiedPatchLine::Addition(bytes) => {
                                        ParsedLine::Addition(bytes)
                                    }
                                    UnifiedPatchLine::Deletion(bytes) => {
                                        ParsedLine::Deletion(bytes)
                                    }
                                })
                                .collect(),
                        })
                        .collect(),
                    created: file.created,
                    mode: file.mode,
                })
                .collect()
        })
}

fn resolve_bases(
    bases: Vec<FileBase>,
    context: &ExecutionContext,
) -> Result<BTreeMap<PathBuf, ResolvedBase>, ToolError> {
    let mut resolved = BTreeMap::new();
    for base in bases {
        let relative = PathBuf::from(&base.path);
        ensure_relative_normal_path(&relative)?;
        ensure_safe_patch_target(context.workspace_root(), &relative)?;
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
            original: base.original.clone(),
            mode: file.mode,
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

fn read_current_target(
    path: &Path,
    relative: &Path,
) -> Result<(Option<Vec<u8>>, Option<Permissions>), ToolError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(forbidden_patch_path(
                relative,
                "the patch target became a symbolic link",
            ));
        }
        Ok(_) => return Err(patch_conflict(relative, "the target is not a regular file")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((None, None)),
        Err(error) => return Err(io_failure(relative, "could not inspect", error)),
    };
    let bytes = fs::read(path).map_err(|error| io_failure(relative, "could not read", error))?;
    Ok((Some(bytes), Some(metadata.permissions())))
}

fn write_atomically(file: &PreparedFile, workspace_root: &Path) -> Result<(), ToolError> {
    ensure_safe_patch_target(workspace_root, &file.relative)?;
    let fresh = file
        .path
        .revalidate()
        .map_err(|error| forbidden_patch_path(&file.relative, error))?;
    if fresh.as_path() != file.path.as_path() {
        return Err(forbidden_patch_path(
            &file.relative,
            "the target resolved to a different path after validation",
        ));
    }
    let (current, permissions) = read_current_target(fresh.as_path(), &file.relative)?;
    if current.as_deref() != file.original.as_deref() {
        return Err(ToolError::StalePatch {
            path: file.relative.clone(),
            expected: file
                .original
                .as_deref()
                .map(sha256)
                .unwrap_or_else(|| "new file".to_owned()),
            actual: current
                .as_deref()
                .map(sha256)
                .unwrap_or_else(|| "missing".to_owned()),
        });
    }
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
    set_result_permissions(
        temporary.as_file_mut(),
        &file.relative,
        permissions,
        file.mode,
    )?;
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
fn set_result_permissions(
    file: &mut File,
    path: &Path,
    permissions: Option<Permissions>,
    requested: Option<PatchFileMode>,
) -> Result<(), ToolError> {
    use std::os::unix::fs::PermissionsExt;

    let mut mode = permissions.map_or(0o644, |permissions| permissions.mode());
    match requested {
        Some(PatchFileMode::Executable) => mode |= 0o111,
        Some(PatchFileMode::Regular) => mode &= !0o111,
        None => {}
    }
    file.set_permissions(Permissions::from_mode(mode))
        .map_err(|error| io_failure(path, "could not set permissions for", error))
}

#[cfg(not(unix))]
fn set_result_permissions(
    file: &mut File,
    path: &Path,
    permissions: Option<Permissions>,
    _requested: Option<PatchFileMode>,
) -> Result<(), ToolError> {
    if let Some(permissions) = permissions {
        file.set_permissions(permissions)
            .map_err(|error| io_failure(path, "could not preserve permissions for", error))?;
    }
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

fn ensure_safe_patch_target(workspace_root: &Path, relative: &Path) -> Result<(), ToolError> {
    ensure_relative_normal_path(relative)?;
    if relative.components().any(|component| {
        component
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(".git")
    }) {
        return Err(forbidden_patch_path(
            relative,
            "Git administration paths are not workspace files",
        ));
    }

    let mut reached = workspace_root.to_path_buf();
    for component in relative.components() {
        reached.push(component.as_os_str());
        match fs::symlink_metadata(&reached) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(forbidden_patch_path(
                    relative,
                    format!(
                        "patch targets may not traverse symbolic link {}",
                        reached.display()
                    ),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(io_failure(relative, "could not inspect", error)),
        }
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
