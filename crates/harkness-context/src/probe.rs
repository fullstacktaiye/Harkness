//! Reading the workspace, behind a trait.
//!
//! Capture asks Git *what* changed and asks a [`WorkspaceProbe`] *what those
//! paths now contain*. Splitting them is what lets snapshot identity be tested
//! against hand-built content without a filesystem, and what leaves room for
//! [#112] to supply the eligibility rules — ignore files, size limits,
//! classification — without changing the identity model underneath.
//!
//! [`FilesystemProbe`] is the implementation capture uses by default: it reads
//! the working tree as it is, treats every untracked file as eligible, and never
//! follows a symlink.
//!
//! [#112]: https://github.com/fullstacktaiye/harkness/issues/112

use std::cell::RefCell;
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use harkness_git::Cancellation;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use sha2::{Digest, Sha256};

use crate::digest::Sha256Hex;
use crate::error::ContextDomainError;
use crate::path::RepoPath;

/// Bytes read from disk in one block while hashing.
///
/// Capture's memory is bounded by this, not by the size of the largest dirty
/// file: a multi-gigabyte file contributes 64 KiB of resident buffer.
const READ_BLOCK_BYTES: usize = 64 * 1024;

/// How many files one untracked directory entry may expand into.
///
/// `git status` reports an untracked directory as a single entry rather than
/// recursing, so a probe that expanded one without a bound would walk a
/// `node_modules/` a user never intended to index.
///
/// Exceeding the bound makes the whole candidate opaque: it contributes one
/// sentinel and a capture diagnostic, and changes beneath it stop being visible
/// to identity. That is a real cost, and it is still the only deterministic
/// answer available — a partial set would depend on the order the filesystem
/// enumerated the tree, and an identity that changed with enumeration order is
/// worse than one that is coarse. A tree this large is one that wants a
/// `.gitignore` entry, which is what [#112] will honour.
const MAX_UNTRACKED_EXPANSION: usize = 10_000;

/// What one path contributes to a workspace identity.
///
/// # Serialization
///
/// One string, which is also exactly what a digest absorbs: `content:<hex>`,
/// `symlink:<hex>`, `staged_blob:<oid>`, `absent`, or `unreadable`. Keeping the
/// stored spelling and the hashed spelling identical means a stored row can be
/// re-digested on load without a second encoding rule to keep in step.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum ContentDigest {
    /// SHA-256 over the file's bytes.
    Content(Sha256Hex),
    /// SHA-256 over a symlink's target *path*, which is never followed.
    ///
    /// A link pointing outside the worktree therefore changes the workspace
    /// identity without anything outside the worktree being read.
    SymlinkTarget(Sha256Hex),
    /// The blob id Git holds in its index for a staged path.
    StagedBlob(String),
    /// Nothing exists at the path: a staged or working-tree deletion.
    Absent,
    /// The path exists and could not be read.
    ///
    /// A sentinel rather than a failure. One file whose permissions changed
    /// under a capture must not cost the whole snapshot, and recording the fact
    /// keeps it visible: a path that becomes readable later changes the digest,
    /// so verification reports it as stale rather than missing it.
    Unreadable,
}

impl ContentDigest {
    /// Hashes file content.
    #[must_use]
    pub fn of_content(bytes: impl AsRef<[u8]>) -> Self {
        Self::Content(Sha256Hex::of(bytes))
    }

    /// Hashes a symlink's target path without following it.
    #[must_use]
    pub fn of_symlink_target(target: &Path) -> Self {
        Self::SymlinkTarget(Sha256Hex::of(RepoPath::from_path(target).as_bytes()))
    }

    /// The canonical spelling, which is both stored and hashed.
    #[must_use]
    pub fn as_digest_input(&self) -> String {
        match self {
            Self::Content(digest) => format!("content:{digest}"),
            Self::SymlinkTarget(digest) => format!("symlink:{digest}"),
            Self::StagedBlob(id) => format!("staged_blob:{id}"),
            Self::Absent => "absent".to_owned(),
            Self::Unreadable => "unreadable".to_owned(),
        }
    }
}

impl fmt::Display for ContentDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.as_digest_input())
    }
}

