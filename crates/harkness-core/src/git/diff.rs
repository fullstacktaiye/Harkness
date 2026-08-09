//! Structured, byte-preserving repository content diffs.
//!
//! The index is the boundary here just as it is in [`super::DetailedStatus`]:
//! staged content is `HEAD` to index, and unstaged content is index to working
//! tree. This module deliberately uses libgit2 only. A diff is local,
//! read-only inspection and must neither acquire the repository lock nor spawn
//! system Git.

use std::{
    ffi::OsStr,
    fs,
    path::{Component, Path, PathBuf},
};

use git2::{
    Delta, Diff, DiffFindOptions, DiffLineType as GitDiffLineType, DiffOptions as GitDiffOptions,
    ErrorCode, FileMode, ObjectType, Oid, Patch, Repository,
};

use crate::git::{FileChange, GitError, commit};

/// The default largest file whose content Harkness will put in a diff model.
pub const DEFAULT_MAX_DIFF_FILE_SIZE: u64 = 1024 * 1024;

/// The default budget for all hunk content in one diff model.
///
/// A per-file bound alone does not bound a response: a tree of files that are
/// each individually small still renders an unbounded model, and a generous
/// context setting multiplies every one of them. This caps the whole batch.
///
/// It is deliberately far below what a machine could hold. Raw content is the
/// cheapest thing a caller pays for: every line becomes its own allocation, and
/// then its own object in a serialised projection, so peak memory runs an order
/// of magnitude above the figure counted here. A caller that genuinely wants
/// more can raise it and will see [`DiffOmission::ContentBudgetExhausted`]
/// saying so, which is the outcome to prefer over a diff nobody can render.
pub const DEFAULT_MAX_DIFF_TOTAL_BYTES: u64 = 8 * 1024 * 1024;

/// The default number of files whose content one diff model will carry.
pub const DEFAULT_MAX_DIFF_FILES: usize = 5_000;

/// The default number of unchanged lines surrounding each hunk.
pub const DEFAULT_DIFF_CONTEXT_LINES: u32 = 3;

/// Which side of the index to inspect.
///
/// This enum is intentionally non-exhaustive: revision and merge-base targets
/// can be added without changing the file and hunk contracts.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DiffTarget {
    /// Compare `HEAD` with the index, using the empty tree for an unborn HEAD.
    Staged,
    /// Compare the index with the working tree, including untracked files.
    Unstaged,
}

/// Bounds and optional path selection for one diff.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct DiffOptions {
    /// The largest old or new file, in bytes, whose hunks will be returned.
    /// Defaults to one mebibyte.
    pub max_file_size: u64,
    /// The combined budget, in bytes, for hunk content across the whole model.
    /// Files reached after it is spent keep their identity record and report
    /// [`DiffOmission::ContentBudgetExhausted`] instead of hunks.
    pub max_total_bytes: u64,
    /// The number of files whose content the model will carry. Later files
    /// report [`DiffOmission::FileBudgetExhausted`] rather than disappearing.
    pub max_files: usize,
    /// The number of unchanged lines surrounding each hunk.
    pub context_lines: u32,
    /// Literal paths to inspect. An empty list selects the whole tree.
    pub paths: Vec<PathBuf>,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            max_file_size: DEFAULT_MAX_DIFF_FILE_SIZE,
            max_total_bytes: DEFAULT_MAX_DIFF_TOTAL_BYTES,
            max_files: DEFAULT_MAX_DIFF_FILES,
            context_lines: DEFAULT_DIFF_CONTEXT_LINES,
            paths: Vec::new(),
        }
    }
}

impl DiffOptions {
    /// Removes every size and count bound.
    ///
    /// Revalidation in [`super::hunk`] must see a superset of whatever a caller
    /// diffed, so a file the caller could legitimately select from is never
    /// omitted merely for exceeding that caller's own display budget.
    #[must_use]
    pub fn unbounded() -> Self {
        Self::default()
            .with_max_file_size(u64::MAX)
            .with_max_total_bytes(u64::MAX)
            .with_max_files(usize::MAX)
    }

    /// Sets the largest file whose content will be returned.
    #[must_use]
    pub fn with_max_file_size(mut self, max_file_size: u64) -> Self {
        self.max_file_size = max_file_size;
        self
    }

    /// Sets the combined hunk-content budget for the whole model.
    #[must_use]
    pub fn with_max_total_bytes(mut self, max_total_bytes: u64) -> Self {
        self.max_total_bytes = max_total_bytes;
        self
    }

    /// Sets how many files carry content before the rest are named only.
    #[must_use]
    pub fn with_max_files(mut self, max_files: usize) -> Self {
        self.max_files = max_files;
        self
    }

    /// Sets the number of unchanged lines surrounding each hunk.
    #[must_use]
    pub fn with_context_lines(mut self, context_lines: u32) -> Self {
        self.context_lines = context_lines;
        self
    }

    /// Restricts the diff to literal repository paths.
    #[must_use]
    pub fn with_paths<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        self.paths = paths
            .into_iter()
            .map(|path| path.as_ref().to_path_buf())
            .collect();
        self
    }
}

/// Why a changed file has no content hunks.
///
/// Every reason a file's content is missing is named here rather than reported
/// by failing the whole diff. One delta Harkness cannot project must not cost a
/// caller the other thousand it can, so inspection is total: the file list is
/// always complete and each entry says for itself why it carries no hunks.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DiffOmission {
    /// At least one side exceeded [`DiffOptions::max_file_size`].
    FileTooLarge { limit: u64 },
    /// The path is an unresolved merge conflict. The index holds stage 1, 2 and
    /// 3 entries and no single resolved blob, so there is no two-sided content
    /// comparison to render. Resolve the path, or inspect the staged side.
    Unmerged,
    /// [`DiffOptions::max_total_bytes`] was spent before this file was reached.
    ContentBudgetExhausted { limit: u64 },
    /// [`DiffOptions::max_files`] was reached before this file.
    FileBudgetExhausted { limit: usize },
    /// Libgit2 described this delta in a shape the file contract cannot carry.
    /// The detail names what was unexpected; the record itself stays valid.
    Unrepresentable { detail: String },
}

