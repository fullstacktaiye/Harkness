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

use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use thiserror::Error;

pub use lock::RepositoryLock;
pub use runner::{Cancellation, CloneCancellation, GitAccess, GitCommand, GitOutput};
pub use status::{DetailedStatus, FileChange, HeadState, StatusEntry};

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
    #[must_use]
    pub fn command(&self, access: GitAccess) -> GitCommand {
        GitCommand::new(&self.git_executable, &self.root, access)
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

    /// Takes the exclusive lock covering this repository and its worktrees.
    ///
    /// Every Git mutation holds it; no read takes it at all. See
    /// [`RepositoryLock`] for the ordering it must be acquired in.
    pub fn lock(&self, cancellation: &Cancellation) -> Result<RepositoryLock, GitError> {
        RepositoryLock::acquire(&self.data_dir, &self.root, cancellation)
    }
}
