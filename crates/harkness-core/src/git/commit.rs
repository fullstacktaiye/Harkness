//! Path-level staging and commits.
//!
//! The index is the boundary between work in a Harkness workspace and history
//! that can be reviewed. This module keeps every mutation behind the shared
//! repository lock and every process behind the shared runner. It also keeps
//! path validation in process: Git never sees a path until Harkness has proved
//! that it resolves within the addressed working tree.

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use git2::{ErrorCode, Repository, Status, StatusOptions};

use crate::git::{
    DetailedStatus, GitError, RepositoryLock,
    runner::{Cancellation, GitAccess, GitCommand},
    status,
};

/// What one commit should do.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct CommitOptions {
    /// Replace the current commit instead of creating a child commit.
    pub amend: bool,
    /// Permit a commit whose tree is identical to its parent.
    pub allow_empty: bool,
    /// Refresh the full repository status after the commit.
    ///
    /// Disable this when the caller will refresh separately. The default is
    /// `true` for callers that want one self-contained operation.
    pub refresh_status: bool,
}

impl Default for CommitOptions {
    fn default() -> Self {
        Self {
            amend: false,
            allow_empty: false,
            refresh_status: true,
        }
    }
}

impl CommitOptions {
    /// Sets whether the current commit should be replaced.
    #[must_use]
    pub fn with_amend(mut self, amend: bool) -> Self {
        self.amend = amend;
        self
    }

    /// Sets whether an unchanged tree may be committed.
    #[must_use]
    pub fn with_allow_empty(mut self, allow_empty: bool) -> Self {
        self.allow_empty = allow_empty;
        self
    }

    /// Sets whether the commit should end with a full status refresh.
    #[must_use]
    pub fn with_status_refresh(mut self, refresh_status: bool) -> Self {
        self.refresh_status = refresh_status;
        self
    }
}

/// What path staging and unstaging should do after updating the index.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StageOptions {
    /// Refresh the full repository status after all requested paths have run.
    ///
    /// Disable this when a caller is applying several user actions before one
    /// explicit [`crate::git::GitService::detailed_status`] call.
    pub refresh_status: bool,
}

impl Default for StageOptions {
    fn default() -> Self {
        Self {
            refresh_status: true,
        }
    }
}

/// What happened when Git was asked to update one explicit path.
#[derive(Debug)]
#[non_exhaustive]
pub enum StagePathResult {
    /// Git accepted and completed the requested index update.
    Succeeded,
    /// Git rejected or could not complete this path's index update.
    Failed(GitError),
    /// A non-path-local failure stopped the operation before this path ran.
    NotAttempted,
}

/// The result of staging or unstaging one requested path.
#[derive(Debug)]
pub struct StagePathOutcome {
    /// The path exactly as supplied by the caller.
    pub path: PathBuf,
    /// Whether Git updated this path, rejected it, or never attempted it.
    pub result: StagePathResult,
}

impl StagePathOutcome {
    /// Whether Git completed the requested update for this path.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        matches!(self.result, StagePathResult::Succeeded)
    }
}

/// The result of an optional full-repository status refresh.
#[derive(Debug)]
#[non_exhaustive]
pub enum StatusRefreshOutcome {
    /// The caller opted out of the refresh.
    Skipped,
    /// The refresh completed and returned the repository's current state.
    Refreshed(DetailedStatus),
    /// The index or commit mutation completed, but its follow-up refresh did
    /// not. Keeping this in the outcome prevents a refresh failure from hiding
    /// a mutation that already happened.
    Failed(GitError),
}

impl StatusRefreshOutcome {
    /// The refreshed status, when one was requested and completed.
    #[must_use]
    pub fn status(&self) -> Option<&DetailedStatus> {
        match self {
            Self::Refreshed(status) => Some(status),
            Self::Skipped | Self::Failed(_) => None,
        }
    }

    /// The refresh error, when the requested refresh failed.
    #[must_use]
    pub fn error(&self) -> Option<&GitError> {
        match self {
            Self::Failed(error) => Some(error),
            Self::Skipped | Self::Refreshed(_) => None,
        }
    }
}

/// What explicit path staging or unstaging produced.
#[derive(Debug)]
pub struct StageOutcome {
    /// One result for every path supplied by the caller, in input order.
    pub paths: Vec<StagePathOutcome>,
    /// The optional full-repository status refresh performed after all paths.
    pub status: StatusRefreshOutcome,
}

