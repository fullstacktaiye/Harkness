//! The bounded, classified set of files one worktree offers as context.
//!
//! Everything downstream — chunking, the index, lexical search, the repository
//! map, instruction discovery — reads a [`FileInventory`] instead of walking the
//! filesystem itself. That is the point of the module: one walk, one exclusion
//! hierarchy, one classification, so two retrieval features cannot disagree
//! about whether a file exists.
//!
//! [`InventoryBuilder`] carries the contract a caller reads: the four-layer
//! exclusion hierarchy, what a walk records, and how a bound is reported. What
//! follows is the part a reader of this file needs and a caller does not.
//!
//! # The walk is ours; the rule matcher is the `ignore` crate's
//!
//! `ignore::WalkBuilder` decides exclusion inside its own iterator, and that is
//! the one thing this module cannot delegate. A `.env` that a repository's
//! `.gitignore` also names would be dropped before the denial layer ever saw it,
//! so `denied_count` would depend on repository content; and `ignored_count`
//! would be unobservable outright, because a walker that filters never says what
//! it skipped. The traversal is therefore this module's — an explicit stack
//! rather than recursion, so a deep tree cannot exhaust one — while every glob
//! is `ignore::gitignore`'s, because gitignore semantics are not worth
//! re-deriving.
//!
//! Layer 1 is matched against every parent directory as well as the path itself,
//! so nothing beneath a denied directory can be recorded even if the pruning
//! that should have stopped the walk earlier ever fails.
//!
//! # What this module does not do
//!
//! It persists nothing ([#114]), watches nothing ([#115]), hashes no content
//! ([#113]), and emits no events: an inventory carries the counts and
//! diagnostics an event needs, and the engine facade is what publishes them.
//!
//! [#113]: https://github.com/fullstacktaiye/harkness/issues/113
//! [#114]: https://github.com/fullstacktaiye/harkness/issues/114
//! [#115]: https://github.com/fullstacktaiye/harkness/issues/115

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, Metadata};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use harkness_git::Cancellation;
use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use thiserror::Error;

use crate::classify::{BINARY_SNIFF_BYTES, CLASSIFY_VERSION, FileClass, FileSample};
use crate::ids::SnapshotId;
use crate::path::RepoPath;
use crate::snapshot::WorkspaceSnapshot;
use crate::text::floor_char_boundary;

/// The most entries one inventory may hold before it reports truncation.
pub const MAX_INVENTORY_FILES: usize = 200_000;

/// The longest one walk may run before it reports truncation.
pub const MAX_WALK_DURATION: Duration = Duration::from_secs(60);

/// The most diagnostics one inventory retains.
///
/// Diagnostics are driven by repository content — a tree full of unreadable
/// directories produces one per directory — so the list is bounded and the
/// overflow is counted by [`FileInventory::dropped_diagnostics`] rather than
/// grown without limit.
pub const MAX_INVENTORY_DIAGNOSTICS: usize = 1_000;

/// The longest text a diagnostic quotes from repository content.
pub const MAX_DIAGNOSTIC_TEXT_BYTES: usize = 256;

/// The largest ignore file this build will read.
///
/// A rule file is repository content, and a file too large to read is refused
/// rather than partially applied: a tightening rule that cannot be applied must
/// not be silently skipped.
pub const MAX_IGNORE_FILE_BYTES: u64 = 1024 * 1024;

/// The conventional name of the global user ignore file inside the data
/// directory.
pub const GLOBAL_IGNORE_FILE: &str = "context-ignore";

/// The conventional location of the repository ignore file inside a worktree.
pub const REPOSITORY_IGNORE_FILE: &str = ".harkness/context-ignore";

/// The credential-bearing names no configuration can re-include.
///
/// Gitignore syntax, matched against every path and every parent directory of
/// the worktree. A path matching one of these is counted and discarded before
/// anything else looks at it: it never becomes an entry, never appears in a
/// diagnostic, and never reaches a log.
///
/// The list is deliberately blunt and occasionally over-broad — `*.key` catches
/// a game asset, `id_rsa*` catches a public key — because the cost of a false
/// positive is one unindexed file and the cost of a false negative is a
/// credential in a prompt. Adding to it is a [`CLASSIFY_VERSION`] bump.
pub const BUILT_IN_DENIALS: &[&str] = &[
    ".env",
    ".env.*",
    "*.pem",
    "*.key",
    "*.p12",
    "*.pfx",
    "id_rsa*",
    "id_ed25519*",
    "id_ecdsa*",
    ".git-credentials",
    ".netrc",
    "**/.aws/credentials",
    "**/.config/gcloud/",
    "**/.config/gcloud/**",
    "**/.kube/config",
    ".npmrc",
    ".pypirc",
    "*.keystore",
    "*.jks",
];

/// Which rule file a diagnostic came from.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum IgnoreLayer {
    /// The compiled-in denial list, which no later layer can override.
    BuiltIn,
    /// The global user ignore file.
    Global,
    /// The repository's own ignore file, which may only tighten.
    Repository,
    /// One `.gitignore` in the repository's chain.
    GitIgnore,
}

impl IgnoreLayer {
    /// Every layer in the order it is consulted.
    pub const ALL: &'static [Self] = &[
        Self::BuiltIn,
        Self::Global,
        Self::Repository,
        Self::GitIgnore,
    ];

    /// The stable spelling of this layer.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BuiltIn => "built_in",
            Self::Global => "global",
            Self::Repository => "repository",
            Self::GitIgnore => "gitignore",
        }
    }
}

impl std::fmt::Display for IgnoreLayer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Why a walk stopped early.
///
/// A truncated inventory is a partial answer. Anything that would report a
/// number derived from it — "this repository has N files", "no match found" —
/// owes the reader this value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum InventoryTruncation {
    /// The entry budget was reached; entries hold exactly `limit` paths.
    FileBudgetExhausted {
        /// The budget that was reached.
        limit: usize,
    },
    /// The time budget was reached.
    WalkTimeExhausted {
        /// The budget that was reached.
        limit: Duration,
    },
}

impl InventoryTruncation {
    /// The stable spelling of this truncation.
    ///
    /// `file_budget_exhausted` is deliberately the spelling
    /// `harkness_git::DiffOmission` already publishes for the same idea one
    /// layer down. It is a truncation spelling rather than an error kind, so it
    /// shares no namespace with [`InventoryError::KINDS`]; what it must not do
    /// is describe one concept with two words in one product.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FileBudgetExhausted { .. } => "file_budget_exhausted",
            Self::WalkTimeExhausted { .. } => "walk_time_exhausted",
        }
    }
}

impl std::fmt::Display for InventoryTruncation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What a directory that the walk refused to descend into is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Boundary {
    /// A repository checked out inside this one that `.gitmodules` does not
    /// declare.
    NestedRepository,
    /// A directory `.gitmodules` declares as a submodule.
    Submodule,
}

impl Boundary {
    /// The stable spelling of this boundary.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NestedRepository => "nested_repository",
            Self::Submodule => "submodule",
        }
    }
}

impl std::fmt::Display for Boundary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One path the walk recorded.
///
/// Metadata only: an inventory never holds file content, and the eight kilobytes
/// a classification reads are discarded once a class has been assigned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InventoryEntry {
    /// Repository-relative path, byte-exact.
    pub path: RepoPath,
    /// Size in bytes as the filesystem reported it, without following a link.
    pub byte_size: u64,
    /// Modification time in nanoseconds since the Unix epoch, when the platform
    /// reported one.
    pub mtime_ns: Option<i64>,
    /// The one class this file holds.
    pub class: FileClass,
    /// Whether the path is a symbolic link, which is recorded and never
    /// followed.
    pub symlink: bool,
    /// Set when the path is a directory the walk refused to descend into.
    pub boundary: Option<Boundary>,
    /// Whether the walk could not read the path's metadata or its opening
    /// bytes.
    pub unreadable: bool,
}

impl InventoryEntry {
    /// Whether this entry's content may be indexed and retrieved.
    ///
    /// Derived rather than stored, so it cannot go stale against the fields it
    /// summarizes. Four things make an entry ineligible: a class that forbids
    /// or refuses retrieval, a symlink (whose content is somewhere else), a
    /// repository boundary (whose content belongs to another repository), and
    /// a path this walk could not read.
    #[must_use]
    pub const fn eligible(&self) -> bool {
        self.class.is_eligible() && !self.symlink && self.boundary.is_none() && !self.unreadable
    }
}

/// Something the walk noticed that is worth reporting but is not a failure.
///
/// Every quoted *text* field — a pattern, a reason, a file in display form — is
/// clamped to [`MAX_DIAGNOSTIC_TEXT_BYTES`], because a repository writes those
/// and none of them may decide how long a Harkness message is. A [`RepoPath`] is
/// deliberately not clamped: it is the walk's own product, bounded by what the
/// filesystem let the walk reach, and a truncated one would name a file that
/// does not exist.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum InventoryDiagnostic {
    /// A negation in a layer that may only tighten was discarded.
    ReInclusionDiscarded {
        /// Which rule file held it.
        layer: IgnoreLayer,
        /// The file, in lossy display form.
        file: String,
        /// One-based line number.
        line: u64,
        /// The discarded pattern.
        pattern: String,
    },
    /// One pattern of a rule file could not be compiled; the rest still apply.
    IgnoreRuleInvalid {
        /// Which rule file held it.
        layer: IgnoreLayer,
        /// The file, in lossy display form.
        file: String,
        /// One-based line number, when the rule engine reported one.
        line: Option<u64>,
        /// The rejected pattern, when the rule engine reported one.
        pattern: Option<String>,
        /// Stable human-readable explanation.
        reason: String,
    },
    /// Two recorded paths differ only by case.
    ///
    /// Kept as two entries, because indexing is keyed by exact bytes and a
    /// case-insensitive filesystem is not a reason to lose one of them.
    CaseCollision {
        /// The path just recorded.
        path: RepoPath,
        /// The path already recorded that it folds onto.
        existing: RepoPath,
    },
    /// A path could not be read; the walk continued.
    Unreadable {
        /// The path, byte-exact.
        path: RepoPath,
        /// Stable human-readable explanation.
        reason: String,
    },
    /// A path listed by its directory was gone by the time it was read.
    Vanished {
        /// The path, byte-exact.
        path: RepoPath,
    },
}

impl InventoryDiagnostic {
    /// Every stable discriminant a diagnostic can carry.
    ///
    /// A diagnostic is not a failure, so these are deliberately not
    /// [`InventoryError::KINDS`] and no spelling is shared with it: a surface
    /// grouping "what the walk noticed" must not be able to confuse one of these
    /// with a walk that failed.
    pub const KINDS: &'static [&'static str] = &[
        "re_inclusion_discarded",
        "invalid_rule",
        "case_collision",
        "unreadable_path",
        "vanished_path",
    ];

    /// The stable spelling of this diagnostic.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::ReInclusionDiscarded { .. } => "re_inclusion_discarded",
            Self::IgnoreRuleInvalid { .. } => "invalid_rule",
            Self::CaseCollision { .. } => "case_collision",
            Self::Unreadable { .. } => "unreadable_path",
            Self::Vanished { .. } => "vanished_path",
        }
    }
}

