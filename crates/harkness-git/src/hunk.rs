//! Atomic, byte-preserving hunk staging and unstaging.
//!
//! A selection never supplies patch text. It names the two blobs and one hunk
//! from the structured diff contract; this module recomputes that diff while
//! holding the repository lock, finds the named hunk, and renders trusted bytes
//! from the fresh model. Libgit2 checks and applies the resulting patch to the
//! index only.
//!
//! Path-level staging in [`super::commit`] shells out to system Git; every
//! index write here goes through libgit2 instead, because only libgit2 offers
//! an apply whose sole target is the index. Three consequences are worth
//! knowing, and the first is the reason this module refuses work rather than
//! guessing:
//!
//! - External `filter=` drivers declared in `.gitattributes`, such as Git LFS
//!   and git-crypt, never run. Writing raw working-tree bytes where `git add`
//!   would have written a driver's output corrupts the index, so a selection
//!   naming a filtered path is refused. Libgit2 owns the `crlf`, `eol` and
//!   `ident` filters, so those keep working normally.
//! - Index extensions libgit2 does not understand, notably the untracked cache
//!   and fsmonitor state, are dropped when the index is rewritten. Git rebuilds
//!   them, so only the next status pays for it.
//! - Per-entry flags survive: `skip-worktree` and `assume-unchanged` are still
//!   set on every entry that had them, including the entry being rewritten.

use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

use git2::{ApplyLocation, ApplyOptions, AttrCheckFlags, AttrValue, Diff, Repository};

use crate::{
    Cancellation, DiffLine, DiffLineKind, DiffOptions, DiffTarget, FileChange, FileDiff, GitError,
    Hunk, RepositoryLock, StageOptions, StatusRefreshOutcome, commit, diff,
};

/// One selected hunk from a [`FileDiff`].
///
/// Blob IDs make the selection stale-safe. Both paths are retained because an
/// edited rename has meaningful content coordinates as well as two names.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct HunkSelection {
    /// The file's path on the old side, absent for an addition.
    pub old_path: Option<PathBuf>,
    /// The file's path on the new side, absent for a deletion.
    pub new_path: Option<PathBuf>,
    /// The old blob object ID the selection was taken against.
    pub old_blob_id: String,
    /// The new blob object ID the selection was taken against.
    pub new_blob_id: String,
    /// The context-line count the coordinates below are expressed in.
    pub context_lines: u32,
    /// First old-side line covered by the selected hunk.
    pub old_start: u32,
    /// Number of old-side lines covered by the selected hunk.
    pub old_lines: u32,
    /// First new-side line covered by the selected hunk.
    pub new_start: u32,
    /// Number of new-side lines covered by the selected hunk.
    pub new_lines: u32,
}

impl HunkSelection {
    /// Captures the stable identity and coordinates needed to select `hunk`.
    #[must_use]
    pub fn new(file: &FileDiff, hunk: &Hunk) -> Self {
        Self {
            old_path: file.old_path.clone(),
            new_path: file.new_path.clone(),
            old_blob_id: file.old_blob_id.clone(),
            new_blob_id: file.new_blob_id.clone(),
            context_lines: file.context_lines,
            old_start: hunk.old_start,
            old_lines: hunk.old_lines,
            new_start: hunk.new_start,
            new_lines: hunk.new_lines,
        }
    }

    /// Reconstructs a selection carried across a serialization boundary.
    ///
    /// The values are intentionally the same identity fields emitted by a
    /// [`FileDiff`] and its [`Hunk`]. They remain untrusted until staging
    /// recomputes the diff and validates every value under the repository lock.
    #[must_use]
    pub fn from_parts(
        old_path: Option<PathBuf>,
        new_path: Option<PathBuf>,
        old_blob_id: impl Into<String>,
        new_blob_id: impl Into<String>,
        context_lines: u32,
        old_range: (u32, u32),
        new_range: (u32, u32),
    ) -> Self {
        Self {
            old_path,
            new_path,
            old_blob_id: old_blob_id.into(),
            new_blob_id: new_blob_id.into(),
            context_lines,
            old_start: old_range.0,
            old_lines: old_range.1,
            new_start: new_range.0,
            new_lines: new_range.1,
        }
    }

    /// The path callers should normally show in an error or selection list.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.new_path.as_deref().or(self.old_path.as_deref())
    }
}

/// One selected changed line from a [`Hunk`] in a [`FileDiff`].
///
/// The enclosing hunk coordinates locate the line's neighbourhood after the
/// diff is recomputed under the repository lock. The optional old and new line
/// numbers then identify the changed line itself without relying on its array
/// index, which can move when the fresh diff is projected.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct LineSelection {
    /// The file's path on the old side, absent for an addition.
    pub old_path: Option<PathBuf>,
    /// The file's path on the new side, absent for a deletion.
    pub new_path: Option<PathBuf>,
    /// The old blob object ID the selection was taken against.
    pub old_blob_id: String,
    /// The new blob object ID the selection was taken against.
    pub new_blob_id: String,
    /// The context-line count the hunk coordinates are expressed in.
    pub context_lines: u32,
    /// First old-side line covered by the selected line's hunk.
    pub old_start: u32,
    /// Number of old-side lines covered by the selected line's hunk.
    pub old_lines: u32,
    /// First new-side line covered by the selected line's hunk.
    pub new_start: u32,
    /// Number of new-side lines covered by the selected line's hunk.
    pub new_lines: u32,
    /// Old-side number of the selected line, absent for an addition.
    pub old_line_number: Option<u32>,
    /// New-side number of the selected line, absent for a deletion.
    pub new_line_number: Option<u32>,
}

impl LineSelection {
    /// Captures the stable file, hunk, and line identity needed to select
    /// `line`.
    #[must_use]
    pub fn new(file: &FileDiff, hunk: &Hunk, line: &DiffLine) -> Self {
        Self {
            old_path: file.old_path.clone(),
            new_path: file.new_path.clone(),
            old_blob_id: file.old_blob_id.clone(),
            new_blob_id: file.new_blob_id.clone(),
            context_lines: file.context_lines,
            old_start: hunk.old_start,
            old_lines: hunk.old_lines,
            new_start: hunk.new_start,
            new_lines: hunk.new_lines,
            old_line_number: line.old_line_number,
            new_line_number: line.new_line_number,
        }
    }

    /// Reconstructs a line selection carried across a serialization boundary.
    ///
    /// Every value remains untrusted until line staging recomputes the diff and
    /// validates the file, hunk, and changed-line coordinates while holding the
    /// repository lock.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        old_path: Option<PathBuf>,
        new_path: Option<PathBuf>,
        old_blob_id: impl Into<String>,
        new_blob_id: impl Into<String>,
        context_lines: u32,
        old_range: (u32, u32),
        new_range: (u32, u32),
        old_line_number: Option<u32>,
        new_line_number: Option<u32>,
    ) -> Self {
        Self {
            old_path,
            new_path,
            old_blob_id: old_blob_id.into(),
            new_blob_id: new_blob_id.into(),
            context_lines,
            old_start: old_range.0,
            old_lines: old_range.1,
            new_start: new_range.0,
            new_lines: new_range.1,
            old_line_number,
            new_line_number,
        }
    }

    /// The path callers should normally show in an error or selection list.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.new_path.as_deref().or(self.old_path.as_deref())
    }
}

/// What a batch of hunk staging or unstaging produced.
///
/// A batch is atomic, so unlike [`super::StageOutcome`] there is no per-item
/// result to report: either every selection reached the index or none did. The
/// status refresh mirrors [`super::StageOutcome::status`] so both staging
/// granularities leave a caller's view of the repository in the same shape.
#[derive(Debug)]
#[non_exhaustive]
pub struct HunkStageOutcome {
    /// How many distinct hunks reached the index.
    ///
    /// This is not the number of selections supplied. Two selections can name
    /// the same hunk — most easily when they were taken at different context
    /// settings — and the batch deduplicates them, so reporting the input count
    /// would tell a caller that more changed than actually did.
    pub hunks: usize,
    /// The optional full-repository status refresh performed after the apply.
    pub status: StatusRefreshOutcome,
}

/// What one atomic batch of line staging or unstaging produced.
#[derive(Debug)]
#[non_exhaustive]
pub struct LineStageOutcome {
    /// How many distinct changed lines reached the index.
    pub lines: usize,
    /// How many distinct hunks contained those lines.
    pub hunks: usize,
    /// The optional full-repository status refresh performed after the apply.
    pub status: StatusRefreshOutcome,
}

