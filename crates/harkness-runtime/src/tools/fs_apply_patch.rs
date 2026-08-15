//! Stale-safe unified-diff application within one workspace.

use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::ffi::{CString, OsStr};
use std::fs::{self, File, Permissions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};

use harkness_git::{
    PatchFileMode, UnifiedPatchLine, parse_unified_patch, resulting_worktree_patch,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
#[cfg(windows)]
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_RENAME_INFORMATION, FileRenameInformation, NtSetInformationFile,
};
#[cfg(windows)]
use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, INVALID_HANDLE_VALUE, RtlNtStatusToDosError,
};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, DELETE, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetFileInformationByHandle, OPEN_EXISTING, ReOpenFile,
};
#[cfg(windows)]
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

use crate::tool::{
    ArtifactRef, Capability, ExecutionContext, RequestEffects, RiskLevel, Tool, ToolError,
    ToolIdentity, ToolMetadata,
};
use crate::trust::{ContainedPath, PathAccess, PathBoundary};

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

    fn request_effects(
        &self,
        input: &Self::Input,
        boundary: &PathBoundary,
    ) -> Result<RequestEffects, ToolError> {
        input
            .bases
            .iter()
            .try_fold(RequestEffects::default(), |effects, base| {
                Ok(effects.with_path(boundary.contain(&base.path)?, PathAccess::Write))
            })
    }

    fn execute(
        &self,
        input: Self::Input,
        context: &mut ExecutionContext,
    ) -> Result<Self::Output, ToolError> {
        execute_patch(input, context, |_| {}, |_| {})
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
    mut before_replace: impl FnMut(&Path),
) -> Result<ApplyPatchOutput, ToolError> {
    context.check_still_permitted()?;
    let parsed = parse_patch(input.patch.as_bytes())?;
    let bases = resolve_bases(input.bases, context)?;
    let mut prepared = validate_all(parsed, &bases, context)?;
    prepared.sort_by(|left, right| left.relative.cmp(&right.relative));
    // The diff is a mandatory part of a successful result. Discover storage
    // failures while the call is still side-effect free rather than after the
    // workspace has already been changed.
    let mut diff_artifact = context.open_artifact("applied.patch", "text/x-diff")?;

    // Once the first rename commits, finishing this already-validated bounded
    // batch is the only outcome whose terminal record describes the workspace.
    context.check_still_permitted()?;
    context.begin_irreversible()?;
    for file in &prepared {
        write_atomically(file, context.workspace_root(), || {
            before_replace(&file.relative);
        })?;
        after_write(&file.relative);
    }

    let touched = prepared
        .iter()
        .map(|file| file.relative.clone())
        .collect::<Vec<_>>();
    let diff = resulting_worktree_patch(context.workspace_root(), &touched)
        .map_err(|error| ToolError::execution_failed(format!("resulting diff failed: {error}")))?;
    diff_artifact.write_all(&diff).map_err(|error| {
        ToolError::execution_failed(format!(
            "resulting diff artifact could not be written: {error}"
        ))
    })?;
    let diff_artifact = diff_artifact.finish()?;
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
    execute_patch(input, context, after_write, |_| {})
}

#[cfg(test)]
pub(super) fn execute_with_before_replace(
    input: ApplyPatchInput,
    context: &mut ExecutionContext,
    before_replace: impl FnMut(&Path),
) -> Result<ApplyPatchOutput, ToolError> {
    execute_patch(input, context, |_| {}, before_replace)
}

