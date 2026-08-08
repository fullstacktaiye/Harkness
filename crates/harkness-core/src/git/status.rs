//! Repository status, in two tiers.
//!
//! [`inspect`] is the cheap tier: one in-process libgit2 walk, run for every
//! catalog entry on every read, which is why it must never spawn a process.
//! [`detailed`] is the on-demand tier: one `git status --porcelain=v2` for the
//! single project a caller names.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use git2::{Branch, ErrorCode, Repository, RepositoryState, Status, StatusOptions};

use crate::{
    catalog::entry::{GitStatus, UpstreamStatus},
    git::{
        GitError,
        runner::{Cancellation, GitAccess, GitCommand},
    },
};

/// Where the checked-out commit sits relative to the branch namespace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HeadState {
    /// The branch exists but has no commit yet. `branch` is the name the first
    /// commit will create.
    Unborn { branch: Option<String> },
    /// A named branch is checked out.
    Branch { name: String },
    /// A commit is checked out with no branch.
    Detached { commit: String },
}

/// A multi-step Git operation a repository is in the middle of.
///
/// Every one of these is a state a command can be left in rather than a state a
/// command runs in: Git wrote `MERGE_HEAD` or a `rebase-merge` directory,
/// stopped, and is waiting for someone to finish or abort what it started. That
/// makes it the one thing a front end has to know after a failure, because the
/// working tree has changed and no further operation will run until it is
/// resolved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PendingOperation {
    /// A merge stopped, usually at a conflict.
    Merge,
    /// A rebase stopped part-way through replaying commits.
    Rebase,
    /// A cherry-pick stopped.
    CherryPick,
    /// A revert stopped.
    Revert,
    /// A bisection is in progress.
    Bisect,
    /// A mailbox of patches is being applied.
    ApplyMailbox,
    /// Git reports an operation in progress that Harkness does not name.
    Other,
}

impl fmt::Display for PendingOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Merge => "merge",
            Self::Rebase => "rebase",
            Self::CherryPick => "cherry-pick",
            Self::Revert => "revert",
            Self::Bisect => "bisection",
            Self::ApplyMailbox => "patch application",
            Self::Other => "Git operation",
        })
    }
}

/// What the repository is in the middle of, if anything.
///
/// Read in process from the state files Git leaves behind, so it costs nothing
/// and stays available on the failure paths that need it most.
pub(crate) fn pending(repository: &Repository) -> Option<PendingOperation> {
    match repository.state() {
        RepositoryState::Clean => None,
        RepositoryState::Merge => Some(PendingOperation::Merge),
        RepositoryState::Revert | RepositoryState::RevertSequence => Some(PendingOperation::Revert),
        RepositoryState::CherryPick | RepositoryState::CherryPickSequence => {
            Some(PendingOperation::CherryPick)
        }
        RepositoryState::Bisect => Some(PendingOperation::Bisect),
        RepositoryState::Rebase
        | RepositoryState::RebaseInteractive
        | RepositoryState::RebaseMerge => Some(PendingOperation::Rebase),
        RepositoryState::ApplyMailbox | RepositoryState::ApplyMailboxOrRebase => {
            Some(PendingOperation::ApplyMailbox)
        }
    }
}

/// How one path changed, on one side of the index.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileChange {
    /// Present here and absent on the other side.
    Added,
    /// Present on both sides with different content.
    Modified,
    /// Absent here and present on the other side.
    Deleted,
    /// Moved from another path, named by [`StatusEntry::rename_source`].
    Renamed,
    /// Copied from another path, named by [`StatusEntry::rename_source`].
    Copied,
    /// The same path changed between a file, a symlink and a submodule.
    TypeChanged,
    /// Not tracked by Git at all.
    Untracked,
    /// Left unresolved by a merge.
    Unmerged,
}

impl fmt::Display for FileChange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
            Self::Copied => "copied",
            Self::TypeChanged => "type-changed",
            Self::Untracked => "untracked",
            Self::Unmerged => "unmerged",
        })
    }
}