impl FromStr for ContentDigest {
    type Err = ContextDomainError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = |reason| ContextDomainError::InvalidDigest {
            value: value.to_owned(),
            expected: "content digest",
            reason,
        };
        match value {
            "absent" => return Ok(Self::Absent),
            "unreadable" => return Ok(Self::Unreadable),
            _ => {}
        }
        let (tag, rest) = value
            .split_once(':')
            .ok_or_else(|| invalid("must be 'absent', 'unreadable', or '<kind>:<digest>'"))?;
        match tag {
            "content" => rest.parse().map(Self::Content),
            "symlink" => rest.parse().map(Self::SymlinkTarget),
            "staged_blob" => {
                if rest.is_empty()
                    || !rest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                {
                    return Err(invalid("a staged blob id must be lowercase hexadecimal"));
                }
                Ok(Self::StagedBlob(rest.to_owned()))
            }
            _ => Err(invalid(
                "unknown content digest kind; expected content, symlink, or staged_blob",
            )),
        }
    }
}

impl Serialize for ContentDigest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.as_digest_input())
    }
}

impl<'de> Deserialize<'de> for ContentDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(de::Error::custom)
    }
}

/// Why a probe could not answer for one path.
///
/// The three variants carry the whole policy decision: a
/// [`ProbeFailure::Skipped`] path contributes [`ContentDigest::Unreadable`] and a
/// line in the capture diagnostics, a [`ProbeFailure::Cancelled`] read ends the
/// operation through its cancellation contract, and a [`ProbeFailure::Fatal`] one
/// ends the capture with an error. Anything a user can cause by accident — a
/// permission bit, a file that vanished mid-walk — is a skip; only a probe that
/// cannot function at all is fatal.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProbeFailure {
    /// This path could not be read, and the capture goes on without it.
    Skipped {
        /// Stable human-readable explanation.
        reason: String,
    },
    /// The read observed its cancellation token.
    Cancelled,
    /// The probe cannot answer at all, and the capture must not continue.
    Fatal {
        /// Stable human-readable explanation.
        reason: String,
    },
}

impl ProbeFailure {
    /// Builds a non-fatal skip.
    #[must_use]
    pub fn skipped(reason: impl Into<String>) -> Self {
        Self::Skipped {
            reason: reason.into(),
        }
    }

    /// Builds a failure that ends the capture.
    #[must_use]
    pub fn fatal(reason: impl Into<String>) -> Self {
        Self::Fatal {
            reason: reason.into(),
        }
    }

    /// The explanation, whichever variant this is.
    #[must_use]
    pub fn reason(&self) -> &str {
        match self {
            Self::Skipped { reason } | Self::Fatal { reason } => reason,
            Self::Cancelled => "the workspace read was cancelled",
        }
    }

    /// Whether this failure ends the capture with an error.
    #[must_use]
    pub const fn is_fatal(&self) -> bool {
        matches!(self, Self::Fatal { .. })
    }

    /// Whether the read stopped because it was cancelled.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

impl fmt::Display for ProbeFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.reason())
    }
}

/// One sub-path an expansion could not walk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnreadablePath {
    /// The repository-relative path that could not be read.
    pub path: RepoPath,
    /// Stable human-readable explanation.
    pub reason: String,
}

/// What one untracked status entry expanded into.
///
/// The two lists are separate because a failure inside a tree must not cost the
/// rest of it. Collapsing a whole subtree to one sentinel would make its digest
/// constant, and a constant digest means every later edit under that tree reads
/// as `Fresh` — the exact false negative snapshot identity exists to prevent. So
/// the files that *were* walked take part in identity as themselves, and each
/// branch that failed is named on its own: it contributes a sentinel under its
/// own path, and becomes readable — and therefore visible as a change — the
/// moment it can be read again.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UntrackedExpansion {
    /// Paths whose content takes part in identity.
    pub paths: Vec<RepoPath>,
    /// Sub-paths that could not be walked.
    pub unreadable: Vec<UnreadablePath>,
}

impl UntrackedExpansion {
    /// Expands to exactly one path, which is what a plain file does.
    #[must_use]
    pub fn of_one(path: RepoPath) -> Self {
        Self {
            paths: vec![path],
            unreadable: Vec::new(),
        }
    }