fn parse_patch(bytes: &[u8]) -> Result<Vec<ParsedFile>, ToolError> {
    // Straight to `harkness-git`'s parser, with no tolerance layer in front.
    // Synthesizing a `diff --git` envelope from `---`/`+++` lines is patch
    // *parsing*, which belongs to the crate that owns production Git behavior —
    // and doing it here would also change what the already-released
    // `fs.apply_patch@1.0.0` applies, turning documents it refused as
    // `patch_conflict` into workspace mutations under a version an approval can
    // already name. A caller that wants the header emits the header.
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
    let mut targets = BTreeMap::<PlatformPathKey, PathBuf>::new();
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
        ensure_resolved_patch_target(context.workspace_root(), path.as_path(), &relative)?;
        let target_key = platform_path_key(path.as_path());
        if let Some(previous) = targets.insert(target_key, relative.clone()) {
            return Err(patch_conflict(
                &relative,
                format!(
                    "this target is platform-equivalent to the already-declared path {}",
                    previous.display()
                ),
            ));
        }
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
        #[cfg(not(unix))]
        if file.mode == Some(PatchFileMode::Executable) {
            return Err(patch_conflict(
                &file.relative,
                "executable file modes are not supported on this platform",
            ));
        }
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

#[cfg(not(unix))]
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

#[cfg(not(windows))]
fn prepare_temporary(file: &PreparedFile, parent: &Path) -> Result<NamedTempFile, ToolError> {
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
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|error| io_failure(&file.relative, "could not sync", error))?;
    Ok(temporary)
}

#[cfg(windows)]
fn prepare_temporary(file: &PreparedFile, parent: &Path) -> Result<NamedTempFile, ToolError> {
    let mut temporary = tempfile::Builder::new()
        .prefix(".harkness-patch-")
        .make_in(parent, |path| {
            fs::OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .custom_flags(FILE_ATTRIBUTE_NORMAL)
                .open(path)
        })
        .map_err(|error| {
            io_failure(
                &file.relative,
                "could not create an atomic temporary file",
                error,
            )
        })?;
    temporary
        .write_all(&file.resulting)
        .map_err(|error| io_failure(&file.relative, "could not write", error))?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|error| io_failure(&file.relative, "could not sync", error))?;
    Ok(temporary)
}

fn validate_current(file: &PreparedFile, current: Option<&[u8]>) -> Result<(), ToolError> {
    if current == file.original.as_deref() {
        return Ok(());
    }
    Err(ToolError::StalePatch {
        path: file.relative.clone(),
        expected: file
            .original
            .as_deref()
            .map(sha256)
            .unwrap_or_else(|| "new file".to_owned()),
        actual: current.map(sha256).unwrap_or_else(|| "missing".to_owned()),
    })
}

#[cfg(unix)]
fn write_atomically(
    file: &PreparedFile,
    workspace_root: &Path,
    before_replace: impl FnOnce(),
) -> Result<(), ToolError> {
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
    // The potentially long write and first sync happen before the final base
    // proof. The commit window below then consists only of bounded metadata,
    // permission, sync, and descriptor-relative rename operations.
    let mut temporary = prepare_temporary(file, parent)?;
    let anchored = AnchoredParent::open(workspace_root, &file.relative, parent)?;

    before_replace();
    revalidate_anchored_target(file, workspace_root, &anchored, parent)?;
    let (current, permissions) = anchored.read_target(&file.relative)?;
    validate_current(file, current.as_deref())?;
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
    anchored.verify_temporary(&temporary, &file.relative)?;

    // Re-resolve and re-read after the temp file is completely ready. This is
    // the last userspace check possible before renameat; the retained directory
    // descriptor makes any later ancestor swap harmless to containment.
    revalidate_anchored_target(file, workspace_root, &anchored, parent)?;
    let (current, _) = anchored.read_target(&file.relative)?;
    validate_current(file, current.as_deref())?;
    anchored.replace(&temporary, &file.relative)?;
    // The path no longer names our file. Disable NamedTempFile's path cleanup
    // so Drop cannot unlink an attacker-created replacement at the old name.
    temporary.disable_cleanup(true);
    anchored.sync(&file.relative)
}

#[cfg(not(any(unix, windows)))]
fn write_atomically(
    file: &PreparedFile,
    workspace_root: &Path,
    before_replace: impl FnOnce(),
) -> Result<(), ToolError> {
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
    let mut temporary = prepare_temporary(file, parent)?;

    before_replace();
    let fresh = revalidate_target(file, workspace_root)?;
    let (current, permissions) = read_current_target(fresh.as_path(), &file.relative)?;
    validate_current(file, current.as_deref())?;
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

    let fresh = revalidate_target(file, workspace_root)?;
    let (current, _) = read_current_target(fresh.as_path(), &file.relative)?;
    validate_current(file, current.as_deref())?;
    temporary
        .persist(fresh.as_path())
        .map_err(|error| io_failure(&file.relative, "could not replace", error.error))?;
    sync_directory(parent, &file.relative)
}

