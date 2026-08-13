//! Unified-patch parsing and raw worktree evidence.
//!
//! Patch syntax and repository diff production are Git behavior, so they live
//! behind this crate rather than making every runtime tool a second libgit2
//! client. Applying the resulting byte edits remains the caller's filesystem
//! responsibility.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use git2::{Delta, Diff, DiffFormat, DiffLineType, DiffOptions, FileMode, Patch, Repository};
use thiserror::Error;

use crate::{GitError, inspection};

/// A parsed Git-style unified patch.
#[derive(Debug)]
pub struct UnifiedPatch {
    files: Vec<UnifiedPatchFile>,
}

impl UnifiedPatch {
    /// Consumes the patch into its file deltas in source order.
    #[must_use]
    pub fn into_files(self) -> Vec<UnifiedPatchFile> {
        self.files
    }
}

/// One file delta from a parsed unified patch.
#[derive(Debug)]
pub struct UnifiedPatchFile {
    /// Workspace-relative target path.
    pub path: PathBuf,
    /// Text hunks in patch order.
    pub hunks: Vec<UnifiedPatchHunk>,
    /// Whether the patch declares a new file.
    pub created: bool,
    /// Mode to establish, only when the patch explicitly creates or changes it.
    pub mode: Option<PatchFileMode>,
}

/// One textual hunk from a unified patch.
#[derive(Debug)]
pub struct UnifiedPatchHunk {
    /// One-based old-side starting line, or zero for a new empty side.
    pub old_start: usize,
    /// Number of old-side lines declared by the header.
    pub old_lines: usize,
    /// Hunk lines in source order.
    pub lines: Vec<UnifiedPatchLine>,
}

/// One line from a parsed patch hunk.
#[derive(Debug)]
pub enum UnifiedPatchLine {
    /// Bytes that must be present and are retained.
    Context(Vec<u8>),
    /// Bytes inserted into the result.
    Addition(Vec<u8>),
    /// Bytes that must be present and are removed.
    Deletion(Vec<u8>),
}

/// Git's two regular-file modes supported by the workspace patch tool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PatchFileMode {
    /// A non-executable regular file (`100644`).
    Regular,
    /// An executable regular file (`100755`).
    Executable,
}

/// A unified patch that cannot be represented by the safe workspace editor.
#[derive(Debug, Error)]
#[error("{}: {detail}", path.display())]
pub struct UnifiedPatchError {
    path: PathBuf,
    detail: String,
}