/// One changed file on one side of the index.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct FileDiff {
    /// The comparison that produced this file.
    pub target: DiffTarget,
    /// How the path changed across the comparison.
    pub change: FileChange,
    /// The path on the old side, absent for an addition.
    pub old_path: Option<PathBuf>,
    /// The path on the new side, absent for a deletion.
    pub new_path: Option<PathBuf>,
    /// The old blob object ID, or the all-zero ID when the side is absent.
    pub old_blob_id: String,
    /// The new blob object ID, or the all-zero ID when the side is absent.
    pub new_blob_id: String,
    /// Git file mode of the old side, or zero when that side is absent.
    pub old_mode: u32,
    /// Git file mode of the new side, or zero when that side is absent.
    pub new_mode: u32,
    /// Context-line count used to form this file's hunks.
    ///
    /// This echoes [`DiffOptions::context_lines`] rather than describing the
    /// file, because hunk coordinates only mean anything alongside the setting
    /// that produced them. It is recorded per file so a selection taken from
    /// one record stays self-describing after the surrounding list is dropped.
    pub context_lines: u32,
    /// Byte size of the old side, zero when absent.
    pub old_size: u64,
    /// Byte size of the new side, zero when absent.
    pub new_size: u64,
    /// Whether Git classified inspected content as binary. An oversized file
    /// uses [`Self::omission`] instead, even if its bytes are also binary.
    pub binary: bool,
    /// A named reason content was intentionally omitted.
    pub omission: Option<DiffOmission>,
    /// Text hunks. Binary and omitted files always leave this empty.
    pub hunks: Vec<Hunk>,
}

/// One unified-diff hunk.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct Hunk {
    /// First old-side line covered by the hunk.
    pub old_start: u32,
    /// Number of old-side lines covered by the hunk.
    pub old_lines: u32,
    /// First new-side line covered by the hunk.
    pub new_start: u32,
    /// Number of new-side lines covered by the hunk.
    pub new_lines: u32,
    /// The raw `@@ ... @@` header, including any function context.
    pub header: Vec<u8>,
    /// Lines in patch order, retained byte-for-byte.
    pub lines: Vec<DiffLine>,
}

/// The role of one raw line in a hunk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DiffLineKind {
    Context,
    Addition,
    Deletion,
    /// The raw no-newline marker when both sides have an unterminated EOF.
    BothEofNoNewline,
    /// The raw no-newline marker when only the old side has an unterminated EOF.
    OldEofNoNewline,
    /// The raw no-newline marker when only the new side has an unterminated EOF.
    NewEofNoNewline,
}

/// One byte-preserving line in a hunk.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct DiffLine {
    pub kind: DiffLineKind,
    /// Old-side line number, when libgit2 associates one with this record.
    pub old_line_number: Option<u32>,
    /// New-side line number, when libgit2 associates one with this record.
    pub new_line_number: Option<u32>,
    /// Raw line bytes. A trailing newline is retained when one exists.
    pub content: Vec<u8>,
}

pub(crate) fn compute(
    root: &Path,
    target: DiffTarget,
    options: &DiffOptions,
) -> Result<Vec<FileDiff>, GitError> {
    compute_targets(root, std::slice::from_ref(&target), options)
}

/// Computes several targets against one repository and index snapshot.
///
/// Every target is read from the same open index, so a combined staged and
/// unstaged model describes one moment. Computing the sides separately would
/// let a concurrent index write land between them and produce a response that
/// duplicates or drops a change while looking internally consistent.
pub(crate) fn compute_targets(
    root: &Path,
    targets: &[DiffTarget],
    options: &DiffOptions,
) -> Result<Vec<FileDiff>, GitError> {
    commit::validate_paths(root, &options.paths)?;
    let selected_paths = selected_paths(root, &options.paths);
    let repository = commit::open(root)?;
    let index = repository
        .index()
        .map_err(|source| inspection(root, source))?;

    let mut budget = Budget::new(options);
    let mut files = Vec::new();
    for target in targets {
        collect_target(
            &repository,
            &index,
            root,
            target,
            options,
            &selected_paths,
            &mut budget,
            &mut files,
        )?;
    }
    Ok(files)
}

/// What remains of the whole-model content allowance.
struct Budget {
    remaining_files: usize,
    remaining_bytes: u64,
    max_files: usize,
    max_total_bytes: u64,
}

impl Budget {
    fn new(options: &DiffOptions) -> Self {
        Self {
            remaining_files: options.max_files,
            remaining_bytes: options.max_total_bytes,
            max_files: options.max_files,
            max_total_bytes: options.max_total_bytes,
        }
    }

    /// The reason this file must be named without content, if any.
    fn exhausted(&self) -> Option<DiffOmission> {
        if self.remaining_files == 0 {
            return Some(DiffOmission::FileBudgetExhausted {
                limit: self.max_files,
            });
        }
        if self.remaining_bytes == 0 {
            return Some(DiffOmission::ContentBudgetExhausted {
                limit: self.max_total_bytes,
            });
        }
        None
    }

