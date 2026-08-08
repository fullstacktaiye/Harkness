//! Atomic, byte-preserving hunk staging and unstaging.
//!
//! A selection never supplies patch text. It names the two blobs and one hunk
//! from the structured diff contract; this module recomputes that diff while
//! holding the repository lock, finds the named hunk, and renders trusted bytes
//! from the fresh model. Libgit2 checks and applies the resulting patch to the
//! index only.

use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

use git2::{ApplyLocation, ApplyOptions, Diff};

use crate::git::{
    DiffLineKind, DiffOptions, DiffTarget, FileChange, FileDiff, GitError, Hunk, RepositoryLock,
    commit, diff,
};

/// One selected hunk from a [`FileDiff`].
///
/// Blob IDs make the selection stale-safe. Both paths are retained because an
/// edited rename has meaningful content coordinates as well as two names.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct HunkSelection {
    pub old_path: Option<PathBuf>,
    pub new_path: Option<PathBuf>,
    pub old_blob_id: String,
    pub new_blob_id: String,
    pub context_lines: u32,
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
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

    /// The path callers should normally show in an error or selection list.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.new_path.as_deref().or(self.old_path.as_deref())
    }
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

pub(crate) fn stage(
    root: &Path,
    _lock: &RepositoryLock,
    selections: &[HunkSelection],
) -> Result<(), GitError> {
    mutate(root, selections, DiffTarget::Unstaged, Direction::Forward)
}

pub(crate) fn unstage(
    root: &Path,
    _lock: &RepositoryLock,
    selections: &[HunkSelection],
) -> Result<(), GitError> {
    mutate(root, selections, DiffTarget::Staged, Direction::Reverse)
}

fn mutate(
    root: &Path,
    selections: &[HunkSelection],
    target: DiffTarget,
    direction: Direction,
) -> Result<(), GitError> {
    let paths = selections
        .iter()
        .flat_map(|selection| [&selection.old_path, &selection.new_path])
        .filter_map(Option::as_ref)
        .cloned()
        .collect::<Vec<_>>();
    commit::validate_paths(root, &paths)?;
    let repository = commit::open(root)?;
    if selections.is_empty() {
        return Ok(());
    }

    let mut contexts = selections
        .iter()
        .map(|selection| selection.context_lines)
        .collect::<Vec<_>>();
    contexts.sort_unstable();
    contexts.dedup();

    let mut current = Vec::new();
    for context_lines in contexts {
        let options = DiffOptions::default()
            .with_max_file_size(u64::MAX)
            .with_context_lines(context_lines)
            .with_paths(&paths);
        current.push((
            context_lines,
            diff::compute(root, target.clone(), &options)?,
        ));
    }

    let mut prepared = Vec::<PreparedFile>::new();
    for selection in selections {
        let path = selection.path().unwrap_or_else(|| Path::new(""));
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
        if file.binary {
            return Err(GitError::BinaryHunkSelection {
                path: path.to_path_buf(),
            });
        }
        if file.change == FileChange::Renamed && file.hunks.is_empty() {
            return Err(GitError::RenameOnlyHunkSelection {
                old_path: file.old_path.clone().unwrap_or_default(),
                new_path: file.new_path.clone().unwrap_or_default(),
            });
        }

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

    let patch = render_patch(&mut prepared, direction);
    let parsed = Diff::from_buffer_ext(&patch, repository.object_format())
        .map_err(|source| GitError::HunkApplication { source })?;

    // Libgit2 builds every postimage before its index writer commits, so both
    // the check and the real apply leave the index unchanged on any failure.
    let mut check = ApplyOptions::new();
    check.check(true);
    repository
        .apply(&parsed, ApplyLocation::Index, Some(&mut check))
        .map_err(|source| GitError::HunkApplication { source })?;
    repository
        .apply(&parsed, ApplyLocation::Index, None)
        .map_err(|source| GitError::HunkApplication { source })
}

fn coordinates_match(selection: &HunkSelection, hunk: &Hunk) -> bool {
    selection.old_start == hunk.old_start
        && selection.old_lines == hunk.old_lines
        && selection.new_start == hunk.new_start
        && selection.new_lines == hunk.new_lines
}

fn render_patch(files: &mut [PreparedFile], direction: Direction) -> Vec<u8> {
    let mut patch = Vec::new();
    for prepared in files {
        prepared.hunks.sort_by_key(|hunk| match direction {
            Direction::Forward => (hunk.old_start, hunk.new_start),
            Direction::Reverse => (hunk.new_start, hunk.old_start),
        });
        render_file(&mut patch, &prepared.file, &prepared.hunks, direction);
    }
    patch
}

fn render_file(patch: &mut Vec<u8>, file: &FileDiff, hunks: &[Hunk], direction: Direction) {
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
    let display_old = old_path.or(new_path).unwrap_or_else(|| Path::new(""));
    let display_new = new_path.or(old_path).unwrap_or_else(|| Path::new(""));

    patch.extend_from_slice(b"diff --git ");
    push_quoted_path(patch, b"a/", display_old);
    patch.push(b' ');
    push_quoted_path(patch, b"b/", display_new);
    patch.push(b'\n');

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
    for line in &hunk.lines {
        let kind = match (direction, line.kind) {
            (Direction::Reverse, DiffLineKind::Addition) => DiffLineKind::Deletion,
            (Direction::Reverse, DiffLineKind::Deletion) => DiffLineKind::Addition,
            (Direction::Reverse, DiffLineKind::OldEofNoNewline) => DiffLineKind::NewEofNoNewline,
            (Direction::Reverse, DiffLineKind::NewEofNoNewline) => DiffLineKind::OldEofNoNewline,
            _ => line.kind,
        };
        match kind {
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

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use git2::Repository;

    use super::HunkSelection;
    use crate::{
        git::{
            Cancellation, CommitOptions, DiffOptions, DiffTarget, FileChange, FileDiff, GitError,
            GitService, Hunk,
        },
        testing::{Fixture, commit_all, configure_commit_identity, initialize_repository},
    };

    const OLD: &[u8] = b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven\ntwelve\nthirteen\nfourteen\nfifteen\nsixteen\nseventeen\neighteen\nnineteen\ntwenty\n";
    const NEW: &[u8] = b"one\nTWO\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven\ntwelve\nthirteen\nfourteen\nfifteen\nsixteen\nseventeen\nEIGHTEEN\nnineteen\ntwenty\n";
    const FIRST_ONLY: &[u8] = b"one\nTWO\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven\ntwelve\nthirteen\nfourteen\nfifteen\nsixteen\nseventeen\neighteen\nnineteen\ntwenty\n";

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

    #[test]
    fn overlapping_valid_selections_fail_without_a_partial_index_write() {
        let fixture = Fixture::new();
        let root = fixture.directory("atomic-hunk-failure");
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

        assert!(matches!(
            service.stage_hunks(&selections, &Cancellation::default()),
            Err(GitError::HunkApplication { .. })
        ));
        assert_eq!(
            fs::read(repository.path().join("index")).unwrap(),
            index_before
        );
        assert_eq!(fs::read(root.join("tracked.txt")).unwrap(), worktree_before);
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

    fn fake_hunk() -> Hunk {
        Hunk {
            old_start: 1,
            old_lines: 1,
            new_start: 1,
            new_lines: 1,
            header: b"@@ -1 +1 @@\n".to_vec(),
            lines: Vec::new(),
        }
    }
}