impl UnifiedPatchError {
    /// Path whose delta was refused, or `<patch>` for document-level errors.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Human-readable reason the patch was refused.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

/// Parses the safe textual subset accepted by `fs.apply_patch`.
///
/// File creation and in-place modification are supported. Deletes, copies,
/// renames, binary deltas, non-regular modes, and mismatched old/new paths are
/// rejected before a caller can begin applying anything.
pub fn parse_unified_patch(bytes: &[u8]) -> Result<UnifiedPatch, UnifiedPatchError> {
    if bytes.is_empty() {
        return Err(error("<patch>", "the patch is empty"));
    }
    let diff = Diff::from_buffer(bytes)
        .map_err(|source| error("<patch>", format!("invalid unified diff: {source}")))?;
    if diff.deltas().len() == 0 {
        return Err(error("<patch>", "the patch contains no file changes"));
    }

    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    for index in 0..diff.deltas().len() {
        let delta = diff
            .get_delta(index)
            .ok_or_else(|| error("<patch>", "a parsed file delta disappeared"))?;
        let created = match delta.status() {
            Delta::Added => true,
            Delta::Modified => false,
            status => {
                return Err(error(
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
            .ok_or_else(|| error("<patch>", "a target path is missing"))?
            .to_path_buf();
        ensure_relative_normal_path(&relative)?;
        if !created {
            let old = delta
                .old_file()
                .path()
                .ok_or_else(|| error(&relative, "the old target path is missing"))?;
            if old != relative {
                return Err(error(
                    &relative,
                    format!(
                        "the old path {} differs from the new path; renames are not supported",
                        old.display()
                    ),
                ));
            }
        }
        if !seen.insert(relative.clone()) {
            return Err(error(
                &relative,
                "the patch targets this path more than once",
            ));
        }
        if delta.old_file().is_binary() || delta.new_file().is_binary() {
            return Err(error(&relative, "binary patches are not supported"));
        }

        let mode = desired_mode(
            &relative,
            created,
            delta.old_file().mode(),
            delta.new_file().mode(),
        )?;
        let patch = Patch::from_diff(&diff, index)
            .map_err(|source| error(&relative, source.to_string()))?;
        let mut hunks = Vec::new();
        if let Some(patch) = patch {
            for hunk_index in 0..patch.num_hunks() {
                let (hunk, line_count) = patch
                    .hunk(hunk_index)
                    .map_err(|source| error(&relative, source.to_string()))?;
                let mut lines = Vec::with_capacity(line_count);
                for line_index in 0..line_count {
                    let line = patch
                        .line_in_hunk(hunk_index, line_index)
                        .map_err(|source| error(&relative, source.to_string()))?;
                    let content = line.content().to_vec();
                    match line.origin_value() {
                        DiffLineType::Context => lines.push(UnifiedPatchLine::Context(content)),
                        DiffLineType::Addition => lines.push(UnifiedPatchLine::Addition(content)),
                        DiffLineType::Deletion => lines.push(UnifiedPatchLine::Deletion(content)),
                        DiffLineType::ContextEOFNL
                        | DiffLineType::AddEOFNL
                        | DiffLineType::DeleteEOFNL => {
                            let Some(previous) = lines.last_mut() else {
                                return Err(error(
                                    &relative,
                                    "a no-newline marker appears before any hunk line",
                                ));
                            };
                            let bytes = match previous {
                                UnifiedPatchLine::Context(bytes)
                                | UnifiedPatchLine::Addition(bytes)
                                | UnifiedPatchLine::Deletion(bytes) => bytes,
                            };
                            if bytes.last() == Some(&b'\n') {
                                bytes.pop();
                            }
                        }
                        kind => {
                            return Err(error(
                                &relative,
                                format!("unsupported line type {kind:?}"),
                            ));
                        }
                    }
                }
                hunks.push(UnifiedPatchHunk {
                    old_start: hunk.old_start() as usize,
                    old_lines: hunk.old_lines() as usize,
                    lines,
                });
            }
        }
        if hunks.is_empty() && mode.is_none() {
            return Err(error(
                &relative,
                "the patch has no textual hunks or mode change",
            ));
        }
        files.push(UnifiedPatchFile {
            path: relative,
            hunks,
            created,
            mode,
        });
    }
    Ok(UnifiedPatch { files })
}

/// Produces the exact raw index-to-worktree patch for `paths`.
///
/// This is read-only Git inspection. It deliberately returns Git's patch bytes
/// rather than the structured display model because the artifact is evidence
/// of what a just-completed mutation actually left on disk.
pub fn resulting_worktree_patch(
    repository_root: impl AsRef<Path>,
    paths: &[PathBuf],
) -> Result<Vec<u8>, GitError> {
    let root = repository_root.as_ref();
    let repository = Repository::open(root).map_err(|source| inspection(root, source))?;
    let index = repository
        .index()
        .map_err(|source| inspection(root, source))?;
    let mut options = DiffOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .show_untracked_content(true)
        .disable_pathspec_match(true)
        .update_index(false);
    for path in paths {
        options.pathspec(path);
    }
    let diff = repository
        .diff_index_to_workdir(Some(&index), Some(&mut options))
        .map_err(|source| inspection(root, source))?;
    let mut bytes = Vec::new();
    diff.print(DiffFormat::Patch, |_delta, _hunk, line| {
        if matches!(line.origin(), ' ' | '+' | '-') {
            bytes.push(line.origin() as u8);
        }
        bytes.extend_from_slice(line.content());
        true
    })
    .map_err(|source| inspection(root, source))?;
    Ok(bytes)
}

fn desired_mode(
    path: &Path,
    created: bool,
    old: FileMode,
    new: FileMode,
) -> Result<Option<PatchFileMode>, UnifiedPatchError> {
    if !created && old == new {
        return Ok(None);
    }
    match new {
        FileMode::Blob | FileMode::BlobGroupWritable => Ok(Some(PatchFileMode::Regular)),
        FileMode::BlobExecutable => Ok(Some(PatchFileMode::Executable)),
        mode => Err(error(
            path,
            format!("unsupported {mode:?} file mode; only regular files are allowed"),
        )),
    }
}

fn ensure_relative_normal_path(path: &Path) -> Result<(), UnifiedPatchError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(error(
            path,
            "patch paths must be non-empty, relative, and contain no . or .. components",
        ));
    }
    Ok(())
}

fn error(path: impl AsRef<Path>, detail: impl Into<String>) -> UnifiedPatchError {
    UnifiedPatchError {
        path: path.as_ref().to_path_buf(),
        detail: detail.into(),
    }
}