    /// Whether the expansion found nothing at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.unreadable.is_empty()
    }
}

/// Reads the workspace content that Git names but does not hash.
///
/// Every method is blocking and takes repository-relative paths. Implementations
/// must not follow symlinks for content and must not read outside the worktree
/// root; identity is meant to describe one worktree, and a probe that reached
/// past it would make that claim false.
pub trait WorkspaceProbe {
    /// Announces that a fresh capture or verification is about to begin.
    ///
    /// Called once, before anything else, by both [`WorkspaceSnapshot::capture`]
    /// and [`WorkspaceSnapshot::verify`]. An implementation that caches anything
    /// about the workspace — the Git index, a directory listing — **must**
    /// invalidate it here. A probe is naturally held for the lifetime of a
    /// worktree rather than of a read, and a cached index served to a later
    /// verification would report `Fresh` across a staged change: a staleness
    /// gate that answers from a snapshot of the past is not a gate.
    ///
    /// The default is a no-op, for probes that hold nothing.
    ///
    /// [`WorkspaceSnapshot::capture`]: crate::WorkspaceSnapshot::capture
    /// [`WorkspaceSnapshot::verify`]: crate::WorkspaceSnapshot::verify
    fn begin_read(&self) {}

    /// Expands one untracked status entry into the paths that take part in
    /// identity.
    ///
    /// Git reports an untracked directory as a single `dir/` entry, so this is
    /// where eligibility is decided. A plain file expands to itself. A failure
    /// *inside* the tree belongs in [`UntrackedExpansion::unreadable`]; `Err` is
    /// for a candidate that could not be read at all.
    ///
    /// `cancellation` is polled during the walk, so a large untracked tree does
    /// not defer a cancellation until it finishes.
    fn expand_untracked(
        &self,
        candidate: &RepoPath,
        cancellation: &Cancellation,
    ) -> Result<UntrackedExpansion, ProbeFailure>;

    /// Describes what the working tree now holds at `path`.
    ///
    /// `cancellation` is polled while content is read, so one very large file
    /// does not defer a cancellation until it has been hashed.
    fn hash_path(
        &self,
        path: &RepoPath,
        cancellation: &Cancellation,
    ) -> Result<ContentDigest, ProbeFailure>;

    /// The blob id Git has staged for `path`, if it has one.
    ///
    /// `None` is an ordinary answer: a staged deletion has no blob, and the
    /// snapshot records [`ContentDigest::Absent`] for it.
    fn staged_blob_id(&self, path: &RepoPath) -> Result<Option<String>, ProbeFailure>;
}

/// Reads the working tree exactly as it is.
///
/// Untracked eligibility is deliberately maximal here — every file under an
/// untracked directory counts, up to a bound — because a snapshot that ignored a
/// new file would report `Fresh` for a workspace the model is about to read
/// differently. Narrowing it is [#112]'s decision to make, with ignore rules and
/// classification behind it.
///
/// [#112]: https://github.com/fullstacktaiye/harkness/issues/112
#[derive(Debug)]
pub struct FilesystemProbe {
    root: PathBuf,
    staged_blobs: RefCell<IndexCache>,
}

/// The Git index, read at most once per capture or verification.
#[derive(Debug)]
enum IndexCache {
    /// Not read yet during the current read.
    Unloaded,
    /// The repository or its index could not be opened.
    Unavailable,
    /// Stage-zero entries, sorted by path for binary search.
    Loaded(Vec<(RepoPath, String)>),
}