impl StageOutcome {
    /// Whether Git completed the requested update for every path.
    #[must_use]
    pub fn all_succeeded(&self) -> bool {
        self.paths.iter().all(StagePathOutcome::succeeded)
    }
}

/// What one successful commit produced.
#[derive(Debug)]
pub struct CommitOutcome {
    /// The full object ID of the new commit.
    pub commit_id: String,
    /// Whether the new commit replaced the previous `HEAD`.
    pub amended: bool,
    /// The optional full-repository status refresh after the commit completed.
    pub status: StatusRefreshOutcome,
}

pub(crate) fn stage(
    git_executable: &Path,
    root: &Path,
    _lock: &RepositoryLock,
    paths: &[PathBuf],
    options: &StageOptions,
    cancellation: &Cancellation,
) -> Result<StageOutcome, GitError> {
    validate_paths(root, paths)?;
    open(root)?;
    let path_outcomes = run_paths(paths, cancellation, |path| {
        GitCommand::new(git_executable, root, GitAccess::LocalWrite)
            // Explicit paths are literal filesystem names, not pathspec
            // patterns. This also prevents a name beginning with `:(` from
            // gaining pathspec-magic semantics.
            .args(["--literal-pathspecs", "add", "--all", "--"])
            .arg(path.as_os_str())
            .run(cancellation)
            .map(|_| ())
    });
    Ok(StageOutcome {
        paths: path_outcomes,
        status: refresh_status(git_executable, root, options.refresh_status, cancellation),
    })
}

pub(crate) fn stage_all(
    git_executable: &Path,
    root: &Path,
    _lock: &RepositoryLock,
    options: &StageOptions,
    cancellation: &Cancellation,
) -> Result<StatusRefreshOutcome, GitError> {
    open(root)?;
    GitCommand::new(git_executable, root, GitAccess::LocalWrite)
        .args(["add", "--all"])
        .run(cancellation)?;
    Ok(refresh_status(
        git_executable,
        root,
        options.refresh_status,
        cancellation,
    ))
}

pub(crate) fn unstage(
    git_executable: &Path,
    root: &Path,
    _lock: &RepositoryLock,
    paths: &[PathBuf],
    options: &StageOptions,
    cancellation: &Cancellation,
) -> Result<StageOutcome, GitError> {
    validate_paths(root, paths)?;
    let repository = open(root)?;
    let unborn = is_unborn(&repository, root)?;
    let path_outcomes = run_paths(paths, cancellation, |path| {
        let command =
            GitCommand::new(git_executable, root, GitAccess::LocalWrite).arg("--literal-pathspecs");
        let command = if unborn {
            // `restore --staged` needs a HEAD tree. Removing an entry only from
            // the index is its unborn-branch equivalent; force is safe here
            // because `--cached` leaves the working tree untouched and every
            // path is explicit and already validated.
            command.args(["rm", "--cached", "-r", "--force", "--ignore-unmatch", "--"])
        } else {
            command.args(["restore", "--staged", "--"])
        };
        command.arg(path.as_os_str()).run(cancellation).map(|_| ())
    });
    Ok(StageOutcome {
        paths: path_outcomes,
        status: refresh_status(git_executable, root, options.refresh_status, cancellation),
    })
}

pub(crate) fn commit(
    git_executable: &Path,
    root: &Path,
    _lock: &RepositoryLock,
    message: &str,
    options: &CommitOptions,
    cancellation: &Cancellation,
) -> Result<CommitOutcome, GitError> {
    if message.trim().is_empty() {
        return Err(GitError::EmptyCommitMessage);
    }

    let repository = open(root)?;
    if let Some(pending) = status::pending(&repository) {
        return Err(GitError::OperationInProgress {
            path: root.to_path_buf(),
            pending,
        });
    }
    let unborn = is_unborn(&repository, root)?;
    if options.amend && unborn {
        return Err(GitError::AmendUnbornBranch);
    }
    if !options.allow_empty && !has_staged_changes(&repository, root)? {
        return Err(GitError::NothingStaged);
    }

    let mut command = GitCommand::new(git_executable, root, GitAccess::LocalWrite).arg("commit");
    if options.amend {
        command = command.arg("--amend");
    }
    if options.allow_empty {
        command = command.arg("--allow-empty");
    }
    command.args(["--message", message]).run(cancellation)?;

    let repository = open(root)?;
    let commit_id = repository
        .head()
        .and_then(|head| head.peel_to_commit())
        .map(|commit| commit.id().to_string())
        .map_err(|source| inspection(root, source))?;
    Ok(CommitOutcome {
        commit_id,
        amended: options.amend,
        status: refresh_status(git_executable, root, options.refresh_status, cancellation),
    })
}

