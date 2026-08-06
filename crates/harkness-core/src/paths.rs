//! The filesystem layout of the Harkness data directory.

use std::{
    fs,
    path::{Path, PathBuf},
};

/// Replaces the platform data directory outright when it is set.
pub(crate) const DATA_DIRECTORY_ENV: &str = "HARKNESS_DATA_DIR";

pub(crate) const CATALOG_FILE: &str = "projects.json";
pub(crate) const CATALOG_LOCK_FILE: &str = "projects.lock";
pub(crate) const WORKTREES_DIRECTORY: &str = "worktrees";
pub(crate) const LOCKS_DIRECTORY: &str = "locks";
pub(crate) const REPOSITORIES_DIRECTORY: &str = "repositories";
pub(crate) const CHECKOUT_DIRECTORY: &str = "checkout";

/// Resolves the Harkness data directory, honoring [`DATA_DIRECTORY_ENV`].
///
/// The override exists so an isolated front end or an integration test can run
/// against its own catalog instead of the real user data directory. An empty
/// value counts as unset, because it would otherwise resolve the catalog
/// relative to the process working directory.
///
/// `None` means the platform exposed no user data directory and no override
/// was given.
pub(crate) fn data_directory() -> Option<PathBuf> {
    std::env::var_os(DATA_DIRECTORY_ENV)
        .filter(|overridden| !overridden.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::data_dir().map(|data_dir| data_dir.join("harkness")))
}

/// Why a path is not the location Harkness reserved for an entry.
pub(crate) enum UnreservedPath {
    /// The storage root does not resolve.
    StorageRootUnavailable,
    /// The candidate path does not resolve.
    CandidateUnavailable,
    /// The candidate resolves somewhere other than its reserved location.
    Mismatch,
}

/// Proves `candidate` is exactly the path reserved at `storage_root`/`reserved`
/// and returns the canonical storage root.
///
/// Both sides must be canonical. `Project::root` was canonicalized at import,
/// so a symlink anywhere above the data directory would make a literal
/// comparison against `data_dir` fail for every managed clone. Equality also
/// subsumes a containment check: a path that resolves outside managed storage,
/// or through a symlink, cannot match.
pub(crate) fn canonical_reserved_root(
    storage_root: &Path,
    reserved: &Path,
    candidate: &Path,
) -> Result<PathBuf, UnreservedPath> {
    let canonical_root =
        fs::canonicalize(storage_root).map_err(|_| UnreservedPath::StorageRootUnavailable)?;
    let canonical_candidate =
        fs::canonicalize(candidate).map_err(|_| UnreservedPath::CandidateUnavailable)?;
    if canonical_candidate != canonical_root.join(reserved) {
        return Err(UnreservedPath::Mismatch);
    }
    Ok(canonical_root)
}