impl FilesystemProbe {
    /// Reads the worktree rooted at `root`.
    ///
    /// Nothing is read here. The Git index is loaded lazily on the first staged
    /// lookup of a read and dropped again by [`WorkspaceProbe::begin_read`], so
    /// one probe held for a worktree's lifetime costs one index read per
    /// capture — not one per staged path, and never one shared across two reads
    /// that must see different indexes.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            staged_blobs: RefCell::new(IndexCache::Unloaded),
        }
    }

    /// The worktree this probe reads.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Joins a repository-relative path onto the worktree root.
    ///
    /// A trailing separator is dropped first: `PathBuf::join` keeps it, and
    /// `symlink_metadata` does not accept it everywhere.
    ///
    /// Every component is checked before the join, because `PathBuf::join`
    /// silently *discards* the root when handed an absolute path and happily
    /// walks upward through `..`. Nothing `collect` passes is either — those
    /// paths come from `git status` and from `DirEntry::file_name` — but this
    /// type is public, `RepoPath::from_bytes` accepts any bytes at all, and a
    /// persisted path will start round-tripping back through here with [#110].
    /// The trait promises never to read outside the worktree root; that promise
    /// should hold because it is enforced, not because every caller so far has
    /// happened to behave.
    ///
    /// [#110]: https://github.com/fullstacktaiye/harkness/issues/110
    fn resolve(&self, path: &RepoPath) -> Result<PathBuf, ProbeFailure> {
        let relative = path.without_trailing_separator().to_path_buf();
        for component in relative.components() {
            if !matches!(component, Component::Normal(_) | Component::CurDir) {
                return Err(ProbeFailure::skipped(format!(
                    "'{}' is not a path inside the worktree",
                    path.display()
                )));
            }
        }
        Ok(self.root.join(relative))
    }
}

impl WorkspaceProbe for FilesystemProbe {
    fn begin_read(&self) {
        *self.staged_blobs.borrow_mut() = IndexCache::Unloaded;
    }

    fn expand_untracked(
        &self,
        candidate: &RepoPath,
        cancellation: &Cancellation,
    ) -> Result<UntrackedExpansion, ProbeFailure> {
        let resolved = self.resolve(candidate)?;
        let metadata = std::fs::symlink_metadata(&resolved)
            .map_err(|error| ProbeFailure::skipped(format!("cannot stat: {error}")))?;
        if !metadata.is_dir() {
            return Ok(UntrackedExpansion::of_one(
                candidate.without_trailing_separator(),
            ));
        }

        let mut expansion = UntrackedExpansion::default();
        let mut pending = vec![(resolved, directory_prefix(candidate))];
        while let Some((directory, prefix)) = pending.pop() {
            if cancellation.is_cancelled() {
                return Err(ProbeFailure::Cancelled);
            }
            let entries = match std::fs::read_dir(&directory) {
                Ok(entries) => entries,
                // Only this branch is lost. Everything already walked stays in
                // the identity, and this path is named on its own, so becoming
                // readable later reads as a change rather than as nothing.
                Err(error) => {
                    expansion.unreadable.push(UnreadablePath {
                        path: directory_at(&prefix),
                        reason: format!("cannot list: {error}"),
                    });
                    continue;
                }
            };
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        expansion.unreadable.push(UnreadablePath {
                            path: directory_at(&prefix),
                            reason: format!("cannot list: {error}"),
                        });
                        break;
                    }
                };
                let name = RepoPath::from_path(Path::new(&entry.file_name()));
                // Git's own administrative directory is never workspace content.
                if name.as_bytes() == b".git" {
                    continue;
                }
                let mut child = prefix.clone();
                child.extend_from_slice(name.as_bytes());
                // `file_type` on a `DirEntry` does not follow symlinks, so a
                // link to a directory is recorded rather than descended into.
                let Ok(file_type) = entry.file_type() else {
                    expansion.unreadable.push(UnreadablePath {
                        path: RepoPath::from_bytes(child),
                        reason: "cannot stat".to_owned(),
                    });
                    continue;
                };
                if file_type.is_dir() {
                    let mut nested = child;
                    nested.push(b'/');
                    pending.push((entry.path(), nested));
                } else {
                    if expansion.paths.len() >= MAX_UNTRACKED_EXPANSION {
                        // A partial set here would depend on the order the
                        // filesystem enumerated the tree, and identity must not.
                        // One opaque entry for the whole candidate is the only
                        // deterministic answer left.
                        return Err(ProbeFailure::skipped(format!(
                            "holds more than {MAX_UNTRACKED_EXPANSION} untracked files"
                        )));
                    }
                    expansion.paths.push(RepoPath::from_bytes(child));
                }
            }
        }
        // The walk order depends on the filesystem; identity must not.
        expansion.paths.sort();
        expansion
            .unreadable
            .sort_by(|left, right| left.path.cmp(&right.path));
        expansion
            .unreadable
            .dedup_by(|left, right| left.path == right.path);
        Ok(expansion)
    }

    fn hash_path(
        &self,
        path: &RepoPath,
        cancellation: &Cancellation,
    ) -> Result<ContentDigest, ProbeFailure> {
        let resolved = self.resolve(path)?;
        let metadata = match std::fs::symlink_metadata(&resolved) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ContentDigest::Absent);
            }
            Err(error) => return Err(ProbeFailure::skipped(format!("cannot stat: {error}"))),
        };
        if metadata.is_symlink() {
            let target = std::fs::read_link(&resolved)
                .map_err(|error| ProbeFailure::skipped(format!("cannot read link: {error}")))?;
            return Ok(ContentDigest::of_symlink_target(&target));
        }
        if metadata.is_dir() {
            return Err(ProbeFailure::skipped("is a directory"));
        }
        // Everything that is not a regular file is refused *before* it is
        // opened, because opening it is the hazard. `open(2)` on a FIFO with no
        // writer blocks forever, and a character device such as `/dev/zero`
        // never reaches end of file — either one would pin a capture with no way
        // out, since the token is polled between files and not inside a read.
        // Git reports an untracked directory as one entry without recursing, so
        // a FIFO one level down reaches this function through the expansion.
        if !metadata.is_file() {
            return Err(ProbeFailure::skipped("is not a regular file"));
        }

        let mut file = File::open(&resolved)
            .map_err(|error| ProbeFailure::skipped(format!("cannot open: {error}")))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; READ_BLOCK_BYTES];
        loop {
            // Per block, not per file. One large untracked file — a VM image, a
            // core dump — would otherwise hold a cancelled capture for as long
            // as hashing it takes, which is exactly the case the promptness
            // promise is about.
            if cancellation.is_cancelled() {
                return Err(ProbeFailure::Cancelled);
            }
            let read = file
                .read(&mut buffer)
                .map_err(|error| ProbeFailure::skipped(format!("cannot read: {error}")))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(ContentDigest::Content(Sha256Hex::finish(hasher)))
    }

    fn staged_blob_id(&self, path: &RepoPath) -> Result<Option<String>, ProbeFailure> {
        let mut cache = self.staged_blobs.borrow_mut();
        if matches!(*cache, IndexCache::Unloaded) {
            *cache = match load_index_blobs(&self.root) {
                Some(blobs) => IndexCache::Loaded(blobs),
                None => IndexCache::Unavailable,
            };
        }
        let IndexCache::Loaded(blobs) = &*cache else {
            return Err(ProbeFailure::skipped("the Git index could not be read"));
        };
        Ok(blobs
            .binary_search_by(|(indexed, _)| indexed.cmp(path))
            .ok()
            .map(|position| blobs[position].1.clone()))
    }
}

