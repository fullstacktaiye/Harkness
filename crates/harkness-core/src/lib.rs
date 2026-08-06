//! Shared application behavior for Harkness front ends.

mod catalog;
mod git;
mod listing;
mod paths;
mod project;
mod remote;
#[cfg(test)]
mod testing;

pub use catalog::entry::{GitStatus, Project, ProjectId, ProjectSource, UpstreamStatus};
pub use git::{
    Cancellation, CloneCancellation, DetailedStatus, FetchOptions, FetchOutcome, FileChange,
    GitAccess, GitCommand, GitError, GitOutput, GitService, HeadState, PullOptions, PullOutcome,
    PullStrategy, PushOptions, PushOutcome, RepositoryLock, StatusEntry,
};
pub use listing::{DirEntry, list_directory};
pub use project::{ProjectError, ProjectService};
pub use remote::normalize_remote;

/// Returns the greeting displayed by every Harkness interface.
#[must_use]
pub const fn greeting() -> &'static str {
    "Hello World"
}

#[cfg(test)]
mod tests {
    #[test]
    fn greeting_is_hello_world() {
        assert_eq!(super::greeting(), "Hello World");
    }
}
