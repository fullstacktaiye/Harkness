//! Lazy, read-only directory listing for project file trees.
//!
//! A listing reads exactly one directory level; expanding a tree node lists
//! that node's directory and nothing more. Two rules keep the traversal safe
//! on arbitrary project roots: `.git` internals are never listed, and
//! symlinked directories are reported as plain leaves rather than followed.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// A single entry of a [`list_directory`] result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirEntry {
    /// The file name within the listed directory.
    pub name: String,
    /// The full path of the entry.
    pub path: PathBuf,
    /// Whether the entry is a real directory (never a symlink to one).
    pub is_dir: bool,
    /// Whether a tree may expand the entry to list its children.
    ///
    /// This is `is_dir` today: files cannot expand, and symlinked
    /// directories report `is_dir == false`, so they are never followed.
    pub expandable: bool,
}

/// Lists the direct children of `path`, directories first, then by
/// case-insensitive name.
///
/// Entries named `.git` are omitted entirely. Metadata is read without
/// following symlinks, so a symlink to a directory is listed as a file-like
/// leaf and its target is never traversed.
pub fn list_directory(path: impl AsRef<Path>) -> io::Result<Vec<DirEntry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        // `DirEntry::metadata` does not follow symlinks, which is exactly the
        // traversal guarantee the tree relies on.
        let is_dir = entry.metadata()?.is_dir();
        entries.push(DirEntry {
            name,
            path: entry.path(),
            is_dir,
            expandable: is_dir,
        });
    }
    entries.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::list_directory;

    #[test]
    fn lists_one_level_sorted_directories_first() {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("Zulu.txt"), b"").unwrap();
        fs::create_dir(root.path().join("beta")).unwrap();
        fs::write(root.path().join("alpha.txt"), b"").unwrap();
        fs::create_dir(root.path().join("Alpha")).unwrap();
        fs::write(root.path().join("beta").join("nested.txt"), b"").unwrap();

        let entries = list_directory(root.path()).unwrap();
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();

        // Directories first (case-insensitive), then files; the nested file
        // is not part of this level's listing.
        assert_eq!(names, ["Alpha", "beta", "alpha.txt", "Zulu.txt"]);
        assert!(entries[..2].iter().all(|entry| entry.expandable));
        assert!(entries[2..].iter().all(|entry| !entry.expandable));
    }

    #[test]
    fn git_internals_are_filtered() {
        let root = TempDir::new().unwrap();
        fs::create_dir(root.path().join(".git")).unwrap();
        fs::write(root.path().join(".git").join("HEAD"), b"").unwrap();
        fs::write(root.path().join("visible.txt"), b"").unwrap();

        let entries = list_directory(root.path()).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "visible.txt");
    }

    #[cfg(unix)]
    #[test]
    fn directory_symlinks_are_listed_but_never_followed() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();
        fs::write(target.path().join("secret.txt"), b"").unwrap();
        symlink(target.path(), root.path().join("linked")).unwrap();
        symlink(
            root.path().join("visible.txt"),
            root.path().join("linked-file"),
        )
        .unwrap();
        fs::write(root.path().join("visible.txt"), b"").unwrap();

        let entries = list_directory(root.path()).unwrap();
        let linked = entries.iter().find(|entry| entry.name == "linked").unwrap();
        let linked_file = entries
            .iter()
            .find(|entry| entry.name == "linked-file")
            .unwrap();

        assert!(!linked.is_dir);
        assert!(!linked.expandable);
        assert!(!linked_file.expandable);
        // The link is a leaf, so the target's contents never surface.
        assert!(!entries.iter().any(|entry| entry.name == "secret.txt"));
    }

    #[cfg(windows)]
    #[test]
    fn directory_symlinks_are_listed_but_never_followed() {
        use std::os::windows::fs::{symlink_dir, symlink_file};

        let root = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();
        fs::write(target.path().join("secret.txt"), b"").unwrap();
        symlink_dir(target.path(), root.path().join("linked")).unwrap();
        symlink_file(
            root.path().join("visible.txt"),
            root.path().join("linked-file"),
        )
        .unwrap();
        fs::write(root.path().join("visible.txt"), b"").unwrap();

        let entries = list_directory(root.path()).unwrap();
        let linked = entries.iter().find(|entry| entry.name == "linked").unwrap();

        assert!(!linked.expandable);
        assert!(!entries.iter().any(|entry| entry.name == "secret.txt"));
    }

    #[test]
    fn missing_directories_fail() {
        let root = TempDir::new().unwrap();

        let result = list_directory(root.path().join("missing"));

        assert!(result.is_err());
    }
}