#[cfg(windows)]
fn write_atomically(
    file: &PreparedFile,
    workspace_root: &Path,
    before_replace: impl FnOnce(),
) -> Result<(), ToolError> {
    ensure_safe_patch_target(workspace_root, &file.relative)?;
    let fresh = file
        .path
        .revalidate()
        .map_err(|error| forbidden_patch_path(&file.relative, error))?;
    ensure_resolved_patch_target(workspace_root, fresh.as_path(), &file.relative)?;
    if fresh.as_path() != file.path.as_path() {
        return Err(forbidden_patch_path(
            &file.relative,
            "the target resolved to a different path after validation",
        ));
    }
    let parent = fresh
        .as_path()
        .parent()
        .ok_or_else(|| patch_conflict(&file.relative, "the target has no parent directory"))?;
    let mut temporary = prepare_temporary(file, parent)?;
    let anchored = WindowsAnchoredParent::open(parent, &file.relative)?;

    before_replace();
    revalidate_windows_target(file, workspace_root, &anchored, parent)?;
    let (current, permissions) = read_current_target(fresh.as_path(), &file.relative)?;
    validate_current(file, current.as_deref())?;
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

    revalidate_windows_target(file, workspace_root, &anchored, parent)?;
    let (current, _) = read_current_target(fresh.as_path(), &file.relative)?;
    validate_current(file, current.as_deref())?;
    anchored.replace(temporary.as_file(), &file.relative, &file.relative)?;
    temporary.disable_cleanup(true);
    anchored.sync(&file.relative)
}

#[cfg(not(any(unix, windows)))]
fn revalidate_target(
    file: &PreparedFile,
    workspace_root: &Path,
) -> Result<ContainedPath, ToolError> {
    ensure_safe_patch_target(workspace_root, &file.relative)?;
    let fresh = file
        .path
        .revalidate()
        .map_err(|error| forbidden_patch_path(&file.relative, error))?;
    ensure_resolved_patch_target(workspace_root, fresh.as_path(), &file.relative)?;
    if fresh.as_path() != file.path.as_path() {
        return Err(forbidden_patch_path(
            &file.relative,
            "the target resolved to a different path after validation",
        ));
    }
    Ok(fresh)
}

#[cfg(unix)]
struct AnchoredParent {
    directory: File,
    target_name: CString,
}

#[cfg(unix)]
impl AnchoredParent {
    fn open(
        workspace_root: &Path,
        relative: &Path,
        expected_parent: &Path,
    ) -> Result<Self, ToolError> {
        let mut directory = open_directory(workspace_root, relative)?;
        if let Some(parent) = relative.parent() {
            for component in parent.components() {
                let std::path::Component::Normal(name) = component else {
                    return Err(forbidden_patch_path(
                        relative,
                        "the patch parent is not a normal relative path",
                    ));
                };
                directory = open_directory_at(&directory, name, relative)?;
            }
        }
        if !same_file_metadata(
            &directory
                .metadata()
                .map_err(|error| io_failure(relative, "could not inspect", error))?,
            &fs::metadata(expected_parent)
                .map_err(|error| io_failure(relative, "could not inspect", error))?,
        ) {
            return Err(forbidden_patch_path(
                relative,
                "the target parent changed while the patch was being prepared",
            ));
        }
        let target_name = path_component(
            relative
                .file_name()
                .ok_or_else(|| patch_conflict(relative, "the target has no file name"))?,
            relative,
        )?;
        Ok(Self {
            directory,
            target_name,
        })
    }

