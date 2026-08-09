//! Bounded, byte-preserving commit history inspection.
//!
//! History is repository-local read-only state. Every operation in this file
//! uses libgit2 in process, takes no repository lock and never consults the
//! project catalog or a remote.

use std::path::Path;

use git2::{ErrorCode, Oid, Repository, Sort, Time};

use crate::git::{Cancellation, GitError};

/// The revision set a commit log page walks.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LogRange {
    /// Every commit reachable from one revision.
    Revision { revision: String },
    /// Commits reachable from `reachable_from` but not from `not_from`.
    Excluding {
        reachable_from: String,
        not_from: String,
    },
    /// Commits on `branch` since its merge-base with `base_branch`.
    BranchAgainstBase { branch: String, base_branch: String },
}

/// Bounds and positions one commit log page.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct LogOptions {
    /// The revision set to walk.
    pub range: LogRange,
    /// Maximum number of commit rows in the returned page.
    pub limit: usize,
    /// First commit to return, normally copied from [`LogPage::next_cursor`].
    ///
    /// The cursor is an object ID instead of an offset so commits added at the
    /// range tip cannot move an already-addressed page.
    pub cursor: Option<Oid>,
}

impl LogOptions {
    /// Walks every commit reachable from `revision`.
    #[must_use]
    pub fn new(revision: impl Into<String>, limit: usize) -> Self {
        Self {
            range: LogRange::Revision {
                revision: revision.into(),
            },
            limit,
            cursor: None,
        }
    }

    /// Walks commits reachable from one revision and not from another.
    #[must_use]
    pub fn excluding(
        reachable_from: impl Into<String>,
        not_from: impl Into<String>,
        limit: usize,
    ) -> Self {
        Self {
            range: LogRange::Excluding {
                reachable_from: reachable_from.into(),
                not_from: not_from.into(),
            },
            limit,
            cursor: None,
        }
    }

    /// Walks a branch from its tip down to, but not including, its merge-base
    /// with `base_branch`.
    #[must_use]
    pub fn branch_against_base(
        branch: impl Into<String>,
        base_branch: impl Into<String>,
        limit: usize,
    ) -> Self {
        Self {
            range: LogRange::BranchAgainstBase {
                branch: branch.into(),
                base_branch: base_branch.into(),
            },
            limit,
            cursor: None,
        }
    }

    /// Starts the page at a continuation returned by an earlier page.
    #[must_use]
    pub fn with_cursor(mut self, cursor: Oid) -> Self {
        self.cursor = Some(cursor);
        self
    }
}

/// One byte-preserving Git author or committer signature.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct CommitSignature {
    /// Name bytes exactly as stored in the commit object.
    pub name: Vec<u8>,
    /// Email bytes exactly as stored in the commit object.
    pub email: Vec<u8>,
    /// Epoch seconds, timezone offset and its sign from the commit object.
    pub time: Time,
}

/// One commit in a history page.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct CommitInfo {
    /// Full object ID of the commit.
    pub id: Oid,
    /// Parent IDs in the order recorded by the commit.
    pub parent_ids: Vec<Oid>,
    /// Original author signature.
    pub author: CommitSignature,
    /// Committer signature.
    pub committer: CommitSignature,
    /// First line of [`Self::message`], retained as bytes.
    pub summary: Vec<u8>,
    /// Full raw commit message, including its original whitespace and bytes.
    pub message: Vec<u8>,
}

/// One bounded page of newest-first history.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct LogPage {
    /// At most [`LogOptions::limit`] commit rows.
    pub commits: Vec<CommitInfo>,
    /// First unreturned commit, if the selected range has another page.
    pub next_cursor: Option<Oid>,
}

pub(crate) fn log(
    root: &Path,
    options: &LogOptions,
    cancellation: &Cancellation,
) -> Result<LogPage, GitError> {
    if options.limit == 0 {
        return Err(GitError::InvalidLogLimit);
    }
    refuse_cancelled(cancellation)?;

    let repository = open(root)?;
    let Some(range) = resolve_range(&repository, root, &options.range)? else {
        return Ok(LogPage {
            commits: Vec::new(),
            next_cursor: None,
        });
    };
    let start = match options.cursor {
        Some(cursor) => require_commit(&repository, root, &cursor.to_string())?,
        None => range.start,
    };

    let mut walk = repository
        .revwalk()
        .map_err(|source| inspection(root, source))?;
    walk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME)
        .map_err(|source| inspection(root, source))?;
    walk.push(start)
        .map_err(|source| inspection(root, source))?;
    if let Some(hidden) = range.hidden {
        walk.hide(hidden)
            .map_err(|source| inspection(root, source))?;
    }

    // Do not trust a caller-provided bound as an allocation request. The walk
    // is still capped by the exact limit, while ordinary pages avoid growth.
    let mut commits = Vec::with_capacity(options.limit.min(256));
    while commits.len() < options.limit {
        refuse_cancelled(cancellation)?;
        let Some(id) = walk.next() else {
            break;
        };
        let id = id.map_err(|source| inspection(root, source))?;
        let commit = repository
            .find_commit(id)
            .map_err(|source| inspection(root, source))?;
        commits.push(commit_info(&commit));
    }

    refuse_cancelled(cancellation)?;
    let next_cursor = walk
        .next()
        .transpose()
        .map_err(|source| inspection(root, source))?;
    Ok(LogPage {
        commits,
        next_cursor,
    })
}

