//! Explicit, lock-aware working-tree discard operations.
//!
//! Tracked restoration and untracked deletion are separate entry points. A
//! caller cannot turn a tracked restore into a filesystem deletion merely by
//! naming a path that Git does not know. Hunk discard shares the staging
//! renderer, but applies its trusted reverse patch to the working tree instead
//! of the index.

use std::{
    fs,
    path::{Path, PathBuf},
};

use git2::{ErrorCode, ObjectType, Repository, Status};

use crate::{
    Cancellation, GitError, HunkSelection, RepositoryLock, StatusRefreshOutcome, commit, hunk,
    runner::{GitAccess, GitCommand},
    status, worktree,
};

/// The Git snapshot from which tracked content is restored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TrackedRestoreSource {
    /// Restore the working tree from the index and preserve staged changes.
    Index,
    /// Restore both the index and working tree from `HEAD`.
    Head,
}

/// The destructive operation a confirmation describes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DiscardOperation {
    /// Restore whole tracked paths from the named boundary.
    RestoreTracked { source: TrackedRestoreSource },
    /// Restore selected tracked hunks from the index.
    RestoreTrackedHunks { hunks: usize },
    /// Restore selected tracked lines from the index.
    RestoreTrackedLines { lines: usize, hunks: usize },
    /// Permanently delete untracked files.
    DeleteUntracked,
}

/// What Git can still supply after the operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DiscardRecoverability {
    /// The restored baseline remains recorded in the index or a commit.
    ///
    /// This does not claim that the discarded edits themselves are recoverable.
    GitRecordedBaseline,
    /// The deleted bytes were never recorded by Git.
    Unrecoverable,
}

/// Front-end-neutral facts used to render a destructive confirmation.
///
/// Both front ends receive the same operation, count, path set, and
/// recoverability classification. Presentation and translation remain their
/// responsibility; the safety claim does not.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct DiscardDescription {
    operation: DiscardOperation,
    paths: Vec<PathBuf>,
    recoverability: DiscardRecoverability,
}

impl DiscardDescription {
    /// Describes a whole-path tracked restore.
    #[must_use]
    pub fn restore_tracked<I, P>(paths: I, source: TrackedRestoreSource) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        Self::new(
            DiscardOperation::RestoreTracked { source },
            paths,
            DiscardRecoverability::GitRecordedBaseline,
        )
    }

    /// Describes selected tracked hunks restored from the index.
    #[must_use]
    pub fn restore_hunks<I, P>(paths: I, hunks: usize) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        Self::new(
            DiscardOperation::RestoreTrackedHunks { hunks },
            paths,
            DiscardRecoverability::GitRecordedBaseline,
        )
    }

    /// Describes selected tracked lines restored from the index.
    #[must_use]
    pub fn restore_lines<I, P>(paths: I, lines: usize, hunks: usize) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        Self::new(
            DiscardOperation::RestoreTrackedLines { lines, hunks },
            paths,
            DiscardRecoverability::GitRecordedBaseline,
        )
    }

    /// Describes explicit untracked-file deletion.
    #[must_use]
    pub fn delete_untracked<I, P>(paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        Self::new(
            DiscardOperation::DeleteUntracked,
            paths,
            DiscardRecoverability::Unrecoverable,
        )
    }

    fn new<I, P>(
        operation: DiscardOperation,
        paths: I,
        recoverability: DiscardRecoverability,
    ) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut paths = paths
            .into_iter()
            .map(|path| path.as_ref().to_path_buf())
            .collect::<Vec<_>>();
        paths.sort_unstable();
        paths.dedup();
        Self {
            operation,
            paths,
            recoverability,
        }
    }

    /// The exact operation being confirmed.
    #[must_use]
    pub fn operation(&self) -> DiscardOperation {
        self.operation
    }

    /// Distinct paths affected by the operation, in stable order.
    #[must_use]
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// Number of distinct tracked files whose content is restored.
    #[must_use]
    pub fn tracked_files(&self) -> usize {
        match self.operation {
            DiscardOperation::RestoreTracked { .. }
            | DiscardOperation::RestoreTrackedHunks { .. }
            | DiscardOperation::RestoreTrackedLines { .. } => self.paths.len(),
            DiscardOperation::DeleteUntracked => 0,
        }
    }

    /// Number of distinct untracked files that will be deleted.
    #[must_use]
    pub fn untracked_files(&self) -> usize {
        usize::from(matches!(self.operation, DiscardOperation::DeleteUntracked)) * self.paths.len()
    }

    /// Whether Git retains a baseline after this operation.
    #[must_use]
    pub fn recoverability(&self) -> DiscardRecoverability {
        self.recoverability
    }
}