    fn matches_path(&self, parent: &Path, relative: &Path) -> Result<bool, ToolError> {
        let anchored = self
            .directory
            .metadata()
            .map_err(|error| io_failure(relative, "could not inspect", error))?;
        let current = fs::metadata(parent)
            .map_err(|error| io_failure(relative, "could not inspect", error))?;
        Ok(same_file_metadata(&anchored, &current))
    }

    fn read_target(
        &self,
        relative: &Path,
    ) -> Result<(Option<Vec<u8>>, Option<Permissions>), ToolError> {
        let descriptor = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                self.target_name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )
        };
        if descriptor < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok((None, None));
            }
            if error.raw_os_error() == Some(libc::ELOOP) {
                return Err(forbidden_patch_path(
                    relative,
                    "the patch target became a symbolic link",
                ));
            }
            return Err(io_failure(relative, "could not open", error));
        }
        let mut file = unsafe { File::from_raw_fd(descriptor) };
        let metadata = file
            .metadata()
            .map_err(|error| io_failure(relative, "could not inspect", error))?;
        if !metadata.file_type().is_file() {
            return Err(patch_conflict(relative, "the target is not a regular file"));
        }
        let permissions = metadata.permissions();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| io_failure(relative, "could not read", error))?;
        Ok((Some(bytes), Some(permissions)))
    }

    fn verify_temporary(
        &self,
        temporary: &NamedTempFile,
        relative: &Path,
    ) -> Result<(), ToolError> {
        let name = path_component(
            temporary
                .path()
                .file_name()
                .ok_or_else(|| patch_conflict(relative, "the temporary file has no file name"))?,
            relative,
        )?;
        let descriptor = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )
        };
        if descriptor < 0 {
            return Err(forbidden_patch_path(
                relative,
                format!(
                    "the temporary file left the validated parent: {}",
                    std::io::Error::last_os_error()
                ),
            ));
        }
        let named = unsafe { File::from_raw_fd(descriptor) };
        let named_metadata = named
            .metadata()
            .map_err(|error| io_failure(relative, "could not inspect", error))?;
        let held_metadata = temporary
            .as_file()
            .metadata()
            .map_err(|error| io_failure(relative, "could not inspect", error))?;
        if !same_file_metadata(&named_metadata, &held_metadata) {
            return Err(forbidden_patch_path(
                relative,
                "the temporary file name was replaced during patch preparation",
            ));
        }
        Ok(())
    }

    fn replace(&self, temporary: &NamedTempFile, relative: &Path) -> Result<(), ToolError> {
        let temporary_name = path_component(
            temporary
                .path()
                .file_name()
                .ok_or_else(|| patch_conflict(relative, "the temporary file has no file name"))?,
            relative,
        )?;
        let replaced = unsafe {
            libc::renameat(
                self.directory.as_raw_fd(),
                temporary_name.as_ptr(),
                self.directory.as_raw_fd(),
                self.target_name.as_ptr(),
            )
        };
        if replaced != 0 {
            return Err(io_failure(
                relative,
                "could not replace",
                std::io::Error::last_os_error(),
            ));
        }
        Ok(())
    }

    fn sync(&self, relative: &Path) -> Result<(), ToolError> {
        self.directory
            .sync_all()
            .map_err(|error| io_failure(relative, "could not sync the parent of", error))
    }
}

