//! Repository-relative paths, kept byte-exact.

use std::fmt;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// Marks a serialized path whose bytes are not valid UTF-8.
const BASE64_PREFIX: &str = "base64:";

/// A repository-relative path, stored as the bytes Git reports.
///
/// A path is not text. Git reports byte strings, and on Unix a filename may hold
/// any byte sequence except `/` and NUL, so a type that could only hold UTF-8
/// would either refuse ordinary files or silently rewrite their names. Digests
/// absorb [`RepoPath::as_bytes`], never the display form, so two files whose
/// names differ only in bytes a lossy conversion would fold together still
/// produce different workspace identities.
///
/// Separators are normalized to `/` — Git's spelling — so a snapshot captured on
/// Windows and one captured on Unix digest the same tree identically.
///
/// # Serialization
///
/// A path serializes as one JSON string: its own text when the bytes are valid
/// UTF-8, and `base64:` followed by the standard Base64 of the raw bytes when
/// they are not. A path whose text would itself begin with `base64:` is encoded
/// the same way, so the encoding is unambiguous and lossless in both directions.
/// One string rather than a `path`/`path_is_lossy`/`path_base64` triple keeps a
/// snapshot holding thousands of entries readable and compact; the CLI's
/// separate-field projection exists for external consumers that cannot decode.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RepoPath(Vec<u8>);

impl RepoPath {
    /// Takes the bytes exactly as Git reported them.
    #[must_use]
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    /// Converts a platform path, normalizing separators to `/`.
    ///
    /// On Unix the bytes are taken as they are. On other platforms the path is
    /// read through its UTF-8 form, which is lossless for every path Git can
    /// report there.
    #[must_use]
    pub fn from_path(path: &Path) -> Self {
        #[cfg(unix)]
        let bytes = {
            use std::os::unix::ffi::OsStrExt;
            path.as_os_str().as_bytes().to_vec()
        };
        #[cfg(not(unix))]
        let bytes = path
            .as_os_str()
            .to_string_lossy()
            .replace('\\', "/")
            .into_bytes();
        Self(bytes)
    }

    /// The exact bytes, which are what every digest absorbs.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Appends one directory-entry name to a repository-relative path.
    ///
    /// Lives here because the platform spelling of a name in bytes does — a
    /// walk that rebuilt it would be a second copy of the rule this type
    /// exists to hold, and the two would drift the first time a platform
    /// needed something different. An empty parent yields the name alone, so a
    /// path at the worktree root carries no leading separator.
    #[must_use]
    pub fn join_bytes(parent: &[u8], name: &std::ffi::OsStr) -> Vec<u8> {
        let mut joined = Vec::with_capacity(parent.len() + 1 + name.len());
        if !parent.is_empty() {
            joined.extend_from_slice(parent);
            joined.push(b'/');
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            joined.extend_from_slice(name.as_bytes());
        }
        #[cfg(not(unix))]
        {
            joined.extend_from_slice(name.to_string_lossy().as_bytes());
        }
        joined
    }

    /// Whether the display form loses information.
    #[must_use]
    pub fn is_lossy(&self) -> bool {
        std::str::from_utf8(&self.0).is_err()
    }

    /// Whether the path carries no bytes at all.
    ///
    /// The empty path is the worktree root itself, which is what makes it the
    /// spelling of "everything" for [`contains`](Self::contains) and for a
    /// whole-worktree reconcile scope.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether `other` is this path or sits beneath it.
    ///
    /// Byte containment with the separator required, which is the whole point:
    /// `src` contains `src/main.rs` and does **not** contain `src-generated.rs`,
    /// and a plain `starts_with` on the bytes would say it did. Every scope
    /// decision in the reconciler and every subtree hint in the watcher is this
    /// question, so it is answered once here rather than re-derived by each of
    /// them.
    ///
    /// The empty path is the worktree root and therefore contains everything,
    /// itself included.
    #[must_use]
    pub fn contains(&self, other: &Self) -> bool {
        if self.0.is_empty() {
            return true;
        }
        if self.0 == other.0 {
            return true;
        }
        other
            .0
            .strip_prefix(self.0.as_slice())
            .is_some_and(|rest| rest.first() == Some(&b'/'))
    }