    fn spend(&mut self, bytes: u64) {
        self.remaining_files = self.remaining_files.saturating_sub(1);
        self.remaining_bytes = self.remaining_bytes.saturating_sub(bytes);
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "one snapshot is threaded through rather than reopened per target"
)]
fn collect_target(
    repository: &Repository,
    index: &git2::Index,
    root: &Path,
    target: &DiffTarget,
    options: &DiffOptions,
    selected_paths: &[PathBuf],
    budget: &mut Budget,
    files: &mut Vec<FileDiff>,
) -> Result<(), GitError> {
    let mut native_options = GitDiffOptions::new();
    native_options
        .context_lines(options.context_lines)
        .include_typechange(true)
        // Supplying the index explicitly prevents libgit2 from refreshing it.
        .update_index(false)
        .max_size(options.max_file_size.min(i64::MAX as u64) as i64);
    if matches!(target, DiffTarget::Unstaged) {
        native_options
            .include_untracked(true)
            .recurse_untracked_dirs(true)
            .show_untracked_content(true);
    }

    let mut diff = match target {
        DiffTarget::Staged => {
            let head_tree = head_tree(repository, root)?;
            repository.diff_tree_to_index(
                head_tree.as_ref(),
                Some(index),
                Some(&mut native_options),
            )
        }
        DiffTarget::Unstaged => {
            repository.diff_index_to_workdir(Some(index), Some(&mut native_options))
        }
    }
    .map_err(|source| inspection(root, source))?;

    let mut find = DiffFindOptions::new();
    find.renames(true);
    if matches!(target, DiffTarget::Unstaged) {
        find.for_untracked(true);
    }
    diff.find_similar(Some(&mut find))
        .map_err(|source| inspection(root, source))?;

    // Rename detection deliberately runs over the whole diff, and
    // `path_selected` narrows the result afterwards, rather than handing
    // `options.paths` to libgit2 as a pathspec. That ordering is load-bearing:
    // `super::hunk` revalidates a selection by recomputing this diff restricted
    // to the selection's own paths, and must see the same rename pairing the
    // caller saw in a whole-tree diff. Filtering before `find_similar` would
    // silently repair against a different pairing.

    for position in 0..diff.deltas().len() {
        let Some(delta) = diff.get_delta(position) else {
            return Err(malformed(format!("diff delta {position} disappeared")));
        };
        let status = delta.status();
        if !path_selected(
            delta.old_file().path(),
            delta.new_file().path(),
            selected_paths,
        ) {
            continue;
        }
        // Neither is ever requested: `include_ignored` and `include_unreadable`
        // stay off, so a match here would be libgit2 volunteering a delta this
        // model has no side to describe.
        if matches!(status, Delta::Unmodified | Delta::Ignored) {
            continue;
        }

        let Some(change) = file_change(status) else {
            files.push(unrepresentable(
                &diff,
                position,
                target,
                options,
                format!("unexpected {status:?} delta"),
            ));
            continue;
        };

        let old_size = resolved_size(repository, &delta.old_file());
        let new_size = resolved_size(repository, &delta.new_file());
        // An unresolved path has stage 1, 2 and 3 index entries and no single
        // resolved blob, so it has no two-sided content comparison to render.
        // It is named and skipped rather than failing the whole inspection.
        let mut omission = if change == FileChange::Unmerged {
            Some(DiffOmission::Unmerged)
        } else if old_size > options.max_file_size || new_size > options.max_file_size {
            Some(DiffOmission::FileTooLarge {
                limit: options.max_file_size,
            })
        } else {
            budget.exhausted()
        };

        let patch = match omission {
            Some(_) => None,
            None => match Patch::from_diff(&diff, position) {
                Ok(patch) => patch,
                Err(source) => {
                    omission = Some(DiffOmission::Unrepresentable {
                        detail: source.message().to_owned(),
                    });
                    None
                }
            },
        };
        // Patch construction performs binary detection and may populate IDs,
        // so reacquire the delta afterward rather than retaining stale flags.
        let delta = match patch.as_ref() {
            Some(patch) => patch.delta(),
            None => match diff.get_delta(position) {
                Some(delta) => delta,
                None => return Err(malformed(format!("diff delta {position} disappeared"))),
            },
        };
        let old_file = &delta.old_file();
        let new_file = &delta.new_file();
        let old_path = old_file.exists().then(|| old_file.path()).flatten();
        let new_path = new_file.exists().then(|| new_file.path()).flatten();
        if old_path.is_none() && old_file.exists() {
            omission.get_or_insert(DiffOmission::Unrepresentable {
                detail: "a present old diff side has no path".to_owned(),
            });
        }
        if new_path.is_none() && new_file.exists() {
            omission.get_or_insert(DiffOmission::Unrepresentable {
                detail: "a present new diff side has no path".to_owned(),
            });
        }
        let new_path = new_path.map(Path::to_path_buf);

        let binary = omission.is_none() && (old_file.is_binary() || new_file.is_binary());
        let mut content_bytes = 0;
        let hunks = match patch.as_ref() {
            Some(patch) if !binary => {
                match collect_hunks(patch, root, budget.remaining_bytes, &mut content_bytes) {
                    Ok(Some(hunks)) => hunks,
                    Ok(None) => {
                        omission = Some(DiffOmission::ContentBudgetExhausted {
                            limit: budget.max_total_bytes,
                        });
                        content_bytes = 0;
                        Vec::new()
                    }
                    Err(source) => {
                        omission = Some(DiffOmission::Unrepresentable {
                            detail: source.to_string(),
                        });
                        content_bytes = 0;
                        Vec::new()
                    }
                }
            }
            Some(_) | None => Vec::new(),
        };
        if !hunks.is_empty() {
            budget.spend(content_bytes);
        }

        let (old_blob_id, old_blob_detail) =
            blob_id(repository, root, target, false, old_file, None);
        let (new_blob_id, new_blob_detail) = blob_id(
            repository,
            root,
            target,
            true,
            new_file,
            new_path.as_deref(),
        );
        for detail in [old_blob_detail, new_blob_detail].into_iter().flatten() {
            omission.get_or_insert(DiffOmission::Unrepresentable { detail });
        }

        files.push(FileDiff {
            target: target.clone(),
            change,
            old_path: old_path.map(Path::to_path_buf),
            new_path,
            old_blob_id,
            new_blob_id,
            old_mode: file_mode(old_file.mode()),
            new_mode: file_mode(new_file.mode()),
            context_lines: options.context_lines,
            old_size,
            new_size,
            binary,
            omission,
            hunks,
        });
    }
    Ok(())
}