impl std::fmt::Display for InventoryDiagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReInclusionDiscarded {
                layer,
                file,
                line,
                pattern,
            } => write!(
                formatter,
                "{file}:{line}: the {layer} layer may only tighten, so the re-inclusion '{pattern}' was discarded"
            ),
            Self::IgnoreRuleInvalid {
                layer,
                file,
                line,
                pattern,
                reason,
            } => {
                write!(formatter, "{file}")?;
                if let Some(line) = line {
                    write!(formatter, ":{line}")?;
                }
                write!(formatter, ": invalid {layer} rule")?;
                if let Some(pattern) = pattern {
                    write!(formatter, " '{pattern}'")?;
                }
                write!(formatter, ": {reason}")
            }
            Self::CaseCollision { path, existing } => write!(
                formatter,
                "'{path}' and '{existing}' differ only by case and are both recorded"
            ),
            Self::Unreadable { path, reason } if path.is_empty() => {
                write!(formatter, "the worktree root is unreadable: {reason}")
            }
            Self::Unreadable { path, reason } => {
                write!(formatter, "'{path}' is unreadable: {reason}")
            }
            Self::Vanished { path } => {
                write!(formatter, "'{path}' disappeared while the walk was running")
            }
        }
    }
}

/// What the walk is allowed to read and how far it may go.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InventoryPolicy {
    global_ignore: Option<PathBuf>,
    repository_ignore: Option<PathBuf>,
    max_files: usize,
    max_walk: Duration,
}

impl Default for InventoryPolicy {
    fn default() -> Self {
        Self {
            global_ignore: None,
            repository_ignore: None,
            max_files: MAX_INVENTORY_FILES,
            max_walk: MAX_WALK_DURATION,
        }
    }
}

impl InventoryPolicy {
    /// The default policy: no configured rule files and the published bounds.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Names the global user ignore file.
    ///
    /// **Layer 2 is not discovered.** The repository layer defaults to
    /// [`REPOSITORY_IGNORE_FILE`] because that path is inside the worktree the
    /// walk was given; this crate holds no data directory, so a caller that does
    /// not name the global file gets no layer 2 at all. The conventional
    /// location is `<data_dir>/`[`GLOBAL_IGNORE_FILE`], and the engine facade is
    /// what joins them — one caller composing that path rather than each of them
    /// composing it differently.
    ///
    /// A configured file that does not exist contributes no rules; one that
    /// exists and cannot be read fails the walk, because a rule meant to
    /// exclude something must not be skipped quietly. A symlink here is followed
    /// — it is the user's own file and a dotfile manager linking it is ordinary
    /// — while the repository's is refused.
    #[must_use]
    pub fn with_global_ignore(mut self, path: impl Into<PathBuf>) -> Self {
        self.global_ignore = Some(path.into());
        self
    }

    /// Names the repository ignore file, overriding the conventional
    /// [`REPOSITORY_IGNORE_FILE`] location inside the worktree.
    #[must_use]
    pub fn with_repository_ignore(mut self, path: impl Into<PathBuf>) -> Self {
        self.repository_ignore = Some(path.into());
        self
    }

    /// Replaces the entry budget.
    #[must_use]
    pub fn with_max_files(mut self, max_files: usize) -> Self {
        self.max_files = max_files;
        self
    }

    /// Replaces the time budget.
    #[must_use]
    pub fn with_max_walk(mut self, max_walk: Duration) -> Self {
        self.max_walk = max_walk;
        self
    }

    /// The configured global user ignore file, if any.
    #[must_use]
    pub fn global_ignore(&self) -> Option<&Path> {
        self.global_ignore.as_deref()
    }

    /// The configured repository ignore file, if any.
    #[must_use]
    pub fn repository_ignore(&self) -> Option<&Path> {
        self.repository_ignore.as_deref()
    }

    /// The entry budget.
    #[must_use]
    pub const fn max_files(&self) -> usize {
        self.max_files
    }

    /// The time budget.
    #[must_use]
    pub const fn max_walk(&self) -> Duration {
        self.max_walk
    }
}

/// One worktree's eligible-file inventory.
///
/// Built only by [`InventoryBuilder::build`], so the guarantee that a denied
/// path never appears in one is a property of this module rather than of every
/// caller that constructs a value.
#[derive(Clone, Debug)]
pub struct FileInventory {
    snapshot: SnapshotId,
    worktree_root: PathBuf,
    entries: Vec<InventoryEntry>,
    denied_count: u64,
    ignored_count: u64,
    truncation: Option<InventoryTruncation>,
    diagnostics: Vec<InventoryDiagnostic>,
    dropped_diagnostics: u64,
    duration: Duration,
    classify_version: u32,
}

impl FileInventory {
    /// The capture this inventory was built for.
    ///
    /// Not a freshness claim. A walk reads the live filesystem and verifies
    /// nothing, so a workspace that moved between the capture and the walk
    /// yields an inventory carrying an id whose snapshot no longer describes the
    /// tree. Establishing that is the caller's, through
    /// [`WorkspaceSnapshot::verify`] before and after — the reconciliation that
    /// makes it automatic is [#115]'s.
    ///
    /// [#115]: https://github.com/fullstacktaiye/harkness/issues/115
    #[must_use]
    pub const fn snapshot(&self) -> SnapshotId {
        self.snapshot
    }

    /// The canonical worktree root the walk started from.
    #[must_use]
    pub fn worktree_root(&self) -> &Path {
        &self.worktree_root
    }

    /// Every recorded path, sorted by exact path bytes.
    #[must_use]
    pub fn entries(&self) -> &[InventoryEntry] {
        &self.entries
    }

    /// How many paths a built-in denial excluded.
    ///
    /// A count and never a path: the whole point of the denial layer is that
    /// nothing downstream — an entry, an event, a log line — learns the name of
    /// a credential file. A denied *directory* counts once and its contents are
    /// never visited, so this is a count of rules applied rather than of files
    /// that exist.
    #[must_use]
    pub const fn denied_count(&self) -> u64 {
        self.denied_count
    }

    /// How many paths an ignore rule excluded, on any of the three
    /// configurable layers.
    #[must_use]
    pub const fn ignored_count(&self) -> u64 {
        self.ignored_count
    }

    /// Why the walk stopped early, if it did.
    #[must_use]
    pub const fn truncation(&self) -> Option<InventoryTruncation> {
        self.truncation
    }

    /// Whether this inventory is a partial answer.
    #[must_use]
    pub const fn is_truncated(&self) -> bool {
        self.truncation.is_some()
    }

    /// What the walk noticed but did not fail on.
    #[must_use]
    pub fn diagnostics(&self) -> &[InventoryDiagnostic] {
        &self.diagnostics
    }

    /// How many diagnostics were discarded past
    /// [`MAX_INVENTORY_DIAGNOSTICS`].
    #[must_use]
    pub const fn dropped_diagnostics(&self) -> u64 {
        self.dropped_diagnostics
    }

    /// How long the walk took, for the event the engine records.
    #[must_use]
    pub const fn duration(&self) -> Duration {
        self.duration
    }

    /// The [`CLASSIFY_VERSION`] the entries were classified under.
    #[must_use]
    pub const fn classify_version(&self) -> u32 {
        self.classify_version
    }

    /// How many entries may have their content indexed.
    #[must_use]
    pub fn eligible_count(&self) -> usize {
        self.entries.iter().filter(|entry| entry.eligible()).count()
    }
}

/// Failures raised while building an inventory.
///
/// Follows the `GitError::KINDS` convention: every variant has a stable
/// discriminant in [`InventoryError::KINDS`], and no kind here collides with
/// [`ContextDomainError::KINDS`](crate::ContextDomainError::KINDS).
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum InventoryError {
    /// The worktree root could not be addressed.
    #[error("the worktree root '{}' is unavailable: {reason}", path.display())]
    RootUnavailable {
        /// The root the walk was addressed to.
        path: PathBuf,
        /// Stable human-readable explanation.
        reason: String,
    },

    /// The worktree root exists but is not a directory.
    #[error("the worktree root '{}' is not a directory", path.display())]
    NotADirectory {
        /// The root the walk was addressed to.
        path: PathBuf,
    },

    /// The root directory itself could not be listed.
    ///
    /// A directory *below* the root that cannot be listed is a diagnostic
    /// instead: one unreadable branch must not cost a whole inventory.
    #[error("failed to walk '{}': {reason}", path.display())]
    WalkFailed {
        /// The directory that could not be listed.
        path: PathBuf,
        /// Stable human-readable explanation.
        reason: String,
    },

    /// A configured ignore file could not be read or compiled at all.
    ///
    /// One malformed *pattern* is an [`InventoryDiagnostic`] and the rest of
    /// the file still applies. This is the other case: a rule file that exists
    /// and cannot be applied fails the walk rather than being skipped, because
    /// skipping it would silently widen what Harkness reads.
    #[error("the {layer} ignore file '{file}' is invalid: {reason}")]
    IgnoreRuleInvalid {
        /// Which layer the file belongs to.
        layer: IgnoreLayer,
        /// The file, in lossy display form.
        file: String,
        /// Stable human-readable explanation.
        reason: String,
    },

    /// The walk observed its cancellation token.
    #[error("the inventory walk was cancelled")]
    Cancelled,
}

impl InventoryError {
    /// Every stable discriminant this error namespace can emit.
    pub const KINDS: &'static [&'static str] = &[
        "root_unavailable",
        "not_a_directory",
        "walk_failed",
        "ignore_rule_invalid",
        "cancelled",
    ];

    /// Stable machine-readable discriminant for caller-facing error handling.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::RootUnavailable { .. } => "root_unavailable",
            Self::NotADirectory { .. } => "not_a_directory",
            Self::WalkFailed { .. } => "walk_failed",
            Self::IgnoreRuleInvalid { .. } => "ignore_rule_invalid",
            Self::Cancelled => "cancelled",
        }
    }
}

/// The entry point that turns a captured workspace into an inventory.
///
/// The walk root is the snapshot's own [`worktree_root`], which is canonical
/// because capture made it so. There is deliberately no entry point taking a
/// bare path: a caller cannot direct the walk at an arbitrary directory, and
/// the containment question is answered once, where the workspace was read.
///
/// # The exclusion hierarchy
///
/// Four layers are consulted for every path, strictly in this order, and the
/// first layer with an opinion decides:
///
/// | # | Layer | May |
/// | --- | --- | --- |
/// | 1 | [`BUILT_IN_DENIALS`] | exclude only, and no later layer may undo it |
/// | 2 | the global user ignore file ([`GLOBAL_IGNORE_FILE`]) | exclude, or explicitly re-include |
/// | 3 | the repository ignore file ([`REPOSITORY_IGNORE_FILE`]) | exclude only |
/// | 4 | the repository's `.gitignore` chain, deepest first | exclude, or explicitly re-include |
///
/// Layer 4 is the `.gitignore` files inside the worktree and nothing else.
/// `.git/info/exclude` lives in the repository's common directory, which a crate
/// addressed purely by path cannot resolve, and Git's machine-level
/// `core.excludesFile` would make one repository's context differ between two
/// machines; layer 2 is where a person's own preferences belong.
///
/// An explicit re-inclusion stops the descent, which is what makes the order
/// meaningful in both directions: a user's own `!keep.log` outranks a
/// repository's `.gitignore`, and *nothing* outranks layer 1. Repository
/// content can therefore narrow what Harkness reads and can never widen it,
/// which is ADR-0006's tightening-only rule applied to the walk.
///
/// One caveat travels with the hierarchy, and it is Git's own: a directory an
/// earlier layer excluded is never descended into, so a re-inclusion naming a
/// path *inside* it has nothing to act on.
///
/// # What a walk produces
///
/// An [`InventoryEntry`] per recorded path, sorted by exact path bytes. A file
/// excluded by a *rule* is counted rather than listed — the rules already name
/// it — while a file excluded by what it *is* (binary, oversized,
/// secret-sensitive, undecodable) is listed with that class and
/// [`InventoryEntry::eligible`] false, because "why is this file not in my
/// context" is a question users ask about exactly those. Symlinks are recorded
/// and never followed; a directory holding its own `.git` is recorded as a
/// [`Boundary`] and never descended into.
///
/// [`worktree_root`]: WorkspaceSnapshot::worktree_root
#[derive(Clone, Copy, Debug)]
pub struct InventoryBuilder;