#[derive(Clone, Copy)]
enum Direction {
    Forward,
    Reverse,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ApplyTarget {
    Index,
    Worktree,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MutationMode {
    Stage,
    Unstage,
    Discard,
}

impl MutationMode {
    fn diff_target(self) -> DiffTarget {
        match self {
            Self::Stage | Self::Discard => DiffTarget::Unstaged,
            Self::Unstage => DiffTarget::Staged,
        }
    }

    fn direction(self) -> Direction {
        match self {
            Self::Stage => Direction::Forward,
            Self::Unstage | Self::Discard => Direction::Reverse,
        }
    }

    fn apply_target(self) -> ApplyTarget {
        match self {
            Self::Stage | Self::Unstage => ApplyTarget::Index,
            Self::Discard => ApplyTarget::Worktree,
        }
    }
}

struct PreparedFile {
    file: FileDiff,
    hunks: Vec<PreparedHunk>,
}

struct PreparedHunk {
    hunk: Hunk,
    /// `None` selects the whole hunk. A non-empty vector selects exactly the
    /// named changed lines and is merged when several selections name the same
    /// freshly recomputed hunk.
    selected_lines: Option<Vec<LineCoordinates>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LineCoordinates {
    old: Option<u32>,
    new: Option<u32>,
}

impl PreparedFile {
    /// The path a refusal about this file should name.
    fn path(&self) -> PathBuf {
        display_path(self.file.new_path.as_deref(), self.file.old_path.as_deref()).to_path_buf()
    }
}

pub(crate) fn stage(
    git_executable: &Path,
    root: &Path,
    _lock: &RepositoryLock,
    selections: &[HunkSelection],
    options: &StageOptions,
    cancellation: &Cancellation,
) -> Result<HunkStageOutcome, GitError> {
    mutate(
        git_executable,
        root,
        selections,
        MutationMode::Stage,
        options,
        cancellation,
    )
}

pub(crate) fn unstage(
    git_executable: &Path,
    root: &Path,
    _lock: &RepositoryLock,
    selections: &[HunkSelection],
    options: &StageOptions,
    cancellation: &Cancellation,
) -> Result<HunkStageOutcome, GitError> {
    mutate(
        git_executable,
        root,
        selections,
        MutationMode::Unstage,
        options,
        cancellation,
    )
}

pub(crate) fn stage_lines(
    git_executable: &Path,
    root: &Path,
    _lock: &RepositoryLock,
    selections: &[LineSelection],
    options: &StageOptions,
    cancellation: &Cancellation,
) -> Result<LineStageOutcome, GitError> {
    mutate_lines(
        git_executable,
        root,
        selections,
        MutationMode::Stage,
        options,
        cancellation,
    )
}

pub(crate) fn unstage_lines(
    git_executable: &Path,
    root: &Path,
    _lock: &RepositoryLock,
    selections: &[LineSelection],
    options: &StageOptions,
    cancellation: &Cancellation,
) -> Result<LineStageOutcome, GitError> {
    mutate_lines(
        git_executable,
        root,
        selections,
        MutationMode::Unstage,
        options,
        cancellation,
    )
}

pub(crate) fn discard(
    git_executable: &Path,
    root: &Path,
    selections: &[HunkSelection],
    cancellation: &Cancellation,
) -> Result<HunkStageOutcome, GitError> {
    mutate(
        git_executable,
        root,
        selections,
        MutationMode::Discard,
        &StageOptions::default(),
        cancellation,
    )
}

pub(crate) fn discard_lines(
    git_executable: &Path,
    root: &Path,
    selections: &[LineSelection],
    cancellation: &Cancellation,
) -> Result<LineStageOutcome, GitError> {
    mutate_lines(
        git_executable,
        root,
        selections,
        MutationMode::Discard,
        &StageOptions::default(),
        cancellation,
    )
}

fn mutate(
    git_executable: &Path,
    root: &Path,
    selections: &[HunkSelection],
    mode: MutationMode,
    options: &StageOptions,
    cancellation: &Cancellation,
) -> Result<HunkStageOutcome, GitError> {
    let paths = selection_paths(selections);
    commit::validate_paths(root, &paths)?;
    let repository = commit::open(root)?;
    // Validated and opened first even for an empty batch, so an accidental
    // no-op reports the same refusals and the same refreshed status a real one
    // would. Cancellation was already honoured by the caller's lock.
    let mut hunks = 0;
    if !selections.is_empty() {
        refuse_filtered_paths(&repository, root, &paths)?;
        let target = mode.diff_target();
        let prepared = prepare(
            root,
            selections,
            &paths,
            &target,
            mode.apply_target(),
            cancellation,
        )?;
        hunks = prepared.iter().map(|file| file.hunks.len()).sum();
        apply(
            &repository,
            &prepared,
            &paths,
            mode.direction(),
            mode.apply_target(),
        )?;
    }
    Ok(HunkStageOutcome {
        hunks,
        status: commit::refresh_status(git_executable, root, options.refresh_status, cancellation),
    })
}

fn mutate_lines(
    git_executable: &Path,
    root: &Path,
    selections: &[LineSelection],
    mode: MutationMode,
    options: &StageOptions,
    cancellation: &Cancellation,
) -> Result<LineStageOutcome, GitError> {
    let paths = selection_paths(selections);
    commit::validate_paths(root, &paths)?;
    let repository = commit::open(root)?;
    let (mut lines, mut hunks) = (0, 0);
    if !selections.is_empty() {
        refuse_filtered_paths(&repository, root, &paths)?;
        let target = mode.diff_target();
        let prepared = prepare_lines(
            root,
            selections,
            &paths,
            &target,
            mode.apply_target(),
            cancellation,
        )?;
        lines = prepared
            .iter()
            .flat_map(|file| &file.hunks)
            .map(|hunk| hunk.selected_lines.as_ref().map_or(0, Vec::len))
            .sum();
        hunks = prepared.iter().map(|file| file.hunks.len()).sum();
        apply(
            &repository,
            &prepared,
            &paths,
            mode.direction(),
            mode.apply_target(),
        )?;
    }
    Ok(LineStageOutcome {
        lines,
        hunks,
        status: commit::refresh_status(git_executable, root, options.refresh_status, cancellation),
    })
}

/// What both selection granularities share: the file they name and the hunk
/// coordinates that locate it in a freshly recomputed diff.
///
/// The two public selection types deliberately stay separate structs so a
/// caller cannot pass one where the other is meant. This trait exists only so
/// the batch-wide checks below are written once.
trait Selection {
    /// The old and new paths, in that order.
    fn sides(&self) -> [Option<&PathBuf>; 2];
    /// `(old_start, old_lines, new_start, new_lines)`.
    fn hunk_coordinates(&self) -> (u32, u32, u32, u32);
}

impl Selection for HunkSelection {
    fn sides(&self) -> [Option<&PathBuf>; 2] {
        [self.old_path.as_ref(), self.new_path.as_ref()]
    }

    fn hunk_coordinates(&self) -> (u32, u32, u32, u32) {
        (
            self.old_start,
            self.old_lines,
            self.new_start,
            self.new_lines,
        )
    }
}

impl Selection for LineSelection {
    fn sides(&self) -> [Option<&PathBuf>; 2] {
        [self.old_path.as_ref(), self.new_path.as_ref()]
    }

    fn hunk_coordinates(&self) -> (u32, u32, u32, u32) {
        (
            self.old_start,
            self.old_lines,
            self.new_start,
            self.new_lines,
        )
    }
}

/// Every distinct path a batch names, on either side.
fn selection_paths<S: Selection>(selections: &[S]) -> Vec<PathBuf> {
    let mut paths = selections
        .iter()
        .flat_map(Selection::sides)
        .flatten()
        .cloned()
        .collect::<Vec<_>>();
    paths.sort_unstable();
    paths.dedup();
    paths
}

/// Refuses paths whose content Git would rewrite with an external filter.
///
/// Libgit2 runs its own `crlf`, `eol` and `ident` filters but never an external
/// `filter=` driver, so an apply would put raw working-tree bytes where
/// `git add` puts the driver's output. Under Git LFS that means replacing a
/// pointer with the payload it points at, which is a refusal rather than a
/// best effort: path-level staging still handles these files correctly.
fn refuse_filtered_paths(
    repository: &Repository,
    root: &Path,
    paths: &[PathBuf],
) -> Result<(), GitError> {
    for path in paths {
        let value = repository
            .get_attr_bytes(path, "filter", AttrCheckFlags::FILE_THEN_INDEX)
            .map_err(|source| inspection(root, source))?;
        let driver = match AttrValue::from_bytes(value) {
            AttrValue::String(driver) => driver.to_owned(),
            AttrValue::Bytes(driver) => String::from_utf8_lossy(driver).into_owned(),
            // A bare `filter` names no driver and Git treats it as unset, but
            // refusing costs nothing and cannot corrupt an index by accident.
            AttrValue::True => "unnamed".to_owned(),
            AttrValue::False | AttrValue::Unspecified => continue,
        };
        return Err(GitError::FilteredHunkSelection {
            path: path.clone(),
            driver,
        });
    }
    Ok(())
}

/// Revalidates every selection against a freshly computed diff.
fn prepare(
    root: &Path,
    selections: &[HunkSelection],
    paths: &[PathBuf],
    target: &DiffTarget,
    apply_target: ApplyTarget,
    cancellation: &Cancellation,
) -> Result<Vec<PreparedFile>, GitError> {
    let mut contexts = selections
        .iter()
        .map(|selection| selection.context_lines)
        .collect::<Vec<_>>();
    contexts.sort_unstable();
    contexts.dedup();

    let mut current = Vec::new();
    for context_lines in contexts {
        if cancellation.is_cancelled() {
            return Err(GitError::Cancelled);
        }
        // Unbounded keeps this model a superset of whatever the caller diffed:
        // a file the caller could legitimately select from is never omitted
        // here merely for exceeding that caller's own display budget.
        let options = DiffOptions::unbounded()
            .with_context_lines(context_lines)
            .with_paths(paths);
        current.push((
            context_lines,
            diff::compute(root, target.clone(), &options)?,
        ));
    }

    let mut prepared = Vec::<PreparedFile>::new();
    for selection in selections {
        if cancellation.is_cancelled() {
            return Err(GitError::Cancelled);
        }
        let path = display_path(selection.new_path.as_deref(), selection.old_path.as_deref());
        let file = current
            .iter()
            .find(|(context, _)| *context == selection.context_lines)
            .and_then(|(_, files)| {
                files.iter().find(|file| {
                    file.old_path == selection.old_path && file.new_path == selection.new_path
                })
            })
            .ok_or_else(|| GitError::StaleHunkSelection {
                path: path.to_path_buf(),
            })?;

        if file.old_blob_id != selection.old_blob_id || file.new_blob_id != selection.new_blob_id {
            return Err(GitError::StaleHunkSelection {
                path: path.to_path_buf(),
            });
        }
        refuse_unsupported(file, apply_target)?;

        let hunk = file
            .hunks
            .iter()
            .find(|hunk| coordinates_match(selection, hunk))
            .ok_or_else(|| GitError::HunkNotFound {
                path: path.to_path_buf(),
                old_start: selection.old_start,
                old_lines: selection.old_lines,
                new_start: selection.new_start,
                new_lines: selection.new_lines,
            })?;

        if let Some(existing) = prepared.iter_mut().find(|prepared| {
            prepared.file.old_path == file.old_path
                && prepared.file.new_path == file.new_path
                && prepared.file.old_blob_id == file.old_blob_id
                && prepared.file.new_blob_id == file.new_blob_id
        }) {
            if !existing.hunks.iter().any(|existing| existing.hunk == *hunk) {
                existing.hunks.push(PreparedHunk {
                    hunk: hunk.clone(),
                    selected_lines: None,
                });
            }
        } else {
            prepared.push(PreparedFile {
                file: file.clone(),
                hunks: vec![PreparedHunk {
                    hunk: hunk.clone(),
                    selected_lines: None,
                }],
            });
        }
    }

    refuse_overlaps(&prepared)?;
    Ok(prepared)
}

/// Revalidates line selections and merges all lines that resolve to one fresh
/// hunk. Selections from genuinely distinct, overlapping hunks remain separate
/// so the ordinary overlap refusal can reject them before the index is opened.
fn prepare_lines(
    root: &Path,
    selections: &[LineSelection],
    paths: &[PathBuf],
    target: &DiffTarget,
    apply_target: ApplyTarget,
    cancellation: &Cancellation,
) -> Result<Vec<PreparedFile>, GitError> {
    let mut contexts = selections
        .iter()
        .map(|selection| selection.context_lines)
        .collect::<Vec<_>>();
    contexts.sort_unstable();
    contexts.dedup();

    let mut current = Vec::new();
    for context_lines in contexts {
        if cancellation.is_cancelled() {
            return Err(GitError::Cancelled);
        }
        let options = DiffOptions::unbounded()
            .with_context_lines(context_lines)
            .with_paths(paths);
        current.push((
            context_lines,
            diff::compute(root, target.clone(), &options)?,
        ));
    }

    let mut prepared = Vec::<PreparedFile>::new();
    for selection in selections {
        if cancellation.is_cancelled() {
            return Err(GitError::Cancelled);
        }
        let path = display_path(selection.new_path.as_deref(), selection.old_path.as_deref());
        let file = current
            .iter()
            .find(|(context, _)| *context == selection.context_lines)
            .and_then(|(_, files)| {
                files.iter().find(|file| {
                    file.old_path == selection.old_path && file.new_path == selection.new_path
                })
            })
            .ok_or_else(|| GitError::StaleHunkSelection {
                path: path.to_path_buf(),
            })?;

        if file.old_blob_id != selection.old_blob_id || file.new_blob_id != selection.new_blob_id {
            return Err(GitError::StaleHunkSelection {
                path: path.to_path_buf(),
            });
        }
        refuse_unsupported(file, apply_target)?;

        let hunk = file
            .hunks
            .iter()
            .find(|hunk| coordinates_match(selection, hunk))
            .ok_or_else(|| GitError::HunkNotFound {
                path: path.to_path_buf(),
                old_start: selection.old_start,
                old_lines: selection.old_lines,
                new_start: selection.new_start,
                new_lines: selection.new_lines,
            })?;
        let line = hunk
            .lines
            .iter()
            .find(|line| line_coordinates_match(selection, line))
            .ok_or_else(|| GitError::LineNotFound {
                path: path.to_path_buf(),
                old_line_number: selection.old_line_number,
                new_line_number: selection.new_line_number,
            })?;
        let coordinates = LineCoordinates {
            old: line.old_line_number,
            new: line.new_line_number,
        };

        if let Some(existing_file) = prepared.iter_mut().find(|prepared| {
            prepared.file.old_path == file.old_path
                && prepared.file.new_path == file.new_path
                && prepared.file.old_blob_id == file.old_blob_id
                && prepared.file.new_blob_id == file.new_blob_id
        }) {
            if let Some(existing_hunk) = existing_file
                .hunks
                .iter_mut()
                .find(|existing| existing.hunk == *hunk)
            {
                // Line preparation only ever records a named-line
                // selection, so this never widens an existing whole-hunk one.
                let selected = existing_hunk.selected_lines.get_or_insert_with(Vec::new);
                if !selected.contains(&coordinates) {
                    selected.push(coordinates);
                }
            } else {
                existing_file.hunks.push(PreparedHunk {
                    hunk: hunk.clone(),
                    selected_lines: Some(vec![coordinates]),
                });
            }
        } else {
            prepared.push(PreparedFile {
                file: file.clone(),
                hunks: vec![PreparedHunk {
                    hunk: hunk.clone(),
                    selected_lines: Some(vec![coordinates]),
                }],
            });
        }
    }

    refuse_overlaps(&prepared)?;
    Ok(prepared)
}

/// Refuses a diff record that no index-only hunk apply can express.
///
/// The change kind is matched explicitly rather than inferred from the paths:
/// a copy also has two differing paths, and rendering one as a rename would
/// delete the source from the index. Records that are real but carry no
/// content, such as a bare `chmod` or a file becoming a symlink, are named for
/// what they are instead of being reported as a missing hunk.
fn refuse_unsupported(file: &FileDiff, apply_target: ApplyTarget) -> Result<(), GitError> {
    let path = display_path(file.new_path.as_deref(), file.old_path.as_deref()).to_path_buf();
    if apply_target == ApplyTarget::Worktree && file.change == FileChange::Unmerged {
        return Err(GitError::UnmergedDiscard { path });
    }
    if apply_target == ApplyTarget::Worktree && file.change == FileChange::Untracked {
        return Err(GitError::UntrackedDiscardRequiresDelete { path });
    }
    match file.change {
        FileChange::Added
        | FileChange::Modified
        | FileChange::Deleted
        | FileChange::Renamed
        | FileChange::Untracked => {}
        change => return Err(GitError::UnsupportedHunkChange { path, change }),
    }
    if file.binary {
        return Err(GitError::BinaryHunkSelection { path });
    }
    if !file.hunks.is_empty() {
        return Ok(());
    }
    if file.change == FileChange::Renamed {
        return Err(GitError::RenameOnlyHunkSelection {
            old_path: file.old_path.clone().unwrap_or_default(),
            new_path: file.new_path.clone().unwrap_or_default(),
        });
    }
    Err(GitError::MetadataOnlyHunkSelection {
        path,
        old_mode: file.old_mode,
        new_mode: file.new_mode,
    })
}

/// Refuses two selections for one file whose line ranges intersect.
///
/// Selections taken at different context settings can name hunks covering the
/// same lines. Libgit2 rejects the combined patch with a line-numbered apply
/// failure, so the overlap is named here while the offending path is still in
/// hand and the index has not been opened for writing.
///
/// This is the source side only. Rendering separately keeps each later hunk
/// anchored to what the earlier ones actually contributed, which is what makes
/// non-overlapping hunks composable; no shift can make two hunks that already
/// claim the same source lines applicable together.
fn refuse_overlaps(prepared: &[PreparedFile]) -> Result<(), GitError> {
    for entry in prepared {
        for (index, prepared_hunk) in entry.hunks.iter().enumerate() {
            let hunk = &prepared_hunk.hunk;
            for other in &entry.hunks[index + 1..] {
                let other = &other.hunk;
                if ranges_intersect(
                    hunk.old_start,
                    hunk.old_lines,
                    other.old_start,
                    other.old_lines,
                ) || ranges_intersect(
                    hunk.new_start,
                    hunk.new_lines,
                    other.new_start,
                    other.new_lines,
                ) {
                    return Err(GitError::OverlappingHunkSelection { path: entry.path() });
                }
            }
        }
    }
    Ok(())
}

/// Whether two half-open line ranges on one side of a patch collide.
///
/// A zero-length side is an insertion point rather than a span, and two
/// insertions at the same point are just as ambiguous as two overlapping
/// spans, so it is treated as occupying a single line.
fn ranges_intersect(start: u32, lines: u32, other_start: u32, other_lines: u32) -> bool {
    let end = start.saturating_add(lines.max(1));
    let other_end = other_start.saturating_add(other_lines.max(1));
    start < other_end && other_start < end
}

/// Renders the batch and hands it to libgit2 for an index-only apply.
fn apply(
    repository: &Repository,
    prepared: &[PreparedFile],
    paths: &[PathBuf],
    direction: Direction,
    target: ApplyTarget,
) -> Result<(), GitError> {
    let patch = render_patch(prepared, direction)?;
    let failure = |source| GitError::HunkApplication {
        paths: paths.to_vec(),
        source,
    };
    let parsed = Diff::from_buffer_ext(&patch, repository.object_format()).map_err(failure)?;

    // Libgit2 builds every postimage before its index writer commits, so the
    // real apply is already all-or-nothing. The check pass is kept because it
    // reaches that same verdict without ever opening `.git/index.lock`, which
    // keeps a batch this module cannot pre-validate from taking the index lock
    // only to roll back. It is cheap: the patch is a few hunks at most.
    let mut check = ApplyOptions::new();
    check.check(true);
    let location = match target {
        ApplyTarget::Index => ApplyLocation::Index,
        ApplyTarget::Worktree => ApplyLocation::WorkDir,
    };
    repository
        .apply(&parsed, location, Some(&mut check))
        .map_err(failure)?;
    repository.apply(&parsed, location, None).map_err(failure)
}

/// Whether a selection names `hunk` in the freshly recomputed diff.
fn coordinates_match<S: Selection>(selection: &S, hunk: &Hunk) -> bool {
    selection.hunk_coordinates()
        == (
            hunk.old_start,
            hunk.old_lines,
            hunk.new_start,
            hunk.new_lines,
        )
}

fn line_coordinates_match(selection: &LineSelection, line: &DiffLine) -> bool {
    matches!(line.kind, DiffLineKind::Addition | DiffLineKind::Deletion)
        && selection.old_line_number == line.old_line_number
        && selection.new_line_number == line.new_line_number
}

/// The path to show for a record, preferring the side a caller would name.
///
/// Every [`FileDiff`] has at least one side, so the empty fallback is only
/// reachable through a hand-built [`HunkSelection`] and exists so that a
/// refusal message never panics on one.
fn display_path<'path>(
    preferred: Option<&'path Path>,
    fallback: Option<&'path Path>,
) -> &'path Path {
    preferred.or(fallback).unwrap_or(Path::new(""))
}

/// One hunk resolved to the groups it will emit, already in emission order.
///
/// "Source" and "result" are the two sides of the rendered patch rather than
/// the diff's old and new: reversing swaps which is which, and every count and
/// coordinate below is expressed the way the patch reads.
struct RenderedHunk<'lines> {
    source_start: u32,
    source_lines: u32,
    result_lines: u32,
    groups: Vec<RenderLineGroup<'lines>>,
}

