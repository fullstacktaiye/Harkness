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

/// Refuses a Git-locked row using the shared typed diagnostic.
pub(crate) fn refuse_if_locked(listed: &[GitWorktree], path: &Path) -> Result<(), GitError> {
    let reason = listed
        .iter()
        .find(|worktree| same_path(&worktree.root, path))
        .and_then(|worktree| worktree.locked.clone());
    if let Some(reason) = reason {
        Err(GitError::WorktreeLocked {
            path: path.to_path_buf(),
            reason: (!reason.is_empty()).then_some(reason),
        })
    } else {
        Ok(())
    }
}

/// Rejects lock requests that would leave later refusals without a reason and
/// returns the exact text Git will store.
///
/// Git trims a lock reason on both ends when it reads the reason back, so the
/// trimmed form is the only spelling that round-trips through
/// [`parse_porcelain`]. Returning it here keeps a caller's stored reason equal
/// to the one a later listing reports.
///
/// This check is intentionally independent of the repository so callers can
/// perform it before any Git process is spawned.
pub(crate) fn validate_lock_reason(reason: &str) -> Result<&str, GitError> {
    let trimmed = reason.trim();
    if trimmed.is_empty() {
        Err(GitError::EmptyWorktreeLockReason)
    } else {
        Ok(trimmed)
    }
}

/// Locks a worktree after its caller has proved the row is currently unlocked.
///
/// The reason is re-validated here rather than trusted from the caller so the
/// primitive stays safe for any future caller outside `ProjectService`;
/// `ProjectService` deliberately validates earlier as well so an invalid
/// request never reaches a porcelain listing.
pub(crate) fn lock_known_unlocked(
    git_executable: &Path,
    parent: &Path,
    _lock: &RepositoryLock,
    destination: &Path,
    reason: &str,
    cancellation: &Cancellation,
) -> Result<(), GitError> {
    let reason = validate_lock_reason(reason)?;
    GitCommand::new(git_executable, parent, GitAccess::LocalWrite)
        .args(["worktree", "lock", "--reason", reason, "--"])
        .arg(destination)
        .run(cancellation)?;
    Ok(())
}

/// Unlocks a worktree after its caller has proved the row is currently locked.
pub(crate) fn unlock_known_locked(
    git_executable: &Path,
    parent: &Path,
    _lock: &RepositoryLock,
    destination: &Path,
    cancellation: &Cancellation,
) -> Result<(), GitError> {
    GitCommand::new(git_executable, parent, GitAccess::LocalWrite)
        .args(["worktree", "unlock", "--"])
        .arg(destination)
        .run(cancellation)?;
    Ok(())
}

/// Relocates a worktree after the caller has identified its unlocked row while
/// holding the repository lock.
pub(crate) fn move_known_unlocked(
    git_executable: &Path,
    parent: &Path,
    _lock: &RepositoryLock,
    source: &Path,
    destination: &Path,
    cancellation: &Cancellation,
) -> Result<(), GitError> {
    match GitCommand::new(git_executable, parent, GitAccess::LocalWrite)
        .args(["worktree", "move", "--"])
        .arg(source)
        .arg(destination)
        .run(cancellation)
    {
        Err(GitError::Failed { stderr, .. }) if is_cross_device_move_diagnostic(&stderr) => {
            Err(GitError::WorktreeMoveAcrossDevices {
                worktree: source.to_path_buf(),
                destination: destination.to_path_buf(),
                stderr,
            })
        }
        Err(error) => Err(error),
        Ok(_) => Ok(()),
    }
}

fn is_cross_device_move_diagnostic(stderr: &str) -> bool {
    let diagnostic = stderr.to_ascii_lowercase();
    diagnostic.contains("cross-device") || diagnostic.contains("not same device")
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
    match (
        canonicalize_with_missing_tail(left),
        canonicalize_with_missing_tail(right),
    ) {
        (Some(left), Some(right)) => left == right,
        _ => left == right,
    }
}

/// Finds a live worktree by the stable name of its Git administrative record.
///
/// Harkness initially creates each checkout at `worktrees/<project-id>`, so
/// Git gives its administrative directory that UUID as well. `git worktree
/// move` deliberately keeps that directory name, and the moved checkout's
/// `.git` file continues to name it. That makes the UUID the identity needed to
/// repair a catalog write interrupted after Git has already moved the checkout.
pub(crate) fn registered_root<'a>(
    listed: &'a [GitWorktree],
    administrative_name: &str,
) -> Option<&'a Path> {
    listed
        .iter()
        .find(|worktree| {
            administrative_name_at(&worktree.root).as_deref() == Some(administrative_name)
        })
        .map(|worktree| worktree.root.as_path())
}

