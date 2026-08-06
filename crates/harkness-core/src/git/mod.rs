//! System Git integration.
//!
//! Everything Harkness does with Git goes through this module: one command
//! runner, one per-repository lock, and the two status tiers. [`GitService`] is
//! the front door, addressed by filesystem path so that nothing here needs the
//! project catalog — and therefore nothing here can take the catalog lock out
//! of order.

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

pub use lock::RepositoryLock;
pub use runner::{Cancellation, CloneCancellation, GitAccess, GitCommand, GitOutput};
pub use status::{DetailedStatus, FileChange, HeadState, StatusEntry};
pub use sync::{
    FetchOptions, FetchOutcome, PullOptions, PullOutcome, PullStrategy, PushOptions, PushOutcome,
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

    /// The named remote does not exist, or none could be chosen.
    #[error("{}", match remote {
        Some(remote) => format!("remote '{remote}' is not configured for this repository"),
        None => "no remote was named, and this repository has neither an 'origin' \
                 nor a single remote to fall back to".to_owned(),
    })]
    NoRemote { remote: Option<String> },

    /// Pushing to the remote's default branch was refused.
    #[error(
        "refusing to push to '{branch}', the default branch of '{remote}'; \
         set PushOptions::allow_default_branch to push to it anyway"
    )]
    DefaultBranchPush { remote: String, branch: String },

    /// The remote's default branch is unrecorded, so the refusal above cannot
    /// be evaluated. Deliberately distinct: assuming `main` would let a push
    /// through on exactly the repositories that cannot be checked.
    #[error(
        "the default branch of '{remote}' is not recorded in refs/remotes/{remote}/HEAD; \
         run 'git remote set-head {remote} --auto', or set PushOptions::allow_default_branch"
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
/// [`ProjectId`]: crate::ProjectId
/// [`ProjectService::git`]: crate::ProjectService::git
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
    /// Both refusals — [`GitError::DefaultBranchPush`] and
    /// [`GitError::NoUpstream`] — are decided before Git is spawned, so a
    /// refused push never reaches the remote at all.
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