    /// The directory this path sits in, or `None` at the worktree root.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        let index = self.0.iter().rposition(|byte| *byte == b'/')?;
        Some(Self(self.0[..index].to_vec()))
    }

    /// Every directory between the worktree root and this path, root first.
    ///
    /// The root itself is the empty path and is always the first item, so a
    /// caller seeding a walk with the chain above a scope never has to special
    /// case it.
    #[must_use]
    pub fn ancestors(&self) -> Vec<Self> {
        let mut ancestors = vec![Self(Vec::new())];
        let mut cursor = 0;
        while let Some(offset) = self.0[cursor..].iter().position(|byte| *byte == b'/') {
            cursor += offset;
            ancestors.push(Self(self.0[..cursor].to_vec()));
            cursor += 1;
        }
        ancestors
    }

    /// The longest path that [contains](Self::contains) every one of `paths`.
    ///
    /// Truncated at a separator rather than at a byte, so the answer is a
    /// directory and never half a name: the common prefix of `src/main.rs` and
    /// `src/mainland.rs` is `src`, not `src/main`. An empty iterator and paths
    /// with nothing in common both give the worktree root.
    #[must_use]
    pub fn common_ancestor<'paths>(paths: impl IntoIterator<Item = &'paths Self>) -> Self {
        let mut paths = paths.into_iter();
        let Some(first) = paths.next() else {
            return Self(Vec::new());
        };
        // The directory of the first path, because the answer is a directory
        // that contains every input and a file cannot contain anything.
        let mut common = first.parent().unwrap_or_else(|| Self(Vec::new())).0;
        for path in paths {
            while !Self(common.clone()).contains(path) {
                let Some(index) = common.iter().rposition(|byte| *byte == b'/') else {
                    return Self(Vec::new());
                };
                common.truncate(index);
            }
        }
        Self(common)
    }

    /// Whether Git reported a directory rather than a file.
    ///
    /// `git status --untracked-files=normal` reports an untracked directory as
    /// one entry with a trailing `/` rather than recursing into it, so a probe
    /// has to expand it before its contents can take part in identity.
    #[must_use]
    pub fn is_directory_entry(&self) -> bool {
        self.0.last() == Some(&b'/')
    }

    /// Drops a status entry's trailing `/`, which marks a directory rather than
    /// forming part of the path.
    ///
    /// Every path recorded in a workspace identity is in this form, so the two
    /// spellings of one directory can never both appear: a set holding
    /// `node_modules/` from one capture and `node_modules` from the next would
    /// report a removal and an addition where nothing moved.
    #[must_use]
    pub fn without_trailing_separator(&self) -> Self {
        match self.0.strip_suffix(b"/") {
            Some(trimmed) => Self(trimmed.to_vec()),
            None => self.clone(),
        }
    }

    /// The lossy display form, safe to log and to show.
    #[must_use]
    pub fn display(&self) -> String {
        String::from_utf8_lossy(&self.0).into_owned()
    }

    /// Rebuilds a platform path for joining onto a worktree root.
    #[must_use]
    pub fn to_path_buf(&self) -> PathBuf {
        #[cfg(unix)]
        {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt;
            PathBuf::from(OsString::from_vec(self.0.clone()))
        }
        #[cfg(not(unix))]
        {
            PathBuf::from(String::from_utf8_lossy(&self.0).into_owned())
        }
    }

    /// The canonical serialized spelling described on the type.
    #[must_use]
    fn to_wire_string(&self) -> String {
        match std::str::from_utf8(&self.0) {
            Ok(text) if !text.starts_with(BASE64_PREFIX) => text.to_owned(),
            _ => format!("{BASE64_PREFIX}{}", BASE64.encode(&self.0)),
        }
    }

    /// Reads the canonical serialized spelling described on the type.
    fn from_wire_string(value: &str) -> Result<Self, String> {
        match value.strip_prefix(BASE64_PREFIX) {
            Some(encoded) => BASE64
                .decode(encoded)
                .map(Self)
                .map_err(|error| format!("path '{value}' is not valid Base64: {error}")),
            None => Ok(Self(value.as_bytes().to_vec())),
        }
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&String::from_utf8_lossy(&self.0))
    }
}

impl Serialize for RepoPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_wire_string())
    }
}

