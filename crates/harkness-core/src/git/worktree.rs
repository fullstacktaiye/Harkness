//! Git worktree lifecycle primitives.
//!
//! Catalog ownership stays in `project`; this module knows only how to
//! validate and mutate one repository while its caller holds the repository
//! lock. Keeping those layers separate preserves the repository-before-catalog
//! lock order.

use std::{
    fs,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::ffi::OsString;

use git2::{ErrorCode, Oid, Repository};

use crate::{
    git::{
        GitError, RepositoryLock, branch, head_branch,
        runner::{Cancellation, GitAccess, GitCommand},
    },
    project::WorktreeBase,
};

/// One row from `git worktree list --porcelain`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitWorktree {
    pub(crate) root: PathBuf,
    pub(crate) branch: Option<String>,
    pub(crate) locked: Option<String>,
    pub(crate) prunable: bool,
}

/// The revision and branch identity Git actually created.
pub(crate) struct AddedWorktree {
    pub(crate) branch: Option<String>,
    pub(crate) commit: Oid,
}

/// Adds a worktree after resolving every caller-controlled revision to a
/// commit and applying typed branch refusals under the repository lock.
pub(crate) fn add(
    git_executable: &Path,
    root: &Path,
    _lock: &RepositoryLock,
    destination: &Path,
    base: &WorktreeBase,
    cancellation: &Cancellation,
) -> Result<AddedWorktree, GitError> {
    let repository = branch::open(root)?;
    let mut command = GitCommand::new(git_executable, root, GitAccess::LocalWrite)
        .without_timeout()
        .args(["worktree", "add"]);

    let (recorded_branch, commit) = match base {
        WorktreeBase::NewBranch { name, start_point } => {
            branch::validate_name(name)?;
            branch::require_missing_branch(&repository, name)?;
            let start = resolve_start(&repository, root, start_point.as_deref())?;
            command = command
                .args(["--no-track", "-b", name, "--"])
                .arg(destination)
                .arg(start.to_string());
            (Some(name.clone()), start)
        }
        WorktreeBase::ExistingBranch { name } => {
            branch::validate_name(name)?;
            branch::require_local_branch(&repository, name)?;
            refuse_checked_out(&repository, root, name)?;
            let commit = resolve_start(&repository, root, Some(name))?;
            command = command.arg("--").arg(destination).arg(name);
            (Some(name.clone()), commit)
        }
        WorktreeBase::Detached { commit } => {
            let commit = resolve_start(&repository, root, Some(commit))?;
            command = command
                .args(["--detach", "--"])
                .arg(destination)
                .arg(commit.to_string());
            (None, commit)
        }
    };

    command.run(cancellation)?;
    Ok(AddedWorktree {
        branch: recorded_branch,
        commit,
    })
}

