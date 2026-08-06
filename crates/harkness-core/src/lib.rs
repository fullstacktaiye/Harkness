//! Shared application behavior for Harkness front ends.

mod listing;
mod project;

pub use listing::{DirEntry, list_directory};
pub use project::{
    CloneCancellation, GitStatus, Project, ProjectError, ProjectId, ProjectService, ProjectSource,
    normalize_remote,
};

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