impl<'de> Deserialize<'de> for RepoPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_wire_string(&value).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::RepoPath;

    #[test]
    fn a_utf8_path_serializes_as_its_own_text() {
        let path = RepoPath::from_path(Path::new("src/main.rs"));
        assert_eq!(
            serde_json::to_string(&path).unwrap(),
            "\"src/main.rs\"".to_owned()
        );
        assert_eq!(
            serde_json::from_str::<RepoPath>("\"src/main.rs\"").unwrap(),
            path
        );
        assert!(!path.is_lossy());
        assert_eq!(path.display(), "src/main.rs");
    }

    #[test]
    fn non_utf8_bytes_round_trip_through_the_base64_form() {
        let path = RepoPath::from_bytes(vec![b'a', 0xff, 0xfe, b'/', b'b']);
        let json = serde_json::to_string(&path).unwrap();
        assert!(json.starts_with("\"base64:"), "{json}");
        assert_eq!(serde_json::from_str::<RepoPath>(&json).unwrap(), path);
        assert!(path.is_lossy());
        assert_ne!(path.display().as_bytes(), path.as_bytes());
    }

    #[test]
    fn a_path_named_like_the_escape_prefix_stays_distinguishable() {
        let literal = RepoPath::from_bytes(b"base64:AAAA".to_vec());
        let json = serde_json::to_string(&literal).unwrap();
        assert_eq!(serde_json::from_str::<RepoPath>(&json).unwrap(), literal);
        assert!(!literal.is_lossy());
    }

    #[test]
    fn ordering_follows_bytes_so_digest_input_is_platform_independent() {
        let mut paths = [
            RepoPath::from_bytes(b"b.txt".to_vec()),
            RepoPath::from_bytes(b"A.txt".to_vec()),
            RepoPath::from_bytes(b"a.txt".to_vec()),
        ];
        paths.sort();
        assert_eq!(
            paths.iter().map(RepoPath::display).collect::<Vec<_>>(),
            ["A.txt", "a.txt", "b.txt"]
        );
    }

    #[test]
    fn a_trailing_separator_marks_an_unexpanded_directory_entry() {
        assert!(RepoPath::from_bytes(b"target/".to_vec()).is_directory_entry());
        assert!(!RepoPath::from_bytes(b"target".to_vec()).is_directory_entry());
    }

    #[test]
    fn dropping_a_trailing_separator_gives_one_spelling_per_directory() {
        let bare = RepoPath::from_bytes(b"target".to_vec());
        assert_eq!(
            RepoPath::from_bytes(b"target/".to_vec()).without_trailing_separator(),
            bare
        );
        assert_eq!(bare.without_trailing_separator(), bare);
        // Only the final separator, so a nested path keeps its structure.
        assert_eq!(
            RepoPath::from_bytes(b"a/b/".to_vec()).without_trailing_separator(),
            RepoPath::from_bytes(b"a/b".to_vec())
        );
    }

    #[test]
    fn containment_requires_a_separator_rather_than_a_byte_prefix() {
        let directory = RepoPath::from_bytes(b"src".to_vec());
        assert!(directory.contains(&RepoPath::from_bytes(b"src".to_vec())));
        assert!(directory.contains(&RepoPath::from_bytes(b"src/main.rs".to_vec())));
        // The failure this exists to prevent: a subtree scope for `src`
        // sweeping a sibling file whose name merely begins with it.
        assert!(!directory.contains(&RepoPath::from_bytes(b"src-generated.rs".to_vec())));
        assert!(!directory.contains(&RepoPath::from_bytes(b"srcs/main.rs".to_vec())));
    }

    #[test]
    fn the_root_contains_everything_including_itself() {
        let root = RepoPath::from_bytes(Vec::new());
        assert!(root.is_empty());
        assert!(root.contains(&root));
        assert!(root.contains(&RepoPath::from_bytes(b"a/b/c.rs".to_vec())));
    }

    #[test]
    fn ancestors_start_at_the_root_and_stop_above_the_path() {
        let path = RepoPath::from_bytes(b"a/b/c.rs".to_vec());
        assert_eq!(
            path.ancestors()
                .iter()
                .map(RepoPath::display)
                .collect::<Vec<_>>(),
            ["", "a", "a/b"]
        );
        assert_eq!(
            RepoPath::from_bytes(b"top.rs".to_vec())
                .ancestors()
                .iter()
                .map(RepoPath::display)
                .collect::<Vec<_>>(),
            [""]
        );
        assert_eq!(
            path.parent().map(|parent| parent.display()),
            Some("a/b".to_owned())
        );
        assert_eq!(RepoPath::from_bytes(b"top.rs".to_vec()).parent(), None);
    }

    #[test]
    fn a_common_ancestor_is_a_directory_and_never_half_a_name() {
        let paths = [
            RepoPath::from_bytes(b"src/main.rs".to_vec()),
            RepoPath::from_bytes(b"src/mainland.rs".to_vec()),
        ];
        assert_eq!(RepoPath::common_ancestor(paths.iter()).display(), "src");

        let unrelated = [
            RepoPath::from_bytes(b"src/main.rs".to_vec()),
            RepoPath::from_bytes(b"docs/guide.md".to_vec()),
        ];
        assert!(RepoPath::common_ancestor(unrelated.iter()).is_empty());
        assert!(RepoPath::common_ancestor(std::iter::empty()).is_empty());

        let one = [RepoPath::from_bytes(b"a/b/c.rs".to_vec())];
        assert_eq!(RepoPath::common_ancestor(one.iter()).display(), "a/b");
    }

    #[test]
    fn platform_paths_rebuild_for_joining() {
        let path = RepoPath::from_path(Path::new("docs/adr/0008.md"));
        assert_eq!(path.to_path_buf(), Path::new("docs/adr/0008.md"));
    }
}
