//! Branch enumeration and lifecycle.
//!
//! Listing is an in-process walk of local refs. Mutations use system Git so
//! checkout safety and configuration stay identical to the user's command
//! line, but the decisions Git should not make on Harkness's behalf are
//! settled here first as typed refusals.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use git2::{BranchType, ErrorCode, Oid, Reference, Repository};

use crate::{
    catalog::entry::UpstreamStatus,
    git::{
        GitError, RepositoryLock, head_branch, recorded_default_branch, resolve_remote,
        runner::{Cancellation, GitAccess, GitCommand},
    },
};

/// Which ref namespace a [`Branch`] came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BranchKind {
    /// A branch under `refs/heads` that may be checked out and changed.
    Local,
    /// A locally cached view of a remote branch under `refs/remotes`.
    RemoteTracking,
}

/// Where a local branch is checked out.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BranchCheckout {
    /// No known working tree has this branch checked out.
    NotCheckedOut,
    /// The working tree addressed by this [`GitService`](crate::GitService)
    /// has this branch checked out.
    CurrentWorktree,
    /// Another working tree has this branch checked out.
    OtherWorktree(PathBuf),
}

/// One local or remote-tracking branch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Branch {
    /// The short ref name, such as `topic` or `origin/main`.
    pub name: String,
    /// Whether this is a local branch or a remote-tracking ref.
    pub kind: BranchKind,
    /// The commit at the tip as a typed object ID. Front-end boundaries can
    /// render it without making core callers parse it back from a string.
    pub tip: Oid,
    /// The configured upstream and locally known divergence from it.
    ///
    /// Remote-tracking branches never have an upstream of their own.
    pub upstream: Option<UpstreamStatus>,
    /// Whether and where this branch is checked out.
    pub checkout: BranchCheckout,
}

/// Controls the cost and contents of a branch listing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BranchListOptions {
    /// Include locally cached remote-tracking refs.
    pub include_remote_tracking: bool,
    /// Walk history to calculate ahead/behind counts for local branches.
    ///
    /// Set this to `false` for branch pickers that need names immediately;
    /// configured upstreams are still reported with zero divergence.
    pub calculate_divergence: bool,
}

impl Default for BranchListOptions {
    fn default() -> Self {
        Self {
            include_remote_tracking: false,
            calculate_divergence: true,
        }
    }
}

/// What creating a branch should do.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CreateBranchOptions {
    /// Revision from which to create the branch. `None` means the current HEAD.
    ///
    /// The revision is resolved to a commit under the repository lock before
    /// Git runs. Creation deliberately never configures tracking, including
    /// when this names a remote-tracking branch; call
    /// [`GitService::set_upstream`](crate::GitService::set_upstream) explicitly
    /// when that relationship is wanted.
    pub start_point: Option<String>,
    /// Check out the new branch before returning.
    pub checkout: bool,
}

