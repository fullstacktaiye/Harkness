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
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommitOptions {
    /// Replace the current commit instead of creating a child commit.
    pub amend: bool,
    /// Permit a commit whose tree is identical to its parent.
    pub allow_empty: bool,
}

/// What one successful commit produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitOutcome {
    /// The full object ID of the new commit.
    pub commit_id: String,
    /// Whether the new commit replaced the previous `HEAD`.
    pub amended: bool,
    /// Per-path state after the commit completed.
    pub status: DetailedStatus,
}

pub(crate) fn stage(
    git_executable: &Path,
    root: &Path,
    _lock: &RepositoryLock,
    paths: &[PathBuf],
    cancellation: &Cancellation,
) -> Result<DetailedStatus, GitError> {
    validate_paths(root, paths)?;
    if !paths.is_empty() {
        GitCommand::new(git_executable, root, GitAccess::LocalWrite)
            // Explicit paths are literal filesystem names, not pathspec
            // patterns. This also prevents a name beginning with `:(` from
            // gaining pathspec-magic semantics.
            .args(["--literal-pathspecs", "add", "--all", "--"])
            .args(paths.iter().map(|path| path.as_os_str()))
            .run(cancellation)?;
    }
    status::detailed(git_executable, root, cancellation)
}

pub(crate) fn stage_all(
    git_executable: &Path,
    root: &Path,
    _lock: &RepositoryLock,
    cancellation: &Cancellation,
) -> Result<DetailedStatus, GitError> {
    GitCommand::new(git_executable, root, GitAccess::LocalWrite)
        .args(["add", "--all"])
        .run(cancellation)?;
    status::detailed(git_executable, root, cancellation)
}

pub(crate) fn unstage(
    git_executable: &Path,
    root: &Path,
    _lock: &RepositoryLock,
    paths: &[PathBuf],
    cancellation: &Cancellation,
) -> Result<DetailedStatus, GitError> {
    validate_paths(root, paths)?;
    if paths.is_empty() {
        return status::detailed(git_executable, root, cancellation);
    }

    let repository = open(root)?;
    let command =
        GitCommand::new(git_executable, root, GitAccess::LocalWrite).arg("--literal-pathspecs");
    let command = if is_unborn(&repository, root)? {
        // `restore --staged` needs a HEAD tree. Removing an entry only from the
        // index is its unborn-branch equivalent; force is safe here because
        // `--cached` leaves the working tree untouched and every path is
        // explicit and already validated.
        command.args(["rm", "--cached", "-r", "--force", "--ignore-unmatch", "--"])
    } else {
        command.args(["restore", "--staged", "--"])
    };
    command
        .args(paths.iter().map(|path| path.as_os_str()))
        .run(cancellation)?;
    status::detailed(git_executable, root, cancellation)
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
        status: status::detailed(git_executable, root, cancellation)?,
    })
}

/// Resolves every path before any Git command is built.
fn validate_paths(root: &Path, paths: &[PathBuf]) -> Result<(), GitError> {
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

fn open(root: &Path) -> Result<Repository, GitError> {
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
            Cancellation, CommitOptions, FileChange, GitError, GitService, HeadState, StatusEntry,
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
        let tracked = entry(&staged.entries, Path::new("tracked.txt"));
        assert_eq!(tracked.staged, Some(FileChange::Modified));
        assert_eq!(tracked.unstaged, None);

        fs::write(root.join("tracked.txt"), "working-tree version\n").unwrap();
        let both = service.detailed_status(&cancellation).unwrap();
        let tracked = entry(&both.entries, Path::new("tracked.txt"));
        assert_eq!(tracked.staged, Some(FileChange::Modified));
        assert_eq!(tracked.unstaged, Some(FileChange::Modified));

        let unstaged = service.unstage(["tracked.txt"], &cancellation).unwrap();
        let tracked = entry(&unstaged.entries, Path::new("tracked.txt"));
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
        let renamed = entry(&staged.entries, Path::new("renamed.txt"));
        assert_eq!(renamed.staged, Some(FileChange::Renamed));
        assert_eq!(
            renamed.rename_source.as_deref(),
            Some(Path::new("tracked.txt"))
        );

        let unstaged = service
            .unstage(["tracked.txt", "renamed.txt"], &cancellation)
            .unwrap();
        assert!(unstaged.entries.iter().all(|entry| entry.staged.is_none()));
        assert_eq!(
            entry(&unstaged.entries, Path::new("tracked.txt")).unstaged,
            Some(FileChange::Deleted)
        );
        assert_eq!(
            entry(&unstaged.entries, Path::new("renamed.txt")).unstaged,
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
        assert!(matches!(staged.head, HeadState::Unborn { .. }));
        assert_eq!(
            entry(&staged.entries, Path::new("first.txt")).staged,
            Some(FileChange::Added)
        );
        // Prove the cached-removal path also works when the staged content no
        // longer matches the working tree.
        fs::write(root.join("first.txt"), "changed after staging\n").unwrap();

        let unstaged = service.unstage(["first.txt"], &cancellation).unwrap();

        let first = entry(&unstaged.entries, Path::new("first.txt"));
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
                &CommitOptions {
                    amend: true,
                    allow_empty: false,
                },
                &Cancellation::default(),
            ),
            Err(GitError::AmendUnbornBranch)
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
        assert!(committed.status.entries.is_empty());
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
                &CommitOptions {
                    amend: true,
                    allow_empty: false,
                },
                &cancellation,
            )
            .unwrap();
        assert!(amended.amended);
        assert_ne!(amended.commit_id, committed.commit_id);
        assert!(amended.status.entries.is_empty());
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
                &CommitOptions {
                    amend: false,
                    allow_empty: true,
                },
                &Cancellation::default(),
            )
            .unwrap();

        assert!(!outcome.amended);
        assert!(outcome.status.entries.is_empty());
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
            let entry = entry(&status.entries, Path::new(path));
            assert_eq!(entry.staged, Some(FileChange::Added));
            assert_eq!(entry.unstaged, None);
        }
    }

    #[cfg(unix)]
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

        let staged = entry(&status.entries, &path);
        assert_eq!(staged.staged, Some(FileChange::Added));
        assert_eq!(staged.unstaged, None);
    }

    fn entry<'a>(entries: &'a [StatusEntry], path: &Path) -> &'a StatusEntry {
        entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap_or_else(|| panic!("status did not include '{}': {entries:?}", path.display()))
    }
}
