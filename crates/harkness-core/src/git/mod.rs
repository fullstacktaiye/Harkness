//! System Git integration.
//!
//! Everything Harkness does with Git goes through this module: one command
//! runner, one per-repository lock, and the two status tiers. [`GitService`] is
//! the front door, addressed by filesystem path so that nothing here needs the
//! project catalog — and therefore nothing here can take the catalog lock out
//! of order.

mod branch;
pub(crate) mod clone;
mod lock;
mod runner;
pub(crate) mod status;
mod sync;

use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use thiserror::Error;

pub use branch::{Branch, BranchKind, CreateBranchOptions};
pub use lock::RepositoryLock;
pub use runner::{Cancellation, CloneCancellation, GitAccess, GitCommand, GitOutput};
pub use status::{DetailedStatus, FileChange, HeadState, PendingOperation, StatusEntry};
pub use sync::{
    FetchOptions, FetchOutcome, PullOptions, PullOutcome, PullStrategy, PushOptions, PushOutcome,
    RefUpdate,
};

use crate::catalog::entry::GitStatus;

/// Failures raised by Git operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum GitError {
    /// The system Git executable could not be launched.
    #[error("failed to start system Git: {source}")]
    Launch {
        #[source]
        source: io::Error,
    },

    /// Git ran and reported failure. Its diagnostic output is retained.
    #[error("git {command} failed: {stderr}")]
    Failed { command: String, stderr: String },

    /// The operation was cancelled through its [`Cancellation`].
    #[error("the Git operation was cancelled")]
    Cancelled,

    /// Git ran past the timeout for its kind of access and was killed.
    #[error("git {command} did not finish within {} seconds", timeout.as_secs())]
    TimedOut { command: String, timeout: Duration },

    /// Another operation holds the repository lock.
    #[error("another operation is already running on the repository at '{}'", path.display())]
    RepositoryBusy { path: PathBuf },

    /// The repository lock file could not be created or locked.
    #[error("failed to lock repository '{}': {source}", path.display())]
    Lock {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// The path is not the working directory of a Git repository.
    #[error("'{}' is not a Git repository", path.display())]
    NotARepository { path: PathBuf },

    /// A branch name fails Git's `check-ref-format --branch` rules.
    #[error("'{name}' is not a valid branch name")]
    InvalidBranchName { name: String },

    /// The branch is checked out in the working tree being addressed.
    #[error("refusing to delete '{branch}', the currently checked-out branch")]
    CurrentBranchDeletion { branch: String },

    /// The branch is the locally recorded default branch of the repository.
    #[error("refusing to delete '{branch}', the repository's default branch")]
    DefaultBranchDeletion { branch: String },

    /// Another worktree has the branch checked out.
    #[error(
        "refusing to delete '{branch}', which is checked out in the worktree at '{}'",
        worktree.display()
    )]
    BranchCheckedOutInWorktree { branch: String, worktree: PathBuf },

    /// The branch contains commits not merged into its upstream or HEAD.
    #[error(
        "refusing to delete unmerged branch '{branch}'; explicitly force the deletion to continue"
    )]
    UnmergedBranchDeletion { branch: String },

    /// The remote rejected the update because it holds commits this repository
    /// does not have.
    #[error("git {command} was rejected: the remote has commits this branch does not: {stderr}")]
    NonFastForward { command: String, stderr: String },

    /// Git could not prove who it was to the remote.
    ///
    /// Recognized from Git's own diagnostic, once and here, so that no caller
    /// has to match on the text of standard error to tell a credential problem
    /// from a missing repository.
    #[error("git {command} could not authenticate to the remote: {stderr}")]
    AuthenticationFailed { command: String, stderr: String },

    /// The branch tracks nothing, so there is no branch to reconcile with or
    /// publish to.
    #[error("branch '{branch}' has no upstream branch configured")]
    NoUpstream { branch: String },

    /// The branch exists but has no commit yet, so there is nothing to publish.
    ///
    /// Distinct from [`NoUpstream`]: asking for an upstream would not help, and
    /// Git's own diagnostic for it names a refspec the caller never wrote.
    ///
    /// [`NoUpstream`]: GitError::NoUpstream
    #[error("branch '{branch}' in '{}' has no commits to push", path.display())]
    UnbornBranch { path: PathBuf, branch: String },

    /// The branch tracks `.`, which is Git's name for the repository itself
    /// rather than a remote.
    ///
    /// A perfectly ordinary upstream for a branch built on another local
    /// branch, and one a pull honors. Reported distinctly because the
    /// alternative was to claim the remote was not configured, which sent the
    /// caller looking for a configuration mistake that was never made.
    #[error(
        "branch '{branch}' tracks '.', the repository itself, which is a valid \
         upstream to pull from but not something to push to"
    )]
    LocalUpstreamUnsupported { branch: String },

    /// The repository is already in the middle of an operation, so there is
    /// nothing coherent to reconcile.
    ///
    /// Refused before Git is spawned. Left to Git, the failure would be
    /// indistinguishable from one this operation had caused.
    #[error(
        "'{}' is in the middle of a {pending}; finish or abort it before pulling",
        path.display()
    )]
    OperationInProgress {
        path: PathBuf,
        pending: PendingOperation,
    },

    /// An operation failed part-way and left the repository mid-operation.
    ///
    /// The failure that caused it is the [`source`]; the point of the variant
    /// is everything around it. A caller that treats an error as "nothing
    /// happened" is wrong here: the index and the working tree have changed,
    /// [`status`] describes them as they now are, and nothing else will run
    /// against this repository until the [`pending`] operation is finished or
    /// aborted.
    ///
    /// [`source`]: std::error::Error::source
    /// [`status`]: GitError::Interrupted::status
    /// [`pending`]: GitError::Interrupted::pending
    #[error(
        "git {command} left '{}' in the middle of a {pending} that has to be \
         resolved: {source}",
        path.display()
    )]
    Interrupted {
        command: String,
        path: PathBuf,
        pending: PendingOperation,
        /// Boxed because this is the only variant carrying a whole status, and
        /// every `Result<_, GitError>` in the crate would otherwise be as wide
        /// as the rarest failure in it.
        status: Option<Box<GitStatus>>,
        #[source]
        source: Box<GitError>,
    },

    /// The named remote does not exist, or none could be chosen.
    #[error("{}", match remote {
        Some(remote) => format!("remote '{remote}' is not configured for this repository"),
        None => "no remote was named, and this repository has neither an 'origin' \
                 nor a single remote to fall back to".to_owned(),
    })]
    NoRemote { remote: Option<String> },

    /// Pushing to the remote's default branch was refused.
    ///
    /// The branch is the one the remote names as its default *now*, asked of
    /// the remote itself rather than read from what a clone recorded once.
    #[error(
        "refusing to push to '{branch}', the default branch of '{remote}'; \
         set PushOptions::allow_default_branch to push to it anyway"
    )]
    DefaultBranchPush { remote: String, branch: String },

    /// The remote's default branch could not be determined, so the refusal
    /// above cannot be evaluated. Deliberately distinct: assuming `main` would
    /// let a push through on exactly the repositories that cannot be checked.
    #[error(
        "the default branch of '{remote}' is neither advertised by the remote nor \
         recorded in refs/remotes/{remote}/HEAD; run 'git remote set-head {remote} --auto', \
         or set PushOptions::allow_default_branch"
    )]
    DefaultBranchUnknown { remote: String },

    /// A commit is checked out rather than a branch, and the operation is about
    /// a branch.
    #[error("'{}' has no branch checked out: {detail}", path.display())]
    DetachedHead { path: PathBuf, detail: String },

    /// Git metadata could not be inspected for a readable directory.
    #[error("failed to inspect Git metadata for '{}': {source}", path.display())]
    Inspection {
        path: PathBuf,
        #[source]
        source: git2::Error,
    },

    /// Git reported a status Harkness cannot parse.
    #[error("Git reported a status that could not be parsed: {detail}")]
    MalformedStatus { detail: String },
}