/// Lists local branches and, when requested, remote-tracking branches.
///
/// This is only a libgit2 ref walk. It never spawns Git, contacts a remote or
/// takes the repository lock. With divergence enabled it performs one history
/// walk per tracked local branch, so event-loop callers must use a worker
/// thread. Cancellation is checked between branches.
pub(crate) fn branches(
    root: &Path,
    options: &BranchListOptions,
    cancellation: &Cancellation,
) -> Result<Vec<Branch>, GitError> {
    refuse_cancelled(cancellation)?;
    let repository = open(root)?;
    let other_checkouts = other_checkouts(&repository, root)?;
    let mut listed = collect(
        &repository,
        root,
        BranchType::Local,
        options.calculate_divergence,
        &other_checkouts,
        cancellation,
    )?;
    if options.include_remote_tracking {
        listed.extend(collect(
            &repository,
            root,
            BranchType::Remote,
            false,
            &other_checkouts,
            cancellation,
        )?);
    }
    listed.sort_by(|left, right| {
        sort_key(left.kind)
            .cmp(&sort_key(right.kind))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(listed)
}

fn collect(
    repository: &Repository,
    root: &Path,
    branch_type: BranchType,
    calculate_divergence: bool,
    other_checkouts: &BTreeMap<String, PathBuf>,
    cancellation: &Cancellation,
) -> Result<Vec<Branch>, GitError> {
    let mut listed = Vec::new();
    let branches = repository
        .branches(Some(branch_type))
        .map_err(|source| inspection(root, source))?;
    for branch in branches {
        refuse_cancelled(cancellation)?;
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
        let full_name = branch
            .get()
            .name()
            .map_err(|source| inspection(root, source))?;
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
            branch_upstream(repository, root, &branch, tip, calculate_divergence)?
        } else {
            None
        };
        listed.push(Branch {
            name,
            kind: match branch_type {
                BranchType::Local => BranchKind::Local,
                BranchType::Remote => BranchKind::RemoteTracking,
            },
            tip,
            upstream,
            checkout: if branch.is_head() {
                BranchCheckout::CurrentWorktree
            } else if let Some(path) = other_checkouts.get(full_name) {
                BranchCheckout::OtherWorktree(path.clone())
            } else {
                BranchCheckout::NotCheckedOut
            },
        });
    }
    Ok(listed)
}

fn branch_upstream(
    repository: &Repository,
    root: &Path,
    branch: &git2::Branch<'_>,
    local: git2::Oid,
    calculate_divergence: bool,
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
    let (ahead, behind) = if calculate_divergence {
        repository
            .graph_ahead_behind(local, tracked)
            .map_err(|source| inspection(root, source))?
    } else {
        (0, 0)
    };
    Ok(Some(UpstreamStatus {
        name,
        ahead,
        behind,
    }))
}

fn sort_key(kind: BranchKind) -> u8 {
    match kind {
        BranchKind::Local => 0,
        BranchKind::RemoteTracking => 1,
    }
}

fn refuse_cancelled(cancellation: &Cancellation) -> Result<(), GitError> {
    if cancellation.is_cancelled() {
        Err(GitError::Cancelled)
    } else {
        Ok(())
    }
}

/// Applies Git's `check-ref-format --branch` rules and Harkness's short-name
/// contract without spawning Git.
pub(crate) fn validate_name(name: &str) -> Result<(), GitError> {
    // `Reference::is_valid_name` implements check-ref-format for a full ref.
    // Its full-ref form deliberately permits a leading dash and the special
    // token HEAD, while `--branch` rejects both. Harkness additionally rejects
    // the revision shorthand `@` and fully qualified `refs/` input: this API
    // accepts short local branch names and must not create
    // `refs/heads/refs/heads/...` by surprise.
    let full = format!("refs/heads/{name}");
    if name.is_empty()
        || name.starts_with('-')
        || name.starts_with("refs/")
        || matches!(name, "HEAD" | "@")
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
    lock: RepositoryLock,
    name: &str,
    options: &CreateBranchOptions,
    cancellation: &Cancellation,
) -> Result<(), GitError> {
    let repository = open(root)?;
    require_missing_branch(&repository, name)?;
    let start = match options.start_point.as_deref() {
        Some(start_point) => repository
            .revparse_single(start_point)
            .and_then(|object| object.peel_to_commit())
            .map(|commit| commit.id())
            .map_err(|_| GitError::InvalidStartPoint {
                start_point: start_point.to_owned(),
            })?,
        None => match repository.head().and_then(|head| head.peel_to_commit()) {
            Ok(commit) => commit.id(),
            Err(error) if matches!(error.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => {
                return Err(GitError::UnbornBranch {
                    path: root.to_path_buf(),
                    branch: head_branch(&repository, root)?,
                });
            }
            Err(source) => return Err(inspection(root, source)),
        },
    };
    let start = start.to_string();

    let mut command =
        GitCommand::new(git_executable, root, GitAccess::LocalWrite).with_repository_lock(lock);
    if options.checkout {
        command = command.args(["switch", "--no-track", "-c", name, &start]);
    } else {
        command = command.args(["branch", "--no-track", "--", name, &start]);
    }
    command.run(cancellation)?;
    Ok(())
}

pub(crate) fn checkout(
    git_executable: &Path,
    root: &Path,
    lock: RepositoryLock,
    name: &str,
    cancellation: &Cancellation,
) -> Result<(), GitError> {
    let repository = open(root)?;
    require_local_branch(&repository, name)?;
    GitCommand::new(git_executable, root, GitAccess::LocalWrite)
        .with_repository_lock(lock)
        // `switch` is branch-only, and `--no-guess` independently prevents a
        // remote-tracking ref from creating a local branch through DWIM.
        .args(["switch", "--no-guess", name])
        .run(cancellation)?;
    Ok(())
}

pub(crate) fn delete(
    git_executable: &Path,
    root: &Path,
    lock: RepositoryLock,
    name: &str,
    force: bool,
    cancellation: &Cancellation,
) -> Result<(), GitError> {
    let repository = open(root)?;
    require_local_branch(&repository, name)?;
    let full_name = format!("refs/heads/{name}");

    if head_names(&repository, &full_name)? {
        return Err(GitError::CurrentBranchDeletion {
            branch: name.to_owned(),
        });
    }
    if local_default_branch(&repository, root, name)?.as_deref() == Some(name) {
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
        .with_repository_lock(lock)
        .args(["branch", if force { "-D" } else { "-d" }, "--", name])
        .run(cancellation)?;
    Ok(())
}

pub(crate) fn set_upstream(
    git_executable: &Path,
    root: &Path,
    lock: RepositoryLock,
    branch: &str,
    upstream: Option<&str>,
    cancellation: &Cancellation,
) -> Result<(), GitError> {
    let repository = open(root)?;
    let local = require_local_branch(&repository, branch)?;
    if let Some(upstream) = upstream {
        require_upstream_branch(&repository, upstream)?;
    } else if local.upstream().is_err() {
        // Clearing an already absent relationship is idempotent and does not
        // need a Git process only to report that there was nothing to clear.
        return Ok(());
    }
    let command = GitCommand::new(git_executable, root, GitAccess::LocalWrite)
        .with_repository_lock(lock)
        .arg("branch");
    let command = match upstream {
        Some(upstream) => command.arg(format!("--set-upstream-to={upstream}")),
        None => command.arg("--unset-upstream"),
    };
    command.args(["--", branch]).run(cancellation)?;
    Ok(())
}

pub(crate) fn rename(
    git_executable: &Path,
    root: &Path,
    lock: RepositoryLock,
    old_name: &str,
    new_name: &str,
    cancellation: &Cancellation,
) -> Result<(), GitError> {
    let repository = open(root)?;
    require_local_branch(&repository, old_name)?;
    require_missing_branch(&repository, new_name)?;
    GitCommand::new(git_executable, root, GitAccess::LocalWrite)
        .with_repository_lock(lock)
        .args(["branch", "-m", "--", old_name, new_name])
        .run(cancellation)?;
    Ok(())
}

fn require_local_branch<'repo>(
    repository: &'repo Repository,
    name: &str,
) -> Result<git2::Branch<'repo>, GitError> {
    match repository.find_branch(name, BranchType::Local) {
        Ok(branch) => Ok(branch),
        Err(error) if error.code() == ErrorCode::NotFound => Err(GitError::NoSuchBranch {
            branch: name.to_owned(),
        }),
        Err(source) => Err(inspection(
            repository.workdir().unwrap_or_else(|| repository.path()),
            source,
        )),
    }
}

fn require_missing_branch(repository: &Repository, name: &str) -> Result<(), GitError> {
    match repository.find_branch(name, BranchType::Local) {
        Ok(_) => Err(GitError::BranchAlreadyExists {
            branch: name.to_owned(),
        }),
        Err(error) if error.code() == ErrorCode::NotFound => Ok(()),
        Err(source) => Err(inspection(
            repository.workdir().unwrap_or_else(|| repository.path()),
            source,
        )),
    }
}

fn require_upstream_branch(repository: &Repository, name: &str) -> Result<(), GitError> {
    match repository.resolve_reference_from_short_name(name) {
        Ok(reference)
            if reference.name().is_ok_and(|name| {
                name.starts_with("refs/heads/") || name.starts_with("refs/remotes/")
            }) =>
        {
            Ok(())
        }
        Ok(_) => Err(GitError::NoSuchBranch {
            branch: name.to_owned(),
        }),
        Err(error)
            if matches!(
                error.code(),
                ErrorCode::NotFound | ErrorCode::InvalidSpec | ErrorCode::Ambiguous
            ) =>
        {
            Err(GitError::NoSuchBranch {
                branch: name.to_owned(),
            })
        }
        Err(source) => Err(inspection(
            repository.workdir().unwrap_or_else(|| repository.path()),
            source,
        )),
    }
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
/// Remote selection follows the same upstream/origin/sole-remote precedence as
/// fetch, pull, and push. This deliberately stays local: a branch deletion
/// must not turn into an unbounded network operation. Repositories assembled
/// with `git init`, `git remote add`, and `git fetch` ordinarily have no
/// recorded remote HEAD. Deletion is intentionally fail-open there because a
/// local branch is reflog-recoverable; unlike a push, it does not destroy the
/// remote's only copy.
fn local_default_branch(
    repository: &Repository,
    root: &Path,
    branch: &str,
) -> Result<Option<String>, GitError> {
    let full_name = format!("refs/heads/{branch}");
    let upstream_remote = match repository.branch_upstream_remote(&full_name) {
        Ok(remote) => Some(
            remote
                .as_str()
                .map(str::to_owned)
                .map_err(|source| inspection(root, source))?,
        ),
        Err(error) if error.code() == ErrorCode::NotFound => None,
        Err(source) => return Err(inspection(root, source)),
    };
    let remote = match resolve_remote(repository, root, None, upstream_remote.as_deref()) {
        Ok(remote) if remote != "." => remote,
        Ok(_) | Err(GitError::NoRemote { .. }) => return Ok(None),
        Err(error) => return Err(error),
    };
    recorded_default_branch(repository, root, &remote)
}

fn other_worktree(
    repository: &Repository,
    root: &Path,
    full_name: &str,
) -> Result<Option<PathBuf>, GitError> {
    Ok(other_checkouts(repository, root)?.remove(full_name))
}

fn other_checkouts(
    repository: &Repository,
    root: &Path,
) -> Result<BTreeMap<String, PathBuf>, GitError> {
    let mut checkouts = BTreeMap::new();
    let addressed = repository.workdir().unwrap_or(root);

    // `Repository::worktrees` lists linked worktrees but not the main one, so
    // inspect the repository at the common directory explicitly as well.
    let main =
        Repository::open(repository.commondir()).map_err(|source| inspection(root, source))?;
    if main
        .workdir()
        .is_some_and(|path| !same_path(path, addressed))
    {
        record_checkout(&main, &mut checkouts)?;
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
        // A prunable worktree has no working directory that can hold a
        // checkout. Stale metadata must not disable every branch operation in
        // the repository.
        if worktree.validate().is_err() {
            continue;
        }
        let Ok(worktree_repository) = Repository::open_from_worktree(&worktree) else {
            continue;
        };
        record_checkout(&worktree_repository, &mut checkouts)?;
    }
    Ok(checkouts)
}

fn record_checkout(
    repository: &Repository,
    checkouts: &mut BTreeMap<String, PathBuf>,
) -> Result<(), GitError> {
    let root = repository.workdir().unwrap_or_else(|| repository.path());
    match repository.head() {
        Ok(head) if head.is_branch() => {
            let name = head.name().map_err(|source| inspection(root, source))?;
            checkouts.insert(name.to_owned(), root.to_path_buf());
            Ok(())
        }
        Ok(_) => Ok(()),
        Err(error) if matches!(error.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => {
            Ok(())
        }
        Err(source) => Err(inspection(root, source)),
    }
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
    let branch = require_local_branch(repository, name)?;
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
        None => match repository.head().and_then(|head| head.peel_to_commit()) {
            Ok(commit) => commit.id(),
            // With no commit on HEAD there is no base containing the named
            // branch's commits, so ordinary deletion must treat it as
            // unmerged rather than leaking an inspection failure.
            Err(error) if matches!(error.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => {
                return Ok(true);
            }
            Err(source) => return Err(inspection(root, source)),
        },
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

    use git2::{BranchType, Oid, Repository, Signature, Time};

    use super::{BranchCheckout, BranchKind, BranchListOptions, CreateBranchOptions};
    use crate::{
        git::{Cancellation, GitError, GitService},
        testing::{
            COMMIT_EPOCH_SECONDS, Fixture, PROCESS_PROJECT_ROOT_ENV, PROCESS_READY_FILE_ENV,
            commit_all, git, initialize_repository, remote_with_clone, spawn_child,
            wait_for_child_signal, wait_for_file,
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
        let local = service
            .branches(&BranchListOptions::default(), &Cancellation::default())
            .unwrap();
        assert_eq!(
            local
                .iter()
                .map(|branch| branch.name.as_str())
                .collect::<Vec<_>>(),
            ["local-only", "main"]
        );
        let main = local.iter().find(|branch| branch.name == "main").unwrap();
        assert_eq!(main.kind, BranchKind::Local);
        assert_eq!(main.tip, main_tip(&root));
        assert_eq!(main.checkout, BranchCheckout::CurrentWorktree);
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

        let all = service
            .branches(
                &BranchListOptions {
                    include_remote_tracking: true,
                    ..BranchListOptions::default()
                },
                &Cancellation::default(),
            )
            .unwrap();
        let kinds = all.iter().map(|branch| branch.kind).collect::<Vec<_>>();
        let first_remote = kinds
            .iter()
            .position(|kind| *kind == BranchKind::RemoteTracking)
            .unwrap();
        assert!(
            kinds[..first_remote]
                .iter()
                .all(|kind| *kind == BranchKind::Local)
        );
        assert!(
            kinds[first_remote..]
                .iter()
                .all(|kind| *kind == BranchKind::RemoteTracking)
        );
        assert!(all.iter().any(|branch| {
            branch.name == "origin/main" && branch.kind == BranchKind::RemoteTracking
        }));
        assert!(all.iter().any(|branch| branch.name == "origin/remote-only"));
        assert!(!all.iter().any(|branch| branch.name == "origin/HEAD"));

        repository.set_head_detached(initial).unwrap();
        assert!(
            service
                .branches(
                    &BranchListOptions {
                        include_remote_tracking: true,
                        ..BranchListOptions::default()
                    },
                    &Cancellation::default(),
                )
                .unwrap()
                .iter()
                .all(|branch| branch.checkout == BranchCheckout::NotCheckedOut)
        );
    }

    #[test]
    fn listing_is_cancellable_and_can_skip_divergence_walks() {
        let fixture = Fixture::new();
        let (_, root) = remote_with_clone(&fixture, "listing-cost");
        let repository = Repository::open(&root).unwrap();
        std::fs::write(root.join("local.txt"), "local\n").unwrap();
        commit_all(&repository, "local commit");
        let service = GitService::new(&root, &fixture.data_dir);

        let quick = service
            .branches(
                &BranchListOptions {
                    calculate_divergence: false,
                    ..BranchListOptions::default()
                },
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(
            quick[0]
                .upstream
                .as_ref()
                .map(|upstream| (upstream.ahead, upstream.behind)),
            Some((0, 0))
        );
        let full = service
            .branches(&BranchListOptions::default(), &Cancellation::default())
            .unwrap();
        assert_eq!(
            full[0]
                .upstream
                .as_ref()
                .map(|upstream| (upstream.ahead, upstream.behind)),
            Some((1, 0))
        );

        let cancelled = Cancellation::default();
        cancelled.cancel();
        assert!(matches!(
            service.branches(&BranchListOptions::default(), &cancelled),
            Err(GitError::Cancelled)
        ));
    }

    #[test]
    fn a_configured_upstream_with_an_unfetched_tip_is_still_reported() {
        let fixture = Fixture::new();
        let root = fixture.directory("unfetched-upstream");
        let repository = initialize_repository(&root);
        let tip = repository.head().unwrap().peel_to_commit().unwrap();
        repository.branch("topic", &tip, false).unwrap();
        repository.remote("origin", root.to_str().unwrap()).unwrap();
        let missing = Oid::from_str("1111111111111111111111111111111111111111").unwrap();
        std::fs::create_dir_all(repository.path().join("refs/remotes/origin")).unwrap();
        std::fs::write(
            repository.path().join("refs/remotes/origin/missing"),
            format!("{missing}\n"),
        )
        .unwrap();
        let mut configuration = repository.config().unwrap();
        configuration
            .set_str("branch.topic.remote", "origin")
            .unwrap();
        configuration
            .set_str("branch.topic.merge", "refs/heads/missing")
            .unwrap();

        let topic = GitService::new(&root, &fixture.data_dir)
            .branches(&BranchListOptions::default(), &Cancellation::default())
            .unwrap()
            .into_iter()
            .find(|branch| branch.name == "topic")
            .unwrap();
        assert_eq!(
            topic
                .upstream
                .map(|upstream| (upstream.name, upstream.ahead, upstream.behind)),
            Some(("origin/missing".to_owned(), 0, 0))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_non_utf8_branch_name_is_rejected_deterministically() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let fixture = Fixture::new();
        let root = fixture.directory("non-utf8-branch");
        let repository = initialize_repository(&root);
        let tip = repository.head().unwrap().target().unwrap();
        let bad_name = OsString::from_vec(vec![b'b', b'a', b'd', 0xff]);
        std::fs::write(
            repository.path().join("refs/heads").join(bad_name),
            format!("{tip}\n"),
        )
        .unwrap();

        assert!(matches!(
            GitService::new(&root, &fixture.data_dir)
                .branches(&BranchListOptions::default(), &Cancellation::default()),
            Err(GitError::Inspection { .. })
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_non_utf8_upstream_name_is_rejected_deterministically() {
        use std::{ffi::OsString, io::Write, os::unix::ffi::OsStringExt};

        let fixture = Fixture::new();
        let root = fixture.directory("non-utf8-upstream");
        let repository = initialize_repository(&root);
        let tip = repository.head().unwrap().peel_to_commit().unwrap();
        repository.branch("topic", &tip, false).unwrap();
        repository.remote("origin", root.to_str().unwrap()).unwrap();
        let bad_name = vec![b'b', b'a', b'd', 0xff];
        let remote_ref = repository
            .path()
            .join("refs/remotes/origin")
            .join(OsString::from_vec(bad_name.clone()));
        std::fs::create_dir_all(remote_ref.parent().unwrap()).unwrap();
        std::fs::write(&remote_ref, format!("{}\n", tip.id())).unwrap();
        let mut config = std::fs::OpenOptions::new()
            .append(true)
            .open(repository.path().join("config"))
            .unwrap();
        config
            .write_all(b"\n[branch \"topic\"]\n\tremote = origin\n\tmerge = refs/heads/")
            .unwrap();
        config.write_all(&bad_name).unwrap();
        config.write_all(b"\n").unwrap();
        drop(config);

        let topic = repository.find_branch("topic", BranchType::Local).unwrap();
        assert!(matches!(
            super::branch_upstream(&repository, &root, &topic, tip.id(), true),
            Err(GitError::Inspection { .. })
        ));
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

        assert!(matches!(
            error,
            GitError::Failed { stderr, .. }
                if stderr.contains("local changes") && stderr.contains("would be overwritten")
        ));
        assert_eq!(head_name(&root), "main");
        assert_eq!(
            std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "uncommitted work\n"
        );
    }

    #[test]
    fn checkout_refuses_remote_names_instead_of_guessing_or_detaching() {
        let fixture = Fixture::new();
        let (_, root) = remote_with_clone(&fixture, "checkout-no-guess");
        let repository = Repository::open(&root).unwrap();
        let tip = repository.head().unwrap().target().unwrap();
        repository
            .reference(
                "refs/remotes/origin/feature",
                tip,
                false,
                "test remote branch",
            )
            .unwrap();
        let service = GitService::new(&root, &fixture.data_dir);

        for name in ["feature", "origin/main"] {
            let error = service
                .checkout_branch(name, &Cancellation::default())
                .unwrap_err();
            assert!(matches!(
                error,
                GitError::NoSuchBranch { branch } if branch == name
            ));
            assert_eq!(head_name(&root), "main");
            assert!(!Repository::open(&root).unwrap().head_detached().unwrap());
        }
        assert!(
            repository
                .find_branch("feature", BranchType::Local)
                .is_err()
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
        let held = GitService::new(&root, &fixture.data_dir)
            .branches(&BranchListOptions::default(), &Cancellation::default())
            .unwrap()
            .into_iter()
            .find(|branch| branch.name == "held")
            .unwrap();
        assert!(matches!(
            held.checkout,
            BranchCheckout::OtherWorktree(path) if super::same_path(&path, &worktree)
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
    fn a_prunable_worktree_does_not_block_unrelated_branch_deletion() {
        let fixture = Fixture::new();
        let root = fixture.directory("prunable-parent");
        initialize_repository(&root);
        let service = GitService::new(&root, &fixture.data_dir);
        service
            .create_branch(
                "topic",
                &CreateBranchOptions::default(),
                &Cancellation::default(),
            )
            .unwrap();
        let stale = fixture.root.path().join("prunable-gone");
        git(
            &root,
            ["worktree", "add", "-b", "held", stale.to_str().unwrap()],
        );
        std::fs::remove_dir_all(&stale).unwrap();

        service
            .delete_branch("topic", true, &Cancellation::default())
            .unwrap();
        assert!(
            Repository::open(&root)
                .unwrap()
                .find_branch("topic", BranchType::Local)
                .is_err()
        );
    }

    #[test]
    fn deletion_without_a_recorded_remote_head_is_documented_fail_open() {
        let fixture = Fixture::new();
        let root = fixture.directory("unrecorded-default");
        let repository = initialize_repository(&root);
        let tip = repository.head().unwrap().peel_to_commit().unwrap();
        repository.branch("topic", &tip, false).unwrap();
        repository.remote("origin", root.to_str().unwrap()).unwrap();
        repository
            .reference(
                "refs/remotes/origin/main",
                tip.id(),
                false,
                "fetched without remote HEAD",
            )
            .unwrap();

        GitService::new(&root, &fixture.data_dir)
            .delete_branch("topic", false, &Cancellation::default())
            .unwrap();
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
                .branches(&BranchListOptions::default(), &Cancellation::default())
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
                .branches(&BranchListOptions::default(), &Cancellation::default())
                .unwrap()
                .into_iter()
                .find(|branch| branch.name == "topic")
                .unwrap()
                .upstream,
            None
        );
    }

    #[test]
    fn creation_never_infers_tracking_from_a_remote_start_point() {
        let fixture = Fixture::new();
        let (_, root) = remote_with_clone(&fixture, "create-no-track");
        let service = GitService::new(&root, &fixture.data_dir);
        service
            .create_branch(
                "topic",
                &CreateBranchOptions {
                    start_point: Some("origin/main".to_owned()),
                    checkout: true,
                },
                &Cancellation::default(),
            )
            .unwrap();
        assert!(
            Repository::open(&root)
                .unwrap()
                .find_branch("topic", BranchType::Local)
                .unwrap()
                .upstream()
                .is_err()
        );
    }

    #[test]
    fn common_branch_failures_are_typed_before_git_runs() {
        let fixture = Fixture::new();
        let root = fixture.directory("typed-branch-failures");
        initialize_repository(&root);
        let real = GitService::new(&root, &fixture.data_dir);
        real.create_branch(
            "topic",
            &CreateBranchOptions::default(),
            &Cancellation::default(),
        )
        .unwrap();
        // The missing executable proves each error below is settled in process.
        let service = real.with_git_executable(fixture.root.path().join("missing-git"));

        assert!(matches!(
            service.create_branch(
                "topic",
                &CreateBranchOptions::default(),
                &Cancellation::default()
            ),
            Err(GitError::BranchAlreadyExists { branch }) if branch == "topic"
        ));
        assert!(matches!(
            service.create_branch(
                "new",
                &CreateBranchOptions {
                    start_point: Some("no-such-revision".to_owned()),
                    checkout: false,
                },
                &Cancellation::default()
            ),
            Err(GitError::InvalidStartPoint { start_point }) if start_point == "no-such-revision"
        ));
        for force in [false, true] {
            assert!(matches!(
                service.delete_branch("nope", force, &Cancellation::default()),
                Err(GitError::NoSuchBranch { branch }) if branch == "nope"
            ));
        }
        assert!(matches!(
            service.set_upstream(
                "topic",
                Some("origin/nope"),
                &Cancellation::default()
            ),
            Err(GitError::NoSuchBranch { branch }) if branch == "origin/nope"
        ));
    }

    #[test]
    fn unborn_head_and_orphan_deletion_have_typed_answers() {
        let fixture = Fixture::new();
        let root = fixture.directory("unborn-branches");
        let repository = initialize_repository(&root);
        let tip = repository.head().unwrap().peel_to_commit().unwrap();
        repository.branch("topic", &tip, false).unwrap();
        repository.set_head("refs/heads/fresh").unwrap();
        let service = GitService::new(&root, &fixture.data_dir);

        assert!(matches!(
            service.create_branch(
                "new",
                &CreateBranchOptions::default(),
                &Cancellation::default()
            ),
            Err(GitError::UnbornBranch { branch, .. }) if branch == "fresh"
        ));
        assert!(matches!(
            service.delete_branch("topic", false, &Cancellation::default()),
            Err(GitError::UnmergedBranchDeletion { branch }) if branch == "topic"
        ));
    }

    #[test]
    fn a_branch_can_be_renamed_without_overwriting_an_existing_branch() {
        let fixture = Fixture::new();
        let root = fixture.directory("rename-branch");
        initialize_repository(&root);
        let service = GitService::new(&root, &fixture.data_dir);
        service
            .create_branch(
                "topic",
                &CreateBranchOptions::default(),
                &Cancellation::default(),
            )
            .unwrap();
        service
            .rename_branch("topic", "renamed", &Cancellation::default())
            .unwrap();
        assert!(matches!(
            service.rename_branch("missing", "new", &Cancellation::default()),
            Err(GitError::NoSuchBranch { branch }) if branch == "missing"
        ));
        assert!(matches!(
            service.rename_branch("renamed", "main", &Cancellation::default()),
            Err(GitError::BranchAlreadyExists { branch }) if branch == "main"
        ));
    }

    #[test]
    fn invalid_names_are_typed_before_a_command_can_spawn() {
        let fixture = Fixture::new();
        let root = fixture.directory("invalid-branch");
        initialize_repository(&root);
        let service = GitService::new(&root, &fixture.data_dir)
            .with_git_executable(fixture.root.path().join("missing-git"));

        for name in [
            "",
            "-option",
            "HEAD",
            "bad..name",
            "bad name",
            "x.lock",
            "refs/heads/x",
            "trailing/",
            "bad@{name",
            r"bad\name",
            "@",
        ] {
            assert!(matches!(
                super::validate_name(name),
                Err(GitError::InvalidBranchName { name: rejected }) if rejected == name
            ));
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
        for name in ["feature/x", "release-1.0", "café"] {
            super::validate_name(name).unwrap();
        }
    }

    #[test]
    fn every_branch_verb_rejects_a_non_repository() {
        let fixture = Fixture::new();
        let root = fixture.directory("not-a-repository");
        let service = GitService::new(&root, &fixture.data_dir);
        let cancellation = Cancellation::default();
        assert!(matches!(
            service.branches(&BranchListOptions::default(), &cancellation),
            Err(GitError::NotARepository { .. })
        ));
        assert!(matches!(
            service.create_branch("topic", &CreateBranchOptions::default(), &cancellation),
            Err(GitError::NotARepository { .. })
        ));
        assert!(matches!(
            service.checkout_branch("topic", &cancellation),
            Err(GitError::NotARepository { .. })
        ));
        assert!(matches!(
            service.delete_branch("topic", false, &cancellation),
            Err(GitError::NotARepository { .. })
        ));
        assert!(matches!(
            service.set_upstream("topic", None, &cancellation),
            Err(GitError::NotARepository { .. })
        ));
        assert!(matches!(
            service.rename_branch("topic", "renamed", &cancellation),
            Err(GitError::NotARepository { .. })
        ));
    }

    #[test]
    fn every_branch_mutation_takes_the_repository_lock_first() {
        let fixture = Fixture::new();
        let root = fixture.directory("mutation-locks");
        let repository = initialize_repository(&root);
        let tip = repository.head().unwrap().peel_to_commit().unwrap();
        repository.branch("topic", &tip, false).unwrap();
        repository
            .reference("refs/remotes/origin/main", tip.id(), false, "test upstream")
            .unwrap();
        repository.remote("origin", root.to_str().unwrap()).unwrap();
        let ready_file = fixture.root.path().join("branch-mutation-lock-held");
        let mut holder = spawn_child(&fixture.data_dir, "hold-repository-lock")
            .env(PROCESS_PROJECT_ROOT_ENV, &root)
            .env(PROCESS_READY_FILE_ENV, &ready_file)
            .spawn()
            .unwrap();
        wait_for_child_signal(&mut holder, &ready_file);
        let service = GitService::new(&root, &fixture.data_dir);
        let live = Cancellation::default();

        assert!(matches!(
            service.create_branch("new", &CreateBranchOptions::default(), &live),
            Err(GitError::RepositoryBusy { .. })
        ));
        assert!(matches!(
            service.checkout_branch("main", &live),
            Err(GitError::RepositoryBusy { .. })
        ));
        assert!(matches!(
            service.delete_branch("topic", false, &live),
            Err(GitError::RepositoryBusy { .. })
        ));
        assert!(matches!(
            service.set_upstream("topic", Some("origin/main"), &live),
            Err(GitError::RepositoryBusy { .. })
        ));
        assert!(matches!(
            service.rename_branch("topic", "renamed", &live),
            Err(GitError::RepositoryBusy { .. })
        ));
        holder.kill().unwrap();
        holder.wait().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn two_racing_branch_mutations_do_not_enter_git_together() {
        let fixture = Fixture::new();
        let root = fixture.directory("racing-mutations");
        initialize_repository(&root);
        let running = fixture.root.path().join("branch-command-running");
        let hanging_git = fixture.shim(
            "hanging-branch-git",
            &format!(
                "#!/bin/sh\n\
                 touch '{}'\n\
                 while :; do sleep 1; done\n",
                running.display()
            ),
        );
        let first = GitService::new(&root, &fixture.data_dir).with_git_executable(hanging_git);
        let second = GitService::new(&root, &fixture.data_dir);
        let cancellation = Cancellation::default();

        std::thread::scope(|scope| {
            let running_mutation = scope.spawn(|| {
                first.create_branch("first", &CreateBranchOptions::default(), &cancellation)
            });
            wait_for_file(&running);
            assert!(matches!(
                second.create_branch(
                    "second",
                    &CreateBranchOptions::default(),
                    &Cancellation::default()
                ),
                Err(GitError::RepositoryBusy { .. })
            ));
            cancellation.cancel();
            assert!(matches!(
                running_mutation.join().unwrap(),
                Err(GitError::Cancelled)
            ));
        });
        assert!(
            Repository::open(&root)
                .unwrap()
                .find_branch("second", BranchType::Local)
                .is_err()
        );
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

    fn main_tip(root: &Path) -> Oid {
        Repository::open(root)
            .unwrap()
            .find_branch("main", BranchType::Local)
            .unwrap()
            .get()
            .target()
            .unwrap()
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