/// Names a delta whose status this model cannot classify.
///
/// The identity that is still readable is kept so a caller can see which path
/// was skipped; only the change classification and content are given up.
fn unrepresentable(
    diff: &Diff<'_>,
    position: usize,
    target: &DiffTarget,
    options: &DiffOptions,
    detail: String,
) -> FileDiff {
    let delta = diff.get_delta(position);
    let side = |new_side: bool| {
        delta.as_ref().map(|delta| {
            if new_side {
                delta.new_file()
            } else {
                delta.old_file()
            }
        })
    };
    let old_file = side(false);
    let new_file = side(true);
    let identity = |file: Option<git2::DiffFile<'_>>| {
        file.map_or_else(
            || (None, String::new(), 0),
            |file| {
                (
                    file.path().map(Path::to_path_buf),
                    file.id().to_string(),
                    file_mode(file.mode()),
                )
            },
        )
    };
    let (old_path, old_blob_id, old_mode) = identity(old_file);
    let (new_path, new_blob_id, new_mode) = identity(new_file);
    FileDiff {
        target: target.clone(),
        change: FileChange::Modified,
        old_path,
        new_path,
        old_blob_id,
        new_blob_id,
        old_mode,
        new_mode,
        context_lines: options.context_lines,
        old_size: 0,
        new_size: 0,
        binary: false,
        omission: Some(DiffOmission::Unrepresentable { detail }),
        hunks: Vec::new(),
    }
}

/// The byte size of one diff side, resolving the blob when libgit2 left it zero.
///
/// Libgit2 populates `size` from a stat for a working-tree side and from the
/// index entry for an index side, but leaves a tree side at zero until the blob
/// is loaded. Trusting that zero would silently exempt the whole `HEAD` side of
/// a staged diff from [`DiffOptions::max_file_size`], and an oversized text file
/// would then come back flagged binary by libgit2's own guard instead of being
/// named as too large.
fn resolved_size(repository: &Repository, file: &git2::DiffFile<'_>) -> u64 {
    let size = file.size();
    if size > 0 || !file.exists() || file.mode() == FileMode::Commit {
        return size;
    }
    if !file.is_valid_id() || file.id().is_zero() {
        return size;
    }
    repository
        .find_blob(file.id())
        .map_or(size, |blob| blob.size() as u64)
}

fn head_tree<'repository>(
    repository: &'repository Repository,
    root: &Path,
) -> Result<Option<git2::Tree<'repository>>, GitError> {
    match repository.head() {
        Ok(head) => head
            .peel_to_tree()
            .map(Some)
            .map_err(|source| inspection(root, source)),
        Err(error) if matches!(error.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => {
            Ok(None)
        }
        Err(source) => Err(inspection(root, source)),
    }
}

/// Collects a patch's hunks, stopping once `limit` content bytes are reached.
///
/// `Ok(None)` means the budget ran out. The check happens while lines are read
/// rather than after, so an oversized patch is never fully materialised only to
/// be discarded. `spent` reports the bytes the returned hunks actually hold.
fn collect_hunks(
    patch: &Patch<'_>,
    root: &Path,
    limit: u64,
    spent: &mut u64,
) -> Result<Option<Vec<Hunk>>, GitError> {
    let mut hunks = Vec::with_capacity(patch.num_hunks());
    let mut used: u64 = 0;
    for hunk_index in 0..patch.num_hunks() {
        let (hunk, line_count) = patch
            .hunk(hunk_index)
            .map_err(|source| inspection(root, source))?;
        used = used.saturating_add(hunk.header().len() as u64);
        if used > limit {
            return Ok(None);
        }
        let mut lines = Vec::with_capacity(line_count);
        for line_index in 0..line_count {
            let line = patch
                .line_in_hunk(hunk_index, line_index)
                .map_err(|source| inspection(root, source))?;
            used = used.saturating_add(line.content().len() as u64);
            if used > limit {
                return Ok(None);
            }
            let Some(kind) = line_kind(line.origin_value()) else {
                return Err(malformed(format!(
                    "patch hunk returned a {:?} line",
                    line.origin_value()
                )));
            };
            lines.push(DiffLine {
                kind,
                old_line_number: line.old_lineno(),
                new_line_number: line.new_lineno(),
                content: line.content().to_vec(),
            });
        }
        hunks.push(Hunk {
            old_start: hunk.old_start(),
            old_lines: hunk.old_lines(),
            new_start: hunk.new_start(),
            new_lines: hunk.new_lines(),
            header: hunk.header().to_vec(),
            lines,
        });
    }
    *spent = used;
    Ok(Some(hunks))
}

/// The object ID of one diff side, with a detail when none could be determined.
///
/// A missing ID names the file rather than failing the diff, because the one
/// routine cause is an unresolved merge: a conflicted index entry carries
/// stages 1 to 3 and no stage-0 blob for either side to point at.
fn blob_id(
    repository: &Repository,
    root: &Path,
    target: &DiffTarget,
    new_side: bool,
    file: &git2::DiffFile<'_>,
    path: Option<&Path>,
) -> (String, Option<String>) {
    if !file.exists() || (file.is_valid_id() && !file.id().is_zero()) {
        return (file.id().to_string(), None);
    }
    if matches!(target, DiffTarget::Unstaged) && new_side && file.mode() != FileMode::Commit {
        let Some(path) = path else {
            return (
                file.id().to_string(),
                Some("an existing worktree side has no path".to_owned()),
            );
        };
        return match hash_worktree_file(repository, &root.join(path)) {
            Ok(id) => (id.to_string(), None),
            Err(error) => (file.id().to_string(), Some(error.to_string())),
        };
    }
    (
        file.id().to_string(),
        Some("a present diff side has no valid blob object ID".to_owned()),
    )
}