/// The `dir/` prefix every path beneath an untracked directory entry carries.
fn directory_prefix(candidate: &RepoPath) -> Vec<u8> {
    let mut bytes = candidate.as_bytes().to_vec();
    if bytes.last() != Some(&b'/') {
        bytes.push(b'/');
    }
    bytes
}

/// Turns a walk prefix back into the directory path it names.
fn directory_at(prefix: &[u8]) -> RepoPath {
    RepoPath::from_bytes(prefix.strip_suffix(b"/").unwrap_or(prefix).to_vec())
}

/// Reads every stage-zero index entry once, sorted for binary search.
///
/// Conflicted paths hold entries at stages one to three and none at stage zero;
/// they are left out here and reported through the working tree instead, so a
/// merge in progress does not invent three staged versions of one path.
fn load_index_blobs(root: &Path) -> Option<Vec<(RepoPath, String)>> {
    /// Mask isolating an index entry's merge stage within its flags.
    const STAGE_MASK: u16 = 0x3000;
    /// Bit position of the merge stage within an index entry's flags.
    const STAGE_SHIFT: u16 = 12;

    let repository = harkness_git::git2::Repository::open(root).ok()?;
    let index = repository.index().ok()?;
    let mut blobs = index
        .iter()
        .filter(|entry| (entry.flags & STAGE_MASK) >> STAGE_SHIFT == 0)
        .map(|entry| {
            (
                RepoPath::from_bytes(entry.path.clone()),
                entry.id.to_string(),
            )
        })
        .collect::<Vec<_>>();
    blobs.sort_by(|left, right| left.0.cmp(&right.0));
    Some(blobs)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use harkness_git::Cancellation;
    use harkness_test_fixtures::{Fixture, git, initialize_repository};

    use super::{ContentDigest, FilesystemProbe, ProbeFailure, UntrackedExpansion, WorkspaceProbe};
    use crate::digest::Sha256Hex;
    use crate::path::RepoPath;

    fn path(text: &str) -> RepoPath {
        RepoPath::from_bytes(text.as_bytes().to_vec())
    }

    #[test]
    fn content_digest_spellings_round_trip() {
        let cases = [
            ContentDigest::of_content(b"body"),
            ContentDigest::SymlinkTarget(Sha256Hex::of(b"../elsewhere")),
            ContentDigest::StagedBlob("0123456789abcdef".to_owned()),
            ContentDigest::Absent,
            ContentDigest::Unreadable,
        ];
        for digest in cases {
            let spelled = digest.as_digest_input();
            assert_eq!(spelled.parse::<ContentDigest>().unwrap(), digest);
            let json = serde_json::to_string(&digest).unwrap();
            assert_eq!(json, format!("\"{spelled}\""));
            assert_eq!(
                serde_json::from_str::<ContentDigest>(&json).unwrap(),
                digest
            );
        }
    }

    #[test]
    fn content_digest_rejects_unknown_or_malformed_spellings() {
        for rejected in [
            "",
            "content",
            "content:short",
            "sha256:0",
            "staged_blob:",
            "staged_blob:XYZ",
            "mystery:0000",
        ] {
            assert!(
                rejected.parse::<ContentDigest>().is_err(),
                "accepted '{rejected}'"
            );
        }
    }

    #[test]
    fn a_probe_hashes_content_and_reports_a_missing_path_as_absent() {
        let fixture = Fixture::new();
        let root = fixture.directory("worktree");
        fs::write(root.join("a.txt"), b"body").unwrap();
        let probe = FilesystemProbe::new(&root);

        assert_eq!(
            probe
                .hash_path(&path("a.txt"), &Cancellation::default())
                .unwrap(),
            ContentDigest::of_content(b"body")
        );
        assert_eq!(
            probe
                .hash_path(&path("gone.txt"), &Cancellation::default())
                .unwrap(),
            ContentDigest::Absent
        );
        assert_eq!(probe.root(), root);
    }

    #[test]
    fn a_file_larger_than_one_read_block_hashes_the_same_as_its_bytes() {
        let fixture = Fixture::new();
        let root = fixture.directory("worktree");
        let content = vec![b'x'; super::READ_BLOCK_BYTES * 2 + 7];
        fs::write(root.join("big.bin"), &content).unwrap();
        let probe = FilesystemProbe::new(&root);
        assert_eq!(
            probe
                .hash_path(&path("big.bin"), &Cancellation::default())
                .unwrap(),
            ContentDigest::of_content(&content)
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_hashes_its_target_path_and_is_never_followed() {
        let fixture = Fixture::new();
        let root = fixture.directory("worktree");
        let outside = fixture.root.path().join("secret.txt");
        fs::write(&outside, b"do not read me").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let probe = FilesystemProbe::new(&root);

        let digest = probe
            .hash_path(&path("link"), &Cancellation::default())
            .unwrap();
        assert_eq!(digest, ContentDigest::of_symlink_target(&outside));
        assert_ne!(digest, ContentDigest::of_content(b"do not read me"));
    }

    #[test]
    fn an_untracked_directory_expands_to_its_files_in_a_stable_order() {
        let fixture = Fixture::new();
        let root = fixture.directory("worktree");
        fs::create_dir_all(root.join("new/nested")).unwrap();
        fs::write(root.join("new/b.txt"), b"b").unwrap();
        fs::write(root.join("new/a.txt"), b"a").unwrap();
        fs::write(root.join("new/nested/c.txt"), b"c").unwrap();
        let probe = FilesystemProbe::new(&root);

        let expanded = probe
            .expand_untracked(&path("new/"), &Cancellation::default())
            .unwrap();
        assert_eq!(
            expanded
                .paths
                .iter()
                .map(RepoPath::display)
                .collect::<Vec<_>>(),
            ["new/a.txt", "new/b.txt", "new/nested/c.txt"]
        );
        assert!(expanded.unreadable.is_empty());
    }

    #[test]
    fn expanding_a_plain_file_yields_the_file_itself() {
        let fixture = Fixture::new();
        let root = fixture.directory("worktree");
        fs::write(root.join("a.txt"), b"a").unwrap();
        let probe = FilesystemProbe::new(&root);
        assert_eq!(
            probe
                .expand_untracked(&path("a.txt"), &Cancellation::default())
                .unwrap(),
            UntrackedExpansion::of_one(path("a.txt"))
        );
    }

    /// Skips rather than fails when the process can read a mode-`000`
    /// directory anyway, which is what running as root does.
    #[cfg(unix)]
    fn deny_directory_access(directory: &std::path::Path) -> bool {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(directory).unwrap().permissions();
        permissions.set_mode(0o000);
        fs::set_permissions(directory, permissions).unwrap();
        fs::read_dir(directory).is_err()
    }

    #[cfg(unix)]
    #[test]
    fn one_unreadable_branch_does_not_cost_the_rest_of_the_tree() {
        let fixture = Fixture::new();
        let root = fixture.directory("worktree");
        fs::create_dir_all(root.join("new/open")).unwrap();
        fs::create_dir(root.join("new/closed")).unwrap();
        fs::write(root.join("new/open/a.txt"), b"a").unwrap();
        fs::write(root.join("new/closed/secret.txt"), b"s").unwrap();
        if !deny_directory_access(&root.join("new/closed")) {
            return;
        }

        let probe = FilesystemProbe::new(&root);
        let expanded = probe
            .expand_untracked(&path("new/"), &Cancellation::default())
            .unwrap();

        // The readable half still takes part in identity, and the branch that
        // failed is named under its own path rather than swallowing the tree.
        assert_eq!(
            expanded
                .paths
                .iter()
                .map(RepoPath::display)
                .collect::<Vec<_>>(),
            ["new/open/a.txt"]
        );
        assert_eq!(
            expanded
                .unreadable
                .iter()
                .map(|entry| entry.path.display())
                .collect::<Vec<_>>(),
            ["new/closed"]
        );
    }

    #[test]
    fn expansion_stops_when_the_walk_is_cancelled() {
        let fixture = Fixture::new();
        let root = fixture.directory("worktree");
        fs::create_dir(root.join("new")).unwrap();
        fs::write(root.join("new/a.txt"), b"a").unwrap();
        let probe = FilesystemProbe::new(&root);

        let cancellation = Cancellation::default();
        cancellation.cancel();
        let failure = probe
            .expand_untracked(&path("new/"), &cancellation)
            .unwrap_err();
        assert!(failure.is_cancelled(), "{failure}");
        assert!(!failure.is_fatal());
    }

    #[test]
    fn a_staged_lookup_reflects_the_index_as_of_the_current_read() {
        let fixture = Fixture::new();
        let root = fixture.directory("worktree");
        initialize_repository(&root);
        fs::write(root.join("tracked.txt"), b"changed\n").unwrap();
        git(&root, ["add", "tracked.txt"]);

        // Constructed before the second `git add`, and reused across both reads:
        // the natural caller pattern, and the one a cache-at-construction probe
        // would answer wrongly.
        let probe = FilesystemProbe::new(&root);
        probe.begin_read();
        let first = probe.staged_blob_id(&path("tracked.txt")).unwrap();

        fs::write(root.join("tracked.txt"), b"changed again\n").unwrap();
        git(&root, ["add", "tracked.txt"]);
        probe.begin_read();
        let second = probe.staged_blob_id(&path("tracked.txt")).unwrap();

        assert!(first.is_some() && second.is_some());
        assert_ne!(first, second, "the probe served a stale index");
    }

    #[test]
    fn a_directory_hashes_as_a_skip_rather_than_content() {
        let fixture = Fixture::new();
        let root = fixture.directory("worktree");
        fs::create_dir(root.join("sub")).unwrap();
        let probe = FilesystemProbe::new(&root);
        assert_eq!(
            probe.hash_path(&path("sub"), &Cancellation::default()),
            Err(ProbeFailure::skipped("is a directory"))
        );
    }

    /// Creates a FIFO, or reports that this platform cannot.
    #[cfg(unix)]
    fn make_fifo(path: &std::path::Path) -> bool {
        std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_is_refused_before_it_is_opened() {
        let fixture = Fixture::new();
        let root = fixture.directory("worktree");
        if !make_fifo(&root.join("pipe")) {
            return;
        }
        let probe = FilesystemProbe::new(&root);

        // Opening it would block forever: `open(2)` on a FIFO with no writer
        // never returns, and nothing polls the token inside `open`. If this
        // regresses the test hangs rather than fails, which is the nature of the
        // bug it covers.
        let failure = probe
            .hash_path(&path("pipe"), &Cancellation::default())
            .unwrap_err();
        assert_eq!(failure.reason(), "is not a regular file");
        assert!(!failure.is_fatal());
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_inside_an_untracked_directory_does_not_stall_a_walk() {
        let fixture = Fixture::new();
        let root = fixture.directory("worktree");
        fs::create_dir(root.join("new")).unwrap();
        fs::write(root.join("new/a.txt"), b"a").unwrap();
        if !make_fifo(&root.join("new/pipe")) {
            return;
        }
        let probe = FilesystemProbe::new(&root);

        // Git reports `new/` as one entry without recursing, so the FIFO reaches
        // hashing through the expansion rather than through a status entry.
        let expanded = probe
            .expand_untracked(&path("new/"), &Cancellation::default())
            .unwrap();
        assert_eq!(
            expanded
                .paths
                .iter()
                .map(RepoPath::display)
                .collect::<Vec<_>>(),
            ["new/a.txt", "new/pipe"]
        );
        for candidate in &expanded.paths {
            let _ = probe.hash_path(candidate, &Cancellation::default());
        }
    }

    #[test]
    fn hashing_stops_between_blocks_when_the_token_is_set() {
        let fixture = Fixture::new();
        let root = fixture.directory("worktree");
        fs::write(
            root.join("big.bin"),
            vec![b'x'; super::READ_BLOCK_BYTES * 4],
        )
        .unwrap();
        let probe = FilesystemProbe::new(&root);

        let cancellation = Cancellation::default();
        cancellation.cancel();
        let failure = probe
            .hash_path(&path("big.bin"), &cancellation)
            .unwrap_err();
        assert!(failure.is_cancelled(), "{failure}");
    }

    #[test]
    fn a_path_that_leaves_the_worktree_is_refused_rather_than_read() {
        let fixture = Fixture::new();
        let root = fixture.directory("worktree");
        let outside = fixture.root.path().join("outside.txt");
        fs::write(&outside, b"do not read me").unwrap();
        let probe = FilesystemProbe::new(&root);

        // `PathBuf::join` discards the root outright for an absolute path and
        // walks upward through `..`, so the trait's promise not to read outside
        // the worktree has to be enforced rather than assumed.
        for escape in [
            RepoPath::from_path(&outside),
            RepoPath::from_bytes(b"../outside.txt".to_vec()),
            RepoPath::from_bytes(b"nested/../../outside.txt".to_vec()),
        ] {
            let failure = probe
                .hash_path(&escape, &Cancellation::default())
                .unwrap_err();
            assert!(
                failure.reason().contains("not a path inside the worktree"),
                "read '{}': {failure}",
                escape.display()
            );
            assert!(
                probe
                    .expand_untracked(&escape, &Cancellation::default())
                    .is_err()
            );
        }
        // A path holding `..` in the middle but staying inside is still refused:
        // the check is on components, not on where they happen to land.
        assert!(
            probe
                .hash_path(
                    &RepoPath::from_bytes(b"a/../a.txt".to_vec()),
                    &Cancellation::default()
                )
                .is_err()
        );
    }

    #[test]
    fn a_worktree_without_a_readable_index_skips_staged_lookups() {
        let fixture = Fixture::new();
        let root = fixture.directory("worktree");
        let probe = FilesystemProbe::new(&root);
        let failure = probe.staged_blob_id(&path("a.txt")).unwrap_err();
        assert!(!failure.is_fatal(), "{failure}");
        assert_eq!(failure.reason(), "the Git index could not be read");
    }
}