fn render_patch(files: &[PreparedFile], direction: Direction) -> Result<Vec<u8>, GitError> {
    let mut patch = Vec::new();
    for prepared in files {
        let mut hunks = prepared.hunks.iter().collect::<Vec<_>>();
        hunks.sort_by_key(|prepared| match direction {
            Direction::Forward => (prepared.hunk.old_start, prepared.hunk.new_start),
            Direction::Reverse => (prepared.hunk.new_start, prepared.hunk.old_start),
        });
        let rendered = hunks
            .iter()
            .map(|hunk| plan_hunk(hunk, direction))
            .collect::<Vec<_>>();
        refuse_stranded_eof_markers(prepared, &rendered, direction)?;
        render_file(&mut patch, &prepared.file, &rendered, direction);
    }
    Ok(patch)
}

/// Resolves one hunk to the exact groups it emits, in emission order.
///
/// The whole-hunk and line-selected paths share this walk, so a selection that
/// happens to name every changed line renders byte for byte like the hunk it
/// was taken from.
fn plan_hunk<'lines>(prepared: &'lines PreparedHunk, direction: Direction) -> RenderedHunk<'lines> {
    let hunk = &prepared.hunk;
    let groups = ordered_render_groups(hunk, prepared.selected_lines.as_deref(), direction);
    let (source_lines, result_lines) =
        groups
            .iter()
            .fold(
                (0u32, 0u32),
                |(source, result), group| match reversed_kind(group.kind, direction) {
                    DiffLineKind::Context => (source.saturating_add(1), result.saturating_add(1)),
                    DiffLineKind::Addition => (source, result.saturating_add(1)),
                    DiffLineKind::Deletion => (source.saturating_add(1), result),
                    _ => (source, result),
                },
            );
    RenderedHunk {
        source_start: match direction {
            Direction::Forward => hunk.old_start,
            Direction::Reverse => hunk.new_start,
        },
        source_lines,
        result_lines,
        groups,
    }
}

/// Resolves every changed line to what it becomes, then orders the result.
fn ordered_render_groups<'lines>(
    hunk: &'lines Hunk,
    selected_lines: Option<&[LineCoordinates]>,
    direction: Direction,
) -> Vec<RenderLineGroup<'lines>> {
    let resolved = resolve_render_groups(hunk, selected_lines, direction);
    let Direction::Reverse = direction else {
        return resolved;
    };
    // Reversal turns an addition into a deletion, so each run's additions have
    // to lead. Flipping signs in place would emit an addition, the no-newline
    // marker that qualifies it, and only then the matching deletion; libgit2's
    // patch parser rejects exactly that shape, and this is the order its own
    // printer produces.
    let mut ordered = Vec::with_capacity(resolved.len());
    let mut run = Vec::new();
    for group in resolved {
        if group.kind == DiffLineKind::Context {
            drain_reversed_run(&mut ordered, &mut run);
            ordered.push(group);
        } else {
            run.push(group);
        }
    }
    drain_reversed_run(&mut ordered, &mut run);
    ordered
}

fn drain_reversed_run<'lines>(
    ordered: &mut Vec<RenderLineGroup<'lines>>,
    run: &mut Vec<RenderLineGroup<'lines>>,
) {
    let (additions, deletions): (Vec<_>, Vec<_>) = run
        .drain(..)
        .partition(|group| group.kind == DiffLineKind::Addition);
    ordered.extend(additions);
    ordered.extend(deletions);
}

/// Decides what each changed line becomes, in the order it will be emitted.
///
/// `None` selects the whole hunk, which keeps libgit2's own line order: there
/// is nothing to drop or convert, so re-pairing the run would only make the
/// rendered patch differ from the diff it was read back from for no gain.
fn resolve_render_groups<'lines>(
    hunk: &'lines Hunk,
    selected_lines: Option<&[LineCoordinates]>,
    direction: Direction,
) -> Vec<RenderLineGroup<'lines>> {
    let groups = line_groups(&hunk.lines);
    let Some(selected_lines) = selected_lines else {
        return groups
            .into_iter()
            .map(|group| RenderLineGroup {
                original_kind: group.kind,
                kind: group.kind,
                lines: group.lines,
            })
            .collect();
    };
    let mut rendered = Vec::new();
    let mut changed_run = Vec::new();
    for group in groups {
        if group.kind == DiffLineKind::Context {
            push_selected_change_run(&mut rendered, &changed_run, selected_lines, direction);
            changed_run.clear();
            rendered.push(RenderLineGroup {
                original_kind: group.kind,
                kind: group.kind,
                lines: group.lines,
            });
        } else {
            changed_run.push(group);
        }
    }
    push_selected_change_run(&mut rendered, &changed_run, selected_lines, direction);
    rendered
}

/// Emits one run of changed lines, pairing each deletion with the addition
/// that replaced it.
///
/// A run is rendered as `-old +new` pairs rather than as every deletion
/// followed by every addition, because an unselected deletion is retained as
/// context and its place on the result side has to be the place of the line
/// that replaced it. Emitting the run in the diff's own order would put a
/// retained line ahead of the addition taking its position and so reorder the
/// file: staging only `two -> TWO` out of `one/two/last -> one/TWO/LAST` has to
/// leave `one/TWO/last`, never `one/last/TWO`.
///
/// Where a run is ragged the pairing is arbitrary, but so is every other
/// ordering, and all of them converge on the same file once the rest of the run
/// is staged.
fn push_selected_change_run<'lines>(
    rendered: &mut Vec<RenderLineGroup<'lines>>,
    run: &[LineGroup<'lines>],
    selected_lines: &[LineCoordinates],
    direction: Direction,
) {
    let deletions = run
        .iter()
        .filter(|group| group.kind == DiffLineKind::Deletion)
        .collect::<Vec<_>>();
    let additions = run
        .iter()
        .filter(|group| group.kind == DiffLineKind::Addition)
        .collect::<Vec<_>>();
    for index in 0..deletions.len().max(additions.len()) {
        for group in [deletions.get(index), additions.get(index)]
            .into_iter()
            .flatten()
        {
            let line = &group.lines[0];
            let selected = selected_lines.contains(&LineCoordinates {
                old: line.old_line_number,
                new: line.new_line_number,
            });
            let kind = match (direction, group.kind, selected) {
                (_, DiffLineKind::Addition, true) => Some(DiffLineKind::Addition),
                (_, DiffLineKind::Deletion, true) => Some(DiffLineKind::Deletion),
                (Direction::Forward, DiffLineKind::Addition, false)
                | (Direction::Reverse, DiffLineKind::Deletion, false) => None,
                (Direction::Forward, DiffLineKind::Deletion, false)
                | (Direction::Reverse, DiffLineKind::Addition, false) => {
                    Some(DiffLineKind::Context)
                }
                (_, kind, _) => Some(kind),
            };
            if let Some(kind) = kind {
                rendered.push(RenderLineGroup {
                    original_kind: group.kind,
                    kind,
                    lines: group.lines,
                });
            }
        }
    }
}

/// Refuses a selection that would strand a no-newline marker mid-file.
///
/// `\ No newline at end of file` says the line before it ends the file on the
/// side it names, so nothing may follow on that side. Retaining an unselected
/// change as context can put an unterminated line ahead of a selected one, and
/// no patch can express that: libgit2 either concatenates the two lines into a
/// single index blob without reporting anything, or rejects the rendering it
/// was handed. Both are worse than saying the selection cannot be applied, so
/// the batch is refused with the index untouched.
fn refuse_stranded_eof_markers(
    prepared: &PreparedFile,
    rendered: &[RenderedHunk<'_>],
    direction: Direction,
) -> Result<(), GitError> {
    let groups = rendered
        .iter()
        .flat_map(|hunk| &hunk.groups)
        .collect::<Vec<_>>();
    for (index, group) in groups.iter().enumerate() {
        for (line_index, line) in group.lines.iter().enumerate() {
            let kind = reversed_kind(emitted_line_kind(group, line_index, line), direction);
            if !is_eof_marker(kind) {
                continue;
            }
            let (source, result) = match kind {
                DiffLineKind::OldEofNoNewline => (true, false),
                DiffLineKind::NewEofNoNewline => (false, true),
                _ => (true, true),
            };
            if groups[index + 1..].iter().any(|later| {
                let (later_source, later_result) = occupied_sides(later, direction);
                (source && later_source) || (result && later_result)
            }) {
                return Err(GitError::UnrepresentableLineSelection {
                    path: prepared.path(),
                });
            }
        }
    }
    Ok(())
}

/// Which sides of the rendered patch a group contributes a line to.
fn occupied_sides(group: &RenderLineGroup<'_>, direction: Direction) -> (bool, bool) {
    match reversed_kind(group.kind, direction) {
        DiffLineKind::Context => (true, true),
        DiffLineKind::Addition => (false, true),
        DiffLineKind::Deletion => (true, false),
        _ => (false, false),
    }
}

fn render_file(
    patch: &mut Vec<u8>,
    file: &FileDiff,
    rendered: &[RenderedHunk<'_>],
    direction: Direction,
) {
    let (old_path, mut new_path, old_id, mut new_id, old_mode, mut new_mode) = match direction {
        Direction::Forward => (
            file.old_path.as_deref(),
            file.new_path.as_deref(),
            file.old_blob_id.as_str(),
            file.new_blob_id.as_str(),
            file.old_mode,
            file.new_mode,
        ),
        Direction::Reverse => (
            file.new_path.as_deref(),
            file.old_path.as_deref(),
            file.new_blob_id.as_str(),
            file.old_blob_id.as_str(),
            file.new_mode,
            file.old_mode,
        ),
    };

    // A partial deletion leaves a real postimage even when the original diff
    // compared the file with `/dev/null`. Render that as an ordinary modified
    // file; otherwise the file header would make libgit2 remove the entire
    // index entry despite the retained lines in the hunk body. A whole-file
    // deletion keeps nothing and so keeps its `/dev/null` header.
    if new_path.is_none() && rendered.iter().map(|hunk| hunk.result_lines).sum::<u32>() > 0 {
        new_path = old_path;
        new_id = old_id;
        new_mode = old_mode;
    }
    let display_old = display_path(old_path, new_path);
    let display_new = display_path(new_path, old_path);

    patch.extend_from_slice(b"diff --git ");
    push_quoted_path(patch, b"a/", display_old);
    patch.push(b' ');
    push_quoted_path(patch, b"b/", display_new);
    patch.push(b'\n');

    // Git writes the mode header before the rename header. Libgit2's parser
    // accepts either order, but matching Git keeps a rendered patch comparable
    // with `git diff` output when one has to be read by a human.
    match (old_path, new_path) {
        (None, Some(_)) if new_mode != 0 => {
            patch.extend_from_slice(format!("new file mode {new_mode:o}\n").as_bytes());
        }
        (Some(_), None) if old_mode != 0 => {
            patch.extend_from_slice(format!("deleted file mode {old_mode:o}\n").as_bytes());
        }
        (Some(_), Some(_)) if old_mode != new_mode => {
            patch.extend_from_slice(
                format!("old mode {old_mode:o}\nnew mode {new_mode:o}\n").as_bytes(),
            );
        }
        _ => {}
    }

    if let (Some(old_path), Some(new_path)) = (old_path, new_path)
        && old_path != new_path
    {
        // The percentage is presentation metadata; the explicit rename paths
        // are what make the parsed delta retain its rename identity.
        patch.extend_from_slice(b"similarity index 100%\nrename from ");
        push_quoted_path(patch, b"", old_path);
        patch.extend_from_slice(b"\nrename to ");
        push_quoted_path(patch, b"", new_path);
        patch.push(b'\n');
    }

    // These IDs name the two endpoints the selection was validated against,
    // which is what makes a stale patch fail loudly rather than land on the
    // wrong content. A partial selection deliberately stops between them, so
    // the result-side ID is not the ID of what gets written: libgit2 hashes the
    // postimage it builds and never compares it with this value. Anything that
    // reads these bytes as a standalone patch has to do the same.
    patch.extend_from_slice(format!("index {old_id}..{new_id}").as_bytes());
    if old_mode != 0 && old_mode == new_mode {
        patch.extend_from_slice(format!(" {old_mode:o}").as_bytes());
    }
    patch.push(b'\n');

    patch.extend_from_slice(b"--- ");
    if let Some(path) = old_path {
        push_quoted_path(patch, b"a/", path);
    } else {
        patch.extend_from_slice(b"/dev/null");
    }
    patch.push(b'\n');
    patch.extend_from_slice(b"+++ ");
    if let Some(path) = new_path {
        push_quoted_path(patch, b"b/", path);
    } else {
        patch.extend_from_slice(b"/dev/null");
    }
    patch.push(b'\n');

    let mut drift = 0i64;
    for hunk in rendered {
        render_hunk(patch, hunk, direction, &mut drift);
    }
}

fn render_hunk(
    patch: &mut Vec<u8>,
    rendered: &RenderedHunk<'_>,
    direction: Direction,
    drift: &mut i64,
) {
    let source_start = rendered.source_start;
    // The image libgit2 walks is the preimage plus whatever earlier hunks of
    // this same file have already contributed, so the result-side start is the
    // source-side start shifted by the running delta. The diff's own
    // result-side coordinate cannot be used: it assumes every earlier hunk of
    // the file applied whole, which a partial selection never does and a hunk
    // left out of the batch does not do at all.
    let result_start = if rendered.result_lines == 0 {
        // Git anchors a side that retains nothing at zero.
        0
    } else {
        u32::try_from((i64::from(source_start) + *drift).max(1)).unwrap_or(u32::MAX)
    };
    *drift += i64::from(rendered.result_lines) - i64::from(rendered.source_lines);
    patch.extend_from_slice(
        format!(
            "@@ -{source_start},{} +{result_start},{} @@\n",
            rendered.source_lines, rendered.result_lines
        )
        .as_bytes(),
    );
    for group in &rendered.groups {
        push_render_group(patch, group, direction);
    }
}

/// One diff line together with the end-of-file markers that qualify it.
///
/// Libgit2 emits a `\ No newline at end of file` record immediately after the
/// line it describes, so reordering must move the two together.
#[derive(Clone, Copy)]
struct LineGroup<'lines> {
    /// The kind of the leading line, before any direction flip.
    kind: DiffLineKind,
    lines: &'lines [DiffLine],
}

/// One line group after an unselected change has either disappeared or become
/// context for the side of the index that must remain unchanged.
struct RenderLineGroup<'lines> {
    original_kind: DiffLineKind,
    kind: DiffLineKind,
    lines: &'lines [DiffLine],
}