impl InventoryBuilder {
    /// Walks one worktree and classifies what it finds.
    ///
    /// Blocking. `cancellation` is polled before every directory entry, so a
    /// cancelled walk returns well inside the workspace's 250 ms visibility
    /// target whatever the size of the tree.
    pub fn build(
        snapshot: &WorkspaceSnapshot,
        policy: &InventoryPolicy,
        cancellation: &Cancellation,
    ) -> Result<FileInventory, InventoryError> {
        Walk::new(
            snapshot.id(),
            snapshot.worktree_root(),
            policy,
            cancellation,
        )?
        .run()
    }
}

/// One directory being walked, and the `.gitignore` that applies inside it.
struct Frame {
    absolute: PathBuf,
    relative: Vec<u8>,
    entries: std::vec::IntoIter<Listed>,
    gitignore: Option<Gitignore>,
}

/// One name a directory listing produced.
struct Listed {
    name: OsString,
    kind: EntryKind,
}

/// What a listing said an entry is, from metadata that never follows a link.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// One rule file to read, and the terms it is read on.
///
/// The three configurable layers differ in four ways and in no others, so they
/// share one reader: what a diagnostic calls them, what their patterns are
/// anchored to, whether a negation is honored, and whether the path may be a
/// symlink. Two implementations of "read a gitignore file" would drift, and the
/// half that drifted last time was the half that reported nothing.
#[derive(Clone, Copy, Debug)]
struct RuleFile<'a> {
    file: &'a Path,
    root: &'a Path,
    layer: IgnoreLayer,
    allow_negation: bool,
    allow_symlink: bool,
}

impl<'a> RuleFile<'a> {
    /// The user's own file: negations honored, a symlink followed.
    const fn global(file: &'a Path, root: &'a Path) -> Self {
        Self {
            file,
            root,
            layer: IgnoreLayer::Global,
            allow_negation: true,
            allow_symlink: true,
        }
    }

    /// The repository's own file: tightening only, and never a symlink.
    const fn repository(file: &'a Path, root: &'a Path) -> Self {
        Self {
            file,
            root,
            layer: IgnoreLayer::Repository,
            allow_negation: false,
            allow_symlink: false,
        }
    }

    /// One `.gitignore`, anchored at its own directory, read on Git's terms.
    const fn gitignore(file: &'a Path, directory: &'a Path) -> Self {
        Self {
            file,
            root: directory,
            layer: IgnoreLayer::GitIgnore,
            allow_negation: true,
            allow_symlink: false,
        }
    }
}

/// What the layered hierarchy decided about one path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Decision {
    /// A built-in denial matched: count it and record nothing.
    Denied,
    /// A configurable layer excluded it: count it and record nothing.
    Excluded,
    /// A layer explicitly re-included it, so lower layers are not consulted.
    Included,
    /// No layer had an opinion.
    Undecided,
}

/// The mutable state of one walk.
struct Walk<'a> {
    snapshot: SnapshotId,
    root: PathBuf,
    policy: &'a InventoryPolicy,
    cancellation: &'a Cancellation,
    denials: Gitignore,
    global: Option<Gitignore>,
    repository: Option<Gitignore>,
    submodules: BTreeSet<Vec<u8>>,
    entries: Vec<InventoryEntry>,
    denied_count: u64,
    ignored_count: u64,
    diagnostics: Vec<InventoryDiagnostic>,
    dropped_diagnostics: u64,
    truncation: Option<InventoryTruncation>,
    started: Instant,
    window: Vec<u8>,
}