/// The confirmed operation and the repository state observed after it.
#[derive(Debug)]
#[non_exhaustive]
pub struct DiscardOutcome {
    pub description: DiscardDescription,
    pub status: StatusRefreshOutcome,
}

/// Opaque identity of the path, index, and `HEAD` state a confirmation showed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscardSnapshot(Vec<DiscardPathSnapshot>);

#[derive(Clone, Debug, Eq, PartialEq)]
struct DiscardPathSnapshot {
    path: PathBuf,
    worktree: WorktreeIdentity,
    index: Option<(git2::Oid, u32)>,
    head: Option<(git2::Oid, u32)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WorktreeIdentity {
    Missing,
    File(git2::Oid, u32),
    Symlink(PathBuf),
    Other,
}

pub(crate) fn snapshot(root: &Path, paths: &[PathBuf]) -> Result<DiscardSnapshot, GitError> {
    commit::validate_paths(root, paths)?;
    let repository = commit::open(root)?;
    snapshot_with_repository(&repository, root, paths)
}

fn snapshot_with_repository(
    repository: &Repository,
    root: &Path,
    paths: &[PathBuf],
) -> Result<DiscardSnapshot, GitError> {
    let index = repository.index().map_err(|source| GitError::Inspection {
        path: root.to_path_buf(),
        source: source.into(),
    })?;
    let head_tree = repository
        .head()
        .ok()
        .and_then(|head| head.peel_to_tree().ok());
    let mut distinct = paths.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    let mut snapshots = Vec::with_capacity(distinct.len());
    for path in distinct {
        let relative = repository_path(root, &path);
        let resolved = if path.is_absolute() {
            path.clone()
        } else {
            root.join(&path)
        };
        let worktree = match fs::symlink_metadata(&resolved) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                WorktreeIdentity::Symlink(fs::read_link(&resolved).map_err(|source| {
                    GitError::DiffContent {
                        path: path.clone(),
                        source,
                    }
                })?)
            }
            Ok(metadata) if metadata.is_file() => {
                let id = git2::Oid::hash_file_ext(
                    ObjectType::Blob,
                    &resolved,
                    repository.object_format(),
                )
                .map_err(|source| GitError::Inspection {
                    path: path.clone(),
                    source: source.into(),
                })?;
                WorktreeIdentity::File(id, worktree_file_mode(&metadata))
            }
            Ok(_) => WorktreeIdentity::Other,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                WorktreeIdentity::Missing
            }
            Err(source) => {
                return Err(GitError::DiffContent {
                    path: path.clone(),
                    source,
                });
            }
        };
        let index = index
            .get_path(relative, 0)
            .map(|entry| (entry.id, entry.mode));
        let head = head_tree
            .as_ref()
            .and_then(|tree| tree.get_path(relative).ok())
            .map(|entry| (entry.id(), entry.filemode_raw() as u32));
        snapshots.push(DiscardPathSnapshot {
            path,
            worktree,
            index,
            head,
        });
    }
    Ok(DiscardSnapshot(snapshots))
}

/// The portion of a regular file's mode Git records in trees and the index.
///
/// Git tracks only whether a regular file is executable, not its complete
/// platform permission mask. Non-Unix working trees have no executable bits
/// to inspect, so they use Git's ordinary blob mode.
fn worktree_file_mode(metadata: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if metadata.permissions().mode() & 0o111 == 0 {
            0o100644
        } else {
            0o100755
        }
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        0o100644
    }
}