fn resolve_start(
    repository: &Repository,
    root: &Path,
    requested: Option<&str>,
) -> Result<git2::Oid, GitError> {
    match requested {
        Some(start_point) => repository
            .revparse_single(start_point)
            .and_then(|object| object.peel_to_commit())
            .map(|commit| commit.id())
            .map_err(|_| GitError::InvalidStartPoint {
                start_point: start_point.to_owned(),
            }),
        None => match repository.head().and_then(|head| head.peel_to_commit()) {
            Ok(commit) => Ok(commit.id()),
            Err(error) if matches!(error.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => {
                Err(GitError::UnbornBranch {
                    path: root.to_path_buf(),
                    branch: head_branch(repository, root)?,
                })
            }
            Err(source) => Err(inspection(root, source)),
        },
    }
}

fn refuse_checked_out(
    repository: &Repository,
    root: &Path,
    branch_name: &str,
) -> Result<(), GitError> {
    let full_name = format!("refs/heads/{branch_name}");
    let worktree = if branch::head_names(repository, &full_name)? {
        Some(root.to_path_buf())
    } else {
        branch::other_worktree(repository, root, &full_name)?
    };
    match worktree {
        Some(worktree) => Err(GitError::BranchCheckedOutInWorktree {
            branch: branch_name.to_owned(),
            worktree,
        }),
        None => Ok(()),
    }
}

/// Removes a linked worktree through Git. `force` affects the checkout only;
/// no branch is ever deleted here.
pub(crate) fn remove(
    git_executable: &Path,
    parent: &Path,
    lock: &RepositoryLock,
    destination: &Path,
    force: bool,
    cancellation: &Cancellation,
) -> Result<(), GitError> {
    let listed = list(git_executable, parent, cancellation)?;
    let worktree = listed
        .into_iter()
        .find(|worktree| same_path(&worktree.root, destination));
    if let Some(reason) = worktree
        .as_ref()
        .and_then(|worktree| worktree.locked.clone())
    {
        return Err(GitError::WorktreeLocked {
            path: destination.to_path_buf(),
            reason: (!reason.is_empty()).then_some(reason),
        });
    }
    // Git and the filesystem may already agree that a checkout is gone while
    // its Harkness row remains. There is no administrative record or directory
    // left to mutate in that state.
    if worktree.is_none() && !destination.exists() {
        return Ok(());
    }
    remove_known_unlocked(
        git_executable,
        parent,
        lock,
        destination,
        force,
        cancellation,
    )
}

/// Removes a worktree after the caller has already established that it is not
/// locked. Used by targeted reconciliation and failed-add cleanup to avoid a
/// second porcelain listing.
pub(crate) fn remove_known_unlocked(
    git_executable: &Path,
    parent: &Path,
    _lock: &RepositoryLock,
    destination: &Path,
    force: bool,
    cancellation: &Cancellation,
) -> Result<(), GitError> {
    let mut command = GitCommand::new(git_executable, parent, GitAccess::LocalWrite)
        .without_timeout()
        .args(["worktree", "remove"]);
    if force {
        command = command.arg("--force");
    }
    command.args(["--"]).arg(destination).run(cancellation)?;
    Ok(())
}

/// Runs the mandatory best-effort cleanup sequence after `worktree add` was
/// attempted: Git removal, filesystem removal, then a targeted retry for any
/// administrative record whose checkout disappeared during cleanup.
pub(crate) fn cleanup_failed_add(
    git_executable: &Path,
    parent: &Path,
    lock: &RepositoryLock,
    destination: &Path,
) {
    let cancellation = Cancellation::default();
    let _ = remove_known_unlocked(
        git_executable,
        parent,
        lock,
        destination,
        true,
        &cancellation,
    );
    let _ = fs::remove_dir_all(destination);
    let _ = remove_known_unlocked(
        git_executable,
        parent,
        lock,
        destination,
        true,
        &cancellation,
    );
}

/// Lists the main and linked worktrees in Git's stable porcelain format.
pub(crate) fn list(
    git_executable: &Path,
    parent: &Path,
    cancellation: &Cancellation,
) -> Result<Vec<GitWorktree>, GitError> {
    // Validate the exact path in-process before spawning Git. Without this,
    // Git discovers upward from a catalogued subdirectory and reports an
    // unrelated ancestor repository's worktrees.
    branch::open(parent)?;
    let output = GitCommand::new(git_executable, parent, GitAccess::LocalRead)
        .args(["worktree", "list", "--porcelain", "-z"])
        .run(cancellation)?;
    parse_porcelain(&output.stdout)
}

fn parse_porcelain(output: &[u8]) -> Result<Vec<GitWorktree>, GitError> {
    let mut listed = Vec::new();
    let mut root = None;
    let mut branch = None;
    let mut locked = None;
    let mut prunable = false;

    for field in output.split(|byte| *byte == 0) {
        if field.is_empty() {
            if let Some(root) = root.take() {
                listed.push(GitWorktree {
                    root,
                    branch: branch.take(),
                    locked: locked.take(),
                    prunable,
                });
                prunable = false;
            }
            continue;
        }
        if let Some(path) = field.strip_prefix(b"worktree ") {
            if let Some(root) = root.take() {
                listed.push(GitWorktree {
                    root,
                    branch: branch.take(),
                    locked: locked.take(),
                    prunable,
                });
            }
            root = Some(path_from_git(path)?);
            branch = None;
            locked = None;
            prunable = false;
        } else if let Some(reference) = field.strip_prefix(b"branch refs/heads/") {
            branch = Some(String::from_utf8(reference.to_vec()).map_err(|_| {
                GitError::MalformedStatus {
                    detail: "worktree branch is not UTF-8".to_owned(),
                }
            })?);
        } else if field == b"detached" {
            branch = None;
        } else if field == b"locked" {
            locked = Some(String::new());
        } else if let Some(reason) = field.strip_prefix(b"locked ") {
            locked = Some(String::from_utf8_lossy(reason).into_owned());
        } else if field == b"prunable" || field.starts_with(b"prunable ") {
            prunable = true;
        }
    }
    if let Some(root) = root {
        listed.push(GitWorktree {
            root,
            branch,
            locked,
            prunable,
        });
    }
    Ok(listed)
}

#[cfg(unix)]
fn path_from_git(path: &[u8]) -> Result<PathBuf, GitError> {
    use std::os::unix::ffi::OsStringExt;

    Ok(PathBuf::from(OsString::from_vec(path.to_vec())))
}

#[cfg(not(unix))]
fn path_from_git(path: &[u8]) -> Result<PathBuf, GitError> {
    String::from_utf8(path.to_vec())
        .map(PathBuf::from)
        .map_err(|_| GitError::MalformedStatus {
            detail: "worktree path is not UTF-8".to_owned(),
        })
}

pub(crate) fn same_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
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
    use super::parse_porcelain;

    #[test]
    fn parses_branch_detached_locked_and_unseparated_porcelain_rows() {
        let rows = parse_porcelain(
            b"worktree /tmp/main\0HEAD aaaa\0branch refs/heads/main\0\
              worktree /tmp/detached\0HEAD bbbb\0detached\0locked portable disk\0\
              prunable stale\0\0",
        )
        .unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].branch.as_deref(), Some("main"));
        assert_eq!(rows[1].branch, None);
        assert_eq!(rows[1].locked.as_deref(), Some("portable disk"));
        assert!(rows[1].prunable);
    }
}