fn hash_worktree_file(repository: &Repository, path: &Path) -> Result<Oid, GitError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| GitError::DiffContent {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(path).map_err(|source| GitError::DiffContent {
            path: path.to_path_buf(),
            source,
        })?;
        return Oid::hash_object_ext(
            ObjectType::Blob,
            os_str_bytes(target.as_os_str()),
            repository.object_format(),
        )
        .map_err(|source| inspection(path, source));
    }
    Oid::hash_file_ext(ObjectType::Blob, path, repository.object_format())
        .map_err(|source| inspection(path, source))
}

/// Git's octal file mode, or zero when the side is absent.
///
/// Libgit2 models a mode as a signed enum; every value Git can store is
/// positive, so a hypothetical negative discriminant is reported as the same
/// absent-side zero rather than wrapping into a plausible-looking mode.
fn file_mode(mode: FileMode) -> u32 {
    u32::try_from(i32::from(mode)).unwrap_or_default()
}

fn file_change(delta: Delta) -> Option<FileChange> {
    Some(match delta {
        Delta::Added => FileChange::Added,
        Delta::Deleted => FileChange::Deleted,
        Delta::Modified => FileChange::Modified,
        Delta::Renamed => FileChange::Renamed,
        Delta::Copied => FileChange::Copied,
        Delta::Typechange => FileChange::TypeChanged,
        Delta::Conflicted => FileChange::Unmerged,
        Delta::Untracked => FileChange::Untracked,
        Delta::Unmodified | Delta::Ignored | Delta::Unreadable => return None,
    })
}

fn line_kind(kind: GitDiffLineType) -> Option<DiffLineKind> {
    Some(match kind {
        GitDiffLineType::Context => DiffLineKind::Context,
        GitDiffLineType::Addition => DiffLineKind::Addition,
        GitDiffLineType::Deletion => DiffLineKind::Deletion,
        GitDiffLineType::ContextEOFNL => DiffLineKind::BothEofNoNewline,
        GitDiffLineType::AddEOFNL => DiffLineKind::OldEofNoNewline,
        GitDiffLineType::DeleteEOFNL => DiffLineKind::NewEofNoNewline,
        GitDiffLineType::FileHeader | GitDiffLineType::HunkHeader | GitDiffLineType::Binary => {
            return None;
        }
    })
}

fn selected_paths(root: &Path, paths: &[PathBuf]) -> Vec<PathBuf> {
    let canonical_root = fs::canonicalize(root).ok();
    paths
        .iter()
        .map(|path| {
            let relative = if path.is_absolute() {
                path.strip_prefix(root)
                    .ok()
                    .or_else(|| {
                        canonical_root
                            .as_deref()
                            .and_then(|root| path.strip_prefix(root).ok())
                    })
                    .unwrap_or(path)
            } else {
                path.as_path()
            };
            normalize_relative(relative)
        })
        .collect()
}

fn normalize_relative(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(component) => normalized.push(component),
            Component::RootDir | Component::Prefix(_) => {}
        }
    }
    normalized
}

fn path_selected(old: Option<&Path>, new: Option<&Path>, selected: &[PathBuf]) -> bool {
    selected.is_empty()
        || selected.iter().any(|selected| {
            old.is_some_and(|path| path == selected || path.starts_with(selected))
                || new.is_some_and(|path| path == selected || path.starts_with(selected))
        })
}

#[cfg(unix)]
fn os_str_bytes(value: &OsStr) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes()
}

#[cfg(not(unix))]
fn os_str_bytes(value: &OsStr) -> &[u8] {
    value.to_str().unwrap_or_default().as_bytes()
}

fn inspection(path: &Path, source: git2::Error) -> GitError {
    GitError::Inspection {
        path: path.to_path_buf(),
        source,
    }
}