impl<'a> Walk<'a> {
    fn new(
        snapshot: SnapshotId,
        root: &Path,
        policy: &'a InventoryPolicy,
        cancellation: &'a Cancellation,
    ) -> Result<Self, InventoryError> {
        let started = Instant::now();
        let metadata =
            fs::symlink_metadata(root).map_err(|error| InventoryError::RootUnavailable {
                path: root.to_path_buf(),
                reason: error.to_string(),
            })?;
        if !metadata.is_dir() {
            return Err(InventoryError::NotADirectory {
                path: root.to_path_buf(),
            });
        }

        let mut walk = Self {
            snapshot,
            root: root.to_path_buf(),
            policy,
            cancellation,
            denials: Gitignore::empty(),
            global: None,
            repository: None,
            submodules: declared_submodules(root),
            entries: Vec::new(),
            denied_count: 0,
            ignored_count: 0,
            diagnostics: Vec::new(),
            dropped_diagnostics: 0,
            truncation: None,
            started,
            window: Vec::with_capacity(BINARY_SNIFF_BYTES),
        };

        let root = root.to_path_buf();
        walk.denials = walk.compile_denials()?;
        walk.global = match policy.global_ignore() {
            Some(file) => walk.load_ignore_file(&RuleFile::global(file, &root))?,
            None => None,
        };
        let repository_ignore = policy
            .repository_ignore()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.join(REPOSITORY_IGNORE_FILE));
        walk.repository =
            walk.load_ignore_file(&RuleFile::repository(&repository_ignore, &root))?;
        Ok(walk)
    }

    /// Compiles the non-overridable denial layer, rooted at the worktree.
    fn compile_denials(&self) -> Result<Gitignore, InventoryError> {
        let mut builder = GitignoreBuilder::new(&self.root);
        for pattern in BUILT_IN_DENIALS {
            builder
                .add_line(None, pattern)
                .map_err(|error| InventoryError::IgnoreRuleInvalid {
                    layer: IgnoreLayer::BuiltIn,
                    file: "<built-in>".to_owned(),
                    reason: clamp(&error.to_string()),
                })?;
        }
        builder
            .build()
            .map_err(|error| InventoryError::IgnoreRuleInvalid {
                layer: IgnoreLayer::BuiltIn,
                file: "<built-in>".to_owned(),
                reason: clamp(&error.to_string()),
            })
    }

    /// Reads one rule file.
    ///
    /// Missing is not an error; unreadable, oversized, or undecodable is — for
    /// the caller to interpret, since the two Harkness-owned layers fail the
    /// walk and the `.gitignore` chain is reported and skipped. A layer that may
    /// only tighten discards its negations line by line and reports each one, so
    /// a repository learns that its re-inclusion had no effect instead of
    /// assuming it worked.
    fn load_ignore_file(
        &mut self,
        rule: &RuleFile<'_>,
    ) -> Result<Option<Gitignore>, InventoryError> {
        let RuleFile {
            file,
            root,
            layer,
            allow_negation,
            allow_symlink,
        } = *rule;
        let display = clamp(&file.to_string_lossy());
        let invalid = |reason: String| InventoryError::IgnoreRuleInvalid {
            layer,
            file: display.clone(),
            reason,
        };

        // `symlink_metadata`, not `metadata`: a repository writes its own rule
        // file, and a committed symlink is how it would aim this reader at
        // `~/.ssh/id_rsa` and read the target back out through a diagnostic
        // quoting the "pattern" it could not compile. The user's own global file
        // is followed, because a dotfile manager symlinking it is ordinary and
        // the path came from the user rather than from a repository.
        let metadata = match fs::symlink_metadata(file) {
            Ok(metadata) if metadata.is_symlink() && !allow_symlink => {
                return Err(invalid(
                    "a repository rule file may not be a symlink".to_owned(),
                ));
            }
            Ok(metadata) if metadata.is_symlink() => match fs::metadata(file) {
                Ok(followed) => followed,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(invalid(clamp(&error.to_string()))),
            },
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(invalid(clamp(&error.to_string()))),
        };
        if !metadata.is_file() {
            return Err(invalid("not a regular file".to_owned()));
        }

        // Bounded by what is *read*, not by what a stat claims. A stat is a
        // promise about a file that may already have grown, and procfs reports
        // zero for content that is not, so a size check alone bounds nothing.
        let text = match read_bounded(file, MAX_IGNORE_FILE_BYTES) {
            Ok(Some(text)) => text,
            Ok(None) => {
                return Err(invalid(format!(
                    "larger than the {MAX_IGNORE_FILE_BYTES} byte limit"
                )));
            }
            Err(error) => return Err(invalid(clamp(&error.to_string()))),
        };

        let mut builder = GitignoreBuilder::new(root);
        for (index, line) in text.lines().enumerate() {
            let line_number = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
            // An editor's byte-order mark belongs to the file, not to the first
            // pattern. Leaving it in compiles `\u{feff}notes.md`, which matches
            // nothing — a tightening rule that silently stopped applying.
            let line = if index == 0 {
                line.strip_prefix('\u{feff}').unwrap_or(line)
            } else {
                line
            };
            if !allow_negation && line.starts_with('!') {
                self.diagnose(InventoryDiagnostic::ReInclusionDiscarded {
                    layer,
                    file: display.clone(),
                    line: line_number,
                    pattern: clamp(line),
                });
                continue;
            }
            if let Err(error) = builder.add_line(None, line) {
                self.diagnose(InventoryDiagnostic::IgnoreRuleInvalid {
                    layer,
                    file: display.clone(),
                    line: Some(line_number),
                    pattern: Some(clamp(line)),
                    reason: clamp(&error.to_string()),
                });
            }
        }
        builder
            .build()
            .map(Some)
            .map_err(|error| invalid(clamp(&error.to_string())))
    }

    /// Walks the tree and assembles the inventory.
    fn run(mut self) -> Result<FileInventory, InventoryError> {
        let root = self.root.clone();
        let listing =
            self.read_directory(&root, None)
                .map_err(|error| InventoryError::WalkFailed {
                    path: root.clone(),
                    reason: error.to_string(),
                })?;
        let gitignore = self.gitignore_for(&root, &listing);
        let mut stack = vec![Frame {
            absolute: root,
            relative: Vec::new(),
            gitignore,
            entries: listing.into_iter(),
        }];

        'walk: loop {
            if self.cancellation.is_cancelled() {
                return Err(InventoryError::Cancelled);
            }
            if self.started.elapsed() >= self.policy.max_walk {
                self.truncation = Some(InventoryTruncation::WalkTimeExhausted {
                    limit: self.policy.max_walk,
                });
                break 'walk;
            }

            let next = {
                let Some(frame) = stack.last_mut() else {
                    break 'walk;
                };
                frame.entries.next().map(|listed| {
                    let absolute = frame.absolute.join(&listed.name);
                    let relative = join_relative(&frame.relative, &listed.name);
                    (listed, absolute, relative)
                })
            };
            let Some((listed, absolute, relative)) = next else {
                stack.pop();
                continue 'walk;
            };

            // The repository's own administrative directory is not content, at
            // the root or anywhere else. It is skipped rather than counted:
            // nothing excluded it, it is simply not part of a worktree.
            if listed.name == ".git" {
                continue 'walk;
            }

            match self.decide(&stack, &absolute, listed.kind) {
                Decision::Denied => {
                    self.denied_count = self.denied_count.saturating_add(1);
                    continue 'walk;
                }
                Decision::Excluded => {
                    self.ignored_count = self.ignored_count.saturating_add(1);
                    continue 'walk;
                }
                Decision::Included | Decision::Undecided => {}
            }

            match listed.kind {
                EntryKind::Symlink => {
                    if !self.record_symlink(&absolute, RepoPath::from_bytes(relative)) {
                        break 'walk;
                    }
                }
                EntryKind::File => {
                    if !self.record_file(&absolute, RepoPath::from_bytes(relative)) {
                        break 'walk;
                    }
                }
                EntryKind::Directory => {
                    let path = RepoPath::from_bytes(relative.clone());
                    let listing = match self.read_directory(&absolute, Some(&path)) {
                        Ok(listing) => listing,
                        Err(error) => {
                            self.diagnose(InventoryDiagnostic::Unreadable {
                                path,
                                reason: clamp(&error.to_string()),
                            });
                            continue 'walk;
                        }
                    };
                    // A directory holding its own `.git` belongs to another
                    // repository. It is recorded as a boundary and the walk
                    // stops there, so no cross-repository content can enter.
                    if listing.iter().any(|entry| entry.name == ".git") {
                        if !self.record_boundary(&absolute, path, &relative) {
                            break 'walk;
                        }
                        continue 'walk;
                    }
                    let gitignore = self.gitignore_for(&absolute, &listing);
                    stack.push(Frame {
                        absolute,
                        relative,
                        gitignore,
                        entries: listing.into_iter(),
                    });
                }
                // A FIFO, socket, or device is not content and is never opened:
                // `open(2)` on one can block forever, which is the reason the
                // workspace probe refuses them too.
                EntryKind::Other => {}
            }
        }

        Ok(self.finish())
    }

    /// Applies the four layers in order, first opinion wins.
    fn decide(&self, stack: &[Frame], absolute: &Path, kind: EntryKind) -> Decision {
        let is_dir = kind == EntryKind::Directory;
        // Layer 1 consults every parent as well as the path itself, so nothing
        // beneath a denied directory can be recorded even if pruning failed.
        //
        // A symlink is matched as *both*, because a denial written for a
        // directory — `**/.config/gcloud/` — does not fire for one and the walk
        // will not follow the link to find out what it points at. Answering
        // "file" there would let a link standing where a credential directory
        // belongs be recorded under its own name, and layer 1's whole contract
        // is that a denied path is a count and never a name. Layers 2 to 4 keep
        // Git's answer, where a symlink is not a directory.
        let denied = self
            .denials
            .matched_path_or_any_parents(absolute, is_dir)
            .is_ignore()
            || (kind == EntryKind::Symlink
                && self
                    .denials
                    .matched_path_or_any_parents(absolute, true)
                    .is_ignore());
        if denied {
            return Decision::Denied;
        }
        for layer in [self.global.as_ref(), self.repository.as_ref()]
            .into_iter()
            .flatten()
        {
            match layer.matched(absolute, is_dir) {
                Match::Ignore(_) => return Decision::Excluded,
                Match::Whitelist(_) => return Decision::Included,
                Match::None => {}
            }
        }
        // Deepest `.gitignore` first, which is how Git resolves its own chain.
        for frame in stack.iter().rev() {
            let Some(gitignore) = frame.gitignore.as_ref() else {
                continue;
            };
            match gitignore.matched(absolute, is_dir) {
                Match::Ignore(_) => return Decision::Excluded,
                Match::Whitelist(_) => return Decision::Included,
                Match::None => {}
            }
        }
        Decision::Undecided
    }

    /// Lists one directory, sorted by name.
    ///
    /// Failing to open the directory is the caller's to interpret: at the root
    /// it fails the walk, and anywhere else it is a diagnostic, so one
    /// unreadable branch never costs the rest of the tree. A single entry that
    /// cannot be typed is always a diagnostic.
    fn read_directory(
        &mut self,
        absolute: &Path,
        path: Option<&RepoPath>,
    ) -> std::io::Result<Vec<Listed>> {
        let reading = fs::read_dir(absolute)?;
        let mut listed = Vec::new();
        for entry in reading {
            // Polled here as well as in the walk loop, because one directory can
            // hold millions of names: a listing that ran to completion before
            // anything was checked would make "cancelled within 250 ms" a claim
            // about tree shape. Stopping mid-listing leaves the outer loop to
            // report the cancellation, or the time budget to report truncation.
            if self.cancellation.is_cancelled() || self.started.elapsed() >= self.policy.max_walk {
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    self.unreadable(absolute, path, &error);
                    continue;
                }
            };
            // `file_type` on a directory entry never follows a link, which is
            // the traversal guarantee the whole walk rests on.
            let kind = match entry.file_type() {
                Ok(kind) if kind.is_symlink() => EntryKind::Symlink,
                Ok(kind) if kind.is_dir() => EntryKind::Directory,
                Ok(kind) if kind.is_file() => EntryKind::File,
                Ok(_) => EntryKind::Other,
                Err(error) => {
                    self.unreadable(&entry.path(), None, &error);
                    continue;
                }
            };
            listed.push(Listed {
                name: entry.file_name(),
                kind,
            });
        }
        // `OsString` orders by its encoded bytes on every platform, which is
        // what makes a truncated walk truncate at the same place twice.
        listed.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(listed)
    }

    /// Compiles the `.gitignore` a directory holds, if it holds one.
    ///
    /// Read through the same loader as the two layers above it, so a malformed
    /// pattern names its line here too. Its failures are diagnostics rather than
    /// errors: a `.gitignore` can only ever exclude, and by the time layer 4 is
    /// read every layer that can *deny* has already spoken, so a rule this build
    /// cannot apply costs coverage rather than safety.
    fn gitignore_for(&mut self, absolute: &Path, listing: &[Listed]) -> Option<Gitignore> {
        if !listing
            .iter()
            .any(|entry| entry.name == ".gitignore" && entry.kind == EntryKind::File)
        {
            return None;
        }
        let file = absolute.join(".gitignore");
        match self.load_ignore_file(&RuleFile::gitignore(&file, absolute)) {
            Ok(gitignore) => gitignore,
            Err(InventoryError::IgnoreRuleInvalid {
                layer,
                file,
                reason,
            }) => {
                self.diagnose(InventoryDiagnostic::IgnoreRuleInvalid {
                    layer,
                    file,
                    line: None,
                    pattern: None,
                    reason,
                });
                None
            }
            Err(_) => None,
        }
    }

    /// Records a symlink: never followed, never read, never eligible.
    fn record_symlink(&mut self, absolute: &Path, path: RepoPath) -> bool {
        match fs::symlink_metadata(absolute) {
            // The listing said link and this stat disagrees, so the path was
            // replaced. Recording it as the regular file it now is keeps the
            // entry describing what the walk actually saw last.
            Ok(metadata) if !metadata.is_symlink() && metadata.is_file() => {
                self.record_file(absolute, path)
            }
            Ok(metadata) => self.record_link(&metadata, path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.diagnose(InventoryDiagnostic::Vanished { path });
                true
            }
            Err(error) => {
                self.diagnose(InventoryDiagnostic::Unreadable {
                    path: path.clone(),
                    reason: clamp(&error.to_string()),
                });
                let class = FileSample::new(&path, 0).classify();
                self.push(InventoryEntry {
                    path,
                    byte_size: 0,
                    mtime_ns: None,
                    class,
                    symlink: true,
                    boundary: None,
                    unreadable: true,
                })
            }
        }
    }

    /// Records one entry for a path a fresh stat says is a symlink.
    ///
    /// Classified from its name and its own size — never from its target, which
    /// is what following it would mean.
    fn record_link(&mut self, metadata: &Metadata, path: RepoPath) -> bool {
        let byte_size = metadata.len();
        let class = FileSample::new(&path, byte_size).classify();
        self.push(InventoryEntry {
            path,
            byte_size,
            mtime_ns: modified_nanos(metadata),
            class,
            symlink: true,
            boundary: None,
            unreadable: false,
        })
    }

    /// Records a directory the walk refused to descend into.
    fn record_boundary(&mut self, absolute: &Path, path: RepoPath, relative: &[u8]) -> bool {
        let metadata = fs::symlink_metadata(absolute).ok();
        let boundary = if self.submodules.contains(relative) {
            Boundary::Submodule
        } else {
            Boundary::NestedRepository
        };
        let class = FileSample::new(&path, 0).classify();
        self.push(InventoryEntry {
            path,
            byte_size: 0,
            mtime_ns: metadata.as_ref().and_then(modified_nanos),
            class,
            symlink: false,
            boundary: Some(boundary),
            unreadable: metadata.is_none(),
        })
    }

    /// Records a regular file, reading at most the sniff window.
    fn record_file(&mut self, absolute: &Path, path: RepoPath) -> bool {
        let metadata = match fs::symlink_metadata(absolute) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Listed and then deleted. Not an error: a walk describes what
                // was there, and a path that is gone was never there for it.
                self.diagnose(InventoryDiagnostic::Vanished { path });
                return true;
            }
            Err(error) => {
                self.diagnose(InventoryDiagnostic::Unreadable {
                    path: path.clone(),
                    reason: clamp(&error.to_string()),
                });
                let class = FileSample::new(&path, 0).classify();
                return self.push(InventoryEntry {
                    path,
                    byte_size: 0,
                    mtime_ns: None,
                    class,
                    symlink: false,
                    boundary: None,
                    unreadable: true,
                });
            }
        };
        // The listing said regular file; this stat is what decides. A path
        // replaced by a symlink in between must be recorded as the link it now
        // is and never opened, because `File::open` *does* follow one and eight
        // kilobytes of `/etc/passwd` would otherwise reach a classification.
        if metadata.is_symlink() {
            return self.record_link(&metadata, path);
        }
        let byte_size = metadata.len();
        let mtime_ns = modified_nanos(&metadata);

        // A name the classifier already refuses is never opened. That is the
        // difference between recording that a file looks credential-bearing and
        // reading one to find out.
        let (class, unreadable) = if FileSample::new(&path, byte_size).is_secret_by_name() {
            (FileClass::SecretSensitive, false)
        } else {
            match self.sniff(absolute) {
                Ok(()) => (
                    FileSample::new(&path, byte_size)
                        .with_window(&self.window)
                        .classify(),
                    false,
                ),
                Err(error) => {
                    self.diagnose(InventoryDiagnostic::Unreadable {
                        path: path.clone(),
                        reason: clamp(&error.to_string()),
                    });
                    (FileSample::new(&path, byte_size).classify(), true)
                }
            }
        };

        self.push(InventoryEntry {
            path,
            byte_size,
            mtime_ns,
            class,
            symlink: false,
            boundary: None,
            unreadable,
        })
    }

    /// Reads at most [`BINARY_SNIFF_BYTES`] into the reused window.
    fn sniff(&mut self, absolute: &Path) -> std::io::Result<()> {
        self.window.clear();
        let file = fs::File::open(absolute)?;
        // Re-checked on the open handle: the listing said regular file, and an
        // entry swapped for a directory or a device in between must not be read
        // as one. A swap to a FIFO can still block the open itself, which is the
        // same residual `FilesystemProbe` documents.
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        file.take(u64::try_from(BINARY_SNIFF_BYTES).unwrap_or(u64::MAX))
            .read_to_end(&mut self.window)?;
        Ok(())
    }

    /// Adds an entry, or reports the budget as exhausted.
    ///
    /// Returns whether the walk may continue.
    fn push(&mut self, entry: InventoryEntry) -> bool {
        if self.entries.len() >= self.policy.max_files {
            self.truncation = Some(InventoryTruncation::FileBudgetExhausted {
                limit: self.policy.max_files,
            });
            return false;
        }
        self.entries.push(entry);
        true
    }

    /// Records a diagnostic, or counts it as dropped.
    fn diagnose(&mut self, diagnostic: InventoryDiagnostic) {
        if self.diagnostics.len() >= MAX_INVENTORY_DIAGNOSTICS {
            self.dropped_diagnostics = self.dropped_diagnostics.saturating_add(1);
            return;
        }
        self.diagnostics.push(diagnostic);
    }

    /// Reports a path the walk could not read, relative to the root where it
    /// can be.
    fn unreadable(&mut self, absolute: &Path, path: Option<&RepoPath>, error: &std::io::Error) {
        let path = path.cloned().unwrap_or_else(|| {
            RepoPath::from_path(absolute.strip_prefix(&self.root).unwrap_or(absolute))
        });
        self.diagnose(InventoryDiagnostic::Unreadable {
            path,
            reason: clamp(&error.to_string()),
        });
    }

    /// Sorts, flags case collisions, and seals the inventory.
    fn finish(mut self) -> FileInventory {
        self.entries
            .sort_by(|left, right| left.path.cmp(&right.path));
        // Sorted by fold, so a collision is an adjacent pair rather than a map
        // holding two owned copies of every path in the walk. Diagnostics are
        // built one at a time, so the bound is applied before the memory is
        // spent rather than after.
        let mut folded = (0..self.entries.len())
            .map(|index| (case_fold(self.entries[index].path.as_bytes()), index))
            .collect::<Vec<_>>();
        folded.sort_unstable();
        let collisions = folded
            .windows(2)
            .filter(|pair| pair[0].0 == pair[1].0)
            .map(|pair| (pair[0].1, pair[1].1))
            .collect::<Vec<_>>();
        drop(folded);
        for (existing, found) in collisions {
            self.diagnose(InventoryDiagnostic::CaseCollision {
                path: self.entries[found].path.clone(),
                existing: self.entries[existing].path.clone(),
            });
        }

        FileInventory {
            snapshot: self.snapshot,
            worktree_root: self.root,
            entries: self.entries,
            denied_count: self.denied_count,
            ignored_count: self.ignored_count,
            truncation: self.truncation,
            diagnostics: self.diagnostics,
            dropped_diagnostics: self.dropped_diagnostics,
            duration: self.started.elapsed(),
            classify_version: CLASSIFY_VERSION,
        }
    }
}

