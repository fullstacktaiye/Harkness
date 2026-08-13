//! The identity two tool calls have to share to be serialized against each
//! other.

use std::path::{Path, PathBuf};

use harkness_core::ProjectId;
use serde::Serialize;

use crate::trust::{BoundaryError, canonical_root};

/// The unit of mutation serialization: one project at one canonical root.
///
/// Both halves are load-bearing, and for the same reason workspace trust binds
/// both. A canonical path alone is not an identity, because a directory removed
/// and recreated by a different catalog entry reuses the path while being a
/// different workspace; a [`ProjectId`] alone is not one either, because a
/// project's linked worktrees are separate checkouts that may legitimately be
/// mutated at the same time. Only the pair names the thing a mutation would
/// interleave with.
///
/// The root is canonicalized once, when the key is built, so two spellings of
/// one directory — a symlink, a relative path, a trailing separator — cannot
/// produce two keys and let two mutations of one worktree run concurrently. A
/// key therefore also *cannot* be built for a directory that does not exist,
/// which is deliberate: scheduling work against a worktree nobody can resolve
/// would serialize it against nothing.
///
/// A non-UTF-8 root fails to serialize rather than being lossily rendered, the
/// same limitation a persisted task's workspace path carries.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct WorkspaceKey {
    // Ordered before the root so a snapshot groups one project's worktrees
    // together, which is how a front end renders them.
    project_id: ProjectId,
    canonical_root: PathBuf,
}

impl WorkspaceKey {
    /// Names the workspace `root` belongs to within `project_id`.
    ///
    /// # Errors
    ///
    /// Returns [`BoundaryError::RootUnavailable`] when `root` cannot be
    /// canonicalized or is not a directory. A key is never built from a lexical
    /// path, exactly as a trust decision is never stored against one.
    pub fn new(project_id: ProjectId, root: impl AsRef<Path>) -> Result<Self, BoundaryError> {
        Ok(Self {
            project_id,
            canonical_root: canonical_root(root.as_ref())?,
        })
    }

    /// Catalog identity this workspace belongs to.
    #[must_use]
    pub const fn project_id(&self) -> ProjectId {
        self.project_id
    }

    /// Canonical root every call scheduled under this key runs against.
    ///
    /// This is also the workspace root handed to the executor, so the path a
    /// call is serialized on and the path it may touch cannot disagree.
    #[must_use]
    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }
}

#[cfg(test)]
mod tests {
    use harkness_core::ProjectId;
    use tempfile::TempDir;

    use super::WorkspaceKey;

    fn project() -> ProjectId {
        ProjectId::default()
    }

    #[test]
    fn two_spellings_of_one_directory_are_one_key() {
        let root = TempDir::new().unwrap();
        let project = project();
        std::fs::create_dir(root.path().join("child")).unwrap();

        let direct = WorkspaceKey::new(project, root.path()).unwrap();
        let indirect = WorkspaceKey::new(project, root.path().join("child").join("..")).unwrap();

        // Two keys here would mean two mutations of one worktree admitted at
        // once, which is the exact thing the mutation slot exists to prevent.
        assert_eq!(direct, indirect);
        assert_eq!(direct.canonical_root(), indirect.canonical_root());
    }

    #[test]
    fn neither_half_of_the_identity_is_sufficient_on_its_own() {
        let root = TempDir::new().unwrap();
        let sibling = TempDir::new().unwrap();
        let one = project();
        let another = project();

        assert_ne!(
            WorkspaceKey::new(one, root.path()).unwrap(),
            WorkspaceKey::new(another, root.path()).unwrap(),
            "one path reused by two catalog entries is two workspaces"
        );
        assert_ne!(
            WorkspaceKey::new(one, root.path()).unwrap(),
            WorkspaceKey::new(one, sibling.path()).unwrap(),
            "one project's two checkouts are two workspaces"
        );
    }

    #[test]
    fn a_root_that_cannot_be_resolved_yields_no_key() {
        let root = TempDir::new().unwrap();
        let missing = root.path().join("never-created");
        let file = root.path().join("regular");
        std::fs::write(&file, b"not a workspace").unwrap();

        assert_eq!(
            WorkspaceKey::new(project(), &missing).unwrap_err().kind(),
            "root_unavailable"
        );
        assert_eq!(
            WorkspaceKey::new(project(), &file).unwrap_err().kind(),
            "root_unavailable"
        );
    }

    #[test]
    fn the_project_orders_before_the_root() {
        // A snapshot is rendered in key order, and a reader expects one
        // project's worktrees to sit together rather than interleaved by path.
        let root = TempDir::new().unwrap();
        let first = ProjectId::default();
        let second = ProjectId::default();
        let (lower, higher) = if first < second {
            (first, second)
        } else {
            (second, first)
        };

        assert!(
            WorkspaceKey::new(lower, root.path()).unwrap()
                < WorkspaceKey::new(higher, root.path()).unwrap()
        );
    }
}