/// Git operations on one repository.
///
/// Stateless and addressed by path. It deliberately cannot resolve a
/// [`ProjectId`]: reading the catalog would mean taking the catalog lock, and
/// the lock ordering documented on [`RepositoryLock`] forbids taking it while
/// this service is about to lock a repository. [`ProjectService::git`] does
/// that resolution instead, releasing the catalog lock before it returns.
///
/// # What a front end has to provide
///
/// The synchronizing verbs are blocking, and the contract they need from a
/// caller with an event loop is the same for all three:
///
/// - **Worker thread.** [`fetch`], [`pull`] and [`push`] block until Git exits.
///   Calling one on a UI thread freezes the window for the length of a network
///   operation, which has no upper bound.
/// - **Pending state.** The repository lock is exclusive, so a second operation
///   on the same repository fails with [`GitError::RepositoryBusy`] rather than
///   queueing. A front end that lets a user press *Pull* twice has to disable
///   the control itself; the core will not serialize on its behalf.
/// - **Cancellation.** Every verb takes a [`Cancellation`], and cancelling
///   kills Git's whole process group. A cancelled [`pull`] is the one case that
///   can leave work behind: see the recovery rule below.
/// - **Refresh after failure, not just after success.** Every outcome carries
///   the status the repository had once the operation finished, so the success
///   path needs no second inspection. Failures carry it only where it exists —
///   [`GitError::Interrupted`] — so any other error should be followed by a
///   [`status`] or [`detailed_status`] call rather than by the assumption that
///   nothing moved.
/// - **Conflict recovery.** [`GitError::Interrupted`] means the working tree
///   changed and Git is waiting: the front end has to offer the user a way to
///   resolve or abort, because every later operation on that repository will
///   refuse with [`GitError::OperationInProgress`] until it does.
///
/// [`ProjectId`]: crate::ProjectId
/// [`ProjectService::git`]: crate::ProjectService::git
/// [`fetch`]: GitService::fetch
/// [`pull`]: GitService::pull
/// [`push`]: GitService::push
/// [`status`]: GitService::status
/// [`detailed_status`]: GitService::detailed_status
#[derive(Clone, Debug)]
pub struct GitService {
    root: PathBuf,
    data_dir: PathBuf,
    git_executable: PathBuf,
}