fn require_snapshot(
    repository: &Repository,
    root: &Path,
    paths: &[PathBuf],
    expected: &DiscardSnapshot,
) -> Result<(), GitError> {
    if &snapshot_with_repository(repository, root, paths)? != expected {
        return Err(GitError::StaleDiscardSelection);
    }
    Ok(())
}

pub(crate) fn restore_tracked(
    git_executable: &Path,
    root: &Path,
    _lock: &RepositoryLock,
    paths: &[PathBuf],
    source: TrackedRestoreSource,
    expected: Option<&DiscardSnapshot>,
    cancellation: &Cancellation,
) -> Result<DiscardOutcome, GitError> {
    commit::validate_paths(root, paths)?;
    let repository = commit::open(root)?;
    if let Some(expected) = expected {
        require_snapshot(&repository, root, paths, expected)?;
    }
    worktree::refuse_locked(git_executable, root, cancellation)?;
    if cancellation.is_cancelled() {
        return Err(GitError::Cancelled);
    }
    for path in paths {
        require_tracked_change(&repository, root, path, source)?;
    }
    refuse_pending(&repository, root)?;

    if !paths.is_empty() {
        let mut command = GitCommand::new(git_executable, root, GitAccess::LocalWrite)
            .args(["--literal-pathspecs", "restore"]);
        command = match source {
            TrackedRestoreSource::Index => command.arg("--worktree"),
            TrackedRestoreSource::Head => {
                let source = head_or_empty_tree(&repository, root)?;
                command
                    .arg(format!("--source={source}"))
                    .args(["--staged", "--worktree"])
            }
        };
        command = command.arg("--");
        for path in paths {
            command = command.arg(path.as_os_str());
        }
        command.run(cancellation)?;
    }

    Ok(DiscardOutcome {
        description: DiscardDescription::restore_tracked(paths, source),
        status: commit::refresh_status(git_executable, root, true, cancellation),
    })
}

pub(crate) fn delete_untracked(
    git_executable: &Path,
    root: &Path,
    _lock: &RepositoryLock,
    paths: &[PathBuf],
    expected: Option<&DiscardSnapshot>,
    cancellation: &Cancellation,
) -> Result<DiscardOutcome, GitError> {
    commit::validate_paths(root, paths)?;
    let repository = commit::open(root)?;
    if let Some(expected) = expected {
        require_snapshot(&repository, root, paths, expected)?;
    }
    worktree::refuse_locked(git_executable, root, cancellation)?;
    if cancellation.is_cancelled() {
        return Err(GitError::Cancelled);
    }

    let mut paths = paths.to_vec();
    paths.sort_unstable();
    paths.dedup();
    let mut resolved = Vec::with_capacity(paths.len());
    for path in &paths {
        require_untracked_file(&repository, root, path)?;
        resolved.push(if path.is_absolute() {
            path.clone()
        } else {
            root.join(path)
        });
    }
    refuse_pending(&repository, root)?;
    // Once every refusal has passed, deletion is deliberately not cancelled
    // between files: a late token must not turn one confirmed batch into an
    // arbitrary prefix of itself.
    for (path, resolved) in paths.iter().zip(resolved) {
        fs::remove_file(resolved).map_err(|source| GitError::UntrackedDiscardIo {
            path: path.clone(),
            source,
        })?;
    }

    Ok(DiscardOutcome {
        description: DiscardDescription::delete_untracked(&paths),
        status: commit::refresh_status(git_executable, root, true, cancellation),
    })
}

pub(crate) fn discard_hunks(
    git_executable: &Path,
    root: &Path,
    _lock: &RepositoryLock,
    selections: &[HunkSelection],
    cancellation: &Cancellation,
) -> Result<DiscardOutcome, GitError> {
    let outcome = hunk::discard(git_executable, root, selections, cancellation)?;
    let paths = selections.iter().filter_map(HunkSelection::path);
    Ok(DiscardOutcome {
        description: DiscardDescription::restore_hunks(paths, outcome.hunks),
        status: outcome.status,
    })
}

