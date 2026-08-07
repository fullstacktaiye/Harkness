//! Advisory locking around the catalog file.

use std::{
    fs::{self, File},
    path::Path,
};

use crate::{
    catalog::{Catalog, read_catalog},
    paths::{CATALOG_FILE, CATALOG_LOCK_FILE},
    project::ProjectError,
};

/// Takes the exclusive catalog lock for one read-modify-write.
///
/// The lock file is created once and never replaced, because atomic
/// persistence swaps `projects.json` for a new inode and a lock held
/// against the old one would exclude nobody. Dropping the returned handle
/// releases the lock, as does the kernel if the process dies.
///
/// Advisory locks are unreliable on NFS. That is acceptable for a local
/// user data directory, which is the only location Harkness supports.
pub(crate) fn lock_exclusive(data_dir: &Path) -> Result<File, ProjectError> {
    fs::create_dir_all(data_dir).map_err(|source| ProjectError::CatalogLock {
        path: data_dir.to_path_buf(),
        source,
    })?;
    let lock_path = data_dir.join(CATALOG_LOCK_FILE);
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|source| ProjectError::CatalogLock {
            path: lock_path.clone(),
            source,
        })?;
    // Blocking, not `try_lock`: these critical sections are one small read
    // plus one write, and a caller has nothing useful to do with a "busy"
    // error except retry.
    lock.lock().map_err(|source| ProjectError::CatalogLock {
        path: lock_path,
        source,
    })?;
    Ok(lock)
}

/// Reads the catalog under a shared lock.
///
/// Reads never create the data directory: a catalog that has never been
/// written has no lock file, and nothing to race with either.
pub(crate) fn read_catalog_shared(data_dir: &Path) -> Result<Catalog, ProjectError> {
    let lock_path = data_dir.join(CATALOG_LOCK_FILE);
    let lock = match File::options().read(true).open(&lock_path) {
        Ok(lock) => lock,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Atomic replacement makes an unlocked read safe when no writer
            // has created the stable lock inode yet.
            return read_catalog(&data_dir.join(CATALOG_FILE));
        }
        Err(source) => {
            return Err(ProjectError::CatalogLock {
                path: lock_path,
                source,
            });
        }
    };
    lock.lock_shared()
        .map_err(|source| ProjectError::CatalogLock {
            path: lock_path,
            source,
        })?;
    read_catalog(&data_dir.join(CATALOG_FILE))
}