/// Reads a file's text, refusing anything past `limit` rather than trusting a
/// stat.
///
/// Returns `Ok(None)` when the content exceeds the bound. Reading one byte past
/// it is what makes the refusal describe the bytes rather than the metadata: a
/// file can grow between a stat and a read, and procfs reports zero for content
/// that is not.
fn read_bounded(file: &Path, limit: u64) -> std::io::Result<Option<String>> {
    let mut bytes = Vec::new();
    fs::File::open(file)?
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Ok(None);
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// The declared submodule paths of a worktree, best effort.
///
/// `.gitmodules` is repository content and this read is advisory: it decides
/// only which of two boundary spellings a directory gets, never whether the walk
/// descends. A missing, oversized, or unparseable file simply declares nothing.
fn declared_submodules(root: &Path) -> BTreeSet<Vec<u8>> {
    const MAX_GITMODULES_BYTES: u64 = 64 * 1024;

    let file = root.join(".gitmodules");
    let mut declared = BTreeSet::new();
    let Ok(metadata) = fs::metadata(&file) else {
        return declared;
    };
    if !metadata.is_file() || metadata.len() > MAX_GITMODULES_BYTES {
        return declared;
    }
    let Ok(text) = fs::read_to_string(&file) else {
        return declared;
    };
    let mut in_submodule = false;
    for line in text.lines() {
        let line = line.trim();
        if let Some(section) = line.strip_prefix('[') {
            // Section-aware, because `path` is an ordinary key: a `[core]`
            // section carrying one would otherwise declare a phantom submodule
            // and relabel somebody's nested repository.
            in_submodule = section.trim_start().starts_with("submodule");
            continue;
        }
        if !in_submodule {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "path" {
            continue;
        }
        let value = value.trim().trim_matches('"').trim_end_matches('/');
        if !value.is_empty() {
            declared.insert(value.as_bytes().to_vec());
        }
    }
    declared
}

/// Joins a directory's relative path with one entry name, in Git's spelling.
///
/// Byte-exact on Unix, where a file name is any byte sequence, and through the
/// UTF-8 form elsewhere, which is lossless for every name Git can report there.
fn join_relative(parent: &[u8], name: &OsStr) -> Vec<u8> {
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
        joined.extend_from_slice(name.to_string_lossy().replace('\\', "/").as_bytes());
    }
    joined
}

/// Modification time in nanoseconds since the Unix epoch.
fn modified_nanos(metadata: &Metadata) -> Option<i64> {
    let modified = metadata.modified().ok()?;
    match modified.duration_since(UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_nanos()).ok(),
        Err(before) => i64::try_from(before.duration().as_nanos())
            .ok()
            .and_then(i64::checked_neg),
    }
}

/// Folds a path for the case-collision check.
///
/// Simple Unicode lowercasing where the bytes are UTF-8, ASCII folding where
/// they are not. This decides only whether to *flag* two entries; both are kept
/// under their exact bytes either way, so a fold that is too eager costs a
/// diagnostic and never an entry.
fn case_fold(path: &[u8]) -> Vec<u8> {
    match std::str::from_utf8(path) {
        Ok(text) => text.to_lowercase().into_bytes(),
        Err(_) => path.to_ascii_lowercase(),
    }
}

