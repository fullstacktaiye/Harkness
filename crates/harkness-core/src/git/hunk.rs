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

use crate::git::{
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

#[derive(Clone, Copy)]
enum Direction {
    Forward,
    Reverse,
}

struct PreparedFile {
    file: FileDiff,
    hunks: Vec<Hunk>,
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
        DiffTarget::Unstaged,
        Direction::Forward,
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
        DiffTarget::Staged,
        Direction::Reverse,
        options,
        cancellation,
    )
}

fn mutate(
    git_executable: &Path,
    root: &Path,
    selections: &[HunkSelection],
    target: DiffTarget,
    direction: Direction,
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
        let prepared = prepare(root, selections, &paths, &target, cancellation)?;
        hunks = prepared.iter().map(|file| file.hunks.len()).sum();
        apply(&repository, &prepared, &paths, direction)?;
    }
    Ok(HunkStageOutcome {
        hunks,
        status: commit::refresh_status(git_executable, root, options.refresh_status, cancellation),
    })
}

/// Every distinct path a batch names, on either side.
fn selection_paths(selections: &[HunkSelection]) -> Vec<PathBuf> {
    let mut paths = selections
        .iter()
        .flat_map(|selection| [&selection.old_path, &selection.new_path])
        .filter_map(Option::as_ref)
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
        refuse_unsupported(file)?;

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
            if !existing.hunks.iter().any(|existing| existing == hunk) {
                existing.hunks.push(hunk.clone());
            }
        } else {
            prepared.push(PreparedFile {
                file: file.clone(),
                hunks: vec![hunk.clone()],
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
fn refuse_unsupported(file: &FileDiff) -> Result<(), GitError> {
    let path = display_path(file.new_path.as_deref(), file.old_path.as_deref()).to_path_buf();
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
fn refuse_overlaps(prepared: &[PreparedFile]) -> Result<(), GitError> {
    for entry in prepared {
        for (index, hunk) in entry.hunks.iter().enumerate() {
            for other in &entry.hunks[index + 1..] {
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
) -> Result<(), GitError> {
    let patch = render_patch(prepared, direction);
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
    repository
        .apply(&parsed, ApplyLocation::Index, Some(&mut check))
        .map_err(failure)?;
    repository
        .apply(&parsed, ApplyLocation::Index, None)
        .map_err(failure)
}

fn coordinates_match(selection: &HunkSelection, hunk: &Hunk) -> bool {
    selection.old_start == hunk.old_start
        && selection.old_lines == hunk.old_lines
        && selection.new_start == hunk.new_start
        && selection.new_lines == hunk.new_lines
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

fn render_patch(files: &[PreparedFile], direction: Direction) -> Vec<u8> {
    let mut patch = Vec::new();
    for prepared in files {
        let mut hunks = prepared.hunks.iter().collect::<Vec<_>>();
        hunks.sort_by_key(|hunk| match direction {
            Direction::Forward => (hunk.old_start, hunk.new_start),
            Direction::Reverse => (hunk.new_start, hunk.old_start),
        });
        render_file(&mut patch, &prepared.file, &hunks, direction);
    }
    patch
}

fn render_file(patch: &mut Vec<u8>, file: &FileDiff, hunks: &[&Hunk], direction: Direction) {
    let (old_path, new_path, old_id, new_id, old_mode, new_mode) = match direction {
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

    for hunk in hunks {
        render_hunk(patch, hunk, direction);
    }
}

fn render_hunk(patch: &mut Vec<u8>, hunk: &Hunk, direction: Direction) {
    let (old_start, old_lines, new_start, new_lines) = match direction {
        Direction::Forward => (
            hunk.old_start,
            hunk.old_lines,
            hunk.new_start,
            hunk.new_lines,
        ),
        Direction::Reverse => (
            hunk.new_start,
            hunk.new_lines,
            hunk.old_start,
            hunk.old_lines,
        ),
    };
    patch.extend_from_slice(
        format!("@@ -{old_start},{old_lines} +{new_start},{new_lines} @@\n").as_bytes(),
    );

    match direction {
        Direction::Forward => {
            for line in &hunk.lines {
                push_line(patch, line, direction);
            }
        }
        // Flipping signs in place would emit an addition, the no-newline
        // marker that qualifies it, and only then the matching deletion.
        // Libgit2's patch parser rejects exactly that shape, so each run of
        // changed lines is re-emitted deletions first: the order libgit2's own
        // printer produces, and the order its parser expects to read back.
        Direction::Reverse => {
            let groups = line_groups(&hunk.lines);
            let mut run = Vec::new();
            for group in &groups {
                if matches!(group.kind, DiffLineKind::Context) {
                    push_reversed_run(patch, &mut run);
                    push_group(patch, group, direction);
                } else {
                    run.push(group);
                }
            }
            push_reversed_run(patch, &mut run);
        }
    }
}

/// One diff line together with the end-of-file markers that qualify it.
///
/// Libgit2 emits a `\ No newline at end of file` record immediately after the
/// line it describes, so reordering must move the two together.
struct LineGroup<'lines> {
    /// The kind of the leading line, before any direction flip.
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

/// Emits one reversed run of changed lines with its deletions leading.
fn push_reversed_run(patch: &mut Vec<u8>, run: &mut Vec<&LineGroup<'_>>) {
    // Reversal turns an addition into a deletion, so additions lead here.
    let (deletions, additions): (Vec<_>, Vec<_>) = run
        .iter()
        .partition(|group| group.kind == DiffLineKind::Addition);
    for group in deletions.into_iter().chain(additions) {
        push_group(patch, group, Direction::Reverse);
    }
    run.clear();
}

fn push_group(patch: &mut Vec<u8>, group: &LineGroup<'_>, direction: Direction) {
    for line in group.lines {
        push_line(patch, line, direction);
    }
}

fn push_line(patch: &mut Vec<u8>, line: &DiffLine, direction: Direction) {
    match reversed_kind(line.kind, direction) {
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
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use git2::Repository;

    use super::{HunkSelection, refuse_unsupported};
    use crate::{
        git::{
            Cancellation, CommitOptions, DiffOmission, DiffOptions, DiffTarget, FileChange,
            FileDiff, GitError, GitService, Hunk, StageOptions, StatusRefreshOutcome,
        },
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
            let error = refuse_unsupported(&record(change)).unwrap_err();
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
                refuse_unsupported(&record(change)).is_ok(),
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