pub(crate) fn discard_lines(
    git_executable: &Path,
    root: &Path,
    _lock: &RepositoryLock,
    selections: &[crate::LineSelection],
    cancellation: &Cancellation,
) -> Result<DiscardOutcome, GitError> {
    let outcome = hunk::discard_lines(git_executable, root, selections, cancellation)?;
    let paths = selections.iter().filter_map(crate::LineSelection::path);
    Ok(DiscardOutcome {
        description: DiscardDescription::restore_lines(paths, outcome.lines, outcome.hunks),
        status: outcome.status,
    })
}

fn refuse_pending(repository: &Repository, root: &Path) -> Result<(), GitError> {
    match status::pending(repository) {
        Some(pending) => Err(GitError::OperationInProgress {
            path: root.to_path_buf(),
            pending,
        }),
        None => Ok(()),
    }
}

fn repository_path<'path>(root: &Path, path: &'path Path) -> &'path Path {
    if path.is_absolute() {
        path.strip_prefix(root).unwrap_or(path)
    } else {
        path
    }
}

fn status_for(repository: &Repository, root: &Path, path: &Path) -> Result<Status, GitError> {
    match repository.status_file(repository_path(root, path)) {
        Ok(status) => Ok(status),
        Err(error) if error.code() == ErrorCode::NotFound => Ok(Status::CURRENT),
        Err(source) => Err(GitError::Inspection {
            path: root.to_path_buf(),
            source: source.into(),
        }),
    }
}

fn require_tracked_change(
    repository: &Repository,
    root: &Path,
    path: &Path,
    source: TrackedRestoreSource,
) -> Result<(), GitError> {
    let status = status_for(repository, root, path)?;
    if status.contains(Status::CONFLICTED) {
        return Err(GitError::UnmergedDiscard {
            path: path.to_path_buf(),
        });
    }
    if status.contains(Status::WT_NEW) {
        return Err(GitError::UntrackedDiscardRequiresDelete {
            path: path.to_path_buf(),
        });
    }
    let relevant = match source {
        TrackedRestoreSource::Index => status.intersects(
            Status::WT_MODIFIED | Status::WT_DELETED | Status::WT_RENAMED | Status::WT_TYPECHANGE,
        ),
        TrackedRestoreSource::Head => status.intersects(
            Status::INDEX_NEW
                | Status::INDEX_MODIFIED
                | Status::INDEX_DELETED
                | Status::INDEX_RENAMED
                | Status::INDEX_TYPECHANGE
                | Status::WT_MODIFIED
                | Status::WT_DELETED
                | Status::WT_RENAMED
                | Status::WT_TYPECHANGE,
        ),
    };
    if !relevant {
        return Err(GitError::NothingToDiscard {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn require_untracked_file(
    repository: &Repository,
    root: &Path,
    path: &Path,
) -> Result<(), GitError> {
    let status = status_for(repository, root, path)?;
    if status.contains(Status::CONFLICTED) {
        return Err(GitError::UnmergedDiscard {
            path: path.to_path_buf(),
        });
    }
    if !status.contains(Status::WT_NEW) {
        return Err(GitError::TrackedDiscardRequiresRestore {
            path: path.to_path_buf(),
        });
    }
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let metadata = fs::symlink_metadata(&resolved).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            GitError::NothingToDiscard {
                path: path.to_path_buf(),
            }
        } else {
            GitError::UntrackedDiscardIo {
                path: path.to_path_buf(),
                source,
            }
        }
    })?;
    if metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(GitError::UntrackedDiscardNotFile {
            path: path.to_path_buf(),
        })
    }
}