/// Runs path-local commands independently so one rejected path cannot hide
/// successful mutations to the paths before or after it.
fn run_paths(
    paths: &[PathBuf],
    cancellation: &Cancellation,
    mut run: impl FnMut(&Path) -> Result<(), GitError>,
) -> Vec<StagePathOutcome> {
    let mut outcomes = Vec::with_capacity(paths.len());
    let mut stopped = false;
    for path in paths {
        let result = if stopped {
            StagePathResult::NotAttempted
        } else if cancellation.is_cancelled() {
            stopped = true;
            StagePathResult::Failed(GitError::Cancelled)
        } else {
            match run(path) {
                Ok(()) => StagePathResult::Succeeded,
                Err(error) => {
                    // A normal Git rejection can be path-local, such as an
                    // explicitly named ignored file. Runner failures are not:
                    // retrying more paths after a launch failure, cancellation,
                    // or timeout would only compound the original problem.
                    stopped = !matches!(error, GitError::Failed { .. });
                    StagePathResult::Failed(error)
                }
            }
        };
        outcomes.push(StagePathOutcome {
            path: path.clone(),
            result,
        });
    }
    outcomes
}

fn refresh_status(
    git_executable: &Path,
    root: &Path,
    enabled: bool,
    cancellation: &Cancellation,
) -> StatusRefreshOutcome {
    if !enabled {
        return StatusRefreshOutcome::Skipped;
    }
    if cancellation.is_cancelled() {
        return StatusRefreshOutcome::Failed(GitError::Cancelled);
    }
    match status::detailed(git_executable, root, cancellation) {
        Ok(status) => StatusRefreshOutcome::Refreshed(status),
        Err(error) => StatusRefreshOutcome::Failed(error),
    }
}

/// Resolves every path before any Git command is built.
pub(crate) fn validate_paths(root: &Path, paths: &[PathBuf]) -> Result<(), GitError> {
    let repository = fs::canonicalize(root).map_err(|_| GitError::NotARepository {
        path: root.to_path_buf(),
    })?;
    for path in paths {
        let candidate = if path.is_absolute() {
            path.clone()
        } else {
            root.join(path)
        };
        let resolved = canonicalize_with_missing_tail(&candidate);
        if resolved
            .as_ref()
            .is_none_or(|resolved| !resolved.starts_with(&repository))
        {
            return Err(GitError::PathOutsideRepository {
                path: path.clone(),
                repository: root.to_path_buf(),
            });
        }
    }
    Ok(())
}

/// Canonicalizes the existing prefix and lexically restores a missing tail.
///
/// Deleted tracked files have to be valid staging targets, so requiring the
/// complete path to exist would reject exactly the deletion `git add` needs to
/// record. Resolving the nearest existing ancestor still catches `..` and
/// symlink escapes while permitting that absent leaf.
fn canonicalize_with_missing_tail(path: &Path) -> Option<PathBuf> {
    let mut current = path;
    let mut missing = Vec::<OsString>::new();
    loop {
        if let Ok(mut resolved) = fs::canonicalize(current) {
            for component in missing.iter().rev() {
                resolved.push(component);
            }
            return Some(resolved);
        }
        missing.push(current.file_name()?.to_os_string());
        current = current.parent()?;
    }
}

fn has_staged_changes(repository: &Repository, root: &Path) -> Result<bool, GitError> {
    let mut options = StatusOptions::new();
    options
        .include_untracked(false)
        .recurse_untracked_dirs(false)
        .include_ignored(false);
    let statuses = repository
        .statuses(Some(&mut options))
        .map_err(|source| inspection(root, source))?;
    Ok(statuses.iter().any(|entry| {
        entry.status().intersects(
            Status::INDEX_NEW
                | Status::INDEX_MODIFIED
                | Status::INDEX_DELETED
                | Status::INDEX_RENAMED
                | Status::INDEX_TYPECHANGE,
        )
    }))
}

