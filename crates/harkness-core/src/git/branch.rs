//! Branch enumeration and lifecycle.
//!
//! Listing is an in-process walk of local refs. Mutations use system Git so
//! checkout safety and configuration stay identical to the user's command
//! line, but the decisions Git should not make on Harkness's behalf are
//! settled here first as typed refusals.

use std::path::{Path, PathBuf};

use git2::{BranchType, ErrorCode, Reference, Repository};

use crate::{
    catalog::entry::UpstreamStatus,
    git::{
        GitError, RepositoryLock,
        runner::{Cancellation, GitAccess, GitCommand},
    },
};

/// Which ref namespace a [`Branch`] came from.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum BranchKind {
    /// A branch under `refs/heads` that may be checked out and changed.
    Local,
    /// A locally cached view of a remote branch under `refs/remotes`.
    RemoteTracking,
}

/// One local or remote-tracking branch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Branch {
    /// The short ref name, such as `topic` or `origin/main`.
    pub name: String,
    /// Whether this is a local branch or a remote-tracking ref.
    pub kind: BranchKind,
    /// The commit at the tip, rendered as a full hexadecimal object ID.
    pub tip: String,
    /// The configured upstream and locally known divergence from it.
    ///
    /// Remote-tracking branches never have an upstream of their own.
    pub upstream: Option<UpstreamStatus>,
    /// Whether this branch is checked out in the addressed working tree.
    pub checked_out: bool,
}

/// What creating a branch should do.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CreateBranchOptions {
    /// Revision from which to create the branch. `None` means the current HEAD.
    pub start_point: Option<String>,
    /// Check out the new branch before returning.
    pub checkout: bool,
}

/// Lists local branches and, when requested, remote-tracking branches.
///
/// This is only a libgit2 ref walk. It never spawns Git, contacts a remote or
/// takes the repository lock.
pub(crate) fn branches(
    root: &Path,
    include_remote_tracking: bool,
) -> Result<Vec<Branch>, GitError> {
    let repository = open(root)?;
    let mut listed = collect(&repository, root, BranchType::Local)?;
    if include_remote_tracking {
        listed.extend(collect(&repository, root, BranchType::Remote)?);
    }
    listed.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(listed)
}

fn collect(
    repository: &Repository,
    root: &Path,
    branch_type: BranchType,
) -> Result<Vec<Branch>, GitError> {
    let mut listed = Vec::new();
    let branches = repository
        .branches(Some(branch_type))
        .map_err(|source| inspection(root, source))?;
    for branch in branches {
        let (branch, _) = branch.map_err(|source| inspection(root, source))?;
        // `origin/HEAD` is a symbolic alias for a default branch, not another
        // remote-tracking branch in its own right.
        if branch_type == BranchType::Remote
            && branch
                .get()
                .symbolic_target()
                .map_err(|source| inspection(root, source))?
                .is_some()
        {
            continue;
        }
        let name = branch
            .name()
            .map_err(|source| inspection(root, source))?
            .ok_or_else(|| inspection(root, git2::Error::from_str("branch name is not UTF-8")))?
            .to_owned();
        let tip = branch
            .get()
            .peel_to_commit()
            .map_err(|source| inspection(root, source))?
            .id();
        let upstream = if branch_type == BranchType::Local {
            upstream(repository, root, &branch, tip)?
        } else {
            None
        };
        listed.push(Branch {
            name,
            kind: match branch_type {
                BranchType::Local => BranchKind::Local,
                BranchType::Remote => BranchKind::RemoteTracking,
            },
            tip: tip.to_string(),
            upstream,
            checked_out: branch.is_head(),
        });
    }
    Ok(listed)
}