pub(crate) fn resolve_revision(root: &Path, revision: &str) -> Result<Oid, GitError> {
    let repository = open(root)?;
    resolve_object(&repository, root, revision)
}

pub(crate) fn merge_base(root: &Path, one: &str, two: &str) -> Result<Oid, GitError> {
    let repository = open(root)?;
    merge_base_in(&repository, root, one, two)
}

#[derive(Clone, Copy)]
struct ResolvedRange {
    start: Oid,
    hidden: Option<Oid>,
}

fn resolve_range(
    repository: &Repository,
    root: &Path,
    range: &LogRange,
) -> Result<Option<ResolvedRange>, GitError> {
    match range {
        LogRange::Revision { revision } => {
            let Some(start) = resolve_log_start(repository, root, revision)? else {
                return Ok(None);
            };
            Ok(Some(ResolvedRange {
                start,
                hidden: None,
            }))
        }
        LogRange::Excluding {
            reachable_from,
            not_from,
        } => {
            let Some(start) = resolve_log_start(repository, root, reachable_from)? else {
                return Ok(None);
            };
            let hidden = require_commit(repository, root, not_from)?;
            Ok(Some(ResolvedRange {
                start,
                hidden: Some(hidden),
            }))
        }
        LogRange::BranchAgainstBase {
            branch,
            base_branch,
        } => {
            let Some(start) = resolve_log_start(repository, root, branch)? else {
                return Ok(None);
            };
            // Resolve each moving ref once so this page describes one coherent
            // snapshot even if a concurrent mutation advances either branch.
            let base = require_commit(repository, root, base_branch)?;
            let hidden = merge_base_ids(repository, root, start, base, branch, base_branch)?;
            Ok(Some(ResolvedRange {
                start,
                hidden: Some(hidden),
            }))
        }
    }
}

fn resolve_log_start(
    repository: &Repository,
    root: &Path,
    revision: &str,
) -> Result<Option<Oid>, GitError> {
    match require_commit(repository, root, revision) {
        Ok(id) => Ok(Some(id)),
        Err(GitError::RevisionNotFound { .. })
            if matches!(revision, "HEAD" | "@") && is_unborn(repository) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn is_unborn(repository: &Repository) -> bool {
    matches!(
        repository.head(),
        Err(error) if matches!(error.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound)
    )
}

fn merge_base_in(
    repository: &Repository,
    root: &Path,
    one: &str,
    two: &str,
) -> Result<Oid, GitError> {
    let one_id = require_commit(repository, root, one)?;
    let two_id = require_commit(repository, root, two)?;
    merge_base_ids(repository, root, one_id, two_id, one, two)
}

fn merge_base_ids(
    repository: &Repository,
    root: &Path,
    one_id: Oid,
    two_id: Oid,
    one: &str,
    two: &str,
) -> Result<Oid, GitError> {
    repository
        .merge_base(one_id, two_id)
        .map_err(|source| match source.code() {
            ErrorCode::NotFound => GitError::NoMergeBase {
                one: one.to_owned(),
                two: two.to_owned(),
            },
            _ => inspection(root, source),
        })
}

fn require_commit(repository: &Repository, root: &Path, revision: &str) -> Result<Oid, GitError> {
    let object = resolve(repository, root, revision)?;
    let id = object.id();
    object
        .peel_to_commit()
        .map(|commit| commit.id())
        .map_err(|source| match source.code() {
            ErrorCode::NotFound | ErrorCode::InvalidSpec | ErrorCode::Peel => {
                GitError::RevisionNotCommit {
                    revision: revision.to_owned(),
                    id,
                }
            }
            _ => inspection(root, source),
        })
}

fn resolve_object(repository: &Repository, root: &Path, revision: &str) -> Result<Oid, GitError> {
    resolve(repository, root, revision).map(|object| object.id())
}

fn resolve<'repository>(
    repository: &'repository Repository,
    root: &Path,
    revision: &str,
) -> Result<git2::Object<'repository>, GitError> {
    repository
        .revparse_single(revision)
        .map_err(|source| match source.code() {
            ErrorCode::Ambiguous => GitError::AmbiguousRevision {
                revision: revision.to_owned(),
            },
            ErrorCode::NotFound | ErrorCode::UnbornBranch => GitError::RevisionNotFound {
                revision: revision.to_owned(),
            },
            _ => inspection(root, source),
        })
}