fn head_or_empty_tree(repository: &Repository, root: &Path) -> Result<git2::Oid, GitError> {
    match repository.head().and_then(|head| head.peel_to_commit()) {
        Ok(commit) => Ok(commit.tree_id()),
        Err(error) if matches!(error.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => {
            let builder = repository
                .treebuilder(None)
                .map_err(|source| GitError::Inspection {
                    path: root.to_path_buf(),
                    source: source.into(),
                })?;
            builder.write().map_err(|source| GitError::Inspection {
                path: root.to_path_buf(),
                source: source.into(),
            })
        }
        Err(source) => Err(GitError::Inspection {
            path: root.to_path_buf(),
            source: source.into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use crate::{
        Cancellation, DiffLineKind, DiffOptions, DiffTarget, DiscardOperation,
        DiscardRecoverability, GitError, GitService, HunkSelection, LineSelection,
        TrackedRestoreSource,
        runner::{GitAccess, GitCommand},
        testing::{Fixture, commit_all, git, initialize_repository},
    };

    fn worktree_text(path: &Path) -> String {
        fs::read_to_string(path).unwrap().replace("\r\n", "\n")
    }

    #[test]
    fn restoring_from_the_index_preserves_staged_content() {
        let fixture = Fixture::new();
        let root = fixture.directory("discard-index");
        initialize_repository(&root);
        let service = GitService::new(&root, &fixture.data_dir);
        let cancellation = Cancellation::default();

        fs::write(root.join("tracked.txt"), "staged\n").unwrap();
        service.stage(["tracked.txt"], &cancellation).unwrap();
        fs::write(root.join("tracked.txt"), "working\n").unwrap();

        let outcome = service
            .restore_tracked(["tracked.txt"], TrackedRestoreSource::Index, &cancellation)
            .unwrap();

        assert_eq!(worktree_text(&root.join("tracked.txt")), "staged\n");
        let staged = service
            .diff(DiffTarget::Staged, &DiffOptions::default())
            .unwrap();
        let unstaged = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        assert_eq!(staged.len(), 1);
        assert!(unstaged.is_empty());
        assert_eq!(
            outcome.description.operation(),
            DiscardOperation::RestoreTracked {
                source: TrackedRestoreSource::Index
            }
        );
        assert_eq!(outcome.description.tracked_files(), 1);
        assert_eq!(
            outcome.description.recoverability(),
            DiscardRecoverability::GitRecordedBaseline
        );
    }

    #[test]
    fn restoring_from_head_discards_both_index_and_worktree_changes() {
        let fixture = Fixture::new();
        let root = fixture.directory("discard-head");
        initialize_repository(&root);
        let service = GitService::new(&root, &fixture.data_dir);
        let cancellation = Cancellation::default();

        fs::write(root.join("tracked.txt"), "staged\n").unwrap();
        service.stage(["tracked.txt"], &cancellation).unwrap();
        fs::write(root.join("tracked.txt"), "working\n").unwrap();

        service
            .restore_tracked(["tracked.txt"], TrackedRestoreSource::Head, &cancellation)
            .unwrap();

        assert_eq!(worktree_text(&root.join("tracked.txt")), "initial\n");
        assert!(
            service
                .detailed_status(&cancellation)
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    fn tracked_restore_and_untracked_deletion_cannot_be_conflated() {
        let fixture = Fixture::new();
        let root = fixture.directory("discard-kinds");
        initialize_repository(&root);
        let service = GitService::new(&root, &fixture.data_dir);
        let cancellation = Cancellation::default();
        fs::write(root.join("untracked.txt"), "not in Git\n").unwrap();

        assert!(matches!(
            service.restore_tracked(
                ["untracked.txt"],
                TrackedRestoreSource::Index,
                &cancellation
            ),
            Err(GitError::UntrackedDiscardRequiresDelete { .. })
        ));
        assert!(root.join("untracked.txt").exists());
        assert!(matches!(
            service.delete_untracked(["tracked.txt"], &cancellation),
            Err(GitError::TrackedDiscardRequiresRestore { .. })
        ));
        assert_eq!(
            fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "initial\n"
        );

        let outcome = service
            .delete_untracked(["untracked.txt"], &cancellation)
            .unwrap();
        assert!(!root.join("untracked.txt").exists());
        assert_eq!(outcome.description.untracked_files(), 1);
        assert_eq!(
            outcome.description.recoverability(),
            DiscardRecoverability::Unrecoverable
        );
    }

    #[test]
    fn duplicate_untracked_paths_are_deleted_once_and_report_success() {
        let fixture = Fixture::new();
        let root = fixture.directory("discard-duplicate-untracked");
        initialize_repository(&root);
        fs::write(root.join("untracked.txt"), "delete once\n").unwrap();
        let service = GitService::new(&root, &fixture.data_dir);

        let outcome = service
            .delete_untracked(["untracked.txt", "untracked.txt"], &Cancellation::default())
            .unwrap();

        assert!(!root.join("untracked.txt").exists());
        assert_eq!(outcome.description.untracked_files(), 1);
    }

    #[test]
    fn a_confirmed_snapshot_refuses_newer_worktree_index_and_head_bytes() {
        let fixture = Fixture::new();
        let root = fixture.directory("discard-snapshot");
        initialize_repository(&root);
        let service = GitService::new(&root, &fixture.data_dir);
        fs::write(root.join("tracked.txt"), "shown edit\n").unwrap();
        let snapshot = service.discard_snapshot(["tracked.txt"]).unwrap();
        fs::write(root.join("tracked.txt"), "newer edit\n").unwrap();

        assert!(matches!(
            service.restore_tracked_if_unchanged(
                ["tracked.txt"],
                TrackedRestoreSource::Index,
                &snapshot,
                &Cancellation::default(),
            ),
            Err(GitError::StaleDiscardSelection)
        ));
        assert_eq!(worktree_text(&root.join("tracked.txt")), "newer edit\n");

        fs::write(root.join("untracked.txt"), "shown untracked\n").unwrap();
        let snapshot = service.discard_snapshot(["untracked.txt"]).unwrap();
        fs::write(root.join("untracked.txt"), "newer untracked\n").unwrap();
        assert!(matches!(
            service.delete_untracked_if_unchanged(
                ["untracked.txt"],
                &snapshot,
                &Cancellation::default(),
            ),
            Err(GitError::StaleDiscardSelection)
        ));
        assert_eq!(
            worktree_text(&root.join("untracked.txt")),
            "newer untracked\n"
        );
    }

    /// Executability is part of Git's worktree identity even when the blob
    /// bytes are unchanged. A confirmation captured before chmod must not be
    /// allowed to erase the newly observed mode change.
    #[cfg(unix)]
    #[test]
    fn a_confirmed_snapshot_refuses_a_newer_executable_bit() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new();
        let root = fixture.directory("discard-snapshot-mode");
        initialize_repository(&root);
        let service = GitService::new(&root, &fixture.data_dir);
        let path = root.join("tracked.txt");
        fs::write(&path, "shown edit\n").unwrap();
        let snapshot = service.discard_snapshot(["tracked.txt"]).unwrap();

        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(permissions.mode() | 0o111);
        fs::set_permissions(&path, permissions).unwrap();

        assert!(matches!(
            service.restore_tracked_if_unchanged(
                ["tracked.txt"],
                TrackedRestoreSource::Index,
                &snapshot,
                &Cancellation::default(),
            ),
            Err(GitError::StaleDiscardSelection)
        ));
        assert_ne!(fs::metadata(&path).unwrap().permissions().mode() & 0o111, 0);
        assert_eq!(worktree_text(&path), "shown edit\n");
    }

    #[test]
    fn restoring_both_rename_paths_recreates_the_source_without_a_staged_deletion() {
        let fixture = Fixture::new();
        let root = fixture.directory("discard-rename");
        initialize_repository(&root);
        let service = GitService::new(&root, &fixture.data_dir);
        fs::rename(root.join("tracked.txt"), root.join("renamed.txt")).unwrap();
        service
            .stage(["tracked.txt", "renamed.txt"], &Cancellation::default())
            .unwrap();
        let paths = [PathBuf::from("tracked.txt"), PathBuf::from("renamed.txt")];
        let snapshot = service.discard_snapshot(&paths).unwrap();

        service
            .restore_tracked_if_unchanged(
                &paths,
                TrackedRestoreSource::Head,
                &snapshot,
                &Cancellation::default(),
            )
            .unwrap();

        assert!(root.join("tracked.txt").exists());
        assert!(!root.join("renamed.txt").exists());
        assert!(
            service
                .detailed_status(&Cancellation::default())
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    fn discarding_one_hunk_leaves_the_other_hunk_and_the_index_untouched() {
        let fixture = Fixture::new();
        let root = fixture.directory("discard-one-hunk");
        let repository = initialize_repository(&root);
        let original = (1..=20)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        fs::write(root.join("tracked.txt"), &original).unwrap();
        commit_all(&repository, "expand fixture");
        let changed = original
            .replace("line 2\n", "line two\n")
            .replace("line 18\n", "line eighteen\n");
        fs::write(root.join("tracked.txt"), changed).unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        assert_eq!(files[0].hunks.len(), 2);
        let selection = HunkSelection::new(&files[0], &files[0].hunks[0]);

        let outcome = service
            .discard_hunks(&[selection], &Cancellation::default())
            .unwrap();
        let content = worktree_text(&root.join("tracked.txt"));
        assert!(content.contains("line 2\n"));
        assert!(content.contains("line eighteen\n"));
        assert!(
            service
                .diff(DiffTarget::Staged, &DiffOptions::default())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            outcome.description.operation(),
            DiscardOperation::RestoreTrackedHunks { hunks: 1 }
        );
    }

    /// Worktree hunk discard is a real mutation, so it must travel through
    /// the same hermetic system-Git policy as whole-path restore.
    #[cfg(unix)]
    #[test]
    fn hunk_discard_applies_through_the_hermetic_git_runner() {
        let fixture = Fixture::new();
        let root = fixture.directory("discard-hunk-runner");
        initialize_repository(&root);
        fs::write(root.join("tracked.txt"), "working edit\n").unwrap();
        let invoked = fixture.root.path().join("hunk-apply-invoked");
        let shim = fixture.shim(
            "hunk-apply-git",
            &format!(
                "#!/bin/sh\n\
                 for argument in \"$@\"; do\n\
                   if [ \"$argument\" = apply ]; then\n\
                     test \"$GIT_TERMINAL_PROMPT\" = 0 || exit 91\n\
                     test \"$LC_ALL\" = C || exit 92\n\
                     test \"$GIT_EDITOR\" = harkness-has-no-editor || exit 93\n\
                     printf invoked > '{}'\n\
                   fi\n\
                 done\n\
                 exec git \"$@\"\n",
                invoked.display()
            ),
        );
        let service = GitService::new(&root, &fixture.data_dir).with_git_executable(shim);
        let files = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let selection = HunkSelection::new(&files[0], &files[0].hunks[0]);

        service
            .discard_hunks(&[selection], &Cancellation::default())
            .unwrap();

        assert_eq!(worktree_text(&root.join("tracked.txt")), "initial\n");
        assert_eq!(fs::read_to_string(invoked).unwrap(), "invoked");
    }

    #[test]
    fn discarding_one_line_leaves_other_lines_and_the_index_untouched() {
        let fixture = Fixture::new();
        let root = fixture.directory("discard-one-line");
        let repository = initialize_repository(&root);
        fs::write(root.join("tracked.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        commit_all(&repository, "expand line fixture");
        fs::write(
            root.join("tracked.txt"),
            "one\nfirst\ntwo\nthree\nsecond\nfour\n",
        )
        .unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let files = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let hunk = &files[0].hunks[0];
        let first = hunk
            .lines
            .iter()
            .find(|line| line.kind == DiffLineKind::Addition && line.content == b"first\n")
            .unwrap();
        let selection = LineSelection::new(&files[0], hunk, first);

        let outcome = service
            .discard_lines(&[selection], &Cancellation::default())
            .unwrap();

        assert_eq!(
            worktree_text(&root.join("tracked.txt")),
            "one\ntwo\nthree\nsecond\nfour\n"
        );
        assert!(
            service
                .diff(DiffTarget::Staged, &DiffOptions::default())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            outcome.description.operation(),
            DiscardOperation::RestoreTrackedLines { lines: 1, hunks: 1 }
        );
    }

    #[test]
    fn a_stale_hunk_and_an_unmerged_path_are_refused_without_moving_content() {
        let fixture = Fixture::new();
        let root = fixture.directory("discard-refusals");
        initialize_repository(&root);
        let service = GitService::new(&root, &fixture.data_dir);
        fs::write(root.join("tracked.txt"), "first edit\n").unwrap();
        let files = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap();
        let selection = HunkSelection::new(&files[0], &files[0].hunks[0]);
        fs::write(root.join("tracked.txt"), "newer edit\n").unwrap();
        assert!(matches!(
            service.discard_hunks(&[selection], &Cancellation::default()),
            Err(GitError::StaleHunkSelection { .. })
        ));
        assert_eq!(
            fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "newer edit\n"
        );

        git(&root, ["switch", "-c", "topic"]);
        fs::write(root.join("tracked.txt"), "topic\n").unwrap();
        git(&root, ["add", "tracked.txt"]);
        git(&root, ["commit", "-m", "topic edit"]);
        git(&root, ["switch", "main"]);
        fs::write(root.join("tracked.txt"), "main\n").unwrap();
        git(&root, ["add", "tracked.txt"]);
        git(&root, ["commit", "-m", "main edit"]);
        let merge = GitCommand::new(Path::new("git"), &root, GitAccess::LocalWrite)
            .args(["merge", "topic"])
            .run(&Cancellation::default());
        assert!(merge.is_err());
        let conflicted = fs::read_to_string(root.join("tracked.txt")).unwrap();

        assert!(matches!(
            service.restore_tracked(
                ["tracked.txt"],
                TrackedRestoreSource::Index,
                &Cancellation::default()
            ),
            Err(GitError::UnmergedDiscard { .. })
        ));
        assert_eq!(
            fs::read_to_string(root.join("tracked.txt")).unwrap(),
            conflicted
        );

        let conflicted_file = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap()
            .into_iter()
            .find(|file| file.change == crate::FileChange::Unmerged)
            .unwrap();
        let selection = HunkSelection::from_parts(
            conflicted_file.old_path,
            conflicted_file.new_path,
            conflicted_file.old_blob_id,
            conflicted_file.new_blob_id,
            conflicted_file.context_lines,
            (1, 1),
            (1, 1),
        );
        assert!(matches!(
            service.discard_hunks(&[selection], &Cancellation::default()),
            Err(GitError::UnmergedDiscard { .. })
        ));
        assert_eq!(
            fs::read_to_string(root.join("tracked.txt")).unwrap(),
            conflicted
        );
    }

    #[test]
    fn a_linked_worktree_lock_blocks_every_discard_operation() {
        let fixture = Fixture::new();
        let root = fixture.directory("discard-locked-parent");
        initialize_repository(&root);
        let linked = fixture.root.path().join("discard-locked-linked");
        git(
            &root,
            [
                "worktree",
                "add",
                "-b",
                "locked-topic",
                linked.to_str().unwrap(),
            ],
        );
        git(
            &root,
            [
                "worktree",
                "lock",
                "--reason",
                "agent is using it",
                linked.to_str().unwrap(),
            ],
        );
        fs::write(linked.join("tracked.txt"), "do not discard\n").unwrap();
        fs::write(linked.join("untracked.txt"), "do not delete\n").unwrap();
        let service = GitService::new(&linked, &fixture.data_dir);
        let file = service
            .diff(DiffTarget::Unstaged, &DiffOptions::default())
            .unwrap()
            .into_iter()
            .find(|file| file.new_path.as_deref() == Some(Path::new("tracked.txt")))
            .unwrap();
        let selection = HunkSelection::new(&file, &file.hunks[0]);

        assert!(matches!(
            service.restore_tracked(
                ["tracked.txt"],
                TrackedRestoreSource::Index,
                &Cancellation::default()
            ),
            Err(GitError::WorktreeLocked { reason, .. })
                if reason.as_deref() == Some("agent is using it")
        ));
        assert!(matches!(
            service.delete_untracked(["untracked.txt"], &Cancellation::default()),
            Err(GitError::WorktreeLocked { .. })
        ));
        assert!(matches!(
            service.discard_hunks(&[selection], &Cancellation::default()),
            Err(GitError::WorktreeLocked { .. })
        ));
        assert_eq!(
            fs::read_to_string(linked.join("tracked.txt")).unwrap(),
            "do not discard\n"
        );
        assert_eq!(
            fs::read_to_string(linked.join("untracked.txt")).unwrap(),
            "do not delete\n"
        );
    }
}