fn is_unborn(repository: &Repository, root: &Path) -> Result<bool, GitError> {
    match repository.head() {
        Ok(head) => Ok(head.target().is_none()),
        Err(error) if matches!(error.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => {
            Ok(true)
        }
        Err(source) => Err(inspection(root, source)),
    }
}

pub(crate) fn open(root: &Path) -> Result<Repository, GitError> {
    match Repository::open(root) {
        Ok(repository) if repository.workdir().is_some() => Ok(repository),
        Ok(_) => Err(GitError::NotARepository {
            path: root.to_path_buf(),
        }),
        Err(error) if error.code() == ErrorCode::NotFound => Err(GitError::NotARepository {
            path: root.to_path_buf(),
        }),
        Err(source) => Err(inspection(root, source)),
    }
}

fn inspection(root: &Path, source: git2::Error) -> GitError {
    GitError::Inspection {
        path: root.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use git2::Repository;

    use crate::{
        git::{
            Cancellation, CommitOptions, DetailedStatus, FileChange, GitError, GitService,
            HeadState, PendingOperation, StageOptions, StagePathResult, StatusEntry,
            StatusRefreshOutcome,
        },
        testing::{
            Fixture, PROCESS_PROJECT_ROOT_ENV, commit_all, configure_commit_identity,
            initialize_repository, spawn_child,
        },
    };

    #[test]
    fn explicit_stage_and_unstage_move_one_path_across_the_index() {
        let fixture = Fixture::new();
        let root = fixture.directory("stage-one-path");
        initialize_repository(&root);
        let service = GitService::new(&root, &fixture.data_dir);
        let cancellation = Cancellation::default();

        fs::write(root.join("tracked.txt"), "staged version\n").unwrap();
        let staged = service.stage(["tracked.txt"], &cancellation).unwrap();
        let tracked = entry(&refreshed(&staged.status).entries, Path::new("tracked.txt"));
        assert_eq!(tracked.staged, Some(FileChange::Modified));
        assert_eq!(tracked.unstaged, None);

        fs::write(root.join("tracked.txt"), "working-tree version\n").unwrap();
        let both = service.detailed_status(&cancellation).unwrap();
        let tracked = entry(&both.entries, Path::new("tracked.txt"));
        assert_eq!(tracked.staged, Some(FileChange::Modified));
        assert_eq!(tracked.unstaged, Some(FileChange::Modified));

        let unstaged = service.unstage(["tracked.txt"], &cancellation).unwrap();
        let tracked = entry(
            &refreshed(&unstaged.status).entries,
            Path::new("tracked.txt"),
        );
        assert_eq!(tracked.staged, None);
        assert_eq!(tracked.unstaged, Some(FileChange::Modified));
    }

    #[test]
    fn stage_all_includes_additions_modifications_and_deletions() {
        let fixture = Fixture::new();
        let root = fixture.directory("stage-all");
        let repository = initialize_repository(&root);
        fs::write(root.join("deleted.txt"), "delete me\n").unwrap();
        commit_all(&repository, "add deletion fixture");
        let service = GitService::new(&root, &fixture.data_dir);

        fs::write(root.join("tracked.txt"), "modified\n").unwrap();
        fs::remove_file(root.join("deleted.txt")).unwrap();
        fs::write(root.join("added.txt"), "added\n").unwrap();
        let status = service.stage_all(&Cancellation::default()).unwrap();

        assert_eq!(
            entry(&status.entries, Path::new("tracked.txt")).staged,
            Some(FileChange::Modified)
        );
        assert_eq!(
            entry(&status.entries, Path::new("deleted.txt")).staged,
            Some(FileChange::Deleted)
        );
        assert_eq!(
            entry(&status.entries, Path::new("added.txt")).staged,
            Some(FileChange::Added)
        );
        assert!(status.entries.iter().all(|entry| entry.unstaged.is_none()));
    }

    #[test]
    fn explicit_paths_report_partial_failures_without_hiding_successes() {
        let fixture = Fixture::new();
        let root = fixture.directory("partial-stage");
        initialize_repository(&root);
        fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(root.join("tracked.txt"), "staged despite sibling failure\n").unwrap();
        fs::write(root.join("ignored.txt"), "ignored\n").unwrap();
        let service = GitService::new(&root, &fixture.data_dir);

        let outcome = service
            .stage(["tracked.txt", "ignored.txt"], &Cancellation::default())
            .unwrap();

        assert!(!outcome.all_succeeded());
        assert!(matches!(
            outcome.paths[0].result,
            StagePathResult::Succeeded
        ));
        assert!(matches!(
            outcome.paths[1].result,
            StagePathResult::Failed(GitError::Failed { .. })
        ));
        assert_eq!(outcome.paths[0].path, Path::new("tracked.txt"));
        assert_eq!(outcome.paths[1].path, Path::new("ignored.txt"));
        assert_eq!(
            entry(
                &refreshed(&outcome.status).entries,
                Path::new("tracked.txt")
            )
            .staged,
            Some(FileChange::Modified)
        );
    }

    #[test]
    fn stage_unstage_stage_all_and_commit_can_skip_the_full_refresh() {
        let fixture = Fixture::new();
        let root = fixture.directory("optional-refresh");
        initialize_repository(&root);
        let service = GitService::new(&root, &fixture.data_dir);
        let no_refresh = StageOptions {
            refresh_status: false,
        };
        let cancellation = Cancellation::default();

        fs::write(root.join("tracked.txt"), "first staged version\n").unwrap();
        let staged = service
            .stage_with_options(["tracked.txt"], &no_refresh, &cancellation)
            .unwrap();
        assert!(staged.all_succeeded());
        assert!(matches!(staged.status, StatusRefreshOutcome::Skipped));
        assert_eq!(
            entry(
                &service.detailed_status(&cancellation).unwrap().entries,
                Path::new("tracked.txt")
            )
            .staged,
            Some(FileChange::Modified)
        );

        let unstaged = service
            .unstage_with_options(["tracked.txt"], &no_refresh, &cancellation)
            .unwrap();
        assert!(unstaged.all_succeeded());
        assert!(matches!(unstaged.status, StatusRefreshOutcome::Skipped));
        assert_eq!(
            entry(
                &service.detailed_status(&cancellation).unwrap().entries,
                Path::new("tracked.txt")
            )
            .staged,
            None
        );

        let staged_all = service
            .stage_all_with_options(&no_refresh, &cancellation)
            .unwrap();
        assert!(matches!(staged_all, StatusRefreshOutcome::Skipped));

        let commit_options = CommitOptions::default().with_status_refresh(false);
        let committed = service
            .commit("skip status refresh", &commit_options, &cancellation)
            .unwrap();
        assert!(matches!(committed.status, StatusRefreshOutcome::Skipped));
        assert_eq!(
            Repository::open(&root)
                .unwrap()
                .head()
                .unwrap()
                .target()
                .unwrap()
                .to_string(),
            committed.commit_id
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_refresh_failure_does_not_hide_a_completed_mutation() {
        let fixture = Fixture::new();
        let root = fixture.directory("failed-refresh");
        initialize_repository(&root);
        let git = fixture.shim(
            "git-with-failed-status",
            "#!/bin/sh\n\
             for argument in \"$@\"; do\n\
               if [ \"$argument\" = status ]; then\n\
                 echo 'status intentionally failed' >&2\n\
                 exit 1\n\
               fi\n\
             done\n\
             exec git \"$@\"\n",
        );
        let service = GitService::new(&root, &fixture.data_dir).with_git_executable(git);
        let cancellation = Cancellation::default();

        fs::write(root.join("tracked.txt"), "staged before refresh fails\n").unwrap();
        let staged = service.stage(["tracked.txt"], &cancellation).unwrap();
        assert!(staged.all_succeeded());
        assert!(matches!(
            staged.status,
            StatusRefreshOutcome::Failed(GitError::Failed { .. })
        ));
        assert_eq!(
            entry(
                &GitService::new(&root, &fixture.data_dir)
                    .detailed_status(&cancellation)
                    .unwrap()
                    .entries,
                Path::new("tracked.txt")
            )
            .staged,
            Some(FileChange::Modified)
        );

        let committed = service
            .commit(
                "commit survives refresh failure",
                &CommitOptions::default(),
                &cancellation,
            )
            .unwrap();
        assert!(matches!(
            committed.status,
            StatusRefreshOutcome::Failed(GitError::Failed { .. })
        ));
        assert_eq!(
            Repository::open(&root)
                .unwrap()
                .head()
                .unwrap()
                .target()
                .unwrap()
                .to_string(),
            committed.commit_id
        );
    }

    #[test]
    fn renamed_paths_keep_using_detailed_status() {
        let fixture = Fixture::new();
        let root = fixture.directory("staged-rename");
        initialize_repository(&root);
        let service = GitService::new(&root, &fixture.data_dir);
        let cancellation = Cancellation::default();

        fs::rename(root.join("tracked.txt"), root.join("renamed.txt")).unwrap();
        let staged = service
            .stage(["tracked.txt", "renamed.txt"], &cancellation)
            .unwrap();
        let renamed = entry(&refreshed(&staged.status).entries, Path::new("renamed.txt"));
        assert_eq!(renamed.staged, Some(FileChange::Renamed));
        assert_eq!(
            renamed.rename_source.as_deref(),
            Some(Path::new("tracked.txt"))
        );

        let unstaged = service
            .unstage(["tracked.txt", "renamed.txt"], &cancellation)
            .unwrap();
        let unstaged_status = refreshed(&unstaged.status);
        assert!(
            unstaged_status
                .entries
                .iter()
                .all(|entry| entry.staged.is_none())
        );
        assert_eq!(
            entry(&unstaged_status.entries, Path::new("tracked.txt")).unstaged,
            Some(FileChange::Deleted)
        );
        assert_eq!(
            entry(&unstaged_status.entries, Path::new("renamed.txt")).unstaged,
            Some(FileChange::Untracked)
        );
    }

    #[test]
    fn an_outside_path_is_refused_before_git_is_spawned() {
        let fixture = Fixture::new();
        let root = fixture.directory("outside-path");
        initialize_repository(&root);
        fs::write(fixture.root.path().join("outside.txt"), "outside\n").unwrap();
        let service = GitService::new(&root, &fixture.data_dir)
            .with_git_executable(fixture.root.path().join("does-not-exist"));

        let error = service
            .stage(["../outside.txt"], &Cancellation::default())
            .unwrap_err();

        assert!(
            matches!(error, GitError::PathOutsideRepository { ref path, .. }
                if path == Path::new("../outside.txt")),
            "{error:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_escape_is_refused_before_git_is_spawned() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let root = fixture.directory("symlink-escape");
        initialize_repository(&root);
        let outside = fixture.root.path().join("outside-through-link.txt");
        fs::write(&outside, "outside\n").unwrap();
        symlink(&outside, root.join("linked.txt")).unwrap();
        let service = GitService::new(&root, &fixture.data_dir)
            .with_git_executable(fixture.root.path().join("does-not-exist"));

        let error = service
            .stage(["linked.txt"], &Cancellation::default())
            .unwrap_err();

        assert!(matches!(error, GitError::PathOutsideRepository { .. }));
    }

    #[test]
    fn unstaging_an_unborn_branch_removes_only_the_index_entry() {
        let fixture = Fixture::new();
        let root = fixture.directory("unborn-unstage");
        let repository = Repository::init(&root).unwrap();
        repository.set_head("refs/heads/main").unwrap();
        configure_commit_identity(&repository);
        drop(repository);
        fs::write(root.join("first.txt"), "staged\n").unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let cancellation = Cancellation::default();

        let staged = service.stage(["first.txt"], &cancellation).unwrap();
        let staged_status = refreshed(&staged.status);
        assert!(matches!(staged_status.head, HeadState::Unborn { .. }));
        assert_eq!(
            entry(&staged_status.entries, Path::new("first.txt")).staged,
            Some(FileChange::Added)
        );
        // Prove the cached-removal path also works when the staged content no
        // longer matches the working tree.
        fs::write(root.join("first.txt"), "changed after staging\n").unwrap();

        let unstaged = service.unstage(["first.txt"], &cancellation).unwrap();

        let first = entry(&refreshed(&unstaged.status).entries, Path::new("first.txt"));
        assert_eq!(first.staged, None);
        assert_eq!(first.unstaged, Some(FileChange::Untracked));
        assert_eq!(
            fs::read_to_string(root.join("first.txt")).unwrap(),
            "changed after staging\n"
        );
    }

    #[test]
    fn commit_refusals_are_distinct_and_happen_before_git_is_spawned() {
        let fixture = Fixture::new();
        let root = fixture.directory("commit-refusals");
        initialize_repository(&root);
        let missing_git = fixture.root.path().join("does-not-exist");
        let service = GitService::new(&root, &fixture.data_dir).with_git_executable(&missing_git);

        assert!(matches!(
            service.commit("  \n", &CommitOptions::default(), &Cancellation::default()),
            Err(GitError::EmptyCommitMessage)
        ));
        assert!(matches!(
            service.commit(
                "nothing",
                &CommitOptions::default(),
                &Cancellation::default()
            ),
            Err(GitError::NothingStaged)
        ));

        let unborn_root = fixture.directory("unborn-amend");
        let repository = Repository::init(&unborn_root).unwrap();
        repository.set_head("refs/heads/main").unwrap();
        configure_commit_identity(&repository);
        drop(repository);
        fs::write(unborn_root.join("first.txt"), "first\n").unwrap();
        GitService::new(&unborn_root, &fixture.data_dir)
            .stage(["first.txt"], &Cancellation::default())
            .unwrap();
        let unborn =
            GitService::new(&unborn_root, &fixture.data_dir).with_git_executable(missing_git);

        assert!(matches!(
            unborn.commit(
                "cannot amend",
                &CommitOptions::default().with_amend(true),
                &Cancellation::default(),
            ),
            Err(GitError::AmendUnbornBranch)
        ));
    }

    #[test]
    fn detailed_status_exposes_a_pending_operation_and_commit_refuses_it() {
        let fixture = Fixture::new();
        let root = fixture.directory("pending-commit");
        let repository = initialize_repository(&root);
        fs::write(root.join("tracked.txt"), "staged during merge\n").unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        service
            .stage(["tracked.txt"], &Cancellation::default())
            .unwrap();
        let head = repository.head().unwrap().target().unwrap();
        fs::write(root.join(".git/MERGE_HEAD"), format!("{head}\n")).unwrap();

        let status = service.detailed_status(&Cancellation::default()).unwrap();
        assert_eq!(status.pending, Some(PendingOperation::Merge));

        let missing_git = fixture.root.path().join("does-not-exist");
        let error = GitService::new(&root, &fixture.data_dir)
            .with_git_executable(missing_git)
            .commit(
                "must not finish an unknown merge",
                &CommitOptions::default(),
                &Cancellation::default(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            GitError::OperationInProgress {
                pending: PendingOperation::Merge,
                ..
            }
        ));
    }

    #[test]
    fn every_index_and_commit_mutation_takes_the_repository_lock() {
        let fixture = Fixture::new();
        let root = fixture.directory("locked-mutations");
        initialize_repository(&root);
        let service = GitService::new(&root, &fixture.data_dir);
        let _held = service.lock(&Cancellation::default()).unwrap();
        let cancelled = Cancellation::default();
        cancelled.cancel();

        assert!(matches!(
            service.stage(["tracked.txt"], &cancelled),
            Err(GitError::Cancelled)
        ));
        assert!(matches!(
            service.stage_all(&cancelled),
            Err(GitError::Cancelled)
        ));
        assert!(matches!(
            service.unstage(["tracked.txt"], &cancelled),
            Err(GitError::Cancelled)
        ));
        assert!(matches!(
            service.stage_hunks(&[], &cancelled),
            Err(GitError::Cancelled)
        ));
        assert!(matches!(
            service.unstage_hunks(&[], &cancelled),
            Err(GitError::Cancelled)
        ));
        assert!(matches!(
            service.commit("locked", &CommitOptions::default(), &cancelled),
            Err(GitError::Cancelled)
        ));
    }

    #[test]
    fn commit_and_amend_return_the_new_commit_and_per_path_status() {
        let fixture = Fixture::new();
        let root = fixture.directory("commit-outcome");
        let repository = initialize_repository(&root);
        let original = repository.head().unwrap().target().unwrap();
        let config = repository.config().unwrap();
        assert_eq!(config.get_string("user.name").unwrap(), "Harkness Tests");
        assert_eq!(
            config.get_string("user.email").unwrap(),
            "tests@harkness.invalid"
        );
        assert!(!config.get_bool("commit.gpgsign").unwrap());
        drop(config);
        drop(repository);
        let service = GitService::new(&root, &fixture.data_dir);
        let cancellation = Cancellation::default();

        fs::write(root.join("tracked.txt"), "committed\n").unwrap();
        service.stage(["tracked.txt"], &cancellation).unwrap();
        let committed = service
            .commit(
                "commit staged path",
                &CommitOptions::default(),
                &cancellation,
            )
            .unwrap();
        assert!(!committed.amended);
        assert!(refreshed(&committed.status).entries.is_empty());
        let repository = Repository::open(&root).unwrap();
        let commit = repository
            .find_commit(committed.commit_id.parse().unwrap())
            .unwrap();
        assert_eq!(commit.message().unwrap(), "commit staged path\n");
        assert_eq!(commit.parent_id(0).unwrap(), original);
        drop(commit);
        drop(repository);

        fs::write(root.join("tracked.txt"), "amended\n").unwrap();
        service.stage(["tracked.txt"], &cancellation).unwrap();
        let amended = service
            .commit(
                "amended commit",
                &CommitOptions::default().with_amend(true),
                &cancellation,
            )
            .unwrap();
        assert!(amended.amended);
        assert_ne!(amended.commit_id, committed.commit_id);
        assert!(refreshed(&amended.status).entries.is_empty());
        let repository = Repository::open(&root).unwrap();
        let amend = repository
            .find_commit(amended.commit_id.parse().unwrap())
            .unwrap();
        assert_eq!(amend.message().unwrap(), "amended commit\n");
        assert_eq!(amend.parent_id(0).unwrap(), original);
    }

    #[test]
    fn commit_fixture_isolated_from_the_developers_global_git_configuration() {
        let fixture = Fixture::new();
        let root = fixture.directory("isolated-commit-repository");
        let home = fixture.directory("isolated-home");
        let xdg_config = fixture.directory("isolated-xdg-config");

        let status = spawn_child(&fixture.data_dir, "commit-with-isolated-config")
            .env(PROCESS_PROJECT_ROOT_ENV, &root)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &xdg_config)
            .status()
            .unwrap();

        assert!(status.success(), "isolated commit child failed: {status}");
    }

    #[test]
    fn an_explicitly_empty_commit_is_allowed() {
        let fixture = Fixture::new();
        let root = fixture.directory("allow-empty");
        initialize_repository(&root);
        let service = GitService::new(&root, &fixture.data_dir);

        let outcome = service
            .commit(
                "intentional empty commit",
                &CommitOptions::default().with_allow_empty(true),
                &Cancellation::default(),
            )
            .unwrap();

        assert!(!outcome.amended);
        assert!(refreshed(&outcome.status).entries.is_empty());
        assert_eq!(
            Repository::open(&root)
                .unwrap()
                .head()
                .unwrap()
                .target()
                .unwrap()
                .to_string(),
            outcome.commit_id
        );
    }

    #[test]
    fn spaces_and_leading_dashes_are_literal_paths() {
        let fixture = Fixture::new();
        let root = fixture.directory("awkward-paths");
        initialize_repository(&root);
        fs::write(root.join("space name.txt"), "space\n").unwrap();
        fs::write(root.join("-option.txt"), "dash\n").unwrap();
        let service = GitService::new(&root, &fixture.data_dir);

        let status = service
            .stage(["space name.txt", "-option.txt"], &Cancellation::default())
            .unwrap();

        for path in ["space name.txt", "-option.txt"] {
            let entry = entry(&refreshed(&status.status).entries, Path::new(path));
            assert_eq!(entry.staged, Some(FileChange::Added));
            assert_eq!(entry.unstaged, None);
        }
    }

    /// macOS rejects an invalid UTF-8 filename at the filesystem boundary, so
    /// it cannot exercise the raw-byte path that Unix filesystems such as
    /// Linux's permit. Raw-byte status parsing remains covered on every Unix.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn non_utf8_paths_reach_git_without_a_string_round_trip() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt, path::PathBuf};

        let fixture = Fixture::new();
        let root = fixture.directory("non-utf8-path");
        initialize_repository(&root);
        let path = PathBuf::from(OsString::from_vec(vec![
            b'n', b'o', b'n', b'-', 0xff, b'.', b't', b'x', b't',
        ]));
        fs::write(root.join(&path), "raw bytes\n").unwrap();
        let service = GitService::new(&root, &fixture.data_dir);

        let status = service.stage([&path], &Cancellation::default()).unwrap();

        let staged = entry(&refreshed(&status.status).entries, &path);
        assert_eq!(staged.staged, Some(FileChange::Added));
        assert_eq!(staged.unstaged, None);
    }

    fn entry<'a>(entries: &'a [StatusEntry], path: &Path) -> &'a StatusEntry {
        entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap_or_else(|| panic!("status did not include '{}': {entries:?}", path.display()))
    }

    fn refreshed(outcome: &StatusRefreshOutcome) -> &DetailedStatus {
        outcome
            .status()
            .unwrap_or_else(|| panic!("status was not refreshed: {outcome:?}"))
    }
}