fn administrative_name_at(root: &Path) -> Option<String> {
    let contents = fs::read(root.join(".git")).ok()?;
    let mut git_dir = contents.strip_prefix(b"gitdir: ")?;
    while git_dir
        .last()
        .is_some_and(|byte| matches!(*byte, b'\n' | b'\r'))
    {
        git_dir = &git_dir[..git_dir.len() - 1];
    }
    let path = path_from_git(git_dir).ok()?;
    path.file_name()?.to_str().map(str::to_owned)
}

/// Canonicalizes as much of a path as still exists, then restores its missing
/// suffix. Git retains administrative rows after a checkout is deleted, and
/// on Windows its path spelling can differ from Rust's canonical catalog path
/// (notably the extended-length prefix). Comparing the nearest surviving
/// ancestor keeps those stale rows matchable without requiring the leaf to
/// exist.
fn canonicalize_with_missing_tail(path: &Path) -> Option<PathBuf> {
    let mut ancestor = path;
    let mut missing = Vec::new();

    loop {
        if let Ok(mut canonical) = ancestor.canonicalize() {
            for component in missing.iter().rev() {
                canonical.push(component);
            }
            return Some(canonical);
        }

        missing.push(ancestor.file_name()?.to_os_string());
        ancestor = ancestor.parent()?;
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
    use std::fs;

    #[cfg(unix)]
    use super::same_path;
    use super::{administrative_name_at, parse_porcelain};

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

    /// A bare `locked` field and a `locked <reason>` field are different
    /// states, and only the NUL-delimited format keeps a reason containing a
    /// newline in one field. Dropping `-z` would silently split such a row.
    #[test]
    fn lock_rows_separate_absent_reasons_from_multi_line_ones() {
        let rows = parse_porcelain(
            b"worktree /tmp/bare\0HEAD aaaa\0detached\0locked\0\
              worktree /tmp/multiline\0HEAD bbbb\0detached\0locked first\nsecond\0\0",
        )
        .unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].locked.as_deref(), Some(""));
        assert_eq!(rows[1].locked.as_deref(), Some("first\nsecond"));
    }

    #[test]
    fn lock_reasons_are_validated_against_the_text_git_stores() {
        use super::validate_lock_reason;

        for blank in ["", " ", " \t\n "] {
            assert!(validate_lock_reason(blank).is_err(), "accepted {blank:?}");
        }
        // Git trims what it stores, so the trimmed spelling is the only one
        // that round-trips back through a listing.
        assert_eq!(
            validate_lock_reason("  agent busy  ").unwrap(),
            "agent busy"
        );
        // A reason that looks like a flag is a value, never an argument.
        assert_eq!(validate_lock_reason("--force").unwrap(), "--force");
    }

    #[test]
    fn administrative_names_accept_git_line_endings_and_relative_paths() {
        let fixture = tempfile::tempdir().unwrap();
        for (name, contents) in [
            ("no-newline", b"gitdir: ../admin/no-newline".as_slice()),
            ("crlf", b"gitdir: ../admin/crlf\r\n".as_slice()),
            (
                "repeated-endings",
                b"gitdir: ../admin/repeated-endings\r\n\n\r".as_slice(),
            ),
        ] {
            let root = fixture.path().join(name);
            fs::create_dir(&root).unwrap();
            fs::write(root.join(".git"), contents).unwrap();
            assert_eq!(administrative_name_at(&root).as_deref(), Some(name));
        }
    }

    #[test]
    fn administrative_names_fail_closed_for_unowned_shapes() {
        let fixture = tempfile::tempdir().unwrap();
        let absent = fixture.path().join("absent");
        fs::create_dir(&absent).unwrap();
        assert_eq!(administrative_name_at(&absent), None);

        let directory = fixture.path().join("git-directory");
        fs::create_dir_all(directory.join(".git")).unwrap();
        assert_eq!(administrative_name_at(&directory), None);

        let payload = fixture.path().join("wrong-payload");
        fs::create_dir(&payload).unwrap();
        fs::write(payload.join(".git"), "not-gitdir: ../admin/foreign\n").unwrap();
        assert_eq!(administrative_name_at(&payload), None);
    }

    #[cfg(unix)]
    #[test]
    fn missing_paths_are_compared_through_their_canonical_ancestor() {
        use std::{fs, os::unix::fs::symlink};

        let fixture = tempfile::tempdir().unwrap();
        let actual = fixture.path().join("actual");
        let alias = fixture.path().join("alias");
        fs::create_dir(&actual).unwrap();
        symlink(&actual, &alias).unwrap();

        assert!(same_path(
            &actual.join("missing").join("checkout"),
            &alias.join("missing").join("checkout")
        ));
    }
}