impl GitService {
    /// Addresses the repository whose working directory is `root`.
    ///
    /// `data_dir` is the Harkness data directory, which is where repository
    /// locks live; nothing is created there until a lock is taken.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            data_dir: data_dir.into(),
            git_executable: PathBuf::from("git"),
        }
    }

    /// Runs a different Git executable than the one on `PATH`.
    #[must_use]
    pub fn with_git_executable(mut self, git_executable: impl Into<PathBuf>) -> Self {
        self.git_executable = git_executable.into();
        self
    }

    /// The working directory every command runs in.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Prepares a Git invocation in this repository.
    ///
    /// Write and network commands acquire the repository lock immediately and
    /// retain it through execution. Local reads remain lock-free.
    pub fn command(
        &self,
        access: GitAccess,
        cancellation: &Cancellation,
    ) -> Result<GitCommand, GitError> {
        let command = GitCommand::new(&self.git_executable, &self.root, access);
        match access {
            GitAccess::LocalRead => Ok(command),
            GitAccess::LocalWrite | GitAccess::Network => {
                Ok(command.with_repository_lock(self.lock(cancellation)?))
            }
        }
    }

    /// Describes the repository cheaply and in process.
    ///
    /// `None` means the root is not the working tree of a repository. Spawns
    /// nothing, because this runs for every catalog entry on every read.
    pub fn status(&self) -> Result<Option<GitStatus>, GitError> {
        status::inspect(&self.root)
    }

    /// Describes every changed path in the repository.
    ///
    /// One `git status` spawn, so it is computed only for a project a caller
    /// names rather than for a whole listing.
    pub fn detailed_status(&self, cancellation: &Cancellation) -> Result<DetailedStatus, GitError> {
        status::detailed(&self.git_executable, &self.root, cancellation)
    }

    /// Lists local branches and optionally the remote-tracking refs already
    /// present in this repository.
    ///
    /// Runs entirely in process and takes no repository lock.
    pub fn branches(&self, include_remote_tracking: bool) -> Result<Vec<Branch>, GitError> {
        branch::branches(&self.root, include_remote_tracking)
    }

    /// Creates a local branch, optionally checking it out in the same call.
    pub fn create_branch(
        &self,
        name: &str,
        options: &CreateBranchOptions,
        cancellation: &Cancellation,
    ) -> Result<(), GitError> {
        branch::validate_name(name)?;
        let lock = self.lock(cancellation)?;
        branch::create(
            &self.git_executable,
            &self.root,
            &lock,
            name,
            options,
            cancellation,
        )
    }

    /// Checks out an existing local branch without discarding local changes.
    pub fn checkout_branch(&self, name: &str, cancellation: &Cancellation) -> Result<(), GitError> {
        branch::validate_name(name)?;
        let lock = self.lock(cancellation)?;
        branch::checkout(&self.git_executable, &self.root, &lock, name, cancellation)
    }

    /// Deletes a local branch after applying the branch-safety guardrails.
    ///
    /// `force` overrides only the unmerged-commit refusal. It cannot delete a
    /// current, default or other-worktree branch.
    pub fn delete_branch(
        &self,
        name: &str,
        force: bool,
        cancellation: &Cancellation,
    ) -> Result<(), GitError> {
        branch::validate_name(name)?;
        let lock = self.lock(cancellation)?;
        branch::delete(
            &self.git_executable,
            &self.root,
            &lock,
            name,
            force,
            cancellation,
        )
    }

    /// Sets a local branch's upstream, or clears it when `upstream` is `None`.
    pub fn set_upstream(
        &self,
        branch: &str,
        upstream: Option<&str>,
        cancellation: &Cancellation,
    ) -> Result<(), GitError> {
        self::branch::validate_name(branch)?;
        let lock = self.lock(cancellation)?;
        self::branch::set_upstream(
            &self.git_executable,
            &self.root,
            &lock,
            branch,
            upstream,
            cancellation,
        )
    }

    /// Updates the remote-tracking refs of one remote.
    ///
    /// Blocks until Git exits, so a front end with an event loop must call it
    /// on a worker thread; `on_progress` receives Git's transfer counters as
    /// they arrive. The repository lock is held for the whole operation,
    /// including the inspection either side of it that decides what moved.
    pub fn fetch(
        &self,
        options: &FetchOptions,
        cancellation: &Cancellation,
        on_progress: impl FnMut(String),
    ) -> Result<FetchOutcome, GitError> {
        let lock = self.lock(cancellation)?;
        sync::fetch(
            &self.git_executable,
            &self.root,
            &lock,
            options,
            cancellation,
            on_progress,
        )
    }

    /// Fetches the checked-out branch's upstream and reconciles the branch with
    /// it.
    ///
    /// Fast-forward only unless [`PullOptions::strategy`] says otherwise, so
    /// the default can neither rewrite history nor invent a merge commit.
    ///
    /// This is the one verb here that writes to the working tree, and therefore
    /// the one whose failures are not all equivalent to "nothing happened". A
    /// merge or rebase that stops at a conflict, and a cancellation that lands
    /// mid-reconciliation, both report [`GitError::Interrupted`] carrying the
    /// state the repository was left in; every other failure leaves the branch
    /// where it was.
    pub fn pull(
        &self,
        options: &PullOptions,
        cancellation: &Cancellation,
        on_progress: impl FnMut(String),
    ) -> Result<PullOutcome, GitError> {
        let lock = self.lock(cancellation)?;
        sync::pull(
            &self.git_executable,
            &self.root,
            &lock,
            options,
            cancellation,
            on_progress,
        )
    }

    /// Publishes the checked-out branch to a remote, under the same name.
    ///
    /// Every refusal is decided before anything is written, so a refused push
    /// never changes the remote. [`GitError::NoUpstream`],
    /// [`GitError::UnbornBranch`] and [`GitError::LocalUpstreamUnsupported`]
    /// are settled without contacting it at all;
    /// [`GitError::DefaultBranchPush`] costs one read-only query, because the
    /// only trustworthy answer to "which branch is the default" comes from the
    /// remote rather than from what a clone recorded.
    pub fn push(
        &self,
        options: &PushOptions,
        cancellation: &Cancellation,
        on_progress: impl FnMut(String),
    ) -> Result<PushOutcome, GitError> {
        let lock = self.lock(cancellation)?;
        sync::push(
            &self.git_executable,
            &self.root,
            &lock,
            options,
            cancellation,
            on_progress,
        )
    }

    /// Takes the exclusive lock covering this repository and its worktrees.
    ///
    /// Every Git mutation holds it; no read takes it at all. See
    /// [`RepositoryLock`] for the ordering it must be acquired in.
    pub fn lock(&self, cancellation: &Cancellation) -> Result<RepositoryLock, GitError> {
        RepositoryLock::acquire(&self.data_dir, &self.root, cancellation)
    }
}

#[cfg(test)]
mod tests {
    use super::{Cancellation, GitAccess, GitError, GitService};
    use crate::testing::{Fixture, initialize_repository};

    #[test]
    fn mutation_commands_hold_the_repository_lock_while_they_exist() {
        let fixture = Fixture::new();
        let root = fixture.directory("command-lock");
        initialize_repository(&root);
        let service = GitService::new(&root, &fixture.data_dir);

        let command = service
            .command(GitAccess::LocalWrite, &Cancellation::default())
            .unwrap();
        let cancelled = Cancellation::default();
        cancelled.cancel();
        assert!(matches!(service.lock(&cancelled), Err(GitError::Cancelled)));

        drop(command);
        service.lock(&Cancellation::default()).unwrap();
    }
}