fn upstream(
    repository: &Repository,
    root: &Path,
    branch: &git2::Branch<'_>,
    local: git2::Oid,
) -> Result<Option<UpstreamStatus>, GitError> {
    let upstream = match branch.upstream() {
        Ok(upstream) => upstream,
        Err(error) if error.code() == ErrorCode::NotFound => return Ok(None),
        Err(source) => return Err(inspection(root, source)),
    };
    let name = upstream
        .name()
        .map_err(|source| inspection(root, source))?
        .ok_or_else(|| inspection(root, git2::Error::from_str("upstream name is not UTF-8")))?
        .to_owned();
    let tracked = match upstream.get().peel_to_commit() {
        Ok(commit) => commit.id(),
        // A configured upstream whose ref has not been fetched is still an
        // upstream. This matches detailed status, which reports zero locally
        // known divergence rather than dropping the relationship.
        Err(error) if error.code() == ErrorCode::NotFound => {
            return Ok(Some(UpstreamStatus {
                name,
                ahead: 0,
                behind: 0,
            }));
        }
        Err(source) => return Err(inspection(root, source)),
    };
    let (ahead, behind) = repository
        .graph_ahead_behind(local, tracked)
        .map_err(|source| inspection(root, source))?;
    Ok(Some(UpstreamStatus {
        name,
        ahead,
        behind,
    }))
}

/// Applies Git's `check-ref-format --branch` rules without spawning Git.
pub(crate) fn validate_name(name: &str) -> Result<(), GitError> {
    // `Reference::is_valid_name` implements check-ref-format for a full ref.
    // Its full-ref form deliberately permits a leading dash and the special
    // token HEAD, while `--branch` rejects both, so those two shorthand rules
    // are stated alongside it.
    let full = format!("refs/heads/{name}");
    if name.is_empty()
        || name.starts_with('-')
        || name == "HEAD"
        || !Reference::is_valid_name(&full)
    {
        return Err(GitError::InvalidBranchName {
            name: name.to_owned(),
        });
    }
    Ok(())
}

pub(crate) fn create(
    git_executable: &Path,
    root: &Path,
    _lock: &RepositoryLock,
    name: &str,
    options: &CreateBranchOptions,
    cancellation: &Cancellation,
) -> Result<(), GitError> {
    let repository = open(root)?;
    let start = options
        .start_point
        .as_deref()
        .map(|start| {
            repository
                .revparse_single(start)
                .and_then(|object| object.peel_to_commit())
                .map(|commit| commit.id().to_string())
                .map_err(|source| inspection(root, source))
        })
        .transpose()?;

    let mut command = GitCommand::new(git_executable, root, GitAccess::LocalWrite);
    if options.checkout {
        command = command.args(["checkout", "--no-track", "-b", name]);
        if let Some(start) = &start {
            command = command.arg(start);
        }
        command = command.arg("--");
    } else {
        command = command.args(["branch", "--no-track", "--", name]);
        if let Some(start) = &start {
            command = command.arg(start);
        }
    }
    command.run(cancellation)?;
    Ok(())
}

pub(crate) fn checkout(
    git_executable: &Path,
    root: &Path,
    _lock: &RepositoryLock,
    name: &str,
    cancellation: &Cancellation,
) -> Result<(), GitError> {
    open(root)?;
    GitCommand::new(git_executable, root, GitAccess::LocalWrite)
        // The trailing `--` makes this branch checkout rather than a path
        // checkout, while ordinary checkout keeps conflicting local work.
        .args(["checkout", name, "--"])
        .run(cancellation)?;
    Ok(())
}

pub(crate) fn delete(
    git_executable: &Path,
    root: &Path,
    _lock: &RepositoryLock,
    name: &str,
    force: bool,
    cancellation: &Cancellation,
) -> Result<(), GitError> {
    let repository = open(root)?;
    let full_name = format!("refs/heads/{name}");

    if head_names(&repository, &full_name)? {
        return Err(GitError::CurrentBranchDeletion {
            branch: name.to_owned(),
        });
    }
    if default_branch(&repository, root)?.as_deref() == Some(name) {
        return Err(GitError::DefaultBranchDeletion {
            branch: name.to_owned(),
        });
    }
    if let Some(worktree) = other_worktree(&repository, root, &full_name)? {
        return Err(GitError::BranchCheckedOutInWorktree {
            branch: name.to_owned(),
            worktree,
        });
    }
    if !force && has_unmerged_commits(&repository, root, name)? {
        return Err(GitError::UnmergedBranchDeletion {
            branch: name.to_owned(),
        });
    }

    GitCommand::new(git_executable, root, GitAccess::LocalWrite)
        .args(["branch", if force { "-D" } else { "-d" }, "--", name])
        .run(cancellation)?;
    Ok(())
}