fn malformed(detail: impl Into<String>) -> GitError {
    GitError::MalformedDiff {
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use git2::Repository;

    use super::{DiffLineKind, DiffOmission, DiffOptions, DiffTarget, FileDiff, Hunk};
    use crate::{
        git::{FileChange, GitError, GitService},
        testing::{Fixture, commit_all, configure_commit_identity, git, initialize_repository},
    };

    #[test]
    fn staged_and_further_edited_content_stays_on_its_side_of_the_index() {
        let fixture = Fixture::new();
        let root = fixture.directory("two-sided");
        let repository = initialize_repository(&root);
        fs::write(root.join("tracked.txt"), b"staged bytes\n").unwrap();
        stage(&repository, Path::new("tracked.txt"));
        fs::write(root.join("tracked.txt"), b"unstaged bytes\n").unwrap();
        let service = GitService::new(&root, &fixture.data_dir);

        let staged = service
            .diff(DiffTarget::Staged, &DiffOptions::default())
            .unwrap();
        let unstaged = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();

        assert_eq!(staged.len(), 1);
        assert_eq!(unstaged.len(), 1);
        assert_eq!(added_lines(&staged[0]), vec![b"staged bytes\n".to_vec()]);
        assert_eq!(
            added_lines(&unstaged[0]),
            vec![b"unstaged bytes\n".to_vec()]
        );
        assert_ne!(staged[0].new_blob_id, unstaged[0].new_blob_id);
        assert_eq!(staged[0].new_blob_id, unstaged[0].old_blob_id);
        assert!(!fixture.data_dir.exists(), "a read-only diff took a lock");
    }

    #[test]
    fn an_unborn_head_diffs_the_index_against_the_empty_tree() {
        let fixture = Fixture::new();
        let root = fixture.directory("unborn-diff");
        let repository = Repository::init(&root).unwrap();
        repository.set_head("refs/heads/main").unwrap();
        configure_commit_identity(&repository);
        fs::write(root.join("first.txt"), b"first\n").unwrap();
        fs::write(root.join("second.txt"), b"second\n").unwrap();
        stage(&repository, Path::new("first.txt"));
        stage(&repository, Path::new("second.txt"));

        let files = GitService::new(&root, &fixture.data_dir)
            .diff(DiffTarget::Staged, &DiffOptions::default())
            .unwrap();

        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|file| file.change == FileChange::Added));
        assert!(
            files
                .iter()
                .all(|file| file.old_blob_id.chars().all(|byte| byte == '0'))
        );
        assert!(files.iter().all(|file| !file.hunks.is_empty()));
    }

    #[test]
    fn a_rename_with_an_edit_is_one_record_with_only_content_hunks() {
        let fixture = Fixture::new();
        let root = fixture.directory("rename-diff");
        let repository = initialize_repository(&root);
        fs::write(
            root.join("old name.txt"),
            b"one\ntwo\nthree\nfour\nfive\nsix\nseven\n",
        )
        .unwrap();
        commit_all(&repository, "add rename source");
        fs::rename(root.join("old name.txt"), root.join("new name.txt")).unwrap();
        fs::write(
            root.join("new name.txt"),
            b"one\ntwo\nTHREE\nfour\nfive\nsix\nseven\n",
        )
        .unwrap();
        let mut index = repository.index().unwrap();
        index.remove_path(Path::new("old name.txt")).unwrap();
        index.add_path(Path::new("new name.txt")).unwrap();
        index.write().unwrap();

        let files = GitService::new(&root, &fixture.data_dir)
            .diff(DiffTarget::Staged, &DiffOptions::default())
            .unwrap();

        assert_eq!(files.len(), 1, "{files:#?}");
        assert_eq!(files[0].change, FileChange::Renamed);
        assert_eq!(
            files[0].old_path.as_deref(),
            Some(Path::new("old name.txt"))
        );
        assert_eq!(
            files[0].new_path.as_deref(),
            Some(Path::new("new name.txt"))
        );
        assert_eq!(files[0].hunks.len(), 1);
        assert_eq!(added_lines(&files[0]), vec![b"THREE\n".to_vec()]);
    }

    #[test]
    fn binary_content_reports_sizes_and_no_lines() {
        let fixture = Fixture::new();
        let root = fixture.directory("binary-diff");
        let repository = initialize_repository(&root);
        fs::write(root.join("binary.dat"), b"old\0bytes").unwrap();
        commit_all(&repository, "add binary");
        fs::write(root.join("binary.dat"), b"new\0binary\0bytes").unwrap();

        let files = GitService::new(&root, &fixture.data_dir)
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let binary = named(&files, Path::new("binary.dat"));

        assert!(binary.binary);
        assert_eq!(binary.old_size, 9);
        assert_eq!(binary.new_size, 16);
        assert_eq!(binary.omission, None);
        assert!(binary.hunks.is_empty());
    }

    #[test]
    fn literal_paths_and_non_utf8_bytes_survive_the_diff() {
        let fixture = Fixture::new();
        let root = fixture.directory("byte-diff");
        let repository = initialize_repository(&root);
        fs::write(root.join("space name.txt"), b"old space\n").unwrap();
        fs::write(root.join("-leading.txt"), b"old leading\n").unwrap();

        // Linux filesystems accept arbitrary non-NUL path bytes. Darwin's
        // filesystem APIs reject this exact invalid UTF-8 sequence with
        // `EILSEQ`, so other platforms retain the raw-content half of this
        // regression under an ordinary path.
        #[cfg(target_os = "linux")]
        let byte_path = {
            use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
            std::path::PathBuf::from(OsStr::from_bytes(b"non-utf8-\xff.txt"))
        };
        #[cfg(not(target_os = "linux"))]
        let byte_path = std::path::PathBuf::from("non-utf8.txt");
        fs::write(root.join(&byte_path), b"old-\xff\n").unwrap();
        commit_all(&repository, "add unusual paths");

        fs::write(root.join("space name.txt"), b"new space\n").unwrap();
        fs::write(root.join("-leading.txt"), b"new leading\n").unwrap();
        fs::write(root.join(&byte_path), b"new-\xfe\n").unwrap();
        let options = DiffOptions::default().with_paths([
            Path::new("space name.txt"),
            Path::new("-leading.txt"),
            byte_path.as_path(),
        ]);

        let files = GitService::new(&root, &fixture.data_dir)
            .diff(DiffTarget::Unstaged, &options)
            .unwrap();

        assert_eq!(files.len(), 3, "{files:#?}");
        assert_eq!(
            added_lines(named(&files, &byte_path)),
            vec![b"new-\xfe\n".to_vec()]
        );
        assert!(
            named(&files, Path::new("space name.txt"))
                .old_path
                .is_some()
        );
        assert!(named(&files, Path::new("-leading.txt")).old_path.is_some());
    }

    #[test]
    fn an_oversized_file_is_named_without_hiding_other_files() {
        let fixture = Fixture::new();
        let root = fixture.directory("bounded-diff");
        let repository = initialize_repository(&root);
        fs::write(root.join("large.txt"), b"old\n").unwrap();
        fs::write(root.join("small.txt"), b"old\n").unwrap();
        commit_all(&repository, "add bounded files");
        fs::write(root.join("large.txt"), b"0123456789abcdef\n").unwrap();
        fs::write(root.join("small.txt"), b"new\n").unwrap();
        let options = DiffOptions::default().with_max_file_size(10);

        let files = GitService::new(&root, &fixture.data_dir)
            .diff(DiffTarget::Unstaged, &options)
            .unwrap();
        let large = named(&files, Path::new("large.txt"));
        let small = named(&files, Path::new("small.txt"));

        assert_eq!(
            large.omission,
            Some(DiffOmission::FileTooLarge { limit: 10 })
        );
        assert!(!large.binary, "the size bound was mislabeled as binary");
        assert!(large.hunks.is_empty());
        assert_eq!(large.new_size, 17);
        assert_eq!(small.omission, None);
        assert!(!small.hunks.is_empty());
    }

    /// An unresolved merge is the state a caller most needs a diff in, and the
    /// index has no stage-0 blob for either side of the conflicted path. The
    /// path must be named as unmerged and every other file must still arrive:
    /// one delta with no representable content cannot cost the caller the rest.
    #[test]
    fn an_unresolved_merge_names_the_conflict_and_keeps_every_other_file() {
        let fixture = Fixture::new();
        let root = fixture.directory("conflicted-diff");
        let repository = initialize_repository(&root);
        fs::write(root.join("conflict.txt"), b"base\n").unwrap();
        fs::write(root.join("clean.txt"), b"base\n").unwrap();
        commit_all(&repository, "base");
        git(&root, ["checkout", "-b", "side"]);
        fs::write(root.join("conflict.txt"), b"side\n").unwrap();
        commit_all(&repository, "side");
        git(&root, ["checkout", "main"]);
        fs::write(root.join("conflict.txt"), b"main\n").unwrap();
        commit_all(&repository, "main");
        // The merge is expected to fail; `git` unwraps, so it is run directly.
        std::process::Command::new("git")
            .args(["merge", "side"])
            .current_dir(&root)
            .output()
            .expect("git merge should run");
        fs::write(root.join("clean.txt"), b"edited\n").unwrap();
        let service = GitService::new(&root, &fixture.data_dir);

        let files = service
            .diff_snapshot(
                &[DiffTarget::Staged, DiffTarget::Unstaged],
                &DiffOptions::default(),
            )
            .expect("a conflicted repository still has a representable diff");

        let conflicted = files
            .iter()
            .filter(|file| file.change == FileChange::Unmerged)
            .collect::<Vec<_>>();
        assert!(
            !conflicted.is_empty(),
            "the unmerged path must appear: {files:#?}"
        );
        for file in conflicted {
            assert_eq!(file.omission, Some(DiffOmission::Unmerged));
            assert!(file.hunks.is_empty());
        }
        let clean = named(&files, Path::new("clean.txt"));
        assert_eq!(clean.omission, None);
        assert!(
            !clean.hunks.is_empty(),
            "an unrelated file lost its content to the conflict"
        );
    }

    /// Libgit2 leaves a tree side's size at zero until the blob is loaded.
    /// Trusting that would exempt the whole `HEAD` side of a staged diff from
    /// the size bound, and libgit2's own guard would then report an oversized
    /// text file as binary, which reads as permanent rather than as a setting.
    #[test]
    fn an_oversized_head_side_is_named_too_large_rather_than_binary() {
        let fixture = Fixture::new();
        let root = fixture.directory("head-side-size");
        let repository = initialize_repository(&root);
        let large = "line\n".repeat(4_000);
        fs::write(root.join("shrunk.txt"), large.as_bytes()).unwrap();
        commit_all(&repository, "add a large file");
        fs::write(root.join("shrunk.txt"), b"tiny\n").unwrap();
        stage(&repository, Path::new("shrunk.txt"));
        let service = GitService::new(&root, &fixture.data_dir);

        let bounded = service
            .diff(
                DiffTarget::Staged,
                &DiffOptions::default().with_max_file_size(1_000),
            )
            .unwrap();
        let file = named(&bounded, Path::new("shrunk.txt"));

        assert_eq!(file.old_size, large.len() as u64);
        assert_eq!(file.new_size, 5);
        assert!(!file.binary, "a size bound was reported as binary content");
        assert_eq!(
            file.omission,
            Some(DiffOmission::FileTooLarge { limit: 1_000 })
        );

        let raised = service
            .diff(
                DiffTarget::Staged,
                &DiffOptions::default().with_max_file_size(u64::MAX),
            )
            .unwrap();

        assert!(
            !named(&raised, Path::new("shrunk.txt")).hunks.is_empty(),
            "raising the bound must actually return the content"
        );
    }

    /// A per-file bound does not bound a response. Both whole-model budgets
    /// have to stop content without dropping the file from the list, so a
    /// caller can see exactly what it did not receive and ask for more.
    #[test]
    fn whole_model_budgets_name_what_they_withheld() {
        let fixture = Fixture::new();
        let root = fixture.directory("budgeted-diff");
        let repository = initialize_repository(&root);
        for index in 0..5 {
            fs::write(root.join(format!("file{index}.txt")), b"old\n").unwrap();
        }
        commit_all(&repository, "add files");
        for index in 0..5 {
            fs::write(root.join(format!("file{index}.txt")), b"new content\n").unwrap();
        }
        let service = GitService::new(&root, &fixture.data_dir);

        let by_count = service
            .diff(
                DiffTarget::Unstaged,
                &DiffOptions::default().with_max_files(2),
            )
            .unwrap();

        assert_eq!(by_count.len(), 5, "no file may vanish from the listing");
        assert_eq!(
            by_count
                .iter()
                .filter(|file| file.omission
                    == Some(DiffOmission::FileBudgetExhausted { limit: 2 }))
                .count(),
            3
        );

        // Each file renders one header plus two short lines, so this admits the
        // first file's content and leaves too little for the second.
        let by_bytes = service
            .diff(
                DiffTarget::Unstaged,
                &DiffOptions::default().with_max_total_bytes(40),
            )
            .unwrap();

        assert_eq!(by_bytes.len(), 5);
        assert!(
            by_bytes.iter().any(|file| matches!(
                file.omission,
                Some(DiffOmission::ContentBudgetExhausted { limit: 40 })
            )),
            "the content budget must name itself: {by_bytes:#?}"
        );
        assert!(
            by_bytes.iter().any(|file| !file.hunks.is_empty()),
            "the budget must not withhold everything"
        );
    }

    /// Both sides must be read from one index, or a concurrent write between
    /// them yields a response describing two different moments.
    #[test]
    fn a_snapshot_reads_every_target_from_one_index() {
        let fixture = Fixture::new();
        let root = fixture.directory("snapshot-diff");
        let repository = initialize_repository(&root);
        fs::write(root.join("tracked.txt"), b"committed\n").unwrap();
        commit_all(&repository, "base");
        fs::write(root.join("tracked.txt"), b"staged\n").unwrap();
        stage(&repository, Path::new("tracked.txt"));
        fs::write(root.join("tracked.txt"), b"worktree\n").unwrap();

        let files = GitService::new(&root, &fixture.data_dir)
            .diff_snapshot(
                &[DiffTarget::Staged, DiffTarget::Unstaged],
                &DiffOptions::default(),
            )
            .unwrap();

        assert_eq!(files.len(), 2);
        assert_eq!(files[0].target, DiffTarget::Staged);
        assert_eq!(files[1].target, DiffTarget::Unstaged);
        assert_eq!(
            files[0].new_blob_id, files[1].old_blob_id,
            "the two targets described different index states"
        );
    }

    #[test]
    fn an_outside_path_is_refused_before_repository_inspection() {
        let fixture = Fixture::new();
        let root = fixture.directory("outside-diff");
        initialize_repository(&root);
        let service = GitService::new(&root, &fixture.data_dir);
        let options = DiffOptions::default().with_paths(["../outside.txt"]);

        let error = service.diff(DiffTarget::Unstaged, &options).unwrap_err();

        assert!(
            matches!(error, GitError::PathOutsideRepository { ref path, .. }
            if path == Path::new("../outside.txt"))
        );
        assert!(!fixture.data_dir.exists());
    }

    #[test]
    fn hunk_coordinates_blob_ids_and_bytes_rebuild_an_applicable_patch() {
        let fixture = Fixture::new();
        let root = fixture.directory("patch-round-trip");
        let repository = initialize_repository(&root);
        repository
            .config()
            .unwrap()
            .set_bool("core.autocrlf", false)
            .unwrap();
        let old = b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven\ntwelve\nthirteen\nfourteen\nfifteen\n";
        let new = b"one\nTWO\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven\ntwelve\nthirteen\nFOURTEEN\nfifteen\n";
        fs::write(root.join("tracked.txt"), old).unwrap();
        commit_all(&repository, "prepare patch baseline");
        fs::write(root.join("tracked.txt"), new).unwrap();
        let files = GitService::new(&root, &fixture.data_dir)
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let file = named(&files, Path::new("tracked.txt"));
        assert_eq!(file.hunks.len(), 2);

        let patch = render_patch(file);
        fs::write(root.join("tracked.txt"), old).unwrap();
        fs::write(root.join("round-trip.patch"), patch).unwrap();
        git(&root, ["apply", "--", "round-trip.patch"]);

        assert_eq!(fs::read(root.join("tracked.txt")).unwrap(), new);
    }

    #[test]
    fn untracked_content_is_an_unstaged_addition() {
        let fixture = Fixture::new();
        let root = fixture.directory("untracked-diff");
        initialize_repository(&root);
        fs::write(root.join("untracked.txt"), b"untracked bytes\n").unwrap();

        let files = GitService::new(&root, &fixture.data_dir)
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let untracked = named(&files, Path::new("untracked.txt"));

        assert_eq!(untracked.change, FileChange::Untracked);
        assert!(untracked.old_blob_id.chars().all(|byte| byte == '0'));
        assert_eq!(added_lines(untracked), vec![b"untracked bytes\n".to_vec()]);
    }

    #[test]
    fn eof_markers_preserve_unterminated_content_in_an_applicable_patch() {
        let fixture = Fixture::new();
        let root = fixture.directory("unterminated-patch");
        let repository = initialize_repository(&root);
        fs::write(root.join("tracked.txt"), b"old without newline").unwrap();
        commit_all(&repository, "prepare unterminated baseline");
        fs::write(root.join("tracked.txt"), b"new without newline").unwrap();
        let files = GitService::new(&root, &fixture.data_dir)
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let file = named(&files, Path::new("tracked.txt"));

        let patch = render_patch(file);
        fs::write(root.join("tracked.txt"), b"old without newline").unwrap();
        fs::write(root.join("unterminated.patch"), patch).unwrap();
        git(&root, ["apply", "--", "unterminated.patch"]);

        assert_eq!(
            fs::read(root.join("tracked.txt")).unwrap(),
            b"new without newline"
        );
    }

    fn stage(repository: &Repository, path: &Path) {
        let mut index = repository.index().unwrap();
        index.add_path(path).unwrap();
        index.write().unwrap();
    }

    fn named<'a>(files: &'a [FileDiff], path: &Path) -> &'a FileDiff {
        files
            .iter()
            .find(|file| {
                file.new_path.as_deref() == Some(path) || file.old_path.as_deref() == Some(path)
            })
            .unwrap_or_else(|| panic!("no diff for '{}' in {files:#?}", path.display()))
    }

    fn added_lines(file: &FileDiff) -> Vec<Vec<u8>> {
        file.hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .filter(|line| line.kind == DiffLineKind::Addition)
            .map(|line| line.content.clone())
            .collect()
    }

    fn render_patch(file: &FileDiff) -> Vec<u8> {
        let old_path = file.old_path.as_deref().unwrap();
        let new_path = file.new_path.as_deref().unwrap();
        let mut patch = format!(
            "diff --git a/{old} b/{new}\nindex {old_id}..{new_id} 100644\n--- a/{old}\n+++ b/{new}\n",
            old = old_path.display(),
            new = new_path.display(),
            old_id = file.old_blob_id,
            new_id = file.new_blob_id,
        )
        .into_bytes();
        for hunk in &file.hunks {
            render_hunk(&mut patch, hunk);
        }
        patch
    }

    fn render_hunk(patch: &mut Vec<u8>, hunk: &Hunk) {
        patch.extend_from_slice(&hunk.header);
        for line in &hunk.lines {
            match line.kind {
                DiffLineKind::Context => patch.push(b' '),
                DiffLineKind::Addition => patch.push(b'+'),
                DiffLineKind::Deletion => patch.push(b'-'),
                DiffLineKind::BothEofNoNewline
                | DiffLineKind::OldEofNoNewline
                | DiffLineKind::NewEofNoNewline => {}
            }
            patch.extend_from_slice(&line.content);
        }
    }
}
