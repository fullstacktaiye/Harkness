//! Shared filesystem path resolution primitives.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Canonicalizes the nearest existing ancestor and restores a missing tail.
///
/// Only `NotFound` is interpreted as an absent component. Permission failures,
/// symlink loops, and other I/O errors are returned so a caller enforcing a
/// boundary fails closed instead of treating an unreadable existing path as a
/// safe lexical destination.
pub fn canonicalize_with_missing_tail(path: &Path) -> io::Result<PathBuf> {
    let mut current = path;
    let mut missing = Vec::<OsString>::new();
    loop {
        match fs::canonicalize(current) {
            Ok(mut resolved) => {
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some(component) = current.file_name() else {
                    return Err(error);
                };
                missing.push(component.to_os_string());
                let Some(parent) = current.parent() else {
                    return Err(error);
                };
                current = parent;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::canonicalize_with_missing_tail;

    #[test]
    fn a_missing_tail_is_restored_below_its_canonical_ancestor() {
        let root = TempDir::new().unwrap();
        assert_eq!(
            canonicalize_with_missing_tail(&root.path().join("missing/leaf")).unwrap(),
            fs::canonicalize(root.path()).unwrap().join("missing/leaf")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_loop_is_an_error_not_a_missing_tail() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        symlink("loop", root.path().join("loop")).unwrap();
        let error = canonicalize_with_missing_tail(&root.path().join("loop/leaf")).unwrap_err();
        assert_ne!(error.kind(), std::io::ErrorKind::NotFound);
    }
}