fn commit_info(commit: &git2::Commit<'_>) -> CommitInfo {
    let message = commit.message_raw_bytes().to_vec();
    let summary = message
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default()
        .to_vec();
    CommitInfo {
        id: commit.id(),
        parent_ids: commit.parent_ids().collect(),
        author: signature(commit.author()),
        committer: signature(commit.committer()),
        summary,
        message,
    }
}

fn signature(signature: git2::Signature<'_>) -> CommitSignature {
    CommitSignature {
        name: signature.name_bytes().to_vec(),
        email: signature.email_bytes().to_vec(),
        time: signature.when(),
    }
}

fn refuse_cancelled(cancellation: &Cancellation) -> Result<(), GitError> {
    if cancellation.is_cancelled() {
        Err(GitError::Cancelled)
    } else {
        Ok(())
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

fn inspection(path: &Path, source: git2::Error) -> GitError {
    GitError::Inspection {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use git2::{ObjectType, Oid, Repository, Signature, Time, build::CheckoutBuilder};

    use super::LogOptions;
    use crate::{
        git::{Cancellation, GitError, GitService},
        testing::{COMMIT_EPOCH_SECONDS, Fixture, initialize_repository},
    };

    #[test]
    fn linear_history_pages_without_gaps_and_cursor_is_stable_after_prepend() {
        let fixture = Fixture::new();
        let root = fixture.directory("linear-log");
        let repository = initialize_repository(&root);
        let initial = repository.head().unwrap().target().unwrap();
        let second = commit_file(&repository, &root, b"second", 1);
        let third = commit_file(&repository, &root, b"third", 2);
        let fourth = commit_file(&repository, &root, b"fourth", 3);
        let service = GitService::new(&root, &fixture.data_dir);

        let options = LogOptions::new("HEAD", 2);
        let first = service.log(&options, &Cancellation::default()).unwrap();
        assert_eq!(ids(&first), vec![fourth, third]);
        assert_eq!(first.next_cursor, Some(second));

        let continuation = options.clone().with_cursor(first.next_cursor.unwrap());
        let second_page = service
            .log(&continuation, &Cancellation::default())
            .unwrap();
        assert_eq!(ids(&second_page), vec![second, initial]);
        assert_eq!(second_page.next_cursor, None);

        let joined = first
            .commits
            .iter()
            .chain(&second_page.commits)
            .map(|commit| commit.id)
            .collect::<Vec<_>>();
        assert_eq!(joined, vec![fourth, third, second, initial]);

        commit_file(&repository, &root, b"new tip", 4);
        assert_eq!(
            service
                .log(&continuation, &Cancellation::default())
                .unwrap(),
            second_page
        );
    }

    #[test]
    fn exclusion_and_merge_base_ranges_list_only_branch_commits() {
        let fixture = Fixture::new();
        let root = fixture.directory("ranges");
        let repository = initialize_repository(&root);
        let common = repository.head().unwrap().target().unwrap();
        let common_commit = repository.find_commit(common).unwrap();
        repository.branch("feature", &common_commit, false).unwrap();
        drop(common_commit);

        let main = commit_file(&repository, &root, b"main advanced", 1);
        checkout(&repository, "feature");
        let first = commit_file(&repository, &root, b"feature one", 2);
        let second = commit_file(&repository, &root, b"feature two", 3);
        let service = GitService::new(&root, &fixture.data_dir)
            .with_git_executable(root.join("must-not-run"));

        let excluding = service
            .log(
                &LogOptions::excluding("feature", "main", 10),
                &Cancellation::default(),
            )
            .unwrap();
        let against_base = service
            .log(
                &LogOptions::branch_against_base("feature", "main", 10),
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(ids(&excluding), vec![second, first]);
        assert_eq!(against_base, excluding);
        assert_eq!(service.resolve_revision("feature").unwrap(), second);
        assert_eq!(service.resolve_revision("main").unwrap(), main);
        assert_eq!(service.merge_base("feature", "main").unwrap(), common);
        assert!(!fixture.data_dir.exists());
    }

    #[test]
    fn merge_commit_reports_every_parent_in_recorded_order() {
        let fixture = Fixture::new();
        let root = fixture.directory("merge-parents");
        let repository = initialize_repository(&root);
        let common = repository.head().unwrap().target().unwrap();
        let common_commit = repository.find_commit(common).unwrap();
        repository.branch("side", &common_commit, false).unwrap();
        drop(common_commit);

        let main = commit_file(&repository, &root, b"main", 1);
        checkout(&repository, "side");
        let side = commit_file(&repository, &root, b"side", 2);
        checkout(&repository, "main");
        let merge = merge_commit(&repository, b"merge", 3, &[main, side]);

        let page = GitService::new(&root, &fixture.data_dir)
            .log(&LogOptions::new("HEAD", 1), &Cancellation::default())
            .unwrap();
        assert_eq!(page.commits[0].id, merge);
        assert_eq!(page.commits[0].parent_ids, vec![main, side]);
    }

    #[test]
    fn message_and_identity_bytes_survive_without_utf8_round_trip() {
        let fixture = Fixture::new();
        let root = fixture.directory("raw-commit");
        let repository = initialize_repository(&root);
        let parent = repository.head().unwrap().target().unwrap();
        let message = b"summary \xff\nbody \xfe\n";
        let id = raw_commit(&repository, parent, message);

        let page = GitService::new(&root, &fixture.data_dir)
            .log(&LogOptions::new("HEAD", 1), &Cancellation::default())
            .unwrap();
        let commit = &page.commits[0];
        assert_eq!(commit.id, id);
        assert_eq!(commit.summary, b"summary \xff");
        assert_eq!(commit.message, message);
        assert_eq!(commit.author.name, b"Auth\xffor");
        assert_eq!(commit.author.email, b"a\xfe@example.invalid");
        assert_eq!(
            commit.author.time,
            Time::new(COMMIT_EPOCH_SECONDS + 10, -90)
        );
        assert_eq!(commit.committer.name, b"Comm\xfdtter");
        assert_eq!(commit.committer.email, b"c\xfc@example.invalid");
        assert_eq!(
            commit.committer.time,
            Time::new(COMMIT_EPOCH_SECONDS + 20, 120)
        );
    }

    #[test]
    fn revision_resolution_distinguishes_missing_ambiguous_and_non_commit_objects() {
        let fixture = Fixture::new();
        let root = fixture.directory("resolve");
        let repository = initialize_repository(&root);
        let head = repository.head().unwrap().target().unwrap();
        let head_object = repository.find_object(head, None).unwrap();
        repository
            .tag_lightweight("release", &head_object, false)
            .unwrap();
        let tag = repository
            .tag(
                "annotated",
                &head_object,
                &signature(1),
                "annotated tag",
                false,
            )
            .unwrap();
        drop(head_object);
        let service = GitService::new(&root, &fixture.data_dir);

        assert_eq!(service.resolve_revision("main").unwrap(), head);
        assert_eq!(service.resolve_revision("release").unwrap(), head);
        assert_eq!(service.resolve_revision("annotated").unwrap(), tag);
        let tagged = service
            .log(&LogOptions::new("annotated", 1), &Cancellation::default())
            .unwrap();
        assert_eq!(ids(&tagged), vec![head]);
        assert_eq!(
            service.resolve_revision(&head.to_string()[..8]).unwrap(),
            head
        );
        assert!(matches!(
            service.resolve_revision("does-not-exist"),
            Err(GitError::RevisionNotFound { revision }) if revision == "does-not-exist"
        ));

        let first_blob = repository
            .odb()
            .unwrap()
            .write(ObjectType::Blob, &144_u64.to_le_bytes())
            .unwrap();
        let second_blob = repository
            .odb()
            .unwrap()
            .write(ObjectType::Blob, &359_u64.to_le_bytes())
            .unwrap();
        assert_eq!(&first_blob.to_string()[..4], "f6d9");
        assert_eq!(&second_blob.to_string()[..4], "f6d9");
        assert!(matches!(
            service.resolve_revision("f6d9"),
            Err(GitError::AmbiguousRevision { revision }) if revision == "f6d9"
        ));
        assert_eq!(
            service.resolve_revision(&first_blob.to_string()).unwrap(),
            first_blob
        );
        let error = service
            .log(
                &LogOptions::new(first_blob.to_string(), 1),
                &Cancellation::default(),
            )
            .unwrap_err();
        assert!(
            matches!(error, GitError::RevisionNotCommit { id, .. } if id == first_blob),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn unborn_head_is_an_empty_log_and_other_invalid_requests_remain_typed() {
        let fixture = Fixture::new();
        let root = fixture.directory("unborn");
        let repository = Repository::init(&root).unwrap();
        repository.set_head("refs/heads/main").unwrap();
        let service = GitService::new(&root, &fixture.data_dir);

        assert_eq!(
            service
                .log(&LogOptions::new("HEAD", 10), &Cancellation::default())
                .unwrap(),
            super::LogPage {
                commits: Vec::new(),
                next_cursor: None,
            }
        );
        assert!(matches!(
            service.resolve_revision("HEAD"),
            Err(GitError::RevisionNotFound { .. })
        ));
        assert!(matches!(
            service.log(&LogOptions::new("HEAD", 0), &Cancellation::default()),
            Err(GitError::InvalidLogLimit)
        ));

        let cancelled = Cancellation::default();
        cancelled.cancel();
        assert!(matches!(
            service.log(&LogOptions::new("HEAD", 1), &cancelled),
            Err(GitError::Cancelled)
        ));
        assert!(!fixture.data_dir.exists());
    }

    #[test]
    fn unrelated_histories_have_a_typed_missing_merge_base() {
        let fixture = Fixture::new();
        let root = fixture.directory("no-merge-base");
        let repository = initialize_repository(&root);
        let main = repository.head().unwrap().target().unwrap();
        let main_commit = repository.find_commit(main).unwrap();
        let tree = main_commit.tree().unwrap();
        let signature = signature(10);
        repository
            .commit(
                Some("refs/heads/orphan"),
                &signature,
                &signature,
                "orphan",
                &tree,
                &[],
            )
            .unwrap();
        drop(tree);
        drop(main_commit);

        assert!(matches!(
            GitService::new(&root, &fixture.data_dir).merge_base("main", "orphan"),
            Err(GitError::NoMergeBase { one, two }) if one == "main" && two == "orphan"
        ));
    }

    fn ids(page: &super::LogPage) -> Vec<Oid> {
        page.commits.iter().map(|commit| commit.id).collect()
    }

    fn checkout(repository: &Repository, branch: &str) {
        repository
            .set_head(&format!("refs/heads/{branch}"))
            .unwrap();
        repository
            .checkout_head(Some(CheckoutBuilder::new().force()))
            .unwrap();
    }

    fn commit_file(repository: &Repository, root: &Path, message: &[u8], offset: i64) -> Oid {
        fs::write(root.join("tracked.txt"), message).unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(Path::new("tracked.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repository.find_tree(tree_id).unwrap();
        let parent = repository.head().unwrap().peel_to_commit().unwrap();
        let signature = signature(offset);
        repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                std::str::from_utf8(message).unwrap(),
                &tree,
                &[&parent],
            )
            .unwrap()
    }

    fn merge_commit(repository: &Repository, message: &[u8], offset: i64, parents: &[Oid]) -> Oid {
        let parents = parents
            .iter()
            .map(|id| repository.find_commit(*id).unwrap())
            .collect::<Vec<_>>();
        let tree = parents[0].tree().unwrap();
        let signature = signature(offset);
        repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                std::str::from_utf8(message).unwrap(),
                &tree,
                &parents.iter().collect::<Vec<_>>(),
            )
            .unwrap()
    }

    fn signature(offset: i64) -> Signature<'static> {
        Signature::new(
            "Harkness Tests",
            "tests@harkness.invalid",
            &Time::new(COMMIT_EPOCH_SECONDS + offset, 0),
        )
        .unwrap()
    }

    fn raw_commit(repository: &Repository, parent: Oid, message: &[u8]) -> Oid {
        let tree = repository.find_commit(parent).unwrap().tree_id();
        let mut raw = Vec::new();
        raw.extend_from_slice(format!("tree {tree}\nparent {parent}\nauthor ").as_bytes());
        raw.extend_from_slice(b"Auth\xffor <a\xfe@example.invalid> 1700000010 -0130\n");
        raw.extend_from_slice(
            b"committer Comm\xfdtter <c\xfc@example.invalid> 1700000020 +0200\n\n",
        );
        raw.extend_from_slice(message);
        let id = repository
            .odb()
            .unwrap()
            .write(ObjectType::Commit, &raw)
            .unwrap();
        repository
            .reference("refs/heads/main", id, true, "raw byte fixture")
            .unwrap();
        id
    }
}
