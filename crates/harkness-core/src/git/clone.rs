//! The managed-repository clone.
//!
//! One caller of [`GitCommand`] like any other. It exists as its own module
//! because it is the one Git operation whose failures are part of
//! [`ProjectError`]: a clone is how a project enters the catalog, so its
//! diagnostics belong to the import rather than to Git.

use std::path::Path;

use crate::{
    git::{
        GitError,
        runner::{Cancellation, GitAccess, GitCommand},
    },
    paths::CHECKOUT_DIRECTORY,
    project::ProjectError,
};

/// Clones `remote` into `CHECKOUT_DIRECTORY` inside `managed_directory`.
///
/// The destination is named relative to the working directory rather than
/// absolutely, so a Harkness data directory given as a relative path still
/// resolves to the same place Git is run from. `remote` reaches Git exactly as
/// the caller wrote it, which is what keeps every URL form and every credential
/// helper working; production remotes are always absolute GitHub URLs, because
/// nothing else survives [`normalize_remote`].
///
/// No timeout: a first clone of a large repository is legitimately slow, and
/// `cancellation` already stops it on the only terms a user cares about.
///
/// [`normalize_remote`]: crate::normalize_remote
pub(crate) fn run(
    git_executable: &Path,
    remote: &str,
    managed_directory: &Path,
    cancellation: &Cancellation,
    on_progress: &mut impl FnMut(String),
) -> Result<(), ProjectError> {
    GitCommand::new(git_executable, managed_directory, GitAccess::Network)
        .args(["clone", "--progress", "--", remote])
        .arg(CHECKOUT_DIRECTORY)
        .run_with_progress(cancellation, on_progress)
        .map(|_| ())
        .map_err(clone_failure)
}

/// Restates a Git failure as the import failure it caused.
///
/// The mapping is exact for the three outcomes a clone has always had, so the
/// errors a front end matches on are unchanged by the runner being shared.
fn clone_failure(error: GitError) -> ProjectError {
    match error {
        GitError::Launch { source } => ProjectError::GitLaunch { source },
        GitError::Cancelled => ProjectError::CloneCancelled,
        GitError::Failed { stderr, .. } => ProjectError::CloneFailed { stderr },
        // A clone is never given a timeout, so this is unreachable rather than
        // merely unlikely; reporting it as a clone failure keeps the promise
        // that a clone fails in one of three ways.
        other => ProjectError::CloneFailed {
            stderr: other.to_string(),
        },
    }
}