fn line_groups(lines: &[DiffLine]) -> Vec<LineGroup<'_>> {
    let mut groups = Vec::new();
    let mut start = 0;
    while start < lines.len() {
        let mut end = start + 1;
        while end < lines.len() && is_eof_marker(lines[end].kind) {
            end += 1;
        }
        groups.push(LineGroup {
            kind: lines[start].kind,
            lines: &lines[start..end],
        });
        start = end;
    }
    groups
}

/// The kind a line is written as, once its group's own kind is resolved.
fn emitted_line_kind(group: &RenderLineGroup<'_>, index: usize, line: &DiffLine) -> DiffLineKind {
    if index == 0 {
        group.kind
    } else if group.kind == DiffLineKind::Context
        && group.original_kind != DiffLineKind::Context
        && is_eof_marker(line.kind)
    {
        // Once an unselected changed line is retained on both sides, its
        // missing final newline belongs to both sides as well. This is only
        // sound when the line really is last on both, which
        // `refuse_stranded_eof_markers` has already established.
        DiffLineKind::BothEofNoNewline
    } else {
        line.kind
    }
}

fn push_render_group(patch: &mut Vec<u8>, group: &RenderLineGroup<'_>, direction: Direction) {
    for (index, line) in group.lines.iter().enumerate() {
        push_line_as(
            patch,
            line,
            emitted_line_kind(group, index, line),
            direction,
        );
    }
}

fn push_line_as(patch: &mut Vec<u8>, line: &DiffLine, kind: DiffLineKind, direction: Direction) {
    match reversed_kind(kind, direction) {
        DiffLineKind::Context => patch.push(b' '),
        DiffLineKind::Addition => patch.push(b'+'),
        DiffLineKind::Deletion => patch.push(b'-'),
        // A marker's own content already begins with the backslash Git writes.
        DiffLineKind::BothEofNoNewline
        | DiffLineKind::OldEofNoNewline
        | DiffLineKind::NewEofNoNewline => {}
    }
    patch.extend_from_slice(&line.content);
}

fn reversed_kind(kind: DiffLineKind, direction: Direction) -> DiffLineKind {
    match (direction, kind) {
        (Direction::Reverse, DiffLineKind::Addition) => DiffLineKind::Deletion,
        (Direction::Reverse, DiffLineKind::Deletion) => DiffLineKind::Addition,
        (Direction::Reverse, DiffLineKind::OldEofNoNewline) => DiffLineKind::NewEofNoNewline,
        (Direction::Reverse, DiffLineKind::NewEofNoNewline) => DiffLineKind::OldEofNoNewline,
        _ => kind,
    }
}

fn is_eof_marker(kind: DiffLineKind) -> bool {
    matches!(
        kind,
        DiffLineKind::BothEofNoNewline
            | DiffLineKind::OldEofNoNewline
            | DiffLineKind::NewEofNoNewline
    )
}

fn push_quoted_path(output: &mut Vec<u8>, prefix: &[u8], path: &Path) {
    output.push(b'"');
    for byte in prefix
        .iter()
        .copied()
        .chain(path_bytes(path).iter().copied())
    {
        match byte {
            b'\\' => output.extend_from_slice(b"\\\\"),
            b'"' => output.extend_from_slice(b"\\\""),
            b'\n' => output.extend_from_slice(b"\\n"),
            b'\r' => output.extend_from_slice(b"\\r"),
            b'\t' => output.extend_from_slice(b"\\t"),
            0x20..=0x7e => output.push(byte),
            _ => output.extend_from_slice(format!("\\{byte:03o}").as_bytes()),
        }
    }
    output.push(b'"');
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt;
    Cow::Borrowed(path.as_os_str().as_bytes())
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Cow<'_, [u8]> {
    Cow::Owned(path.to_string_lossy().replace('\\', "/").into_bytes())
}

