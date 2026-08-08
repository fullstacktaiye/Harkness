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
    Delta, DiffFindOptions, DiffLineType as GitDiffLineType, DiffOptions as GitDiffOptions,
    ErrorCode, FileMode, ObjectType, Oid, Patch, Repository,
};

use crate::git::{FileChange, GitError, commit};

/// The default largest file whose content Harkness will put in a diff model.
const DEFAULT_MAX_DIFF_FILE_SIZE: u64 = 1024 * 1024;

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
    /// The number of unchanged lines surrounding each hunk.
    pub context_lines: u32,
    /// Literal paths to inspect. An empty list selects the whole tree.
    pub paths: Vec<PathBuf>,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            max_file_size: DEFAULT_MAX_DIFF_FILE_SIZE,
            context_lines: 3,
            paths: Vec::new(),
        }
    }
}

impl DiffOptions {
    /// Sets the largest file whose content will be returned.
    #[must_use]
    pub fn with_max_file_size(mut self, max_file_size: u64) -> Self {
        self.max_file_size = max_file_size;
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
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DiffOmission {
    /// At least one side exceeded [`DiffOptions::max_file_size`].
    FileTooLarge { limit: u64 },
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
    commit::validate_paths(root, &options.paths)?;
    let selected_paths = selected_paths(root, &options.paths);
    let repository = commit::open(root)?;
    let index = repository
        .index()
        .map_err(|source| inspection(root, source))?;

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
            let head_tree = head_tree(&repository, root)?;
            repository.diff_tree_to_index(
                head_tree.as_ref(),
                Some(&index),
                Some(&mut native_options),
            )
        }
        DiffTarget::Unstaged => {
            repository.diff_index_to_workdir(Some(&index), Some(&mut native_options))
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

    let mut files = Vec::new();
    for index in 0..diff.deltas().len() {
        let Some(delta) = diff.get_delta(index) else {
            return Err(malformed(format!("diff delta {index} disappeared")));
        };
        if delta.status() == Delta::Unmodified
            || !path_selected(
                delta.old_file().path(),
                delta.new_file().path(),
                &selected_paths,
            )
        {
            continue;
        }

        let old_size = delta.old_file().size();
        let new_size = delta.new_file().size();
        let omission = (old_size > options.max_file_size || new_size > options.max_file_size)
            .then_some(DiffOmission::FileTooLarge {
                limit: options.max_file_size,
            });

        let patch = if omission.is_none() {
            Patch::from_diff(&diff, index).map_err(|source| inspection(root, source))?
        } else {
            None
        };
        // Patch construction performs binary detection and may populate IDs,
        // so reacquire the delta afterward rather than retaining stale flags.
        let delta = match patch.as_ref() {
            Some(patch) => patch.delta(),
            None => diff
                .get_delta(index)
                .ok_or_else(|| malformed(format!("diff delta {index} disappeared")))?,
        };
        let old_file = delta.old_file();
        let new_file = delta.new_file();
        let old_path = old_file
            .exists()
            .then(|| path_buf(old_file.path(), "old"))
            .transpose()?;
        let new_path = new_file
            .exists()
            .then(|| path_buf(new_file.path(), "new"))
            .transpose()?;
        let binary = omission.is_none() && (old_file.is_binary() || new_file.is_binary());
        let hunks = match patch.as_ref() {
            Some(patch) if !binary => collect_hunks(patch, root)?,
            Some(_) | None => Vec::new(),
        };

        files.push(FileDiff {
            target: target.clone(),
            change: file_change(delta.status())?,
            old_path,
            new_path: new_path.clone(),
            old_blob_id: blob_id(&repository, root, &target, false, old_file, None)?,
            new_blob_id: blob_id(
                &repository,
                root,
                &target,
                true,
                new_file,
                new_path.as_deref(),
            )?,
            old_size,
            new_size,
            binary,
            omission,
            hunks,
        });
    }
    Ok(files)
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

fn collect_hunks(patch: &Patch<'_>, root: &Path) -> Result<Vec<Hunk>, GitError> {
    let mut hunks = Vec::with_capacity(patch.num_hunks());
    for hunk_index in 0..patch.num_hunks() {
        let (hunk, line_count) = patch
            .hunk(hunk_index)
            .map_err(|source| inspection(root, source))?;
        let mut lines = Vec::with_capacity(line_count);
        for line_index in 0..line_count {
            let line = patch
                .line_in_hunk(hunk_index, line_index)
                .map_err(|source| inspection(root, source))?;
            lines.push(DiffLine {
                kind: line_kind(line.origin_value())?,
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
    Ok(hunks)
}

fn blob_id(
    repository: &Repository,
    root: &Path,
    target: &DiffTarget,
    new_side: bool,
    file: git2::DiffFile<'_>,
    path: Option<&Path>,
) -> Result<String, GitError> {
    if !file.exists() {
        return Ok(file.id().to_string());
    }
    if file.is_valid_id() && !file.id().is_zero() {
        return Ok(file.id().to_string());
    }
    if matches!(target, DiffTarget::Unstaged) && new_side && file.mode() != FileMode::Commit {
        let path = path.ok_or_else(|| malformed("an existing worktree side has no path"))?;
        return hash_worktree_file(repository, &root.join(path)).map(|id| id.to_string());
    }
    Err(malformed("a present diff side has no valid blob object ID"))
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

fn path_buf(path: Option<&Path>, side: &str) -> Result<PathBuf, GitError> {
    path.map(Path::to_path_buf)
        .ok_or_else(|| malformed(format!("a present {side} diff side has no path")))
}

fn file_change(delta: Delta) -> Result<FileChange, GitError> {
    Ok(match delta {
        Delta::Added => FileChange::Added,
        Delta::Deleted => FileChange::Deleted,
        Delta::Modified => FileChange::Modified,
        Delta::Renamed => FileChange::Renamed,
        Delta::Copied => FileChange::Copied,
        Delta::Typechange => FileChange::TypeChanged,
        Delta::Conflicted => FileChange::Unmerged,
        Delta::Untracked => FileChange::Untracked,
        Delta::Unmodified | Delta::Ignored | Delta::Unreadable => {
            return Err(malformed(format!("unexpected {delta:?} delta")));
        }
    })
}

fn line_kind(kind: GitDiffLineType) -> Result<DiffLineKind, GitError> {
    Ok(match kind {
        GitDiffLineType::Context => DiffLineKind::Context,
        GitDiffLineType::Addition => DiffLineKind::Addition,
        GitDiffLineType::Deletion => DiffLineKind::Deletion,
        GitDiffLineType::ContextEOFNL => DiffLineKind::BothEofNoNewline,
        GitDiffLineType::AddEOFNL => DiffLineKind::OldEofNoNewline,
        GitDiffLineType::DeleteEOFNL => DiffLineKind::NewEofNoNewline,
        GitDiffLineType::FileHeader | GitDiffLineType::HunkHeader | GitDiffLineType::Binary => {
            return Err(malformed(format!("patch hunk returned a {kind:?} line")));
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