/// Bounds text a diagnostic quotes, marking that it was cut.
///
/// The ellipsis is the difference from `provenance`'s bound: a diagnostic is
/// read by a person deciding whether a pattern is the one they wrote, and a
/// silent truncation there reads as a pattern that ends where it does not.
fn clamp(text: &str) -> String {
    if text.len() <= MAX_DIAGNOSTIC_TEXT_BYTES {
        return text.to_owned();
    }
    format!(
        "{}…",
        &text[..floor_char_boundary(text, MAX_DIAGNOSTIC_TEXT_BYTES)]
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use harkness_core::ProjectId;
    use harkness_git::{Cancellation, GitService};
    use harkness_test_fixtures::{Fixture, initialize_repository};

    use super::{
        BUILT_IN_DENIALS, Boundary, FileInventory, IgnoreLayer, InventoryBuilder,
        InventoryDiagnostic, InventoryError, InventoryPolicy, InventoryTruncation,
        MAX_DIAGNOSTIC_TEXT_BYTES, MAX_IGNORE_FILE_BYTES, MAX_INVENTORY_DIAGNOSTICS,
        REPOSITORY_IGNORE_FILE,
    };
    use crate::classify::{FileClass, OVERSIZED_FILE_THRESHOLD};
    use crate::snapshot::{CaptureRequest, WorkspaceSnapshot};
    use crate::{ContextDomainError, FilesystemProbe, RepoPath};

    /// A hermetic worktree with a captured snapshot to walk.
    struct Workspace {
        fixture: Fixture,
        root: PathBuf,
    }

    impl Workspace {
        fn new(name: &str) -> Self {
            let fixture = Fixture::new();
            let root = fixture.directory(name);
            initialize_repository(&root);
            // The fixture commits one file of its own. Removing it from the
            // working tree keeps every assertion below about the files its own
            // test wrote — and leaves one committed path that no longer exists
            // on disk, which is what a sparse checkout looks like from here.
            fs::remove_file(root.join("tracked.txt")).unwrap();
            Self { fixture, root }
        }

        fn write(&self, relative: &str, content: impl AsRef<[u8]>) -> PathBuf {
            let path = self.root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, content).unwrap();
            path
        }

        fn directory(&self, relative: &str) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(&path).unwrap();
            path
        }

        fn snapshot(&self) -> WorkspaceSnapshot {
            let probe = FilesystemProbe::new(&self.root);
            WorkspaceSnapshot::capture(
                &CaptureRequest::new(ProjectId::new()),
                &GitService::new(&self.root, &self.fixture.data_dir),
                &probe,
                &Cancellation::default(),
            )
            .unwrap()
        }

        fn build(&self, policy: &InventoryPolicy) -> FileInventory {
            InventoryBuilder::build(&self.snapshot(), policy, &Cancellation::default()).unwrap()
        }

        fn inventory(&self) -> FileInventory {
            self.build(&InventoryPolicy::new())
        }
    }

    /// Creates a file symlink, or skips the platform that cannot.
    #[cfg(unix)]
    fn link(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[cfg(windows)]
    fn link(target: &Path, link: &Path) {
        std::os::windows::fs::symlink_file(target, link).unwrap();
    }

    fn paths(inventory: &FileInventory) -> Vec<String> {
        inventory
            .entries()
            .iter()
            .map(|entry| entry.path.display())
            .collect()
    }

    fn class_of(inventory: &FileInventory, path: &str) -> FileClass {
        inventory
            .entries()
            .iter()
            .find(|entry| entry.path.display() == path)
            .unwrap_or_else(|| panic!("{path} is not in the inventory: {:?}", paths(inventory)))
            .class
    }

    #[test]
    fn a_mixed_repository_classifies_every_file_and_denies_the_credentials() {
        let workspace = Workspace::new("mixed");
        workspace.write(".gitignore", "ignored.log\n");
        workspace.write("ignored.log", "noise\n");
        workspace.write(".env", "SECRET=1\n");
        workspace.write("deploy/id_rsa", "-----BEGIN PRIVATE KEY-----\n");
        workspace.write("vendor/left-pad/index.js", "module.exports = 1\n");
        workspace.write("big.txt", "x".repeat(2 * 1024 * 1024));
        workspace.write("logo.png", b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR");
        let mut utf16 = vec![0xFF, 0xFE];
        for unit in "notes\n".encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        workspace.write("notes.txt", utf16);
        workspace.write("src/main.rs", "fn main() {}\n");
        workspace.write("Cargo.lock", "[[package]]\n");
        workspace.write("AGENTS.md", "# instructions\n");

        let inventory = workspace.inventory();

        assert_eq!(
            class_of(&inventory, "vendor/left-pad/index.js"),
            FileClass::Vendor
        );
        assert_eq!(class_of(&inventory, "big.txt"), FileClass::Oversized);
        assert_eq!(class_of(&inventory, "logo.png"), FileClass::Binary);
        assert_eq!(class_of(&inventory, "notes.txt"), FileClass::Documentation);
        assert_eq!(class_of(&inventory, "src/main.rs"), FileClass::Source);
        assert_eq!(class_of(&inventory, "Cargo.lock"), FileClass::Lockfile);
        assert_eq!(class_of(&inventory, "AGENTS.md"), FileClass::Instruction);
        assert_eq!(class_of(&inventory, ".gitignore"), FileClass::Configuration);

        // Two built-in denials, one `.gitignore` exclusion.
        assert_eq!(inventory.denied_count(), 2);
        assert_eq!(inventory.ignored_count(), 1);
        assert!(!paths(&inventory).contains(&"ignored.log".to_owned()));
    }

    #[test]
    fn a_denied_path_appears_in_no_part_of_the_inventory() {
        let workspace = Workspace::new("denied");
        workspace.write(".env", "TOKEN=hunter2\n");
        workspace.write(".env.production", "TOKEN=hunter2\n");
        workspace.write("deploy/id_ed25519", "key\n");
        workspace.write("deploy/server.pem", "cert\n");
        workspace.write(".aws/credentials", "[default]\n");
        workspace.write(".config/gcloud/credentials.db", "binary\n");
        workspace.write("keep.txt", "ordinary\n");

        let inventory = workspace.inventory();

        // The whole product, not only its entries: an inventory reaches a log
        // as one value, so the scan is over everything it renders.
        let rendered = format!("{inventory:#?}");
        for secret in [
            ".env",
            "id_ed25519",
            "server.pem",
            "credentials",
            "hunter2",
            "gcloud",
        ] {
            assert!(
                !rendered.contains(secret),
                "'{secret}' survived into the inventory:\n{rendered}"
            );
        }
        assert_eq!(paths(&inventory), ["keep.txt"]);
        // Five denied files and one denied directory, which is counted once and
        // never descended into.
        assert_eq!(inventory.denied_count(), 6);
        assert_eq!(inventory.diagnostics(), &[]);
    }

    #[test]
    fn a_repository_re_inclusion_is_discarded_and_named() {
        let workspace = Workspace::new("re-inclusion");
        workspace.write(".env", "SECRET=1\n");
        workspace.write(REPOSITORY_IGNORE_FILE, "# tighten\n!.env\nbuild-notes.md\n");
        workspace.write("build-notes.md", "notes\n");
        workspace.write("keep.rs", "fn main() {}\n");

        let inventory = workspace.inventory();

        assert!(!paths(&inventory).contains(&".env".to_owned()));
        assert!(!paths(&inventory).contains(&"build-notes.md".to_owned()));
        let discarded = inventory
            .diagnostics()
            .iter()
            .find_map(|diagnostic| match diagnostic {
                InventoryDiagnostic::ReInclusionDiscarded {
                    layer,
                    line,
                    pattern,
                    ..
                } => Some((*layer, *line, pattern.clone())),
                _ => None,
            })
            .expect("the discarded negation is reported");
        assert_eq!(discarded, (IgnoreLayer::Repository, 2, "!.env".to_owned()));
        // The rest of the file still applies, which is what "tightening only"
        // means: the negation is dropped, not the file.
        assert_eq!(inventory.ignored_count(), 1);
    }

    #[test]
    fn a_repository_ignore_file_excludes_what_gitignore_does_not() {
        let workspace = Workspace::new("repository-tightening");
        workspace.write(".gitignore", "target\n");
        workspace.write(REPOSITORY_IGNORE_FILE, "docs/**\n");
        workspace.write("docs/design.md", "# design\n");
        workspace.write("docs/deep/notes.md", "# notes\n");
        workspace.write("README.md", "# readme\n");

        let inventory = workspace.inventory();

        assert!(
            !paths(&inventory)
                .iter()
                .any(|path| path.starts_with("docs/")),
            "{:?}",
            paths(&inventory)
        );
        assert!(paths(&inventory).contains(&"README.md".to_owned()));
    }

    #[test]
    fn every_layer_outranks_the_one_below_it_in_both_directions() {
        let workspace = Workspace::new("layers");
        let global = workspace.fixture.root.path().join("context-ignore");
        // Layer 2 excludes one path and tries to re-include a denied one.
        fs::write(&global, "global-only.md\n!.env\n").unwrap();
        // Layer 3 tries to re-include what layer 2 excluded, and excludes one
        // path of its own.
        workspace.write(
            REPOSITORY_IGNORE_FILE,
            "!global-only.md\nrepository-only.md\n",
        );
        // Layer 4 tries to re-include what layers 1 to 3 excluded.
        workspace.write(
            ".gitignore",
            "!.env\n!global-only.md\n!repository-only.md\ngit-only.md\n",
        );
        workspace.write(".env", "SECRET=1\n");
        workspace.write("global-only.md", "one\n");
        workspace.write("repository-only.md", "two\n");
        workspace.write("git-only.md", "three\n");
        workspace.write("kept.md", "four\n");

        let policy = InventoryPolicy::new().with_global_ignore(&global);
        let inventory = workspace.build(&policy);

        assert_eq!(
            paths(&inventory),
            [".gitignore", ".harkness/context-ignore", "kept.md"],
            "a lower layer re-included something a higher one excluded"
        );
        assert_eq!(inventory.denied_count(), 1);
        assert_eq!(inventory.ignored_count(), 3);
    }

    #[test]
    fn a_user_re_inclusion_outranks_the_repositorys_gitignore() {
        let workspace = Workspace::new("user-wins");
        let global = workspace.fixture.root.path().join("context-ignore");
        fs::write(&global, "!notes/keep.md\n").unwrap();
        workspace.write(".gitignore", "*.md\n");
        workspace.write("notes/keep.md", "kept\n");
        workspace.write("notes/other.md", "dropped\n");

        let inventory = workspace.build(&InventoryPolicy::new().with_global_ignore(&global));

        assert!(paths(&inventory).contains(&"notes/keep.md".to_owned()));
        assert!(!paths(&inventory).contains(&"notes/other.md".to_owned()));
    }

    #[test]
    fn nothing_under_a_pruned_directory_is_reconsidered() {
        // The same rule Git applies to its own negations: a directory that was
        // excluded is never descended into, so a re-inclusion below it has
        // nothing to act on. Stated as a test because it is the one place the
        // layer ordering does not read the way the table suggests.
        let workspace = Workspace::new("pruning");
        let global = workspace.fixture.root.path().join("context-ignore");
        fs::write(&global, "!build/keep.md\n").unwrap();
        workspace.write(".gitignore", "build\n");
        workspace.write("build/keep.md", "kept\n");

        let inventory = workspace.build(&InventoryPolicy::new().with_global_ignore(&global));

        assert!(!paths(&inventory).contains(&"build/keep.md".to_owned()));
        assert_eq!(inventory.ignored_count(), 1, "the directory counts once");
    }

    #[test]
    fn a_nested_repository_and_a_submodule_are_boundaries_the_walk_stops_at() {
        let workspace = Workspace::new("boundaries");
        workspace.write(
            ".gitmodules",
            "[submodule \"lib\"]\n\tpath = lib\n\turl = ../lib\n",
        );
        let nested = workspace.directory("nested");
        initialize_repository(&nested);
        fs::write(nested.join("inside.rs"), "fn main() {}\n").unwrap();
        let submodule = workspace.directory("lib");
        initialize_repository(&submodule);
        fs::write(submodule.join("inside.rs"), "fn main() {}\n").unwrap();

        let inventory = workspace.inventory();

        let boundary = |path: &str| {
            inventory
                .entries()
                .iter()
                .find(|entry| entry.path.display() == path)
                .unwrap_or_else(|| panic!("{path} is missing"))
                .boundary
        };
        assert_eq!(boundary("nested"), Some(Boundary::NestedRepository));
        assert_eq!(boundary("lib"), Some(Boundary::Submodule));
        assert!(
            !paths(&inventory)
                .iter()
                .any(|path| path.contains("inside.rs")),
            "{:?}",
            paths(&inventory)
        );
        for entry in inventory.entries() {
            if entry.boundary.is_some() {
                assert!(!entry.eligible());
            }
        }
    }

    #[test]
    fn the_file_budget_truncates_at_exactly_its_limit() {
        let workspace = Workspace::new("budget");
        for index in 0..150 {
            workspace.write(&format!("file-{index:03}.txt"), "x\n");
        }

        let inventory = workspace.build(&InventoryPolicy::new().with_max_files(100));

        assert_eq!(inventory.entries().len(), 100);
        assert_eq!(
            inventory.truncation(),
            Some(InventoryTruncation::FileBudgetExhausted { limit: 100 })
        );
        assert!(inventory.is_truncated());
    }

    #[test]
    fn the_time_budget_truncates_rather_than_failing() {
        let workspace = Workspace::new("time-budget");
        workspace.write("a.txt", "a\n");

        let inventory = workspace.build(&InventoryPolicy::new().with_max_walk(Duration::ZERO));

        assert_eq!(
            inventory.truncation(),
            Some(InventoryTruncation::WalkTimeExhausted {
                limit: Duration::ZERO
            })
        );
        assert!(inventory.entries().is_empty());
    }

    #[test]
    fn a_cancelled_walk_stops_promptly_and_yields_no_inventory() {
        // The token is polled before every entry, so the size of the tree does
        // not decide how long noticing takes. Cancelling before the walk starts
        // is the deterministic form of the same check: no sleep decides whether
        // this test passes.
        let workspace = Workspace::new("cancelled");
        for index in 0..1_000 {
            workspace.write(
                &format!("tree/{}/file-{index}.rs", index % 25),
                "fn a() {}\n",
            );
        }
        let snapshot = workspace.snapshot();
        let cancellation = Cancellation::default();
        cancellation.cancel();

        let started = Instant::now();
        let error =
            InventoryBuilder::build(&snapshot, &InventoryPolicy::new(), &cancellation).unwrap_err();
        let elapsed = started.elapsed();

        assert_eq!(error.kind(), "cancelled");
        assert!(elapsed < Duration::from_millis(250), "took {elapsed:?}");
    }

    #[test]
    fn an_empty_repository_is_an_empty_inventory_rather_than_an_error() {
        let workspace = Workspace::new("empty");

        let inventory = workspace.inventory();

        assert!(inventory.entries().is_empty());
        assert_eq!(inventory.denied_count(), 0);
        assert_eq!(inventory.ignored_count(), 0);
        assert!(inventory.truncation().is_none());
        assert_eq!(inventory.eligible_count(), 0);
        assert_eq!(inventory.classify_version(), crate::CLASSIFY_VERSION);
    }

    #[test]
    fn entries_are_sorted_by_exact_path_bytes() {
        let workspace = Workspace::new("ordering");
        workspace.write("b.txt", "b\n");
        workspace.write("A.txt", "a\n");
        workspace.write("a/z.txt", "z\n");
        workspace.write("a.txt", "a\n");

        let inventory = workspace.inventory();

        assert_eq!(paths(&inventory), ["A.txt", "a.txt", "a/z.txt", "b.txt"]);
        let mut sorted = inventory.entries().to_vec();
        sorted.sort_by(|left, right| left.path.cmp(&right.path));
        assert_eq!(sorted, inventory.entries());
    }

    #[test]
    fn hidden_files_are_visited_rather_than_skipped() {
        let workspace = Workspace::new("hidden");
        workspace.write(".hidden-notes.md", "# hidden\n");
        workspace.write(".hidden-dir/file.rs", "fn main() {}\n");

        let inventory = workspace.inventory();

        assert!(paths(&inventory).contains(&".hidden-notes.md".to_owned()));
        assert!(paths(&inventory).contains(&".hidden-dir/file.rs".to_owned()));
    }

    #[test]
    fn the_repositorys_own_git_directory_is_never_content() {
        let workspace = Workspace::new("git-directory");
        workspace.write("kept.rs", "fn main() {}\n");

        let inventory = workspace.inventory();

        assert_eq!(paths(&inventory), ["kept.rs"]);
        assert_eq!(inventory.ignored_count(), 0, "skipping .git is not a rule");
    }

    #[test]
    fn a_malformed_rule_leaves_the_rest_of_its_file_applying() {
        let workspace = Workspace::new("malformed");
        workspace.write(REPOSITORY_IGNORE_FILE, "{unclosed\nnotes.md\n");
        workspace.write("notes.md", "notes\n");
        workspace.write("kept.rs", "fn main() {}\n");

        let inventory = workspace.inventory();

        let invalid = inventory
            .diagnostics()
            .iter()
            .find_map(|diagnostic| match diagnostic {
                InventoryDiagnostic::IgnoreRuleInvalid {
                    layer,
                    line,
                    pattern,
                    ..
                } => Some((*layer, *line, pattern.clone())),
                _ => None,
            })
            .expect("the malformed pattern is reported");
        assert_eq!(
            invalid,
            (
                IgnoreLayer::Repository,
                Some(1),
                Some("{unclosed".to_owned())
            )
        );
        assert!(!paths(&inventory).contains(&"notes.md".to_owned()));
        assert!(paths(&inventory).contains(&"kept.rs".to_owned()));
    }

    #[test]
    fn an_unreadable_rule_file_fails_the_walk_rather_than_widening_it() {
        let workspace = Workspace::new("unreadable-rules");
        let global = workspace.fixture.root.path().join("context-ignore");
        fs::create_dir(&global).unwrap();

        let error = InventoryBuilder::build(
            &workspace.snapshot(),
            &InventoryPolicy::new().with_global_ignore(&global),
            &Cancellation::default(),
        )
        .unwrap_err();

        assert_eq!(error.kind(), "ignore_rule_invalid");
    }

    #[test]
    fn an_oversized_rule_file_is_refused_rather_than_partially_applied() {
        let workspace = Workspace::new("oversized-rules");
        let global = workspace.fixture.root.path().join("context-ignore");
        let bytes = usize::try_from(MAX_IGNORE_FILE_BYTES).unwrap() + 1;
        fs::write(&global, "#".repeat(bytes)).unwrap();

        let error = InventoryBuilder::build(
            &workspace.snapshot(),
            &InventoryPolicy::new().with_global_ignore(&global),
            &Cancellation::default(),
        )
        .expect_err("an oversized rule file is refused");

        assert_eq!(error.kind(), "ignore_rule_invalid");
    }

    #[test]
    fn a_missing_rule_file_contributes_no_rules() {
        let workspace = Workspace::new("missing-rules");
        workspace.write("kept.rs", "fn main() {}\n");
        let global = workspace.fixture.root.path().join("context-ignore");

        let inventory = workspace.build(&InventoryPolicy::new().with_global_ignore(&global));

        assert_eq!(paths(&inventory), ["kept.rs"]);
    }

    #[test]
    fn a_walk_sees_the_working_tree_rather_than_the_index() {
        // What a sparse checkout looks like from here: `tracked.txt` is
        // committed and not on disk, and an inventory describes what is on
        // disk. The walk consults no index, so a materialized subset is simply
        // the subset it finds.
        let workspace = Workspace::new("sparse");
        workspace.write("present.rs", "fn main() {}\n");

        let inventory = workspace.inventory();

        assert_eq!(paths(&inventory), ["present.rs"]);
    }

    #[test]
    fn the_error_namespace_is_exact_and_disjoint_from_the_domains() {
        let cases = [
            (
                InventoryError::RootUnavailable {
                    path: PathBuf::from("/tmp/gone"),
                    reason: "missing".to_owned(),
                },
                "root_unavailable",
            ),
            (
                InventoryError::NotADirectory {
                    path: PathBuf::from("/tmp/file"),
                },
                "not_a_directory",
            ),
            (
                InventoryError::WalkFailed {
                    path: PathBuf::from("/tmp/root"),
                    reason: "permission denied".to_owned(),
                },
                "walk_failed",
            ),
            (
                InventoryError::IgnoreRuleInvalid {
                    layer: IgnoreLayer::Repository,
                    file: ".harkness/context-ignore".to_owned(),
                    reason: "unreadable".to_owned(),
                },
                "ignore_rule_invalid",
            ),
            (InventoryError::Cancelled, "cancelled"),
        ];

        let kinds = cases.iter().map(|(_, kind)| *kind).collect::<Vec<_>>();
        assert_eq!(kinds, InventoryError::KINDS);
        for (error, expected) in cases {
            assert_eq!(error.kind(), expected, "unexpected kind for {error:?}");
        }
        for kind in InventoryError::KINDS {
            assert!(
                !ContextDomainError::KINDS.contains(kind),
                "'{kind}' collides with the domain namespace"
            );
        }
    }

    #[test]
    fn the_inventory_vocabulary_has_stable_spellings() {
        assert_eq!(
            IgnoreLayer::ALL
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["built_in", "global", "repository", "gitignore"]
        );
        assert_eq!(Boundary::NestedRepository.to_string(), "nested_repository");
        assert_eq!(Boundary::Submodule.to_string(), "submodule");

        let rendered = InventoryDiagnostic::ReInclusionDiscarded {
            layer: IgnoreLayer::Repository,
            file: ".harkness/context-ignore".to_owned(),
            line: 4,
            pattern: "!.env".to_owned(),
        }
        .to_string();
        assert_eq!(
            rendered,
            ".harkness/context-ignore:4: the repository layer may only tighten, \
             so the re-inclusion '!.env' was discarded"
        );
        let rendered = InventoryDiagnostic::IgnoreRuleInvalid {
            layer: IgnoreLayer::GitIgnore,
            file: ".gitignore".to_owned(),
            line: None,
            pattern: None,
            reason: "unreadable".to_owned(),
        }
        .to_string();
        assert_eq!(rendered, ".gitignore: invalid gitignore rule: unreadable");
        let rendered = InventoryDiagnostic::CaseCollision {
            path: RepoPath::from_bytes(b"readme.md".to_vec()),
            existing: RepoPath::from_bytes(b"README.md".to_vec()),
        }
        .to_string();
        assert_eq!(
            rendered,
            "'readme.md' and 'README.md' differ only by case and are both recorded"
        );
    }

    #[test]
    fn quoted_repository_text_is_clamped_on_a_character_boundary() {
        // A pattern and a path are both repository content, so neither decides
        // how long a Harkness message is.
        assert_eq!(super::clamp("short"), "short");
        let long = "é".repeat(MAX_DIAGNOSTIC_TEXT_BYTES);
        let clamped = super::clamp(&long);
        assert!(clamped.ends_with('…'), "{clamped}");
        assert!(clamped.len() <= MAX_DIAGNOSTIC_TEXT_BYTES + '…'.len_utf8());
    }

    #[test]
    fn a_rule_file_saved_with_a_byte_order_mark_still_tightens() {
        let workspace = Workspace::new("bom");
        workspace.write(REPOSITORY_IGNORE_FILE, "\u{feff}notes.md\nother.md\n");
        workspace.write("notes.md", "one\n");
        workspace.write("other.md", "two\n");
        workspace.write("kept.rs", "fn main() {}\n");

        let inventory = workspace.inventory();

        assert_eq!(paths(&inventory), [".harkness/context-ignore", "kept.rs"]);
        assert_eq!(inventory.ignored_count(), 2);
    }

    #[test]
    fn a_repository_rule_file_may_not_be_a_symlink() {
        // A committed link is how a repository would aim the reader at a file
        // outside the worktree and read it back through a diagnostic.
        let workspace = Workspace::new("linked-rules");
        let elsewhere = workspace.fixture.root.path().join("private-key");
        fs::write(&elsewhere, "!.env\nsupersecret\n").unwrap();
        fs::create_dir_all(workspace.root.join(".harkness")).unwrap();
        link(&elsewhere, &workspace.root.join(REPOSITORY_IGNORE_FILE));

        let error = InventoryBuilder::build(
            &workspace.snapshot(),
            &InventoryPolicy::new(),
            &Cancellation::default(),
        )
        .expect_err("a symlinked repository rule file is refused");

        assert_eq!(error.kind(), "ignore_rule_invalid");
        assert!(
            !format!("{error}").contains("supersecret"),
            "the target's content reached the error: {error}"
        );
    }

    #[test]
    fn a_users_own_rule_file_may_be_a_symlink() {
        let workspace = Workspace::new("linked-global");
        let real = workspace
            .fixture
            .root
            .path()
            .join("dotfiles-context-ignore");
        fs::write(&real, "notes.md\n").unwrap();
        let configured = workspace.fixture.root.path().join("context-ignore");
        link(&real, &configured);
        workspace.write("notes.md", "one\n");
        workspace.write("kept.rs", "fn main() {}\n");

        let inventory = workspace.build(&InventoryPolicy::new().with_global_ignore(&configured));

        assert_eq!(paths(&inventory), ["kept.rs"]);
    }

    #[test]
    fn an_unreadable_gitignore_is_reported_rather_than_silently_empty() {
        let workspace = Workspace::new("gitignore-io");
        workspace.write(".gitignore", "{unclosed\nnotes.md\n");
        workspace.write("notes.md", "one\n");
        workspace.write("kept.rs", "fn main() {}\n");

        let inventory = workspace.inventory();

        // The chain now reports its bad pattern by line, exactly as the two
        // layers above it do, and the rest of the file still applies.
        let invalid = inventory
            .diagnostics()
            .iter()
            .find_map(|diagnostic| match diagnostic {
                InventoryDiagnostic::IgnoreRuleInvalid {
                    layer,
                    line,
                    pattern,
                    ..
                } => Some((*layer, *line, pattern.clone())),
                _ => None,
            })
            .expect("the malformed gitignore pattern is reported");
        assert_eq!(
            invalid,
            (
                IgnoreLayer::GitIgnore,
                Some(1),
                Some("{unclosed".to_owned())
            )
        );
        assert!(!paths(&inventory).contains(&"notes.md".to_owned()));
    }

    #[test]
    fn a_phantom_path_key_outside_a_submodule_section_declares_nothing() {
        let workspace = Workspace::new("gitmodules-sections");
        workspace.write(".gitmodules", "[core]\n\tpath = lib\n");
        let nested = workspace.directory("lib");
        initialize_repository(&nested);

        let inventory = workspace.inventory();

        let boundary = inventory
            .entries()
            .iter()
            .find(|entry| entry.path.display() == "lib")
            .expect("the nested repository is recorded")
            .boundary;
        assert_eq!(boundary, Some(Boundary::NestedRepository));
    }

    #[test]
    fn a_utf32_file_is_binary_despite_opening_with_a_utf16_mark() {
        // `FF FE 00 00` is the UTF-32LE mark and also, to two bytes of context,
        // the UTF-16LE one. Reading the rest as UTF-16 decodes every `0x0000` as
        // a valid NUL character, so a decoder that accepted them would hand an
        // indexer a binary blob.
        let workspace = Workspace::new("utf32");
        let mut utf32 = vec![0xFF, 0xFE, 0x00, 0x00];
        for point in "hello".chars() {
            utf32.extend_from_slice(&u32::from(point).to_le_bytes());
        }
        workspace.write("data/blob.bin", utf32);

        let inventory = workspace.inventory();

        let entry = &inventory.entries()[0];
        assert_eq!(entry.class, FileClass::Binary);
        assert!(!entry.eligible());
    }

    #[test]
    fn the_truncation_and_diagnostic_spellings_are_published_and_disjoint() {
        assert_eq!(
            InventoryTruncation::FileBudgetExhausted { limit: 1 }.to_string(),
            "file_budget_exhausted"
        );
        assert_eq!(
            InventoryTruncation::WalkTimeExhausted {
                limit: Duration::ZERO
            }
            .to_string(),
            "walk_time_exhausted"
        );

        let diagnostics = [
            InventoryDiagnostic::ReInclusionDiscarded {
                layer: IgnoreLayer::Repository,
                file: "f".to_owned(),
                line: 1,
                pattern: "!x".to_owned(),
            },
            InventoryDiagnostic::IgnoreRuleInvalid {
                layer: IgnoreLayer::GitIgnore,
                file: "f".to_owned(),
                line: None,
                pattern: None,
                reason: "r".to_owned(),
            },
            InventoryDiagnostic::CaseCollision {
                path: RepoPath::from_bytes(b"a".to_vec()),
                existing: RepoPath::from_bytes(b"A".to_vec()),
            },
            InventoryDiagnostic::Unreadable {
                path: RepoPath::from_bytes(b"a".to_vec()),
                reason: "r".to_owned(),
            },
            InventoryDiagnostic::Vanished {
                path: RepoPath::from_bytes(b"a".to_vec()),
            },
        ];
        let kinds = diagnostics
            .iter()
            .map(InventoryDiagnostic::kind)
            .collect::<Vec<_>>();
        assert_eq!(kinds, InventoryDiagnostic::KINDS);
        for kind in InventoryDiagnostic::KINDS {
            assert!(
                !InventoryError::KINDS.contains(kind),
                "'{kind}' is both a diagnostic and a failure"
            );
        }
    }

    #[test]
    fn a_root_that_is_not_a_directory_is_refused_by_name() {
        let workspace = Workspace::new("root-checks");
        let snapshot = workspace.snapshot();

        // A caller cannot address the walk at an arbitrary directory — the root
        // comes from the snapshot — so the refusals are exercised on the walk
        // that reads one.
        let missing = super::Walk::new(
            snapshot.id(),
            &workspace.root.join("gone"),
            &InventoryPolicy::new(),
            &Cancellation::default(),
        )
        .err()
        .expect("a missing root is refused");
        assert_eq!(missing.kind(), "root_unavailable");

        let file = workspace.write("a-file.txt", "x\n");
        let not_a_directory = super::Walk::new(
            snapshot.id(),
            &file,
            &InventoryPolicy::new(),
            &Cancellation::default(),
        )
        .err()
        .expect("a file is not a worktree root");
        assert_eq!(not_a_directory.kind(), "not_a_directory");
    }

    #[test]
    fn the_built_in_denials_compile_and_match_at_any_depth() {
        let workspace = Workspace::new("denial-depth");
        workspace.write("a/b/c/.env", "SECRET=1\n");
        workspace.write("a/b/c/keep.rs", "fn main() {}\n");
        assert!(!BUILT_IN_DENIALS.is_empty());

        let inventory = workspace.inventory();

        assert_eq!(paths(&inventory), ["a/b/c/keep.rs"]);
        assert_eq!(inventory.denied_count(), 1);
    }

    #[test]
    fn a_secret_sensitive_entry_is_recorded_and_never_eligible() {
        let workspace = Workspace::new("secret-sensitive");
        workspace.write("deploy/api_key.txt", "abc\n");

        let inventory = workspace.inventory();

        let entry = &inventory.entries()[0];
        assert_eq!(entry.path.display(), "deploy/api_key.txt");
        assert_eq!(entry.class, FileClass::SecretSensitive);
        assert!(!entry.eligible());
        assert_eq!(inventory.eligible_count(), 0);
    }

    #[test]
    fn diagnostics_are_bounded_and_the_overflow_is_counted() {
        assert_eq!(MAX_INVENTORY_DIAGNOSTICS, 1_000);
        let workspace = Workspace::new("diagnostic-bound");
        let mut rules = String::new();
        for index in 0..MAX_INVENTORY_DIAGNOSTICS + 10 {
            rules.push_str(&format!("!re-include-{index}\n"));
        }
        workspace.write(REPOSITORY_IGNORE_FILE, rules);

        let inventory = workspace.inventory();

        assert_eq!(inventory.diagnostics().len(), MAX_INVENTORY_DIAGNOSTICS);
        assert_eq!(inventory.dropped_diagnostics(), 10);
    }

    #[test]
    fn an_oversized_class_is_recorded_but_not_eligible() {
        let workspace = Workspace::new("oversized");
        workspace.write(
            "big.md",
            "x".repeat(usize::try_from(OVERSIZED_FILE_THRESHOLD).unwrap() + 1),
        );

        let inventory = workspace.inventory();

        let entry = &inventory.entries()[0];
        assert_eq!(entry.class, FileClass::Oversized);
        assert!(!entry.eligible());
        assert!(entry.byte_size > OVERSIZED_FILE_THRESHOLD);
        assert!(entry.mtime_ns.is_some());
    }

    #[test]
    #[ignore = "latency target; meaningful only in a release build"]
    fn a_medium_repository_meets_the_walk_latency_target() {
        const FILES: usize = 10_000;
        const RUNS: usize = 5;

        let workspace = Workspace::new("benchmark");
        // Sized past `BINARY_SNIFF_BYTES` so every file pays the full 8 KiB
        // read and the 1 KiB marker scan. A budget measured on 29-byte files
        // measures neither, and those two are what a real walk spends its time
        // on.
        let body = "fn main() { let value = 1; }\n".repeat(400);
        for index in 0..FILES {
            workspace.write(
                &format!("src/module-{}/file-{index}.rs", index % 100),
                &body,
            );
        }
        let snapshot = workspace.snapshot();
        let policy = InventoryPolicy::new();

        let mut timings = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            let started = Instant::now();
            let inventory =
                InventoryBuilder::build(&snapshot, &policy, &Cancellation::default()).unwrap();
            timings.push(started.elapsed());
            assert_eq!(inventory.entries().len(), FILES);
        }
        timings.sort_unstable();
        let worst = *timings.last().unwrap();
        println!(
            "inventory of {FILES} files: {timings:?} on {} / {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        assert!(
            worst < Duration::from_millis(1_500),
            "slowest run {worst:?}"
        );
    }

    #[cfg(unix)]
    mod unix {
        use std::fs;
        use std::os::unix::ffi::OsStrExt;

        use super::{InventoryDiagnostic, InventoryPolicy, Workspace, paths};
        use crate::RepoPath;

        #[test]
        fn a_directory_symlink_is_recorded_and_its_target_is_never_walked() {
            // Unix-only because a Windows directory link needs developer mode
            // or elevation, not because the walk treats the platforms
            // differently: a link is recorded and not followed on both.
            let workspace = Workspace::new("symlinks");
            let outside = workspace.fixture.directory("outside");
            fs::write(outside.join("neighbour.txt"), "not ours\n").unwrap();
            std::os::unix::fs::symlink(&outside, workspace.root.join("linked")).unwrap();
            workspace.write("kept.rs", "fn main() {}\n");

            let inventory = workspace.inventory();

            assert_eq!(paths(&inventory), ["kept.rs", "linked"]);
            let linked = &inventory.entries()[1];
            assert!(linked.symlink);
            assert!(linked.boundary.is_none());
            assert!(!linked.eligible(), "a link's content is somewhere else");
        }

        #[test]
        fn a_non_utf8_path_round_trips_byte_exactly_and_reports_itself_lossy() {
            let workspace = Workspace::new("lossy-paths");
            let name = std::ffi::OsStr::from_bytes(b"weird-\xff\xfe.txt");
            fs::write(workspace.root.join(name), "content\n").unwrap();

            let inventory = workspace.inventory();

            let entry = inventory
                .entries()
                .iter()
                .find(|entry| entry.path.is_lossy())
                .expect("the lossy path is recorded");
            assert_eq!(entry.path.as_bytes(), b"weird-\xff\xfe.txt");
            assert_ne!(entry.path.display().as_bytes(), entry.path.as_bytes());
            assert_eq!(
                entry.path,
                RepoPath::from_bytes(b"weird-\xff\xfe.txt".to_vec())
            );
        }

        /// Restores a path's mode however the test ends.
        ///
        /// A failing assertion unwinds past any restoring statement, and the
        /// fixture's `TempDir` then cannot remove a mode-`000` directory: the
        /// real failure would be reported behind a cleanup error, by the very
        /// tests written to catch a regression in unreadable-path handling.
        struct Restore {
            path: std::path::PathBuf,
            mode: u32,
        }

        impl Drop for Restore {
            fn drop(&mut self) {
                use std::os::unix::fs::PermissionsExt;

                let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(self.mode));
            }
        }

        /// Denies access and restores it on drop, or returns `None` when the
        /// process can read a mode-`000` path anyway — which is what running as
        /// root does, and a reason to skip rather than to fail.
        fn deny_access(path: &std::path::Path, is_dir: bool, mode: u32) -> Option<Restore> {
            use std::os::unix::fs::PermissionsExt;

            fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
            let restore = Restore {
                path: path.to_path_buf(),
                mode,
            };
            let denied = if is_dir {
                fs::read_dir(path).is_err()
            } else {
                fs::File::open(path).is_err()
            };
            denied.then_some(restore)
        }

        #[test]
        fn an_unreadable_directory_is_a_diagnostic_rather_than_a_failure() {
            let workspace = Workspace::new("unreadable-branch");
            workspace.write("readable/file.rs", "fn main() {}\n");
            let closed = workspace.directory("closed");
            fs::write(closed.join("hidden.rs"), "fn main() {}\n").unwrap();
            let Some(_restore) = deny_access(&closed, true, 0o755) else {
                return;
            };

            let inventory = workspace.build(&InventoryPolicy::new());

            assert!(paths(&inventory).contains(&"readable/file.rs".to_owned()));
            assert!(
                inventory.diagnostics().iter().any(|diagnostic| matches!(
                    diagnostic,
                    InventoryDiagnostic::Unreadable { path, .. } if path.display() == "closed"
                )),
                "{:?}",
                inventory.diagnostics()
            );
        }

        #[test]
        fn a_file_that_cannot_be_opened_is_recorded_and_never_eligible() {
            let workspace = Workspace::new("unreadable-file");
            let closed = workspace.write("closed.rs", "fn main() {}\n");
            let Some(_restore) = deny_access(&closed, false, 0o644) else {
                return;
            };

            let inventory = workspace.build(&InventoryPolicy::new());

            let entry = &inventory.entries()[0];
            assert_eq!(entry.path.display(), "closed.rs");
            assert!(entry.unreadable);
            assert!(!entry.eligible());
            assert!(
                inventory
                    .diagnostics()
                    .iter()
                    .any(|diagnostic| matches!(diagnostic, InventoryDiagnostic::Unreadable { .. }))
            );
        }

        #[test]
        fn two_paths_that_differ_only_by_case_are_both_kept_and_flagged() {
            let workspace = Workspace::new("case-collision");
            workspace.write("README.md", "one\n");
            workspace.write("readme.md", "two\n");
            let listed = fs::read_dir(&workspace.root).unwrap().count();

            let inventory = workspace.inventory();

            let collisions = inventory
                .diagnostics()
                .iter()
                .filter(|diagnostic| {
                    matches!(diagnostic, InventoryDiagnostic::CaseCollision { .. })
                })
                .count();
            if listed >= 3 {
                // Case-sensitive filesystem: both names exist.
                assert_eq!(paths(&inventory), ["README.md", "readme.md"]);
                assert_eq!(collisions, 1);
            } else {
                // Case-insensitive: the second write replaced the first.
                assert_eq!(paths(&inventory).len(), 1);
                assert_eq!(collisions, 0);
            }
        }
    }
}