/// One path reported by a detailed status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusEntry {
    /// The path, relative to the repository root and kept as raw bytes on
    /// platforms where a path need not be UTF-8.
    pub path: PathBuf,
    /// How the index differs from HEAD.
    pub staged: Option<FileChange>,
    /// How the working tree differs from the index.
    pub unstaged: Option<FileChange>,
    /// Where a renamed or copied path came from.
    pub rename_source: Option<PathBuf>,
    /// Whether a merge left this path unresolved.
    pub conflicted: bool,
}

/// Everything one `git status` invocation reports about a repository.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DetailedStatus {
    /// What is checked out.
    pub head: HeadState,
    /// The tracked branch and the divergence from it, when one is configured.
    pub upstream: Option<UpstreamStatus>,
    /// A multi-step Git operation waiting to be completed or aborted.
    pub pending: Option<PendingOperation>,
    /// Every changed, untracked or conflicted path.
    pub entries: Vec<StatusEntry>,
}

impl DetailedStatus {
    /// Whether any path was left unresolved by a merge.
    #[must_use]
    pub fn has_conflicts(&self) -> bool {
        self.entries.iter().any(|entry| entry.conflicted)
    }
}

/// Describes the repository whose working directory is `path`.
///
/// `None` means the directory is not the working tree of a repository, which is
/// an ordinary answer rather than a failure: most imported directories are not.
///
/// Runs entirely in process. Divergence is resolved from local refs, so a
/// listing never touches the network and never blocks on one.
pub(crate) fn inspect(path: &Path) -> Result<Option<GitStatus>, GitError> {
    // Structurally rather than after the fact: `Repository::open` does not walk
    // upward the way `discover` does, so a plain directory nested inside a
    // repository cannot report its ancestor's state. `exists` rather than
    // `is_dir`, because a linked worktree's `.git` is a file.
    if !path.join(".git").exists() {
        return Ok(None);
    }
    let repository = match Repository::open(path) {
        Ok(repository) => repository,
        Err(error) if error.code() == ErrorCode::NotFound => return Ok(None),
        Err(source) => return Err(inspection(path, source)),
    };
    // A bare repository has no working tree to describe.
    if repository.workdir().is_none() {
        return Ok(None);
    }

    let (branch, upstream) = match repository.head() {
        Ok(head) if head.is_branch() => {
            let name = head
                .shorthand()
                .map_err(|source| inspection(path, source))?;
            let name = name.to_owned();
            let local = head.target();
            (Some(name), divergence(path, &repository, head, local)?)
        }
        // A detached head has no branch, and therefore nothing to diverge from.
        Ok(_) => (None, None),
        Err(error) if matches!(error.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => {
            (None, None)
        }
        Err(source) => return Err(inspection(path, source)),
    };

    // One walk answers all three questions. `dirty` only asks whether any entry
    // differs, so untracked directories are left unrecursed: libgit2 still
    // reports the directory itself, at a fraction of the cost of walking a
    // large `target/` or `node_modules/`.
    let mut options = StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(false)
        .include_ignored(false);
    let statuses = repository
        .statuses(Some(&mut options))
        .map_err(|source| inspection(path, source))?;
    let dirty = !statuses.is_empty();
    let mut staged = 0;
    let mut unstaged = 0;
    for entry in statuses.iter() {
        if is_staged(entry.status()) {
            staged += 1;
        }
        if is_unstaged(entry.status()) {
            unstaged += 1;
        }
    }

    Ok(Some(GitStatus {
        branch,
        dirty,
        upstream,
        staged,
        unstaged,
    }))
}

/// Reports the detailed status of the repository rooted at `root`.
///
/// One spawn, bounded by the local-read timeout, so it cannot hang a caller
/// even on a repository whose index refresh is slow.
pub(crate) fn detailed(
    git_executable: &Path,
    root: &Path,
    cancellation: &Cancellation,
) -> Result<DetailedStatus, GitError> {
    let repository = Repository::open(root).map_err(|source| inspection(root, source))?;
    let output = GitCommand::new(git_executable, root, GitAccess::LocalRead)
        .args([
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            // Explicit, so a user's `status.showUntrackedFiles` cannot change
            // the shape of the output this parser depends on.
            "--untracked-files=normal",
            "--ignored=no",
        ])
        .run(cancellation)?;
    let mut status = parse_porcelain_v2(&output.stdout)?;
    status.pending = pending(&repository);
    Ok(status)
}

/// Counts as a difference between the index and HEAD.
fn is_staged(status: Status) -> bool {
    status.intersects(
        Status::INDEX_NEW
            | Status::INDEX_MODIFIED
            | Status::INDEX_DELETED
            | Status::INDEX_RENAMED
            | Status::INDEX_TYPECHANGE,
    )
}

/// Counts as a difference between the working tree and the index.
///
/// `WT_NEW` is deliberately absent. Untracked directories are not recursed, so
/// an untracked tree reports as one entry and counting it would report a
/// directory as though it were a file. A conflicted path counts here, because
/// it is work that has to be resolved before anything can be committed.
fn is_unstaged(status: Status) -> bool {
    status.intersects(
        Status::WT_MODIFIED
            | Status::WT_DELETED
            | Status::WT_RENAMED
            | Status::WT_TYPECHANGE
            | Status::CONFLICTED,
    )
}

/// Resolves the upstream of a checked-out branch and the distance to it.
///
/// A branch with no upstream configured, or whose remote-tracking ref has never
/// been fetched, simply has no divergence to report.
fn divergence(
    path: &Path,
    repository: &Repository,
    head: git2::Reference<'_>,
    local: Option<git2::Oid>,
) -> Result<Option<UpstreamStatus>, GitError> {
    let branch = Branch::wrap(head);
    let upstream = match branch.upstream() {
        Ok(upstream) => upstream,
        Err(error) if error.code() == ErrorCode::NotFound => return Ok(None),
        Err(source) => return Err(inspection(path, source)),
    };
    let name = match upstream.name() {
        Ok(Some(name)) => name.to_owned(),
        Ok(None) => return Ok(None),
        Err(source) => return Err(inspection(path, source)),
    };
    let (Some(local), Some(tracked)) = (local, upstream.get().target()) else {
        return Ok(None);
    };
    // Purely a walk of local refs: `graph_ahead_behind` counts the commits each
    // side already has, and never contacts the remote to find out what it has
    // now.
    let (ahead, behind) = repository
        .graph_ahead_behind(local, tracked)
        .map_err(|source| inspection(path, source))?;
    Ok(Some(UpstreamStatus {
        name,
        ahead,
        behind,
    }))
}

fn inspection(path: &Path, source: git2::Error) -> GitError {
    GitError::Inspection {
        path: path.to_path_buf(),
        source,
    }
}

/// Parses `git status --porcelain=v2 --branch -z` output.
///
/// The `-z` form terminates every record with a NUL instead of a newline, which
/// is what makes a path containing a newline representable. A rename record is
/// the one place where a single record spans two NUL-separated fields; reading
/// one field there would desynchronize every record that follows it.
fn parse_porcelain_v2(output: &[u8]) -> Result<DetailedStatus, GitError> {
    let mut oid = None;
    let mut head_name = None;
    let mut upstream_name = None;
    let mut ahead_behind = None;
    let mut entries = Vec::new();

    let mut records = output.split(|&byte| byte == 0);
    while let Some(record) = records.next() {
        // The output ends with a terminator, so the final split is empty.
        if record.is_empty() {
            continue;
        }
        match record[0] {
            b'#' => parse_header(
                record,
                &mut oid,
                &mut head_name,
                &mut upstream_name,
                &mut ahead_behind,
            )?,
            b'1' => entries.push(parse_changed(record, ORDINARY_FIELDS, None)?),
            b'2' => {
                let source = records
                    .next()
                    .filter(|source| !source.is_empty())
                    .ok_or_else(|| malformed("a rename record is missing its source path"))?;
                entries.push(parse_changed(record, RENAME_FIELDS, Some(source))?);
            }
            b'u' => entries.push(parse_unmerged(record)?),
            b'?' => entries.push(parse_untracked(record)?),
            // `--ignored=no` suppresses these. Skipping rather than rejecting
            // keeps a repository configuration Harkness did not anticipate from
            // failing the whole parse.
            b'!' => {}
            _ => {
                return Err(malformed(format!(
                    "unknown status record '{}'",
                    String::from_utf8_lossy(record)
                )));
            }
        }
    }

    let (Some(oid), Some(head_name)) = (oid, head_name) else {
        return Err(malformed(
            "the status output carries no '# branch.oid' and '# branch.head' pair",
        ));
    };
    let head = if oid == INITIAL_COMMIT {
        HeadState::Unborn {
            branch: (head_name != DETACHED_HEAD).then_some(head_name),
        }
    } else if head_name == DETACHED_HEAD {
        HeadState::Detached { commit: oid }
    } else {
        HeadState::Branch { name: head_name }
    };
    // `# branch.ab` is absent when the upstream ref itself is missing, which
    // leaves an upstream that is configured but that nothing local can be
    // measured against.
    let upstream = upstream_name.map(|name| {
        let (ahead, behind) = ahead_behind.unwrap_or((0, 0));
        UpstreamStatus {
            name,
            ahead,
            behind,
        }
    });

    Ok(DetailedStatus {
        head,
        upstream,
        pending: None,
        entries,
    })
}

/// `# branch.oid` reports this instead of a commit before the first commit.
const INITIAL_COMMIT: &str = "(initial)";

/// `# branch.head` reports this instead of a name when HEAD is detached.
const DETACHED_HEAD: &str = "(detached)";

/// `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>`
const ORDINARY_FIELDS: usize = 9;

/// `2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path>`
const RENAME_FIELDS: usize = 10;

/// `u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>`
const UNMERGED_FIELDS: usize = 11;

/// `? <path>`
const UNTRACKED_FIELDS: usize = 2;

fn parse_header(
    record: &[u8],
    oid: &mut Option<String>,
    head_name: &mut Option<String>,
    upstream_name: &mut Option<String>,
    ahead_behind: &mut Option<(usize, usize)>,
) -> Result<(), GitError> {
    // A branch name is a byte string like any other ref name, so this is lossy
    // rather than fallible: a name Harkness cannot render is still better
    // reported approximately than not at all.
    let header = String::from_utf8_lossy(record);
    let mut fields = header.splitn(3, ' ');
    let (Some("#"), Some(key)) = (fields.next(), fields.next()) else {
        return Ok(());
    };
    let value = fields.next().unwrap_or_default();
    match key {
        "branch.oid" => *oid = Some(value.to_owned()),
        "branch.head" => *head_name = Some(value.to_owned()),
        "branch.upstream" => *upstream_name = Some(value.to_owned()),
        "branch.ab" => *ahead_behind = Some(parse_ahead_behind(value)?),
        // `# stash <n>`, and whatever a later Git adds.
        _ => {}
    }
    Ok(())
}

/// Parses the `+<ahead> -<behind>` value of `# branch.ab`.
fn parse_ahead_behind(value: &str) -> Result<(usize, usize), GitError> {
    let mut counts = value.split_whitespace();
    let ahead = counts
        .next()
        .and_then(|count| count.strip_prefix('+'))
        .and_then(|count| count.parse().ok());
    let behind = counts
        .next()
        .and_then(|count| count.strip_prefix('-'))
        .and_then(|count| count.parse().ok());
    match (ahead, behind) {
        (Some(ahead), Some(behind)) => Ok((ahead, behind)),
        _ => Err(malformed(format!(
            "'# branch.ab {value}' is not a divergence pair"
        ))),
    }
}

/// Parses an ordinary or a rename record, which differ only in field count and
/// in carrying a source path.
fn parse_changed(
    record: &[u8],
    fields: usize,
    rename_source: Option<&[u8]>,
) -> Result<StatusEntry, GitError> {
    let fields = split_record(record, fields)?;
    let (staged, unstaged) = change_kinds(fields[1])?;
    Ok(StatusEntry {
        path: path_from_bytes(fields[fields.len() - 1]),
        staged,
        unstaged,
        rename_source: rename_source.map(path_from_bytes),
        conflicted: false,
    })
}

fn parse_unmerged(record: &[u8]) -> Result<StatusEntry, GitError> {
    let fields = split_record(record, UNMERGED_FIELDS)?;
    let (staged, unstaged) = change_kinds(fields[1])?;
    Ok(StatusEntry {
        path: path_from_bytes(fields[UNMERGED_FIELDS - 1]),
        staged,
        unstaged,
        rename_source: None,
        conflicted: true,
    })
}

fn parse_untracked(record: &[u8]) -> Result<StatusEntry, GitError> {
    let fields = split_record(record, UNTRACKED_FIELDS)?;
    Ok(StatusEntry {
        path: path_from_bytes(fields[UNTRACKED_FIELDS - 1]),
        staged: None,
        unstaged: Some(FileChange::Untracked),
        rename_source: None,
        conflicted: false,
    })
}

/// Splits a record into exactly `fields` space-separated fields.
///
/// The split is bounded so the trailing path keeps any spaces it contains.
fn split_record(record: &[u8], fields: usize) -> Result<Vec<&[u8]>, GitError> {
    let split = record
        .splitn(fields, |&byte| byte == b' ')
        .collect::<Vec<_>>();
    if split.len() != fields || split[fields - 1].is_empty() {
        return Err(malformed(format!(
            "status record '{}' does not have {fields} fields",
            String::from_utf8_lossy(record)
        )));
    }
    Ok(split)
}

/// Splits the two-letter `XY` field into its staged and unstaged halves.
fn change_kinds(field: &[u8]) -> Result<(Option<FileChange>, Option<FileChange>), GitError> {
    let [staged, unstaged] = field else {
        return Err(malformed(format!(
            "'{}' is not a two-letter change field",
            String::from_utf8_lossy(field)
        )));
    };
    Ok((change_kind(*staged)?, change_kind(*unstaged)?))
}

fn change_kind(code: u8) -> Result<Option<FileChange>, GitError> {
    Ok(match code {
        b'.' => None,
        b'A' => Some(FileChange::Added),
        b'M' => Some(FileChange::Modified),
        b'D' => Some(FileChange::Deleted),
        b'R' => Some(FileChange::Renamed),
        b'C' => Some(FileChange::Copied),
        b'T' => Some(FileChange::TypeChanged),
        b'U' => Some(FileChange::Unmerged),
        unknown => {
            return Err(malformed(format!(
                "unknown change code '{}'",
                char::from(unknown)
            )));
        }
    })
}

fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
        PathBuf::from(OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

fn malformed(detail: impl Into<String>) -> GitError {
    GitError::MalformedStatus {
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use git2::{Repository, WorktreeAddOptions};

    use super::{DetailedStatus, FileChange, HeadState, StatusEntry, parse_porcelain_v2};
    use crate::{
        catalog::entry::UpstreamStatus,
        git::{Cancellation, GitAccess, GitCommand, GitError, GitService},
        testing::{Fixture, commit_all, initialize_repository},
    };

    /// A branch checked out with one staged and one unstaged change, as real
    /// Git emits it.
    const SAMPLE: &[u8] = b"# branch.oid 1111111111111111111111111111111111111111\0\
        # branch.head main\0\
        # branch.upstream origin/main\0\
        # branch.ab +2 -3\0\
        1 M. N... 100644 100644 100644 aaa bbb staged.txt\0\
        1 .M N... 100644 100644 100644 ccc ddd unstaged.txt\0\
        ? untracked.txt\0";

    #[test]
    fn a_branch_with_an_upstream_parses_into_head_divergence_and_entries() {
        let status = parse_porcelain_v2(SAMPLE).unwrap();

        assert_eq!(
            status.head,
            HeadState::Branch {
                name: "main".to_owned()
            }
        );
        assert_eq!(
            status.upstream,
            Some(UpstreamStatus {
                name: "origin/main".to_owned(),
                ahead: 2,
                behind: 3,
            })
        );
        assert_eq!(
            status.entries,
            vec![
                entry("staged.txt", Some(FileChange::Modified), None),
                entry("unstaged.txt", None, Some(FileChange::Modified)),
                entry("untracked.txt", None, Some(FileChange::Untracked)),
            ]
        );
        assert!(!status.has_conflicts());
    }

    #[test]
    fn an_unborn_head_keeps_the_branch_the_first_commit_will_create() {
        let status = parse_porcelain_v2(b"# branch.oid (initial)\0# branch.head main\0").unwrap();

        assert_eq!(
            status.head,
            HeadState::Unborn {
                branch: Some("main".to_owned())
            }
        );
        assert_eq!(status.upstream, None);
        assert!(status.entries.is_empty());
    }

    #[test]
    fn a_detached_head_reports_its_commit() {
        let status = parse_porcelain_v2(
            b"# branch.oid 2222222222222222222222222222222222222222\0# branch.head (detached)\0",
        )
        .unwrap();

        assert_eq!(
            status.head,
            HeadState::Detached {
                commit: "2222222222222222222222222222222222222222".to_owned()
            }
        );
    }

    /// Git omits `# branch.ab` when the upstream ref is configured but absent,
    /// which must leave the upstream reported rather than dropped.
    #[test]
    fn an_absent_divergence_header_leaves_the_upstream_at_zero() {
        let status = parse_porcelain_v2(
            b"# branch.oid 3333333333333333333333333333333333333333\0\
              # branch.head main\0\
              # branch.upstream origin/main\0",
        )
        .unwrap();

        assert_eq!(
            status.upstream,
            Some(UpstreamStatus {
                name: "origin/main".to_owned(),
                ahead: 0,
                behind: 0,
            })
        );
    }

    /// A rename record's path field is two NUL-separated values. Consuming one
    /// would leave the source path to be read as the next record, desyncing
    /// every record after it, so the record that follows is asserted too.
    #[test]
    fn a_rename_record_consumes_both_of_its_paths() {
        let status = parse_porcelain_v2(
            b"# branch.oid 4444444444444444444444444444444444444444\0\
              # branch.head main\0\
              2 R. N... 100644 100644 100644 aaa bbb R100 new name.txt\0old name.txt\0\
              1 .M N... 100644 100644 100644 ccc ddd after.txt\0",
        )
        .unwrap();

        assert_eq!(
            status.entries,
            vec![
                StatusEntry {
                    path: PathBuf::from("new name.txt"),
                    staged: Some(FileChange::Renamed),
                    unstaged: None,
                    rename_source: Some(PathBuf::from("old name.txt")),
                    conflicted: false,
                },
                entry("after.txt", None, Some(FileChange::Modified)),
            ]
        );
    }

    #[test]
    fn a_conflict_record_is_reported_as_conflicted() {
        let status = parse_porcelain_v2(
            b"# branch.oid 5555555555555555555555555555555555555555\0\
              # branch.head main\0\
              u UU N... 100644 100644 100644 100644 aaa bbb ccc conflict.txt\0",
        )
        .unwrap();

        assert_eq!(
            status.entries,
            vec![StatusEntry {
                path: PathBuf::from("conflict.txt"),
                staged: Some(FileChange::Unmerged),
                unstaged: Some(FileChange::Unmerged),
                rename_source: None,
                conflicted: true,
            }]
        );
        assert!(status.has_conflicts());
    }

    /// Git paths are byte strings. Decoding them as UTF-8 would either fail the
    /// whole status or silently rename the file the caller then acts on.
    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_utf8_survives_parsing() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

        let mut output =
            b"# branch.oid 6666666666666666666666666666666666666666\0# branch.head main\0? "
                .to_vec();
        output.extend_from_slice(b"invalid-\xff-name.txt\0");

        let status = parse_porcelain_v2(&output).unwrap();

        assert_eq!(
            status.entries[0].path,
            PathBuf::from(OsStr::from_bytes(b"invalid-\xff-name.txt"))
        );
    }

    #[test]
    fn malformed_records_are_rejected_rather_than_guessed_at() {
        for output in [
            b"1 M. N... 100644 100644 100644 aaa bbb only-entries.txt\0".as_slice(),
            b"# branch.oid 7777777777777777777777777777777777777777\0\
              # branch.head main\0\
              1 M. N... 100644 short.txt\0",
            b"# branch.oid 7777777777777777777777777777777777777777\0\
              # branch.head main\0\
              # branch.ab ahead behind\0",
            b"# branch.oid 7777777777777777777777777777777777777777\0\
              # branch.head main\0\
              2 R. N... 100644 100644 100644 aaa bbb R100 dangling.txt\0",
        ] {
            assert!(
                matches!(
                    parse_porcelain_v2(output),
                    Err(GitError::MalformedStatus { .. })
                ),
                "expected '{}' to be rejected",
                String::from_utf8_lossy(output)
            );
        }
    }

    /// The parser tests above pin the grammar; this pins that real Git still
    /// speaks it.
    #[test]
    fn real_git_output_matches_the_parsed_grammar() {
        let fixture = Fixture::new();
        let root = fixture.directory("detailed-status");
        let repository = initialize_repository(&root);
        std::fs::write(root.join("tracked.txt"), "modified\n").unwrap();
        std::fs::write(root.join("added.txt"), "added\n").unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(Path::new("added.txt")).unwrap();
        index.write().unwrap();
        std::fs::write(root.join("untracked.txt"), "untracked\n").unwrap();

        let status = GitService::new(&root, &fixture.data_dir)
            .detailed_status(&Cancellation::default())
            .unwrap();

        assert_eq!(
            status.head,
            HeadState::Branch {
                name: "main".to_owned()
            }
        );
        assert_eq!(status.upstream, None);
        assert!(entries(&status).contains(&(
            PathBuf::from("added.txt"),
            Some(FileChange::Added),
            None
        )));
        assert!(entries(&status).contains(&(
            PathBuf::from("tracked.txt"),
            None,
            Some(FileChange::Modified)
        )));
        assert!(entries(&status).contains(&(
            PathBuf::from("untracked.txt"),
            None,
            Some(FileChange::Untracked)
        )));
    }

    #[test]
    fn a_directory_that_is_not_a_repository_reports_no_metadata() {
        let fixture = Fixture::new();
        let plain = fixture.directory("plain");
        let nested = plain.join("nested");
        std::fs::create_dir(&nested).unwrap();
        initialize_repository(&plain);

        // The ancestor is a repository and this directory is not, which
        // `Repository::open` cannot confuse the way `discover` would.
        assert_eq!(super::inspect(&nested).unwrap(), None);
    }

    /// A bare repository has no working tree to describe, and the `.git` check
    /// means Harkness never opens one to find that out.
    #[test]
    fn a_bare_repository_reports_no_metadata() {
        let fixture = Fixture::new();
        let bare = fixture.directory("bare.git");
        Repository::init_bare(&bare).unwrap();
        assert!(!bare.join(".git").exists());
        assert!(Repository::open(&bare).is_ok());

        assert_eq!(super::inspect(&bare).unwrap(), None);
    }

    /// The presence check is `exists`, not `is_dir`: a `.git` file is how a
    /// linked worktree points at its parent, and rejecting one would make every
    /// worktree Harkness creates report no Git metadata at all.
    #[test]
    fn a_linked_worktree_reports_its_own_branch() {
        let fixture = Fixture::new();
        let root = fixture.directory("worktree-parent");
        let repository = initialize_repository(&root);
        let worktree_root = fixture.root.path().join("linked-worktree");
        repository
            .worktree("linked", &worktree_root, Some(&WorktreeAddOptions::new()))
            .unwrap();
        assert!(worktree_root.join(".git").is_file());

        let status = super::inspect(&worktree_root).unwrap().unwrap();

        assert_eq!(status.branch.as_deref(), Some("linked"));
        assert!(!status.dirty);
    }

    #[test]
    fn staged_and_unstaged_counts_come_from_one_walk() {
        let fixture = Fixture::new();
        let root = fixture.directory("counted");
        let repository = initialize_repository(&root);
        std::fs::write(root.join("second.txt"), "second\n").unwrap();
        std::fs::write(root.join("third.txt"), "third\n").unwrap();
        commit_all(&repository, "two more files");
        std::fs::write(root.join("tracked.txt"), "staged change\n").unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(Path::new("tracked.txt")).unwrap();
        index.write().unwrap();
        std::fs::write(root.join("second.txt"), "unstaged change\n").unwrap();
        std::fs::write(root.join("third.txt"), "another unstaged change\n").unwrap();
        std::fs::write(root.join("untracked.txt"), "untracked\n").unwrap();

        let status = super::inspect(&root).unwrap().unwrap();

        assert_eq!(status.staged, 1);
        assert_eq!(status.unstaged, 2, "untracked paths are not counted");
        assert!(status.dirty);
    }

    /// Divergence is a walk of local refs. Committing in the origin must change
    /// nothing until a fetch brings the new commit into a local ref.
    #[test]
    fn divergence_is_resolved_without_contacting_the_remote() {
        let fixture = Fixture::new();
        let origin = fixture.directory("divergence-origin");
        let origin_repository = initialize_repository(&origin);
        let mut service = fixture.service();
        let clone = service
            .import_repository(origin.to_str().unwrap(), &Cancellation::default(), |_| {})
            .unwrap();
        let cloned = Repository::open(&clone.root).unwrap();

        let upstream = super::inspect(&clone.root).unwrap().unwrap().upstream;
        assert_eq!(
            upstream,
            Some(UpstreamStatus {
                name: "origin/main".to_owned(),
                ahead: 0,
                behind: 0,
            })
        );

        std::fs::write(clone.root.join("local.txt"), "local\n").unwrap();
        commit_all(&cloned, "local commit");
        std::fs::write(origin.join("remote.txt"), "remote\n").unwrap();
        commit_all(&origin_repository, "remote commit");

        let before_fetch = super::inspect(&clone.root).unwrap().unwrap().upstream;
        assert_eq!(
            before_fetch.map(|upstream| (upstream.ahead, upstream.behind)),
            Some((1, 0)),
            "the origin's commit was seen without a fetch"
        );

        GitCommand::new("git", &clone.root, GitAccess::Network)
            .arg("fetch")
            .run(&Cancellation::default())
            .unwrap();

        let after_fetch = super::inspect(&clone.root).unwrap().unwrap().upstream;
        assert_eq!(
            after_fetch.map(|upstream| (upstream.ahead, upstream.behind)),
            Some((1, 1))
        );
    }

    fn entries(status: &DetailedStatus) -> Vec<(PathBuf, Option<FileChange>, Option<FileChange>)> {
        status
            .entries
            .iter()
            .map(|entry| (entry.path.clone(), entry.staged, entry.unstaged))
            .collect()
    }

    fn entry(path: &str, staged: Option<FileChange>, unstaged: Option<FileChange>) -> StatusEntry {
        StatusEntry {
            path: PathBuf::from(path),
            staged,
            unstaged,
            rename_source: None,
            conflicted: false,
        }
    }
}