pub(crate) fn set_upstream(
    git_executable: &Path,
    root: &Path,
    _lock: &RepositoryLock,
    branch: &str,
    upstream: Option<&str>,
    cancellation: &Cancellation,
) -> Result<(), GitError> {
    open(root)?;
    let command = GitCommand::new(git_executable, root, GitAccess::LocalWrite).arg("branch");
    let command = match upstream {
        Some(upstream) => command.arg(format!("--set-upstream-to={upstream}")),
        None => command.arg("--unset-upstream"),
    };
    command.args(["--", branch]).run(cancellation)?;
    Ok(())
}

fn head_names(repository: &Repository, full_name: &str) -> Result<bool, GitError> {
    match repository.head() {
        // Repository::head resolves symbolic HEAD to the branch reference, so
        // its own full name is the checked-out branch name.
        Ok(head) => head.name().map(|name| name == full_name).map_err(|source| {
            inspection(
                repository.workdir().unwrap_or_else(|| repository.path()),
                source,
            )
        }),
        Err(error) if matches!(error.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => {
            Ok(false)
        }
        Err(source) => Err(inspection(
            repository.workdir().unwrap_or_else(|| repository.path()),
            source,
        )),
    }
}

/// The local branch named by a recorded remote HEAD.
///
/// Origin wins, then a sole other remote. This deliberately stays local: a
/// branch deletion must not turn into an unbounded network operation.
fn default_branch(repository: &Repository, root: &Path) -> Result<Option<String>, GitError> {
    let remotes = repository
        .remotes()
        .map_err(|source| inspection(root, source))?;
    let mut names = Vec::new();
    for remote in remotes.iter() {
        let remote = remote.map_err(|source| inspection(root, source))?;
        if let Some(remote) = remote {
            names.push(remote);
        }
    }
    names.sort_unstable();
    let remote = if names.contains(&"origin") {
        Some("origin")
    } else if names.len() == 1 {
        names.first().copied()
    } else {
        None
    };
    let Some(remote) = remote else {
        return Ok(None);
    };
    let reference_name = format!("refs/remotes/{remote}/HEAD");
    let reference = match repository.find_reference(&reference_name) {
        Ok(reference) => reference,
        Err(error) if error.code() == ErrorCode::NotFound => return Ok(None),
        Err(source) => return Err(inspection(root, source)),
    };
    let prefix = format!("refs/remotes/{remote}/");
    Ok(reference
        .symbolic_target()
        .map_err(|source| inspection(root, source))?
        .and_then(|target| target.strip_prefix(&prefix))
        .map(str::to_owned))
}

fn other_worktree(
    repository: &Repository,
    root: &Path,
    full_name: &str,
) -> Result<Option<PathBuf>, GitError> {
    let addressed = repository.workdir().unwrap_or(root);

    // `Repository::worktrees` lists linked worktrees but not the main one, so
    // inspect the repository at the common directory explicitly as well.
    let main =
        Repository::open(repository.commondir()).map_err(|source| inspection(root, source))?;
    if main
        .workdir()
        .is_some_and(|path| !same_path(path, addressed))
        && head_names(&main, full_name)?
    {
        return Ok(main.workdir().map(Path::to_path_buf));
    }

    let worktrees = repository
        .worktrees()
        .map_err(|source| inspection(root, source))?;
    for name in worktrees.iter() {
        let name = name.map_err(|source| inspection(root, source))?;
        let Some(name) = name else {
            continue;
        };
        let worktree = repository
            .find_worktree(name)
            .map_err(|source| inspection(root, source))?;
        if same_path(worktree.path(), addressed) {
            continue;
        }
        let worktree_repository =
            Repository::open_from_worktree(&worktree).map_err(|source| inspection(root, source))?;
        if head_names(&worktree_repository, full_name)? {
            return Ok(Some(worktree.path().to_path_buf()));
        }
    }
    Ok(None)
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// Matches `git branch -d`: compare with an upstream when one resolves,
/// otherwise with HEAD.
fn has_unmerged_commits(
    repository: &Repository,
    root: &Path,
    name: &str,
) -> Result<bool, GitError> {
    let branch = repository
        .find_branch(name, BranchType::Local)
        .map_err(|source| inspection(root, source))?;
    let tip = branch
        .get()
        .peel_to_commit()
        .map_err(|source| inspection(root, source))?
        .id();
    let upstream = match branch.upstream() {
        Ok(upstream) => match upstream.get().peel_to_commit() {
            Ok(commit) => Some(commit.id()),
            Err(error) if error.code() == ErrorCode::NotFound => None,
            Err(source) => return Err(inspection(root, source)),
        },
        Err(error) if error.code() == ErrorCode::NotFound => None,
        Err(source) => return Err(inspection(root, source)),
    };
    let base = match upstream {
        Some(upstream) => upstream,
        None => repository
            .head()
            .and_then(|head| head.peel_to_commit())
            .map_err(|source| inspection(root, source))?
            .id(),
    };
    if tip == base {
        return Ok(false);
    }
    repository
        .graph_descendant_of(base, tip)
        .map(|merged| !merged)
        .map_err(|source| inspection(root, source))
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

fn inspection(path: &Path, source: git2::Error) -> GitError {
    GitError::Inspection {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use git2::{BranchType, Repository, Signature, Time};

    use super::{BranchKind, CreateBranchOptions};
    use crate::{
        git::{Cancellation, GitError, GitService},
        testing::{
            COMMIT_EPOCH_SECONDS, Fixture, commit_all, git, initialize_repository,
            remote_with_clone,
        },
    };

    #[test]
    fn listing_reports_local_and_remote_branches_upstream_divergence_and_detachment() {
        let fixture = Fixture::new();
        let (remote, root) = remote_with_clone(&fixture, "branches");
        let repository = Repository::open(&root).unwrap();
        let initial = repository.head().unwrap().target().unwrap();
        repository
            .branch(
                "local-only",
                &repository.find_commit(initial).unwrap(),
                false,
            )
            .unwrap();
        std::fs::write(root.join("local.txt"), "local\n").unwrap();
        commit_all(&repository, "local commit");
        commit_without_tree_change(&Repository::open_bare(&remote).unwrap(), "refs/heads/main");
        git(&root, ["fetch", "--", "origin"]);
        repository
            .reference(
                "refs/remotes/origin/remote-only",
                initial,
                false,
                "test remote branch",
            )
            .unwrap();

        // A missing executable makes a spawn fail. Both successful calls prove
        // that enumeration remained an in-process ref walk.
        let service = GitService::new(&root, &fixture.data_dir)
            .with_git_executable(fixture.root.path().join("missing-git"));
        let local = service.branches(false).unwrap();
        assert_eq!(
            local
                .iter()
                .map(|branch| branch.name.as_str())
                .collect::<Vec<_>>(),
            ["local-only", "main"]
        );
        let main = local.iter().find(|branch| branch.name == "main").unwrap();
        assert_eq!(main.kind, BranchKind::Local);
        assert!(main.checked_out);
        assert_eq!(
            main.upstream.as_ref().map(|upstream| (
                upstream.name.as_str(),
                upstream.ahead,
                upstream.behind
            )),
            Some(("origin/main", 1, 1))
        );
        assert_eq!(
            local
                .iter()
                .find(|branch| branch.name == "local-only")
                .unwrap()
                .upstream,
            None
        );

        let all = service.branches(true).unwrap();
        assert!(all.iter().any(|branch| {
            branch.name == "origin/main" && branch.kind == BranchKind::RemoteTracking
        }));
        assert!(all.iter().any(|branch| branch.name == "origin/remote-only"));
        assert!(!all.iter().any(|branch| branch.name == "origin/HEAD"));

        repository.set_head_detached(initial).unwrap();
        assert!(
            service
                .branches(true)
                .unwrap()
                .iter()
                .all(|branch| !branch.checked_out)
        );
    }

    #[test]
    fn a_branch_is_created_from_an_explicit_start_and_optionally_checked_out() {
        let fixture = Fixture::new();
        let root = fixture.directory("create-branch");
        let repository = initialize_repository(&root);
        let initial = repository.head().unwrap().target().unwrap();
        std::fs::write(root.join("second.txt"), "second\n").unwrap();
        commit_all(&repository, "second");
        let main_tip = repository.head().unwrap().target().unwrap();
        let service = GitService::new(&root, &fixture.data_dir);

        service
            .create_branch(
                "from-initial",
                &CreateBranchOptions {
                    start_point: Some(initial.to_string()),
                    checkout: false,
                },
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(
            Repository::open(&root)
                .unwrap()
                .find_branch("from-initial", BranchType::Local)
                .unwrap()
                .get()
                .target(),
            Some(initial)
        );
        assert_eq!(head_name(&root), "main");

        service
            .create_branch(
                "checked-out",
                &CreateBranchOptions {
                    start_point: Some(main_tip.to_string()),
                    checkout: true,
                },
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(head_name(&root), "checked-out");
    }

    #[test]
    fn conflicting_local_modifications_are_kept_when_checkout_fails() {
        let fixture = Fixture::new();
        let root = fixture.directory("checkout-conflict");
        initialize_repository(&root);
        let service = GitService::new(&root, &fixture.data_dir);
        service
            .create_branch(
                "topic",
                &CreateBranchOptions::default(),
                &Cancellation::default(),
            )
            .unwrap();
        let repository = Repository::open(&root).unwrap();
        std::fs::write(root.join("tracked.txt"), "committed on main\n").unwrap();
        commit_all(&repository, "main change");
        std::fs::write(root.join("tracked.txt"), "uncommitted work\n").unwrap();

        let error = service
            .checkout_branch("topic", &Cancellation::default())
            .unwrap_err();

        assert!(matches!(error, GitError::Failed { .. }));
        assert_eq!(head_name(&root), "main");
        assert_eq!(
            std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "uncommitted work\n"
        );
    }

    #[test]
    fn deleting_the_current_branch_is_refused_even_when_forced() {
        let fixture = Fixture::new();
        let root = fixture.directory("delete-current");
        initialize_repository(&root);
        let error = GitService::new(&root, &fixture.data_dir)
            .delete_branch("main", true, &Cancellation::default())
            .unwrap_err();
        assert!(matches!(
            error,
            GitError::CurrentBranchDeletion { branch } if branch == "main"
        ));
    }

    #[test]
    fn deleting_the_recorded_default_branch_is_refused() {
        let fixture = Fixture::new();
        let (_, root) = remote_with_clone(&fixture, "delete-default");
        let service = GitService::new(&root, &fixture.data_dir);
        service
            .create_branch(
                "topic",
                &CreateBranchOptions {
                    start_point: None,
                    checkout: true,
                },
                &Cancellation::default(),
            )
            .unwrap();

        let error = service
            .delete_branch("main", true, &Cancellation::default())
            .unwrap_err();
        assert!(matches!(
            error,
            GitError::DefaultBranchDeletion { branch } if branch == "main"
        ));
    }

    #[test]
    fn deleting_a_branch_held_by_another_worktree_names_that_worktree() {
        let fixture = Fixture::new();
        let root = fixture.directory("worktree-parent");
        initialize_repository(&root);
        let worktree = fixture.root.path().join("held-worktree");
        git(
            &root,
            ["worktree", "add", "-b", "held", worktree.to_str().unwrap()],
        );

        let error = GitService::new(&root, &fixture.data_dir)
            .delete_branch("held", true, &Cancellation::default())
            .unwrap_err();
        assert!(matches!(
            error,
            GitError::BranchCheckedOutInWorktree {
                branch,
                worktree: held
            } if branch == "held" && super::same_path(&held, &worktree)
        ));

        // The main worktree is not returned by libgit2's linked-worktree list,
        // so exercise the reverse direction as well.
        let error = GitService::new(&worktree, &fixture.data_dir)
            .delete_branch("main", true, &Cancellation::default())
            .unwrap_err();
        assert!(matches!(
            error,
            GitError::BranchCheckedOutInWorktree {
                branch,
                worktree: held
            } if branch == "main" && super::same_path(&held, &root)
        ));
    }

    #[test]
    fn an_unmerged_branch_requires_the_explicit_force_override() {
        let fixture = Fixture::new();
        let root = fixture.directory("delete-unmerged");
        initialize_repository(&root);
        let service = GitService::new(&root, &fixture.data_dir);
        service
            .create_branch(
                "unmerged",
                &CreateBranchOptions {
                    start_point: None,
                    checkout: true,
                },
                &Cancellation::default(),
            )
            .unwrap();
        let repository = Repository::open(&root).unwrap();
        std::fs::write(root.join("branch.txt"), "branch-only\n").unwrap();
        commit_all(&repository, "unmerged commit");
        service
            .checkout_branch("main", &Cancellation::default())
            .unwrap();

        let error = service
            .delete_branch("unmerged", false, &Cancellation::default())
            .unwrap_err();
        assert!(matches!(
            error,
            GitError::UnmergedBranchDeletion { branch } if branch == "unmerged"
        ));
        service
            .delete_branch("unmerged", true, &Cancellation::default())
            .unwrap();
        assert!(
            Repository::open(&root)
                .unwrap()
                .find_branch("unmerged", BranchType::Local)
                .is_err()
        );
    }

    #[test]
    fn an_upstream_can_be_set_and_cleared() {
        let fixture = Fixture::new();
        let root = fixture.directory("set-upstream");
        let repository = initialize_repository(&root);
        let tip = repository.head().unwrap().target().unwrap();
        repository
            .branch("topic", &repository.find_commit(tip).unwrap(), false)
            .unwrap();
        repository
            .reference("refs/remotes/origin/main", tip, false, "test upstream")
            .unwrap();
        repository.remote("origin", root.to_str().unwrap()).unwrap();
        let service = GitService::new(&root, &fixture.data_dir);

        service
            .set_upstream("topic", Some("origin/main"), &Cancellation::default())
            .unwrap();
        assert_eq!(
            service
                .branches(false)
                .unwrap()
                .into_iter()
                .find(|branch| branch.name == "topic")
                .unwrap()
                .upstream
                .unwrap()
                .name,
            "origin/main"
        );
        service
            .set_upstream("topic", None, &Cancellation::default())
            .unwrap();
        assert_eq!(
            service
                .branches(false)
                .unwrap()
                .into_iter()
                .find(|branch| branch.name == "topic")
                .unwrap()
                .upstream,
            None
        );
    }

    #[test]
    fn invalid_names_are_typed_before_a_command_can_spawn() {
        let fixture = Fixture::new();
        let root = fixture.directory("invalid-branch");
        initialize_repository(&root);
        let service = GitService::new(&root, &fixture.data_dir)
            .with_git_executable(fixture.root.path().join("missing-git"));

        for name in ["", "-option", "HEAD", "bad..name", "bad name"] {
            let error = service
                .create_branch(
                    name,
                    &CreateBranchOptions::default(),
                    &Cancellation::default(),
                )
                .unwrap_err();
            assert!(
                matches!(error, GitError::InvalidBranchName { name: rejected } if rejected == name)
            );
        }
    }

    #[test]
    fn every_branch_mutation_takes_the_repository_lock_first() {
        let fixture = Fixture::new();
        let root = fixture.directory("mutation-locks");
        initialize_repository(&root);
        let service = GitService::new(&root, &fixture.data_dir);
        let _held = service.lock(&Cancellation::default()).unwrap();
        let cancelled = Cancellation::default();
        cancelled.cancel();

        assert!(matches!(
            service.create_branch("new", &CreateBranchOptions::default(), &cancelled),
            Err(GitError::Cancelled)
        ));
        assert!(matches!(
            service.checkout_branch("main", &cancelled),
            Err(GitError::Cancelled)
        ));
        assert!(matches!(
            service.delete_branch("topic", false, &cancelled),
            Err(GitError::Cancelled)
        ));
        assert!(matches!(
            service.set_upstream("main", None, &cancelled),
            Err(GitError::Cancelled)
        ));
    }

    fn head_name(root: &Path) -> String {
        Repository::open(root)
            .unwrap()
            .head()
            .unwrap()
            .shorthand()
            .unwrap()
            .to_owned()
    }

    fn commit_without_tree_change(repository: &Repository, reference: &str) {
        let parent = repository
            .find_reference(reference)
            .unwrap()
            .peel_to_commit()
            .unwrap();
        let tree = parent.tree().unwrap();
        let signature = Signature::new(
            "Harkness Tests",
            "tests@harkness.invalid",
            &Time::new(COMMIT_EPOCH_SECONDS + 1, 0),
        )
        .unwrap();
        repository
            .commit(
                Some(reference),
                &signature,
                &signature,
                "remote commit",
                &tree,
                &[&parent],
            )
            .unwrap();
    }
}