fn inspection(root: &Path, source: git2::Error) -> GitError {
    GitError::Inspection {
        path: root.to_path_buf(),
        source: source.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use git2::Repository;

    use super::{
        ApplyTarget, Direction, HunkSelection, LineSelection, prepare_lines, refuse_unsupported,
        render_patch, selection_paths,
    };
    use crate::{
        Cancellation, CommitOptions, DiffLine, DiffLineKind, DiffOmission, DiffOptions, DiffTarget,
        FileChange, FileDiff, GitError, GitService, Hunk, StageOptions, StatusRefreshOutcome,
        testing::{Fixture, commit_all, configure_commit_identity, git, initialize_repository},
    };

    const OLD: &[u8] = b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven\ntwelve\nthirteen\nfourteen\nfifteen\nsixteen\nseventeen\neighteen\nnineteen\ntwenty\n";
    const NEW: &[u8] = b"one\nTWO\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven\ntwelve\nthirteen\nfourteen\nfifteen\nsixteen\nseventeen\nEIGHTEEN\nnineteen\ntwenty\n";
    const FIRST_ONLY: &[u8] = b"one\nTWO\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven\ntwelve\nthirteen\nfourteen\nfifteen\nsixteen\nseventeen\neighteen\nnineteen\ntwenty\n";
    const SECOND_ONLY: &[u8] = b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven\ntwelve\nthirteen\nfourteen\nfifteen\nsixteen\nseventeen\nEIGHTEEN\nnineteen\ntwenty\n";

    #[test]
    fn one_hunk_moves_across_each_side_of_the_index_and_back() {
        let fixture = Fixture::new();
        let root = fixture.directory("round-trip-hunk");
        let repository = initialize_repository(&root);
        fs::write(root.join("tracked.txt"), OLD).unwrap();
        commit_all(&repository, "prepare two hunks");
        fs::write(root.join("tracked.txt"), NEW).unwrap();
        let worktree_before = fs::read(root.join("tracked.txt")).unwrap();
        let index_before = index_bytes(&repository, Path::new("tracked.txt")).unwrap();
        let service = GitService::new(&root, &fixture.data_dir);

        let unstaged = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let file = named(&unstaged, Path::new("tracked.txt"));
        assert_eq!(file.hunks.len(), 2);
        service
            .stage_hunks(
                &[HunkSelection::new(file, &file.hunks[0])],
                &Cancellation::default(),
            )
            .unwrap();

        assert_eq!(
            index_bytes(&repository, Path::new("tracked.txt")).unwrap(),
            FIRST_ONLY
        );
        assert_eq!(fs::read(root.join("tracked.txt")).unwrap(), worktree_before);
        let staged = service
            .diff(DiffTarget::Staged, &DiffOptions::default())
            .unwrap();
        let remaining = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        assert_eq!(named(&staged, Path::new("tracked.txt")).hunks.len(), 1);
        assert_eq!(named(&remaining, Path::new("tracked.txt")).hunks.len(), 1);

        let staged_file = named(&staged, Path::new("tracked.txt"));
        service
            .unstage_hunks(
                &[HunkSelection::new(staged_file, &staged_file.hunks[0])],
                &Cancellation::default(),
            )
            .unwrap();

        assert_eq!(
            index_bytes(&repository, Path::new("tracked.txt")).unwrap(),
            index_before
        );
        assert_eq!(fs::read(root.join("tracked.txt")).unwrap(), worktree_before);
    }

    /// Regression: reversal used to keep libgit2's line order, which puts the
    /// flipped addition and its no-newline marker ahead of the matching
    /// deletion. Libgit2's own parser rejects that shape, so every unterminated
    /// file was permanently stuck once one of its hunks had been staged.
    #[test]
    fn an_unterminated_file_round_trips_in_both_directions() {
        let fixture = Fixture::new();
        let root = fixture.directory("unterminated-hunk");
        let repository = initialize_repository(&root);
        fs::write(root.join("eof.txt"), b"old without newline").unwrap();
        commit_all(&repository, "prepare unterminated baseline");
        fs::write(root.join("eof.txt"), b"new without newline").unwrap();
        let service = GitService::new(&root, &fixture.data_dir);

        let unstaged = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let file = named(&unstaged, Path::new("eof.txt"));
        service
            .stage_hunks(
                &[HunkSelection::new(file, &file.hunks[0])],
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(
            index_bytes(&repository, Path::new("eof.txt")).unwrap(),
            b"new without newline"
        );

        let staged = service
            .diff(DiffTarget::Staged, &DiffOptions::default())
            .unwrap();
        let staged_file = named(&staged, Path::new("eof.txt"));
        service
            .unstage_hunks(
                &[HunkSelection::new(staged_file, &staged_file.hunks[0])],
                &Cancellation::default(),
            )
            .unwrap();

        assert_eq!(
            index_bytes(&repository, Path::new("eof.txt")).unwrap(),
            b"old without newline"
        );
        assert_eq!(
            fs::read(root.join("eof.txt")).unwrap(),
            b"new without newline"
        );
    }

    /// The same reversal ordering, with the unterminated line buried in a hunk
    /// that also has ordinary context and a second changed line.
    #[test]
    fn an_unterminated_last_line_reverses_inside_a_larger_hunk() {
        let fixture = Fixture::new();
        let root = fixture.directory("unterminated-tail-hunk");
        let repository = initialize_repository(&root);
        fs::write(root.join("tail.txt"), b"one\ntwo\nthree\nfour\nlast").unwrap();
        commit_all(&repository, "prepare unterminated tail");
        fs::write(root.join("tail.txt"), b"one\nTWO\nthree\nfour\nLAST").unwrap();
        let service = GitService::new(&root, &fixture.data_dir);

        let unstaged = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let file = named(&unstaged, Path::new("tail.txt"));
        assert_eq!(file.hunks.len(), 1, "{file:#?}");
        service
            .stage_hunks(
                &[HunkSelection::new(file, &file.hunks[0])],
                &Cancellation::default(),
            )
            .unwrap();

        let staged = service
            .diff(DiffTarget::Staged, &DiffOptions::default())
            .unwrap();
        let staged_file = named(&staged, Path::new("tail.txt"));
        service
            .unstage_hunks(
                &[HunkSelection::new(staged_file, &staged_file.hunks[0])],
                &Cancellation::default(),
            )
            .unwrap();

        assert_eq!(
            index_bytes(&repository, Path::new("tail.txt")).unwrap(),
            b"one\ntwo\nthree\nfour\nlast"
        );
    }

    #[test]
    fn an_unterminated_final_line_and_the_change_around_it_stage_independently() {
        let fixture = Fixture::new();
        let root = fixture.directory("unterminated-line-selection");
        let repository = initialize_repository(&root);
        fs::write(root.join("tail.txt"), b"one\ntwo\nlast").unwrap();
        commit_all(&repository, "prepare line-selected tail");
        fs::write(root.join("tail.txt"), b"one\nTWO\nLAST").unwrap();
        let service = GitService::new(&root, &fixture.data_dir);

        let unstaged = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let file = named(&unstaged, Path::new("tail.txt"));
        let hunk = &file.hunks[0];
        let final_old = changed_line(hunk, DiffLineKind::Deletion, b"last");
        let final_new = changed_line(hunk, DiffLineKind::Addition, b"LAST");
        service
            .stage_lines(
                &[
                    LineSelection::new(file, hunk, final_old),
                    LineSelection::new(file, hunk, final_new),
                ],
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(
            index_bytes(&repository, Path::new("tail.txt")).unwrap(),
            b"one\ntwo\nLAST"
        );

        let staged = service
            .diff(DiffTarget::Staged, &DiffOptions::default())
            .unwrap();
        let staged_file = named(&staged, Path::new("tail.txt"));
        let staged_hunk = &staged_file.hunks[0];
        let final_old = changed_line(staged_hunk, DiffLineKind::Deletion, b"last");
        let final_new = changed_line(staged_hunk, DiffLineKind::Addition, b"LAST");
        service
            .unstage_lines(
                &[
                    LineSelection::new(staged_file, staged_hunk, final_old),
                    LineSelection::new(staged_file, staged_hunk, final_new),
                ],
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(
            index_bytes(&repository, Path::new("tail.txt")).unwrap(),
            b"one\ntwo\nlast"
        );

        let unstaged = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let file = named(&unstaged, Path::new("tail.txt"));
        let hunk = &file.hunks[0];
        let earlier_old = changed_line(hunk, DiffLineKind::Deletion, b"two\n");
        let earlier_new = changed_line(hunk, DiffLineKind::Addition, b"TWO\n");
        service
            .stage_lines(
                &[
                    LineSelection::new(file, hunk, earlier_old),
                    LineSelection::new(file, hunk, earlier_new),
                ],
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(
            index_bytes(&repository, Path::new("tail.txt")).unwrap(),
            b"one\nTWO\nlast"
        );
        assert_eq!(fs::read(root.join("tail.txt")).unwrap(), b"one\nTWO\nLAST");
    }

    #[test]
    fn partial_hunk_headers_recount_every_emitted_selection_shape() {
        let fixture = Fixture::new();
        let root = fixture.directory("recount-line-headers");
        let repository = initialize_repository(&root);
        fs::write(
            root.join("tracked.txt"),
            b"zero\nold first\nmiddle\nold last\nend\n",
        )
        .unwrap();
        commit_all(&repository, "prepare header recount");
        fs::write(
            root.join("tracked.txt"),
            b"zero\nnew first\nmiddle\nnew last\nextra\nend\n",
        )
        .unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let file = named(&files, Path::new("tracked.txt"));
        let hunk = &file.hunks[0];

        for shape in [
            vec![(DiffLineKind::Addition, b"new first\n".as_slice())],
            vec![(DiffLineKind::Deletion, b"old first\n".as_slice())],
            vec![
                (DiffLineKind::Deletion, b"old first\n".as_slice()),
                (DiffLineKind::Addition, b"new first\n".as_slice()),
            ],
            vec![(DiffLineKind::Addition, b"extra\n".as_slice())],
        ] {
            let selections = shape
                .into_iter()
                .map(|(kind, content)| {
                    LineSelection::new(file, hunk, changed_line(hunk, kind, content))
                })
                .collect::<Vec<_>>();
            let paths = selection_paths(&selections);
            let prepared = prepare_lines(
                &root,
                &selections,
                &paths,
                &DiffTarget::Unstaged,
                ApplyTarget::Index,
                &Cancellation::default(),
            )
            .unwrap();
            assert_patch_header_matches_body(&render_patch(&prepared, Direction::Forward).unwrap());
        }
    }

    /// Libgit2 never runs an external `filter=` driver, so applying raw
    /// working-tree bytes would put unfiltered content where `git add` puts the
    /// driver's output. Under Git LFS that replaces a pointer with its payload.
    #[test]
    fn a_filtered_path_is_refused_before_the_index_changes() {
        let fixture = Fixture::new();
        let root = fixture.directory("filtered-hunk");
        let repository = initialize_repository(&root);
        fs::write(root.join(".gitattributes"), b"secret.txt filter=redact\n").unwrap();
        let mut config = repository.config().unwrap();
        config
            .set_str("filter.redact.clean", "sed s/SECRET/REDACTED/g")
            .unwrap();
        config.set_str("filter.redact.smudge", "cat").unwrap();
        drop(config);
        fs::write(
            root.join("secret.txt"),
            b"line one\nSECRET value\nline three\n",
        )
        .unwrap();
        git(&root, ["add", "--", ".gitattributes", "secret.txt"]);
        git(&root, ["commit", "-m", "filtered baseline"]);
        let cleaned = index_bytes(&repository, Path::new("secret.txt")).unwrap();
        assert_eq!(cleaned, b"line one\nREDACTED value\nline three\n");
        fs::write(
            root.join("secret.txt"),
            b"line one\nSECRET changed\nline three\n",
        )
        .unwrap();

        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let file = named(&files, Path::new("secret.txt"));
        let error = service
            .stage_hunks(
                &[HunkSelection::new(file, &file.hunks[0])],
                &Cancellation::default(),
            )
            .unwrap_err();

        assert!(
            matches!(error, GitError::FilteredHunkSelection { ref path, ref driver }
            if path == Path::new("secret.txt") && driver == "redact"),
            "{error:?}"
        );
        assert_eq!(
            index_bytes(&repository, Path::new("secret.txt")).unwrap(),
            cleaned,
            "a refused batch still rewrote the index"
        );
        let line = file.hunks[0]
            .lines
            .iter()
            .find(|line| line.kind == DiffLineKind::Addition)
            .unwrap();
        let error = service
            .stage_lines(
                &[LineSelection::new(file, &file.hunks[0], line)],
                &Cancellation::default(),
            )
            .unwrap_err();
        assert!(
            matches!(error, GitError::FilteredHunkSelection { ref path, ref driver }
            if path == Path::new("secret.txt") && driver == "redact"),
            "{error:?}"
        );
        assert_eq!(
            index_bytes(&repository, Path::new("secret.txt")).unwrap(),
            cleaned,
            "a refused line selection rewrote the index"
        );
    }

    #[test]
    fn a_commit_after_partial_staging_contains_only_the_selected_hunk() {
        let fixture = Fixture::new();
        let root = fixture.directory("commit-hunk");
        let repository = initialize_repository(&root);
        fs::write(root.join("tracked.txt"), OLD).unwrap();
        commit_all(&repository, "prepare commit hunks");
        fs::write(root.join("tracked.txt"), NEW).unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let file = named(&files, Path::new("tracked.txt"));
        service
            .stage_hunks(
                &[HunkSelection::new(file, &file.hunks[0])],
                &Cancellation::default(),
            )
            .unwrap();

        let committed = service
            .commit(
                "commit one hunk",
                &CommitOptions::default().with_status_refresh(false),
                &Cancellation::default(),
            )
            .unwrap();
        let commit = repository
            .find_commit(committed.commit_id.parse().unwrap())
            .unwrap();
        let entry = commit
            .tree()
            .unwrap()
            .get_path(Path::new("tracked.txt"))
            .unwrap();
        assert_eq!(
            repository.find_blob(entry.id()).unwrap().content(),
            FIRST_ONLY
        );
        assert_eq!(fs::read(root.join("tracked.txt")).unwrap(), NEW);
    }

    #[test]
    fn a_commit_after_line_staging_contains_only_the_selected_lines() {
        let fixture = Fixture::new();
        let root = fixture.directory("commit-lines");
        let repository = initialize_repository(&root);
        fs::write(root.join("tracked.txt"), b"one\ntwo\nthree\nfour\n").unwrap();
        commit_all(&repository, "prepare line selection");
        fs::write(
            root.join("tracked.txt"),
            b"one\nselected\ntwo\nthree\nnot selected\nfour\n",
        )
        .unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let file = named(&files, Path::new("tracked.txt"));
        assert_eq!(file.hunks.len(), 1);
        let hunk = &file.hunks[0];
        let selected = changed_line(hunk, DiffLineKind::Addition, b"selected\n");

        let outcome = service
            .stage_lines(
                &[LineSelection::new(file, hunk, selected)],
                &Cancellation::default(),
            )
            .unwrap();

        assert_eq!(outcome.lines, 1);
        assert_eq!(outcome.hunks, 1);
        assert_eq!(
            index_bytes(&repository, Path::new("tracked.txt")).unwrap(),
            b"one\nselected\ntwo\nthree\nfour\n"
        );
        let committed = service
            .commit(
                "commit one line",
                &CommitOptions::default().with_status_refresh(false),
                &Cancellation::default(),
            )
            .unwrap();
        let commit = repository
            .find_commit(committed.commit_id.parse().unwrap())
            .unwrap();
        let entry = commit
            .tree()
            .unwrap()
            .get_path(Path::new("tracked.txt"))
            .unwrap();
        assert_eq!(
            repository.find_blob(entry.id()).unwrap().content(),
            b"one\nselected\ntwo\nthree\nfour\n"
        );
        assert_eq!(
            fs::read(root.join("tracked.txt")).unwrap(),
            b"one\nselected\ntwo\nthree\nnot selected\nfour\n"
        );
    }

    #[test]
    fn unselected_deletions_become_context_and_selected_deletions_remain_independent() {
        let fixture = Fixture::new();
        let root = fixture.directory("partial-deletions");
        let repository = initialize_repository(&root);
        fs::write(
            root.join("tracked.txt"),
            b"one\nkeep deletion\nthree\nremove deletion\nfive\n",
        )
        .unwrap();
        commit_all(&repository, "prepare deletions");
        fs::write(
            root.join("tracked.txt"),
            b"one\nthree\nselected addition\nfive\n",
        )
        .unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let file = named(&files, Path::new("tracked.txt"));
        assert_eq!(file.hunks.len(), 1);
        let hunk = &file.hunks[0];
        let addition = changed_line(hunk, DiffLineKind::Addition, b"selected addition\n");
        let deletion = changed_line(hunk, DiffLineKind::Deletion, b"remove deletion\n");

        let outcome = service
            .stage_lines(
                &[
                    LineSelection::new(file, hunk, addition),
                    LineSelection::new(file, hunk, deletion),
                ],
                &Cancellation::default(),
            )
            .unwrap();

        assert_eq!(outcome.lines, 2);
        assert_eq!(outcome.hunks, 1, "same-hunk lines were not merged");
        assert_eq!(
            index_bytes(&repository, Path::new("tracked.txt")).unwrap(),
            b"one\nkeep deletion\nthree\nselected addition\nfive\n"
        );
        assert_eq!(
            fs::read(root.join("tracked.txt")).unwrap(),
            b"one\nthree\nselected addition\nfive\n"
        );
    }

    #[test]
    fn line_unstaging_reverses_only_the_selected_lines() {
        let fixture = Fixture::new();
        let root = fixture.directory("unstage-lines");
        let repository = initialize_repository(&root);
        fs::write(root.join("tracked.txt"), b"one\ntwo\nthree\nfour\n").unwrap();
        commit_all(&repository, "prepare unstaging lines");
        fs::write(
            root.join("tracked.txt"),
            b"one\nfirst\ntwo\nthree\nsecond\nfour\n",
        )
        .unwrap();
        git(&root, ["add", "--", "tracked.txt"]);
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Staged, &DiffOptions::default())
            .unwrap();
        let file = named(&files, Path::new("tracked.txt"));
        let hunk = &file.hunks[0];
        let first = changed_line(hunk, DiffLineKind::Addition, b"first\n");

        service
            .unstage_lines(
                &[LineSelection::new(file, hunk, first)],
                &Cancellation::default(),
            )
            .unwrap();

        assert_eq!(
            index_bytes(&repository, Path::new("tracked.txt")).unwrap(),
            b"one\ntwo\nthree\nsecond\nfour\n"
        );
        assert_eq!(
            fs::read(root.join("tracked.txt")).unwrap(),
            b"one\nfirst\ntwo\nthree\nsecond\nfour\n"
        );
    }

    /// Regression: a partial hunk was rendered with the fresh diff's own
    /// new-side start, which assumes every earlier hunk of the file applied
    /// whole. Selecting one line in a file's second hunk, or leaving part of
    /// the first behind, then placed every later hunk past the end of the image
    /// libgit2 was building and the whole atomic batch was refused.
    #[test]
    fn later_hunks_follow_the_shift_the_earlier_ones_actually_applied() {
        let fixture = Fixture::new();
        let root = fixture.directory("multi-hunk-drift");
        let repository = initialize_repository(&root);
        let baseline = (1..=40)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        fs::write(root.join("wide.txt"), &baseline).unwrap();
        commit_all(&repository, "prepare a file with two distant hunks");
        let mut edited = String::new();
        for line in 1..=40 {
            edited.push_str(&format!("line {line}\n"));
            if line == 5 {
                edited.push_str("added a\nadded b\nadded c\n");
            }
            if line == 30 {
                edited.push_str("late added\n");
            }
        }
        fs::write(root.join("wide.txt"), &edited).unwrap();
        let service = GitService::new(&root, &fixture.data_dir);

        // Only the later hunk, with the earlier one left entirely unstaged: the
        // image libgit2 walks is still the preimage, so the later hunk has to
        // be anchored at its old-side start rather than its new-side one.
        let unstaged = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let file = named(&unstaged, Path::new("wide.txt"));
        assert_eq!(file.hunks.len(), 2, "{file:#?}");
        let late = changed_line(&file.hunks[1], DiffLineKind::Addition, b"late added\n");
        service
            .stage_lines(
                &[LineSelection::new(file, &file.hunks[1], late)],
                &Cancellation::default(),
            )
            .unwrap();
        let staged_once = index_bytes(&repository, Path::new("wide.txt")).unwrap();
        assert!(
            staged_once
                .windows(10)
                .any(|window| window == b"late added")
        );
        assert!(!staged_once.windows(7).any(|window| window == b"added a"));

        // Then one line of the earlier hunk, which shortens the first hunk
        // relative to the diff and shifts everything after it.
        let unstaged = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let file = named(&unstaged, Path::new("wide.txt"));
        let first = changed_line(&file.hunks[0], DiffLineKind::Addition, b"added a\n");
        let outcome = service
            .stage_lines(
                &[LineSelection::new(file, &file.hunks[0], first)],
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(outcome.lines, 1);
        assert_eq!(
            String::from_utf8(index_bytes(&repository, Path::new("wide.txt")).unwrap()).unwrap(),
            baseline
                .replace("line 5\n", "line 5\nadded a\n")
                .replace("line 30\n", "line 30\nlate added\n")
        );
        assert_eq!(fs::read(root.join("wide.txt")).unwrap(), edited.as_bytes());
    }

    /// The same shift on the way back out of the index, where the direction
    /// flip makes the new side the one the patch is read from.
    #[test]
    fn later_hunks_follow_the_shift_when_unstaging_too() {
        let fixture = Fixture::new();
        let root = fixture.directory("multi-hunk-drift-reverse");
        let repository = initialize_repository(&root);
        let baseline = (1..=40)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        fs::write(root.join("wide.txt"), &baseline).unwrap();
        commit_all(&repository, "prepare a staged file with two hunks");
        let mut edited = String::new();
        for line in 1..=40 {
            edited.push_str(&format!("line {line}\n"));
            if line == 5 {
                edited.push_str("added a\nadded b\nadded c\n");
            }
            if line == 30 {
                edited.push_str("late added\n");
            }
        }
        fs::write(root.join("wide.txt"), &edited).unwrap();
        git(&root, ["add", "--", "wide.txt"]);
        let service = GitService::new(&root, &fixture.data_dir);

        let staged = service
            .diff(DiffTarget::Staged, &DiffOptions::default())
            .unwrap();
        let file = named(&staged, Path::new("wide.txt"));
        assert_eq!(file.hunks.len(), 2, "{file:#?}");
        let first = changed_line(&file.hunks[0], DiffLineKind::Addition, b"added a\n");
        let late = changed_line(&file.hunks[1], DiffLineKind::Addition, b"late added\n");
        service
            .unstage_lines(
                &[
                    LineSelection::new(file, &file.hunks[0], first),
                    LineSelection::new(file, &file.hunks[1], late),
                ],
                &Cancellation::default(),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8(index_bytes(&repository, Path::new("wide.txt")).unwrap()).unwrap(),
            baseline.replace("line 5\n", "line 5\nadded b\nadded c\n"),
        );
        assert_eq!(fs::read(root.join("wide.txt")).unwrap(), edited.as_bytes());
    }

    /// Regression: retaining an unselected change as context promoted its
    /// no-newline marker to both sides even when a selected line still had to
    /// follow it. Libgit2 then concatenated the two lines into one index blob
    /// and reported success, or rejected the rendering it had just been given.
    #[test]
    fn a_selection_that_would_strand_an_eof_marker_is_refused() {
        let fixture = Fixture::new();
        let root = fixture.directory("stranded-eof");
        let repository = initialize_repository(&root);
        fs::write(root.join("tail.txt"), b"one\ntwo\nlast").unwrap();
        commit_all(&repository, "prepare an unterminated tail");
        fs::write(root.join("tail.txt"), b"one\ntwo\nLAST").unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let index_before = fs::read(repository.path().join("index")).unwrap();

        let unstaged = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let file = named(&unstaged, Path::new("tail.txt"));
        let hunk = &file.hunks[0];
        let addition = changed_line(hunk, DiffLineKind::Addition, b"LAST");

        assert!(matches!(
            service.stage_lines(
                &[LineSelection::new(file, hunk, addition)],
                &Cancellation::default(),
            ),
            Err(GitError::UnrepresentableLineSelection { .. })
        ));
        assert_eq!(
            fs::read(repository.path().join("index")).unwrap(),
            index_before,
            "a refused selection still touched the index"
        );

        // The deletion alone is expressible, because nothing has to follow the
        // marker once the unterminated line is the one being removed.
        let deletion = changed_line(hunk, DiffLineKind::Deletion, b"last");
        service
            .stage_lines(
                &[LineSelection::new(file, hunk, deletion)],
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(
            index_bytes(&repository, Path::new("tail.txt")).unwrap(),
            b"one\ntwo\n"
        );
    }

    /// The same refusal on the unstaging side, which used to surface as a
    /// libgit2 parse error against this module's own rendering.
    #[test]
    fn a_stranded_eof_marker_is_refused_when_unstaging_too() {
        let fixture = Fixture::new();
        let root = fixture.directory("stranded-eof-reverse");
        let repository = initialize_repository(&root);
        fs::write(root.join("tail.txt"), b"one\ntwo\nlast").unwrap();
        commit_all(&repository, "prepare a staged unterminated tail");
        fs::write(root.join("tail.txt"), b"one\ntwo\nLAST").unwrap();
        git(&root, ["add", "--", "tail.txt"]);
        let service = GitService::new(&root, &fixture.data_dir);
        let index_before = fs::read(repository.path().join("index")).unwrap();

        let staged = service
            .diff(DiffTarget::Staged, &DiffOptions::default())
            .unwrap();
        let file = named(&staged, Path::new("tail.txt"));
        let hunk = &file.hunks[0];
        let deletion = changed_line(hunk, DiffLineKind::Deletion, b"last");

        assert!(matches!(
            service.unstage_lines(
                &[LineSelection::new(file, hunk, deletion)],
                &Cancellation::default(),
            ),
            Err(GitError::UnrepresentableLineSelection { .. })
        ));
        assert_eq!(
            fs::read(repository.path().join("index")).unwrap(),
            index_before,
            "a refused selection still touched the index"
        );
    }

    /// An unselected deletion is retained where the line replacing it stood, so
    /// a partial stage never reorders the file. The earlier pair here is
    /// selected and the later one is not, which is the shape that would
    /// otherwise put the retained line ahead of the addition taking its place.
    #[test]
    fn a_retained_deletion_keeps_the_place_of_the_line_that_replaced_it() {
        let fixture = Fixture::new();
        let root = fixture.directory("retained-order");
        let repository = initialize_repository(&root);
        fs::write(root.join("tracked.txt"), b"top\nfirst\nsecond\n").unwrap();
        commit_all(&repository, "prepare an adjacent replacement pair");
        fs::write(root.join("tracked.txt"), b"top\nFIRST\nSECOND\n").unwrap();
        let service = GitService::new(&root, &fixture.data_dir);

        let unstaged = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let file = named(&unstaged, Path::new("tracked.txt"));
        let hunk = &file.hunks[0];
        let selections = [
            LineSelection::new(
                file,
                hunk,
                changed_line(hunk, DiffLineKind::Deletion, b"first\n"),
            ),
            LineSelection::new(
                file,
                hunk,
                changed_line(hunk, DiffLineKind::Addition, b"FIRST\n"),
            ),
        ];
        let paths = selection_paths(&selections);
        let prepared = prepare_lines(
            &root,
            &selections,
            &paths,
            &DiffTarget::Unstaged,
            ApplyTarget::Index,
            &Cancellation::default(),
        )
        .unwrap();
        let patch = render_patch(&prepared, Direction::Forward).unwrap();
        assert_patch_header_matches_body(&patch);
        let body = String::from_utf8(patch).unwrap();
        assert!(
            body.contains("-first\n+FIRST\n second\n"),
            "the retained deletion moved out of place:\n{body}"
        );

        service
            .stage_lines(&selections, &Cancellation::default())
            .unwrap();
        assert_eq!(
            index_bytes(&repository, Path::new("tracked.txt")).unwrap(),
            b"top\nFIRST\nsecond\n"
        );
    }

    #[test]
    fn stale_and_missing_hunks_are_distinct_and_leave_the_index_untouched() {
        let fixture = Fixture::new();
        let root = fixture.directory("hunk-refusals");
        let repository = initialize_repository(&root);
        fs::write(root.join("tracked.txt"), OLD).unwrap();
        commit_all(&repository, "prepare refusal");
        fs::write(root.join("tracked.txt"), NEW).unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let file = named(&files, Path::new("tracked.txt"));
        let selection = HunkSelection::new(file, &file.hunks[0]);

        let mut missing = selection.clone();
        missing.old_start += 1;
        let index_before = fs::read(repository.path().join("index")).unwrap();
        let worktree_before = fs::read(root.join("tracked.txt")).unwrap();
        assert!(matches!(
            service.stage_hunks(&[missing], &Cancellation::default()),
            Err(GitError::HunkNotFound { .. })
        ));
        assert_eq!(
            fs::read(repository.path().join("index")).unwrap(),
            index_before
        );
        assert_eq!(fs::read(root.join("tracked.txt")).unwrap(), worktree_before);

        fs::write(root.join("tracked.txt"), b"edited after diff\n").unwrap();
        let index_before = fs::read(repository.path().join("index")).unwrap();
        let worktree_before = fs::read(root.join("tracked.txt")).unwrap();
        assert!(matches!(
            service.stage_hunks(&[selection], &Cancellation::default()),
            Err(GitError::StaleHunkSelection { .. })
        ));
        assert_eq!(
            fs::read(repository.path().join("index")).unwrap(),
            index_before
        );
        assert_eq!(fs::read(root.join("tracked.txt")).unwrap(), worktree_before);
    }

    #[test]
    fn binary_and_rename_only_records_have_typed_refusals() {
        let fixture = Fixture::new();
        let binary_root = fixture.directory("binary-hunk");
        let binary_repository = initialize_repository(&binary_root);
        fs::write(binary_root.join("binary.dat"), b"old\0bytes").unwrap();
        commit_all(&binary_repository, "add binary");
        fs::write(binary_root.join("binary.dat"), b"new\0bytes").unwrap();
        let binary_service = GitService::new(&binary_root, &fixture.data_dir);
        let binary_files = binary_service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let binary = named(&binary_files, Path::new("binary.dat"));
        let fake = fake_hunk();
        let index_before = fs::read(binary_repository.path().join("index")).unwrap();
        assert!(matches!(
            binary_service.stage_hunks(
                &[HunkSelection::new(binary, &fake)],
                &Cancellation::default()
            ),
            Err(GitError::BinaryHunkSelection { .. })
        ));
        assert!(matches!(
            binary_service.stage_lines(
                &[LineSelection::new(binary, &fake, &fake_line())],
                &Cancellation::default()
            ),
            Err(GitError::BinaryHunkSelection { .. })
        ));
        assert_eq!(
            fs::read(binary_repository.path().join("index")).unwrap(),
            index_before
        );

        let rename_root = fixture.directory("rename-only-hunk");
        let rename_repository = initialize_repository(&rename_root);
        fs::rename(
            rename_root.join("tracked.txt"),
            rename_root.join("renamed.txt"),
        )
        .unwrap();
        let rename_service = GitService::new(&rename_root, &fixture.data_dir);
        let rename_files = rename_service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let rename = rename_files
            .iter()
            .find(|file| file.change == FileChange::Renamed)
            .unwrap();
        assert!(rename.hunks.is_empty());
        let index_before = fs::read(rename_repository.path().join("index")).unwrap();
        assert!(matches!(
            rename_service.stage_hunks(
                &[HunkSelection::new(rename, &fake)],
                &Cancellation::default()
            ),
            Err(GitError::RenameOnlyHunkSelection { .. })
        ));
        assert!(matches!(
            rename_service.stage_lines(
                &[LineSelection::new(rename, &fake, &fake_line())],
                &Cancellation::default()
            ),
            Err(GitError::RenameOnlyHunkSelection { .. })
        ));
        assert_eq!(
            fs::read(rename_repository.path().join("index")).unwrap(),
            index_before
        );
    }

    /// A bare `chmod` and a file becoming a symlink are both real changes with
    /// no content hunk. Reporting them as a missing hunk would be a lie: the
    /// diff is there, it just cannot be expressed as an index-only patch.
    #[cfg(unix)]
    #[test]
    fn metadata_only_records_are_named_rather_than_reported_as_missing() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let fixture = Fixture::new();
        let root = fixture.directory("metadata-hunk");
        let repository = initialize_repository(&root);
        fs::write(root.join("mode.txt"), OLD).unwrap();
        fs::write(root.join("target.txt"), b"target\n").unwrap();
        fs::write(root.join("thing"), b"plain\n").unwrap();
        commit_all(&repository, "prepare metadata changes");
        fs::set_permissions(root.join("mode.txt"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_file(root.join("thing")).unwrap();
        symlink("target.txt", root.join("thing")).unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let fake = fake_hunk();

        let mode = named(&files, Path::new("mode.txt"));
        assert!(mode.hunks.is_empty(), "{mode:#?}");
        let error = service
            .stage_hunks(&[HunkSelection::new(mode, &fake)], &Cancellation::default())
            .unwrap_err();
        assert!(
            matches!(error, GitError::MetadataOnlyHunkSelection { ref path, old_mode, new_mode }
            if path == Path::new("mode.txt") && old_mode == 0o100_644 && new_mode == 0o100_755),
            "{error:?}"
        );

        let typechange = named(&files, Path::new("thing"));
        assert_eq!(typechange.change, FileChange::TypeChanged);
        let error = service
            .stage_hunks(
                &[HunkSelection::new(typechange, &fake)],
                &Cancellation::default(),
            )
            .unwrap_err();
        assert!(
            matches!(error, GitError::UnsupportedHunkChange { ref path, change }
            if path == Path::new("thing") && change == FileChange::TypeChanged),
            "{error:?}"
        );
    }

    /// Copy detection is off today, so this guard is the only thing standing
    /// between a future `DiffFindOptions::copies` and a rendered `rename from`
    /// that would delete the copy source out of the index.
    #[test]
    fn change_kinds_without_an_index_only_patch_are_refused() {
        for change in [
            FileChange::Copied,
            FileChange::TypeChanged,
            FileChange::Unmerged,
        ] {
            let error = refuse_unsupported(&record(change), ApplyTarget::Index).unwrap_err();
            assert!(
                matches!(error, GitError::UnsupportedHunkChange { change: refused, .. }
                if refused == change),
                "{change} was not refused: {error:?}"
            );
        }
        for change in [
            FileChange::Added,
            FileChange::Modified,
            FileChange::Deleted,
            FileChange::Untracked,
        ] {
            assert!(
                refuse_unsupported(&record(change), ApplyTarget::Index).is_ok(),
                "{change} was refused"
            );
        }
    }

    #[test]
    fn stage_and_unstage_hunks_work_on_an_unborn_branch() {
        let fixture = Fixture::new();
        let root = fixture.directory("unborn-hunk");
        let repository = Repository::init(&root).unwrap();
        repository.set_head("refs/heads/main").unwrap();
        configure_commit_identity(&repository);
        fs::write(root.join("first.txt"), b"first commit\n").unwrap();
        let worktree_before = fs::read(root.join("first.txt")).unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let file = named(&files, Path::new("first.txt"));
        let selection = HunkSelection::new(file, &file.hunks[0]);

        service
            .stage_hunks(std::slice::from_ref(&selection), &Cancellation::default())
            .unwrap();
        assert_eq!(
            index_bytes(&repository, Path::new("first.txt")).unwrap(),
            worktree_before
        );
        assert_eq!(fs::read(root.join("first.txt")).unwrap(), worktree_before);

        service
            .unstage_hunks(&[selection], &Cancellation::default())
            .unwrap();
        assert!(index_bytes(&repository, Path::new("first.txt")).is_none());
        assert_eq!(fs::read(root.join("first.txt")).unwrap(), worktree_before);
    }

    #[test]
    fn selections_retain_non_default_context() {
        let fixture = Fixture::new();
        let root = fixture.directory("zero-context-hunk");
        let repository = initialize_repository(&root);
        fs::write(root.join("tracked.txt"), OLD).unwrap();
        commit_all(&repository, "prepare zero context");
        fs::write(root.join("tracked.txt"), NEW).unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let options = DiffOptions::default().with_context_lines(0);
        let files = service.diff(DiffTarget::Unstaged, &options).unwrap();
        let file = named(&files, Path::new("tracked.txt"));

        service
            .stage_hunks(
                &[HunkSelection::new(file, &file.hunks[0])],
                &Cancellation::default(),
            )
            .unwrap();

        assert_eq!(
            index_bytes(&repository, Path::new("tracked.txt")).unwrap(),
            FIRST_ONLY
        );
    }

    /// Two selections covering the same lines are named for what they are,
    /// with the path in hand, rather than surfacing libgit2's line-numbered
    /// apply failure. Either way the index must not move.
    #[test]
    fn overlapping_selections_are_refused_without_a_partial_index_write() {
        let fixture = Fixture::new();
        let root = fixture.directory("overlapping-hunk");
        let repository = initialize_repository(&root);
        fs::write(root.join("tracked.txt"), OLD).unwrap();
        commit_all(&repository, "prepare overlapping selections");
        fs::write(root.join("tracked.txt"), NEW).unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let zero_context = service
            .diff(
                DiffTarget::Unstaged,
                &DiffOptions::default().with_context_lines(0),
            )
            .unwrap();
        let default_context = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let zero = named(&zero_context, Path::new("tracked.txt"));
        let default = named(&default_context, Path::new("tracked.txt"));
        let selections = [
            HunkSelection::new(zero, &zero.hunks[0]),
            HunkSelection::new(default, &default.hunks[0]),
        ];
        let index_before = fs::read(repository.path().join("index")).unwrap();
        let worktree_before = fs::read(root.join("tracked.txt")).unwrap();

        let error = service
            .stage_hunks(&selections, &Cancellation::default())
            .unwrap_err();

        assert!(
            matches!(error, GitError::OverlappingHunkSelection { ref path }
            if path == Path::new("tracked.txt")),
            "{error:?}"
        );
        assert_eq!(
            fs::read(repository.path().join("index")).unwrap(),
            index_before
        );
        assert_eq!(fs::read(root.join("tracked.txt")).unwrap(), worktree_before);
    }

    #[test]
    fn same_line_from_overlapping_hunks_is_refused_but_same_hunk_lines_are_merged() {
        let fixture = Fixture::new();
        let root = fixture.directory("overlapping-lines");
        let repository = initialize_repository(&root);
        fs::write(root.join("tracked.txt"), OLD).unwrap();
        commit_all(&repository, "prepare overlapping lines");
        fs::write(root.join("tracked.txt"), NEW).unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let zero_context = service
            .diff(
                DiffTarget::Unstaged,
                &DiffOptions::default().with_context_lines(0),
            )
            .unwrap();
        let default_context = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let zero = named(&zero_context, Path::new("tracked.txt"));
        let default = named(&default_context, Path::new("tracked.txt"));
        let zero_line = changed_line(&zero.hunks[0], DiffLineKind::Addition, b"TWO\n");
        let default_line = changed_line(&default.hunks[0], DiffLineKind::Addition, b"TWO\n");
        let index_before = fs::read(repository.path().join("index")).unwrap();

        let error = service
            .stage_lines(
                &[
                    LineSelection::new(zero, &zero.hunks[0], zero_line),
                    LineSelection::new(default, &default.hunks[0], default_line),
                ],
                &Cancellation::default(),
            )
            .unwrap_err();

        assert!(
            matches!(error, GitError::OverlappingHunkSelection { ref path }
                if path == Path::new("tracked.txt")),
            "{error:?}"
        );
        assert_eq!(
            fs::read(repository.path().join("index")).unwrap(),
            index_before
        );
    }

    #[test]
    fn stale_line_selection_is_refused_with_the_index_untouched() {
        let fixture = Fixture::new();
        let root = fixture.directory("stale-line");
        let repository = initialize_repository(&root);
        fs::write(root.join("tracked.txt"), b"one\ntwo\nthree\n").unwrap();
        commit_all(&repository, "prepare stale line");
        fs::write(root.join("tracked.txt"), b"one\nTWO\nthree\n").unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let file = named(&files, Path::new("tracked.txt"));
        let line = changed_line(&file.hunks[0], DiffLineKind::Addition, b"TWO\n");
        let selection = LineSelection::new(file, &file.hunks[0], line);
        fs::write(root.join("tracked.txt"), b"one\nTWO AGAIN\nthree\n").unwrap();
        let index_before = fs::read(repository.path().join("index")).unwrap();

        assert!(matches!(
            service.stage_lines(&[selection], &Cancellation::default()),
            Err(GitError::StaleHunkSelection { .. })
        ));
        assert_eq!(
            fs::read(repository.path().join("index")).unwrap(),
            index_before
        );
    }

    /// One batch is one transaction. The refusal is raised by the third
    /// selection, and the first two must not reach the index either.
    #[test]
    fn a_batch_spanning_two_files_lands_or_refuses_together() {
        let fixture = Fixture::new();
        let root = fixture.directory("batch-hunk");
        let repository = initialize_repository(&root);
        fs::write(root.join("first.txt"), OLD).unwrap();
        fs::write(root.join("second.txt"), OLD).unwrap();
        commit_all(&repository, "prepare batch");
        fs::write(root.join("first.txt"), NEW).unwrap();
        fs::write(root.join("second.txt"), NEW).unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let first = named(&files, Path::new("first.txt"));
        let second = named(&files, Path::new("second.txt"));
        let mut stale = HunkSelection::new(second, &second.hunks[0]);
        stale.new_blob_id = "0".repeat(stale.new_blob_id.len());
        let index_before = fs::read(repository.path().join("index")).unwrap();

        assert!(matches!(
            service.stage_hunks(
                &[
                    HunkSelection::new(first, &first.hunks[0]),
                    HunkSelection::new(first, &first.hunks[1]),
                    stale,
                ],
                &Cancellation::default(),
            ),
            Err(GitError::StaleHunkSelection { .. })
        ));
        assert_eq!(
            fs::read(repository.path().join("index")).unwrap(),
            index_before,
            "a refused batch staged part of itself"
        );

        service
            .stage_hunks(
                &[
                    HunkSelection::new(first, &first.hunks[0]),
                    HunkSelection::new(first, &first.hunks[1]),
                    HunkSelection::new(second, &second.hunks[1]),
                ],
                &Cancellation::default(),
            )
            .unwrap();

        assert_eq!(
            index_bytes(&repository, Path::new("first.txt")).unwrap(),
            NEW,
            "both selected hunks of the first file should be staged"
        );
        assert_eq!(
            index_bytes(&repository, Path::new("second.txt")).unwrap(),
            SECOND_ONLY,
            "only the second hunk of the second file should be staged"
        );
    }

    /// The new mode fields exist to keep this true; nothing else pins it.
    #[cfg(unix)]
    #[test]
    fn an_executable_file_keeps_its_mode_through_a_partial_stage() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new();
        let root = fixture.directory("executable-hunk");
        let repository = initialize_repository(&root);
        fs::write(root.join("run.sh"), OLD).unwrap();
        fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        commit_all(&repository, "prepare executable");
        fs::write(root.join("run.sh"), NEW).unwrap();
        fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let file = named(&files, Path::new("run.sh"));

        service
            .stage_hunks(
                &[HunkSelection::new(file, &file.hunks[0])],
                &Cancellation::default(),
            )
            .unwrap();

        assert_eq!(
            index_mode(&repository, Path::new("run.sh")),
            Some(0o100_755),
            "hunk staging dropped the executable bit"
        );
        assert_eq!(
            index_bytes(&repository, Path::new("run.sh")).unwrap(),
            FIRST_ONLY
        );
    }

    /// A deletion and an untracked addition each have exactly one hunk, so
    /// selecting it moves the whole file. Both render a `/dev/null` side.
    #[test]
    fn whole_file_records_stage_and_unstage_through_their_only_hunk() {
        let fixture = Fixture::new();
        let root = fixture.directory("whole-file-hunk");
        let repository = initialize_repository(&root);
        fs::write(root.join("gone.txt"), OLD).unwrap();
        commit_all(&repository, "prepare whole-file records");
        fs::remove_file(root.join("gone.txt")).unwrap();
        // Deliberately unlike the deleted file: identical content would be
        // paired as a rename and neither record would be a whole-file one.
        fs::write(root.join("fresh.txt"), b"brand new content\n").unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let gone = named(&files, Path::new("gone.txt"));
        let fresh = named(&files, Path::new("fresh.txt"));
        assert_eq!(gone.change, FileChange::Deleted);
        assert_eq!(fresh.change, FileChange::Untracked);

        service
            .stage_hunks(
                &[
                    HunkSelection::new(gone, &gone.hunks[0]),
                    HunkSelection::new(fresh, &fresh.hunks[0]),
                ],
                &Cancellation::default(),
            )
            .unwrap();

        assert!(index_bytes(&repository, Path::new("gone.txt")).is_none());
        assert_eq!(
            index_bytes(&repository, Path::new("fresh.txt")).unwrap(),
            b"brand new content\n"
        );

        let staged = service
            .diff(DiffTarget::Staged, &DiffOptions::default())
            .unwrap();
        let staged_fresh = named(&staged, Path::new("fresh.txt"));
        service
            .unstage_hunks(
                &[HunkSelection::new(staged_fresh, &staged_fresh.hunks[0])],
                &Cancellation::default(),
            )
            .unwrap();

        assert!(index_bytes(&repository, Path::new("fresh.txt")).is_none());
        assert!(
            root.join("fresh.txt").exists(),
            "unstaging removed a working-tree file"
        );
    }

    #[test]
    fn whole_file_records_also_support_partial_line_application() {
        let fixture = Fixture::new();
        let root = fixture.directory("whole-file-lines");
        let repository = initialize_repository(&root);
        fs::write(root.join("gone.txt"), b"one\ntwo\nthree\n").unwrap();
        commit_all(&repository, "prepare whole-file lines");
        fs::remove_file(root.join("gone.txt")).unwrap();
        fs::write(root.join("fresh.txt"), b"alpha\nbeta\ngamma\n").unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let gone = named(&files, Path::new("gone.txt"));
        let fresh = named(&files, Path::new("fresh.txt"));
        let gone_line = changed_line(&gone.hunks[0], DiffLineKind::Deletion, b"two\n");
        let fresh_line = changed_line(&fresh.hunks[0], DiffLineKind::Addition, b"beta\n");

        service
            .stage_lines(
                &[
                    LineSelection::new(gone, &gone.hunks[0], gone_line),
                    LineSelection::new(fresh, &fresh.hunks[0], fresh_line),
                ],
                &Cancellation::default(),
            )
            .unwrap();

        assert_eq!(
            index_bytes(&repository, Path::new("gone.txt")).unwrap(),
            b"one\nthree\n"
        );
        assert_eq!(
            index_bytes(&repository, Path::new("fresh.txt")).unwrap(),
            b"beta\n"
        );
        assert!(!root.join("gone.txt").exists());
        assert_eq!(
            fs::read(root.join("fresh.txt")).unwrap(),
            b"alpha\nbeta\ngamma\n"
        );
    }

    #[test]
    fn whole_file_records_also_support_partial_line_unstaging() {
        let fixture = Fixture::new();
        let root = fixture.directory("whole-file-line-unstaging");
        let repository = initialize_repository(&root);
        fs::write(root.join("gone.txt"), b"one\ntwo\nthree\n").unwrap();
        commit_all(&repository, "prepare whole-file line unstaging");
        fs::remove_file(root.join("gone.txt")).unwrap();
        fs::write(root.join("fresh.txt"), b"alpha\nbeta\ngamma\n").unwrap();
        git(&root, ["add", "--all"]);
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Staged, &DiffOptions::default())
            .unwrap();
        let gone = named(&files, Path::new("gone.txt"));
        let fresh = named(&files, Path::new("fresh.txt"));
        let gone_line = changed_line(&gone.hunks[0], DiffLineKind::Deletion, b"two\n");
        let fresh_line = changed_line(&fresh.hunks[0], DiffLineKind::Addition, b"beta\n");

        service
            .unstage_lines(
                &[
                    LineSelection::new(gone, &gone.hunks[0], gone_line),
                    LineSelection::new(fresh, &fresh.hunks[0], fresh_line),
                ],
                &Cancellation::default(),
            )
            .unwrap();

        assert_eq!(
            index_bytes(&repository, Path::new("gone.txt")).unwrap(),
            b"two\n"
        );
        assert_eq!(
            index_bytes(&repository, Path::new("fresh.txt")).unwrap(),
            b"alpha\ngamma\n"
        );
        assert!(!root.join("gone.txt").exists());
        assert_eq!(
            fs::read(root.join("fresh.txt")).unwrap(),
            b"alpha\nbeta\ngamma\n"
        );
    }

    /// Libgit2 owns the `crlf` filter, so its diff already reports normalized
    /// lines and the apply must keep the index free of carriage returns.
    #[test]
    fn crlf_normalization_survives_hunk_staging() {
        let crlf = |bytes: &[u8]| {
            String::from_utf8(bytes.to_vec())
                .unwrap()
                .replace('\n', "\r\n")
                .into_bytes()
        };
        let fixture = Fixture::new();
        let root = fixture.directory("crlf-hunk");
        let repository = initialize_repository(&root);
        repository
            .config()
            .unwrap()
            .set_bool("core.autocrlf", true)
            .unwrap();
        fs::write(root.join("crlf.txt"), crlf(OLD)).unwrap();
        git(&root, ["add", "--", "crlf.txt"]);
        git(&root, ["commit", "-m", "crlf baseline"]);
        fs::write(root.join("crlf.txt"), crlf(NEW)).unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let file = named(&files, Path::new("crlf.txt"));

        service
            .stage_hunks(
                &[HunkSelection::new(file, &file.hunks[0])],
                &Cancellation::default(),
            )
            .unwrap();

        assert_eq!(
            index_bytes(&repository, Path::new("crlf.txt")).unwrap(),
            FIRST_ONLY,
            "the index gained carriage returns"
        );
        assert_eq!(fs::read(root.join("crlf.txt")).unwrap(), crlf(NEW));
    }

    #[test]
    fn an_edited_rename_can_round_trip_one_content_hunk() {
        let fixture = Fixture::new();
        let root = fixture.directory("edited-rename-hunk");
        let repository = initialize_repository(&root);
        fs::write(root.join("old.txt"), OLD).unwrap();
        commit_all(&repository, "prepare edited rename");
        fs::rename(root.join("old.txt"), root.join("new.txt")).unwrap();
        fs::write(root.join("new.txt"), NEW).unwrap();
        let worktree_before = fs::read(root.join("new.txt")).unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let rename = files
            .iter()
            .find(|file| {
                file.old_path.as_deref() == Some(Path::new("old.txt"))
                    && file.new_path.as_deref() == Some(Path::new("new.txt"))
            })
            .unwrap();
        assert_eq!(rename.change, FileChange::Renamed);
        service
            .stage_hunks(
                &[HunkSelection::new(rename, &rename.hunks[0])],
                &Cancellation::default(),
            )
            .unwrap();
        let repository_after_stage = Repository::open(&root).unwrap();
        assert!(index_bytes(&repository_after_stage, Path::new("old.txt")).is_none());
        assert_eq!(
            index_bytes(&repository_after_stage, Path::new("new.txt")).unwrap(),
            FIRST_ONLY
        );
        let staged = service
            .diff(DiffTarget::Staged, &DiffOptions::default())
            .unwrap();
        let staged_rename = staged
            .iter()
            .find(|file| file.change == FileChange::Renamed)
            .unwrap();

        service
            .unstage_hunks(
                &[HunkSelection::new(staged_rename, &staged_rename.hunks[0])],
                &Cancellation::default(),
            )
            .unwrap();

        let repository_after_unstage = Repository::open(&root).unwrap();
        assert_eq!(
            index_bytes(&repository_after_unstage, Path::new("old.txt")).unwrap(),
            OLD
        );
        assert!(index_bytes(&repository_after_unstage, Path::new("new.txt")).is_none());
        assert_eq!(fs::read(root.join("new.txt")).unwrap(), worktree_before);
        assert!(!root.join("old.txt").exists());
    }

    /// Revalidation restricts its diff to the selection's own paths, but
    /// `diff::compute` still detects renames across the whole tree first. If
    /// that order ever changes, the unrelated rename below starts pairing
    /// differently and this stage stops matching its selection.
    #[test]
    fn an_unrelated_rename_does_not_disturb_a_narrowed_revalidation() {
        let fixture = Fixture::new();
        let root = fixture.directory("unrelated-rename-hunk");
        let repository = initialize_repository(&root);
        fs::write(root.join("edited.txt"), OLD).unwrap();
        fs::write(root.join("moved.txt"), b"moved content\n").unwrap();
        commit_all(&repository, "prepare unrelated rename");
        fs::write(root.join("edited.txt"), NEW).unwrap();
        fs::rename(root.join("moved.txt"), root.join("elsewhere.txt")).unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let edited = named(&files, Path::new("edited.txt"));

        service
            .stage_hunks(
                &[HunkSelection::new(edited, &edited.hunks[0])],
                &Cancellation::default(),
            )
            .unwrap();

        assert_eq!(
            index_bytes(&repository, Path::new("edited.txt")).unwrap(),
            FIRST_ONLY
        );
        assert_eq!(
            index_bytes(&repository, Path::new("moved.txt")).unwrap(),
            b"moved content\n",
            "the unrelated rename was staged as a side effect"
        );
    }

    /// Both staging granularities must leave a caller's view of the repository
    /// in the same shape, and both must be able to skip that work.
    #[test]
    fn a_hunk_batch_refreshes_status_unless_it_is_asked_not_to() {
        let fixture = Fixture::new();
        let root = fixture.directory("status-hunk");
        let repository = initialize_repository(&root);
        fs::write(root.join("tracked.txt"), OLD).unwrap();
        commit_all(&repository, "prepare status refresh");
        fs::write(root.join("tracked.txt"), NEW).unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let file = named(&files, Path::new("tracked.txt"));

        assert!(!fixture.data_dir.exists(), "a read-only diff took a lock");

        let outcome = service
            .stage_hunks(
                &[HunkSelection::new(file, &file.hunks[0])],
                &Cancellation::default(),
            )
            .unwrap();

        assert!(
            fixture.data_dir.exists(),
            "an index write skipped the repository lock"
        );
        let StatusRefreshOutcome::Refreshed(status) = outcome.status else {
            panic!("default hunk options should refresh status: {:?}", outcome);
        };
        let entry = status
            .entries
            .iter()
            .find(|entry| entry.path == Path::new("tracked.txt"))
            .unwrap();
        assert_eq!(entry.staged, Some(FileChange::Modified));
        assert_eq!(entry.unstaged, Some(FileChange::Modified));

        let skipped = service
            .stage_hunks_with_options(
                &[],
                &StageOptions {
                    refresh_status: false,
                },
                &Cancellation::default(),
            )
            .unwrap();
        assert!(matches!(skipped.status, StatusRefreshOutcome::Skipped));
    }

    #[test]
    fn non_utf8_content_is_applied_as_raw_bytes() {
        let fixture = Fixture::new();
        let root = fixture.directory("byte-content-hunk");
        let repository = initialize_repository(&root);
        let old = b"one\nold-\xff\nthree\n";
        let new = b"one\nnew-\xfe\nthree\n";
        fs::write(root.join("bytes.txt"), old).unwrap();
        commit_all(&repository, "prepare byte content");
        fs::write(root.join("bytes.txt"), new).unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let file = named(&files, Path::new("bytes.txt"));

        service
            .stage_hunks(
                &[HunkSelection::new(file, &file.hunks[0])],
                &Cancellation::default(),
            )
            .unwrap();

        assert_eq!(
            index_bytes(&repository, Path::new("bytes.txt")).unwrap(),
            new
        );
        assert_eq!(fs::read(root.join("bytes.txt")).unwrap(), new);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_non_utf8_path_stages_without_a_string_round_trip() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt, path::PathBuf};

        let fixture = Fixture::new();
        let root = fixture.directory("byte-path-hunk");
        let repository = initialize_repository(&root);
        let path = PathBuf::from(OsStr::from_bytes(b"hunk-\xff.txt"));
        fs::write(root.join(&path), OLD).unwrap();
        commit_all(&repository, "add byte path");
        fs::write(root.join(&path), NEW).unwrap();
        let worktree_before = fs::read(root.join(&path)).unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(
                DiffTarget::Unstaged,
                &DiffOptions::default().with_paths([&path]),
            )
            .unwrap();
        let file = named(&files, &path);

        service
            .stage_hunks(
                &[HunkSelection::new(file, &file.hunks[0])],
                &Cancellation::default(),
            )
            .unwrap();

        assert_eq!(index_bytes(&repository, &path).unwrap(), FIRST_ONLY);
        assert_eq!(fs::read(root.join(&path)).unwrap(), worktree_before);
    }

    fn named<'a>(files: &'a [FileDiff], path: &Path) -> &'a FileDiff {
        files
            .iter()
            .find(|file| {
                file.old_path.as_deref() == Some(path) || file.new_path.as_deref() == Some(path)
            })
            .unwrap_or_else(|| panic!("no diff for '{}' in {files:#?}", path.display()))
    }

    fn changed_line<'a>(hunk: &'a Hunk, kind: DiffLineKind, content: &[u8]) -> &'a DiffLine {
        hunk.lines
            .iter()
            .find(|line| line.kind == kind && line.content == content)
            .unwrap_or_else(|| {
                panic!(
                    "no {kind:?} line {:?} in {hunk:#?}",
                    String::from_utf8_lossy(content)
                )
            })
    }

    fn assert_patch_header_matches_body(patch: &[u8]) {
        let patch = String::from_utf8(patch.to_vec()).unwrap();
        let mut lines = patch.lines();
        let header = lines
            .find(|line| line.starts_with("@@ "))
            .unwrap_or_else(|| panic!("patch has no hunk header:\n{patch}"));
        let mut fields = header.split_whitespace();
        assert_eq!(fields.next(), Some("@@"));
        let old = fields.next().unwrap();
        let new = fields.next().unwrap();
        let declared_old = old.split_once(',').unwrap().1.parse::<usize>().unwrap();
        let declared_new = new.split_once(',').unwrap().1.parse::<usize>().unwrap();
        let body = lines
            .take_while(|line| !line.starts_with("@@ ") && !line.starts_with("diff --git "))
            .collect::<Vec<_>>();
        let actual_old = body
            .iter()
            .filter(|line| line.starts_with(' ') || line.starts_with('-'))
            .count();
        let actual_new = body
            .iter()
            .filter(|line| line.starts_with(' ') || line.starts_with('+'))
            .count();
        assert_eq!(declared_old, actual_old, "old count in patch:\n{patch}");
        assert_eq!(declared_new, actual_new, "new count in patch:\n{patch}");
    }

    fn index_bytes(repository: &Repository, path: &Path) -> Option<Vec<u8>> {
        let repository = Repository::open(repository.workdir().unwrap()).unwrap();
        let index = repository.index().unwrap();
        let entry = index.get_path(path, 0)?;
        Some(repository.find_blob(entry.id).unwrap().content().to_vec())
    }

    fn index_mode(repository: &Repository, path: &Path) -> Option<u32> {
        let repository = Repository::open(repository.workdir().unwrap()).unwrap();
        let index = repository.index().unwrap();
        Some(index.get_path(path, 0)?.mode)
    }

    fn fake_hunk() -> Hunk {
        Hunk {
            old_start: 1,
            old_lines: 1,
            new_start: 1,
            new_lines: 1,
            header: b"@@ -1 +1 @@\n".to_vec(),
            intra_line_degradation: None,
            lines: Vec::new(),
        }
    }

    fn fake_line() -> DiffLine {
        DiffLine {
            kind: DiffLineKind::Addition,
            old_line_number: None,
            new_line_number: Some(1),
            content: b"fake\n".to_vec(),
            paired_line_index: None,
            intra_line_ranges: None,
        }
    }

    /// A minimal record for exercising the change-kind guard directly, because
    /// copy detection and merge conflicts cannot be produced through the diff
    /// service as it is configured today.
    fn record(change: FileChange) -> FileDiff {
        FileDiff {
            target: DiffTarget::Unstaged,
            change,
            old_path: Some(PathBuf::from("old.txt")),
            new_path: Some(PathBuf::from("new.txt")),
            old_blob_id: "0".repeat(40),
            new_blob_id: "1".repeat(40),
            old_mode: 0o100_644,
            new_mode: 0o100_644,
            context_lines: 3,
            old_size: 1,
            new_size: 1,
            binary: false,
            omission: None::<DiffOmission>,
            hunks: vec![fake_hunk()],
        }
    }
}
