//! Shared application behavior for Harkness front ends.

mod catalog;
mod git;
mod listing;
mod paths;
mod project;
mod remote;

pub use catalog::entry::{GitStatus, Project, ProjectId, ProjectSource};
pub use listing::{DirEntry, list_directory};
pub use project::{CloneCancellation, ProjectError, ProjectService};
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