#[cfg(unix)]
fn revalidate_anchored_target(
    file: &PreparedFile,
    workspace_root: &Path,
    anchored: &AnchoredParent,
    parent: &Path,
) -> Result<(), ToolError> {
    ensure_safe_patch_target(workspace_root, &file.relative)?;
    let fresh = file
        .path
        .revalidate()
        .map_err(|error| forbidden_patch_path(&file.relative, error))?;
    ensure_resolved_patch_target(workspace_root, fresh.as_path(), &file.relative)?;
    if fresh.as_path() != file.path.as_path() || !anchored.matches_path(parent, &file.relative)? {
        return Err(forbidden_patch_path(
            &file.relative,
            "the target parent changed while the patch was being prepared",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn open_directory(path: &Path, relative: &Path) -> Result<File, ToolError> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| patch_conflict(relative, "the target path contains a NUL byte"))?;
    let descriptor = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if descriptor < 0 {
        return Err(forbidden_patch_path(
            relative,
            format!(
                "the workspace root could not be opened without following links: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

#[cfg(unix)]
fn open_directory_at(parent: &File, name: &OsStr, relative: &Path) -> Result<File, ToolError> {
    let name = path_component(name, relative)?;
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if descriptor < 0 {
        return Err(forbidden_patch_path(
            relative,
            format!(
                "the target parent could not be opened without following links: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

#[cfg(unix)]
fn path_component(name: &OsStr, relative: &Path) -> Result<CString, ToolError> {
    CString::new(name.as_bytes())
        .map_err(|_| patch_conflict(relative, "the target path contains a NUL byte"))
}

#[cfg(unix)]
fn same_file_metadata(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
struct WindowsAnchoredParent {
    directory: File,
    identity: WindowsFileIdentity,
}

#[cfg(windows)]
#[derive(Clone, Copy, Eq, PartialEq)]
struct WindowsFileIdentity {
    volume: u32,
    index: u64,
}

#[cfg(windows)]
impl WindowsAnchoredParent {
    fn open(parent: &Path, relative: &Path) -> Result<Self, ToolError> {
        let directory = open_windows_directory(parent, relative)?;
        let information = windows_file_information(&directory, relative)?;
        if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(forbidden_patch_path(
                relative,
                "the target parent is a Windows reparse point",
            ));
        }
        Ok(Self {
            identity: windows_identity(&information),
            directory,
        })
    }

    fn matches_path(&self, parent: &Path, relative: &Path) -> Result<bool, ToolError> {
        let current = open_windows_directory(parent, relative)?;
        let information = windows_file_information(&current, relative)?;
        Ok(
            information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
                && windows_identity(&information) == self.identity,
        )
    }

    fn replace(&self, temporary: &File, target: &Path, relative: &Path) -> Result<(), ToolError> {
        let target = target
            .file_name()
            .ok_or_else(|| patch_conflict(relative, "the target has no file name"))?
            .encode_wide()
            .collect::<Vec<_>>();
        let target_bytes = target
            .len()
            .checked_mul(std::mem::size_of::<u16>())
            .ok_or_else(|| patch_conflict(relative, "the target path is too long"))?;
        // The native rename contract requires the complete fixed structure
        // plus the variable-width name, even though the structure declares its
        // first `WCHAR` inline.
        let structure_bytes = std::mem::size_of::<FILE_RENAME_INFORMATION>()
            .checked_add(target_bytes)
            .ok_or_else(|| patch_conflict(relative, "the target path is too long"))?;
        let words = structure_bytes.div_ceil(std::mem::size_of::<u64>());
        let mut storage = vec![0u64; words];
        let rename = storage.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
        unsafe {
            (*rename).Anonymous.ReplaceIfExists = true;
            (*rename).RootDirectory = self.directory.as_raw_handle() as HANDLE;
            (*rename).FileNameLength = u32::try_from(target_bytes)
                .map_err(|_| patch_conflict(relative, "the target path is too long"))?;
            std::ptr::copy_nonoverlapping(
                target.as_ptr(),
                (*rename).FileName.as_mut_ptr(),
                target.len(),
            );
        }

        let rename_handle = unsafe {
            ReOpenFile(
                temporary.as_raw_handle() as HANDLE,
                FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                0,
            )
        };
        if rename_handle == INVALID_HANDLE_VALUE {
            return Err(io_failure(
                relative,
                "could not prepare the atomic replacement",
                std::io::Error::last_os_error(),
            ));
        }
        let mut io_status = IO_STATUS_BLOCK::default();
        let status = unsafe {
            NtSetInformationFile(
                rename_handle,
                &mut io_status,
                rename.cast_const().cast(),
                u32::try_from(structure_bytes).unwrap_or(u32::MAX),
                FileRenameInformation,
            )
        };
        unsafe {
            CloseHandle(rename_handle);
        }
        if status < 0 {
            let error = unsafe { RtlNtStatusToDosError(status) };
            return Err(io_failure(
                relative,
                "could not replace",
                std::io::Error::from_raw_os_error(i32::try_from(error).unwrap_or(i32::MAX)),
            ));
        }
        Ok(())
    }

    fn sync(&self, _relative: &Path) -> Result<(), ToolError> {
        // Windows does not offer Unix's portable directory-fsync contract, and
        // FlushFileBuffers commonly rejects directory handles opened only for
        // traversal. The replacement file itself was flushed before rename;
        // retain the established non-Unix best-effort directory semantics.
        Ok(())
    }
}

#[cfg(windows)]
fn revalidate_windows_target(
    file: &PreparedFile,
    workspace_root: &Path,
    anchored: &WindowsAnchoredParent,
    parent: &Path,
) -> Result<(), ToolError> {
    ensure_safe_patch_target(workspace_root, &file.relative)?;
    let fresh = file
        .path
        .revalidate()
        .map_err(|error| forbidden_patch_path(&file.relative, error))?;
    ensure_resolved_patch_target(workspace_root, fresh.as_path(), &file.relative)?;
    if fresh.as_path() != file.path.as_path() || !anchored.matches_path(parent, &file.relative)? {
        return Err(forbidden_patch_path(
            &file.relative,
            "the target parent changed while the patch was being prepared",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn open_windows_directory(path: &Path, relative: &Path) -> Result<File, ToolError> {
    let path = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            FILE_GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(forbidden_patch_path(
            relative,
            format!(
                "the target parent could not be opened without following reparse points: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }
    Ok(unsafe { File::from_raw_handle(handle as _) })
}

#[cfg(windows)]
fn windows_file_information(
    file: &File,
    relative: &Path,
) -> Result<BY_HANDLE_FILE_INFORMATION, ToolError> {
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut information) } == 0
    {
        return Err(io_failure(
            relative,
            "could not inspect",
            std::io::Error::last_os_error(),
        ));
    }
    Ok(information)
}

#[cfg(windows)]
fn windows_identity(information: &BY_HANDLE_FILE_INFORMATION) -> WindowsFileIdentity {
    WindowsFileIdentity {
        volume: information.dwVolumeSerialNumber,
        index: (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    }
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

#[cfg(not(any(unix, windows)))]
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
    if relative
        .components()
        .any(|component| is_git_administration_component(component.as_os_str()))
    {
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

fn ensure_resolved_patch_target(
    workspace_root: &Path,
    resolved: &Path,
    relative: &Path,
) -> Result<(), ToolError> {
    let resolved_relative = resolved.strip_prefix(workspace_root).map_err(|_| {
        forbidden_patch_path(
            relative,
            "the resolved patch target is not inside the workspace root",
        )
    })?;
    if resolved_relative
        .components()
        .any(|component| is_git_administration_component(component.as_os_str()))
    {
        return Err(forbidden_patch_path(
            relative,
            "the patch target resolves into Git administration data",
        ));
    }
    Ok(())
}

fn is_git_administration_component(component: &std::ffi::OsStr) -> bool {
    let component = component.to_string_lossy();
    #[cfg(windows)]
    let component = component
        .split(':')
        .next()
        .unwrap_or_default()
        .trim_end_matches([' ', '.']);
    component.eq_ignore_ascii_case(".git")
}

#[cfg(any(windows, target_os = "macos"))]
type PlatformPathKey = String;

#[cfg(any(windows, target_os = "macos"))]
fn platform_path_key(path: &Path) -> PlatformPathKey {
    path.components()
        .map(|component| {
            let component = component.as_os_str().to_string_lossy();
            #[cfg(windows)]
            let component = component.trim_end_matches([' ', '.']);
            component.to_lowercase()
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(not(any(windows, target_os = "macos")))]
type PlatformPathKey = PathBuf;

#[cfg(not(any(windows, target_os = "macos")))]
fn platform_path_key(path: &Path) -> PlatformPathKey {
    path.to_path_buf()
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
