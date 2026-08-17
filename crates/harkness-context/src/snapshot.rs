//! Workspace identity: what makes two workspaces the same one.
//!
//! `HEAD` is not an identity. A developer edits a file and `HEAD` does not move;
//! two linked worktrees sit at one commit with different uncommitted work; a
//! path is staged and never committed. Each of those is an ordinary Monday, and
//! each makes "same commit" mean "different bytes". ADR-0008 fixes the answer:
//! identity is a composite digest over ten components, and this module is where
//! that digest is computed, recorded, and re-checked.
//!
//! [`WorkspaceSnapshot::capture`] reads the components, [`WorkspaceSnapshot::digest`]
//! folds them into one value, and [`WorkspaceSnapshot::verify`] recomputes
//! cheaply and reports [`FreshnessState`]. Capture tolerates a workspace that
//! moves underneath it — a file that changes mid-hash contributes the bytes that
//! were read — because a snapshot is an honest record of what was read rather
//! than a lock on the filesystem. Verification is what turns that honesty into
//! safety later.
//!
//! Snapshots hold hashes and paths, never file contents, so they are safe to
//! persist and to display. The only absolute path is the worktree root.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use harkness_core::ProjectId;
use harkness_git::{
    Cancellation, DetailedStatus, FileChange, GitError, GitService, HeadState, StatusEntry,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::digest::{
    DOMAIN_PATH_SET, DOMAIN_SNAPSHOT, DigestWriter, Sha256Hex, empty_path_set_digest,
};
use crate::error::ContextDomainError;
use crate::ids::SnapshotId;
use crate::path::RepoPath;
use crate::probe::{ContentDigest, ProbeFailure, WorkspaceProbe};

/// The composite digest that answers "is this the same workspace?".
///
/// Two captures of one unchanged workspace produce two [`SnapshotId`] values and
/// one `SnapshotDigest`, which is the whole point of separating them.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SnapshotDigest(Sha256Hex);

impl SnapshotDigest {
    /// The underlying digest.
    #[must_use]
    pub fn as_sha256(&self) -> &Sha256Hex {
        &self.0
    }
}

impl std::fmt::Display for SnapshotDigest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::str::FromStr for SnapshotDigest {
    type Err = ContextDomainError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.parse().map(Self)
    }
}

/// One path and what it contributed to a workspace identity.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileDigestEntry {
    /// Repository-relative path, byte-exact.
    pub path: RepoPath,
    /// What the path held when it was read.
    pub digest: ContentDigest,
}

impl FileDigestEntry {
    /// Records one path's contribution.
    #[must_use]
    pub fn new(path: RepoPath, digest: ContentDigest) -> Self {
        Self { path, digest }
    }
}

/// Which part of a workspace identity a divergence belongs to.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SnapshotComponent {
    /// The repository the worktree belongs to.
    RepositoryIdentity,
    /// The canonicalized worktree root.
    WorktreeRoot,
    /// The checked-out commit.
    Head,
    /// The checked-out branch, or its absence.
    Branch,
    /// Staged paths and their blob ids.
    Index,
    /// Modified tracked paths and their content hashes.
    TrackedDirty,
    /// Untracked eligible paths and their content hashes.
    Untracked,
}

impl SnapshotComponent {
    /// Returns the stable persisted spelling of this component.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RepositoryIdentity => "repository_identity",
            Self::WorktreeRoot => "worktree_root",
            Self::Head => "head",
            Self::Branch => "branch",
            Self::Index => "index",
            Self::TrackedDirty => "tracked_dirty",
            Self::Untracked => "untracked",
        }
    }
}

impl std::fmt::Display for SnapshotComponent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// How one component moved between a capture and a verification.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PathDivergence {
    /// The path takes part now and did not before.
    Added,
    /// The path took part before and does not now.
    Removed,
    /// The path takes part in both and holds something else.
    Changed,
}

/// One named difference between a snapshot and the workspace it described.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StalePath {
    /// Repository-relative path, absent for a component that names no path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<RepoPath>,
    /// Which part of the identity diverged.
    pub component: SnapshotComponent,
    /// How it diverged.
    pub change: PathDivergence,
}

/// Why a workspace could not be checked at all.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum UnverifiableReason {
    /// The repository is gone, or is no longer a repository.
    RepositoryUnavailable,
    /// The worktree root no longer exists.
    WorktreeRootMissing,
    /// Git could not report the repository's status.
    StatusUnavailable,
    /// The check observed its cancellation token.
    Cancelled,
}

impl UnverifiableReason {
    /// The same spelling `Serialize` emits.
    ///
    /// It exists so a caller putting this reason into a message or a JSON field
    /// of its own does not reach for `Debug`. A lowercased `Debug` rendering
    /// produces `repositoryunavailable`, agrees with nothing, and would case-fold
    /// any path a future variant carried.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RepositoryUnavailable => "repository_unavailable",
            Self::WorktreeRootMissing => "worktree_root_missing",
            Self::StatusUnavailable => "status_unavailable",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Whether a snapshot still describes its workspace.
///
/// Anything other than [`FreshnessState::Fresh`] must stop a mutation that was
/// planned against the snapshot. `Unverifiable` is not a soft `Fresh`: it says
/// the question could not be answered, which is the one case where proceeding
/// would be a guess.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[non_exhaustive]
pub enum FreshnessState {
    /// The workspace is byte-for-byte the one the snapshot describes.
    Fresh,
    /// The workspace moved. Every divergence is named.
    Stale {
        /// What diverged, in path order within each component.
        changed: Vec<StalePath>,
    },
    /// The workspace could not be read.
    Unverifiable {
        /// Why the check could not be completed.
        reason: UnverifiableReason,
    },
}

impl FreshnessState {
    /// Whether the workspace is unchanged.
    #[must_use]
    pub const fn is_fresh(&self) -> bool {
        matches!(self, Self::Fresh)
    }
}

/// The three path sets that make up the content half of a workspace identity.
///
/// Entries are sorted by path and hold one entry per path, which is what makes
/// the rolled-up digests order-independent: the same set of files digests
/// identically however the filesystem happened to enumerate them.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SnapshotFiles {
    staged: Vec<FileDigestEntry>,
    tracked_dirty: Vec<FileDigestEntry>,
    untracked: Vec<FileDigestEntry>,
}

impl SnapshotFiles {
    /// Normalizes three collected path sets into the canonical form.
    #[must_use]
    pub fn new(
        staged: Vec<FileDigestEntry>,
        tracked_dirty: Vec<FileDigestEntry>,
        untracked: Vec<FileDigestEntry>,
    ) -> Self {
        Self {
            staged: canonicalize_entries(staged),
            tracked_dirty: canonicalize_entries(tracked_dirty),
            untracked: canonicalize_entries(untracked),
        }
    }

    /// Staged paths and the blob ids Git holds for them.
    #[must_use]
    pub fn staged(&self) -> &[FileDigestEntry] {
        &self.staged
    }

    /// Modified tracked paths and their working-tree content hashes.
    #[must_use]
    pub fn tracked_dirty(&self) -> &[FileDigestEntry] {
        &self.tracked_dirty
    }

    /// Untracked eligible paths and their content hashes.
    #[must_use]
    pub fn untracked(&self) -> &[FileDigestEntry] {
        &self.untracked
    }

    /// How many paths took part in this identity.
    #[must_use]
    pub fn len(&self) -> usize {
        self.staged.len() + self.tracked_dirty.len() + self.untracked.len()
    }

    /// Whether the workspace was clean.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Sorts by path and keeps one entry per path, preferring a real reading over
/// the [`ContentDigest::Absent`] marker.
///
/// A capture does not currently produce a repeated path: a rename's source is
/// deleted by definition, so Git reports no rename at all once that path exists
/// again — `git mv a b` followed by staging a new `a` comes back as an add
/// beside a modify. Loading refuses a duplicate outright. What remains is
/// [`SnapshotFiles::new`], which is public and takes whatever it is handed, so
/// the set it produces has to be defined rather than merely observed.
///
/// The preference is stated rather than inherited. `ContentDigest`'s derived
/// `Ord` happens to give the same answer, purely because `StagedBlob` is
/// declared above `Absent`; reordering the variants is an edit nothing would
/// flag, and it would silently invert which reading survived.
fn canonicalize_entries(mut entries: Vec<FileDigestEntry>) -> Vec<FileDigestEntry> {
    fn absent_last(digest: &ContentDigest) -> u8 {
        u8::from(matches!(digest, ContentDigest::Absent))
    }

    entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| absent_last(&left.digest).cmp(&absent_last(&right.digest)))
            .then_with(|| left.digest.cmp(&right.digest))
    });
    entries.dedup_by(|left, right| left.path == right.path);
    entries
}

/// Rolls one path set up into an order-independent digest.
fn path_set_digest(entries: &[FileDigestEntry]) -> Sha256Hex {
    let mut writer = DigestWriter::new(DOMAIN_PATH_SET);
    writer.integer(entries.len() as u64);
    for entry in entries {
        writer
            .field(entry.path.as_bytes())
            .field(entry.digest.as_digest_input().as_bytes());
    }
    writer.finish()
}

/// Everything a snapshot needs that the workspace itself cannot supply.
///
/// The generations are the caller's knowledge, not Git's: a configuration change
/// that alters which files are eligible, or an index rebuild that moves every
/// chunk boundary, changes the workspace's meaning without changing a byte of
/// it. Both therefore belong to identity, and both have to be handed in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureRequest {
    /// Catalog project the workspace belongs to.
    pub project_id: ProjectId,
    /// Digest of the ordered discovered instruction set and its contents.
    ///
    /// [#120] computes this. Until then, [`empty_path_set_digest`] states that
    /// no instructions were discovered rather than leaving the field absent.
    ///
    /// [#120]: https://github.com/fullstacktaiye/harkness/issues/120
    pub instructions_digest: Sha256Hex,
    /// Bumped when context-relevant configuration changes.
    pub config_generation: u64,
    /// Generation of the index this snapshot was taken against; `0` means none.
    pub index_generation: u64,
}

impl CaptureRequest {
    /// Captures a workspace with no instructions, configuration, or index yet.
    #[must_use]
    pub fn new(project_id: ProjectId) -> Self {
        Self {
            project_id,
            instructions_digest: empty_path_set_digest(),
            config_generation: 0,
            index_generation: 0,
        }
    }

    /// Records the digest of the discovered instruction set.
    #[must_use]
    pub fn with_instructions_digest(mut self, digest: Sha256Hex) -> Self {
        self.instructions_digest = digest;
        self
    }

    /// Records the configuration generation this capture is taken under.
    #[must_use]
    pub const fn with_config_generation(mut self, generation: u64) -> Self {
        self.config_generation = generation;
        self
    }

    /// Records the index generation this capture is taken against.
    #[must_use]
    pub const fn with_index_generation(mut self, generation: u64) -> Self {
        self.index_generation = generation;
        self
    }
}

/// One path a capture could not read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkippedPath {
    /// Repository-relative path that was skipped.
    pub path: RepoPath,
    /// Stable human-readable explanation from the probe.
    pub reason: String,
}

/// What a capture or a verification actually did.
///
/// Recorded so [#133] can show why a capture took as long as it did and what it
/// could not read, without the surface having to re-walk the workspace to find
/// out. This crate defines the diagnostics; emitting them as events belongs to
/// [#110].
///
/// [#110]: https://github.com/fullstacktaiye/harkness/issues/110
/// [#133]: https://github.com/fullstacktaiye/harkness/issues/133
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CaptureDiagnostics {
    /// How many paths were hashed.
    pub paths_hashed: usize,
    /// Every path that could not be read, with the reason.
    pub paths_skipped: Vec<SkippedPath>,
    /// How long the capture took.
    pub duration: Duration,
}

/// A captured snapshot and what capturing it involved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Capture {
    /// The workspace identity that was captured.
    pub snapshot: WorkspaceSnapshot,
    /// What the capture read and skipped.
    pub diagnostics: CaptureDiagnostics,
}

/// A freshness verdict and what reaching it involved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Verification {
    /// Whether the snapshot still describes its workspace.
    pub state: FreshnessState,
    /// What the check read and skipped.
    pub diagnostics: CaptureDiagnostics,
}

/// One capture of the exact state of one worktree.
///
/// Fields are read through accessors because three of them — the index,
/// tracked-dirty, and untracked digests — are derived from [`SnapshotFiles`] and
/// must never be settable independently of it. A record whose digest disagrees
/// with its own contents is refused on load rather than trusted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceSnapshot {
    id: SnapshotId,
    project_id: ProjectId,
    repository_identity: String,
    worktree_root: PathBuf,
    head: Option<String>,
    branch: Option<String>,
    files: SnapshotFiles,
    index_digest: Sha256Hex,
    tracked_dirty_digest: Sha256Hex,
    untracked_digest: Sha256Hex,
    instructions_digest: Sha256Hex,
    config_generation: u64,
    index_generation: u64,
    captured_at: OffsetDateTime,
}

impl WorkspaceSnapshot {
    /// Assembles a snapshot from components that have already been read.
    ///
    /// The three content digests are computed here and nowhere else, so they
    /// cannot drift from the entry lists they summarize.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn assemble(
        id: SnapshotId,
        request: &CaptureRequest,
        repository_identity: String,
        worktree_root: PathBuf,
        head: Option<String>,
        branch: Option<String>,
        files: SnapshotFiles,
        captured_at: OffsetDateTime,
    ) -> Self {
        let index_digest = path_set_digest(files.staged());
        let tracked_dirty_digest = path_set_digest(files.tracked_dirty());
        let untracked_digest = path_set_digest(files.untracked());
        Self {
            id,
            project_id: request.project_id,
            repository_identity,
            worktree_root,
            head,
            branch,
            files,
            index_digest,
            tracked_dirty_digest,
            untracked_digest,
            instructions_digest: request.instructions_digest.clone(),
            config_generation: request.config_generation,
            index_generation: request.index_generation,
            captured_at,
        }
    }

    /// This capture's identity. Two captures of one workspace differ here.
    #[must_use]
    pub fn id(&self) -> SnapshotId {
        self.id
    }

    /// The catalog project the workspace belongs to.
    #[must_use]
    pub fn project_id(&self) -> ProjectId {
        self.project_id
    }

    /// The repository's shared mutation domain, from
    /// [`harkness_git::repository_identity`].
    #[must_use]
    pub fn repository_identity(&self) -> &str {
        &self.repository_identity
    }

    /// The canonicalized worktree root, the only absolute path a snapshot holds.
    #[must_use]
    pub fn worktree_root(&self) -> &Path {
        &self.worktree_root
    }

    /// The checked-out commit, or `None` on an unborn branch.
    #[must_use]
    pub fn head(&self) -> Option<&str> {
        self.head.as_deref()
    }

    /// The checked-out branch, or `None` when `HEAD` is detached.
    #[must_use]
    pub fn branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }

    /// The three path sets this identity covers.
    #[must_use]
    pub fn files(&self) -> &SnapshotFiles {
        &self.files
    }

    /// Digest over staged paths and their blob ids.
    #[must_use]
    pub fn index_digest(&self) -> &Sha256Hex {
        &self.index_digest
    }

    /// Digest over modified tracked paths and their content hashes.
    #[must_use]
    pub fn tracked_dirty_digest(&self) -> &Sha256Hex {
        &self.tracked_dirty_digest
    }

    /// Digest over untracked eligible paths and their content hashes.
    #[must_use]
    pub fn untracked_digest(&self) -> &Sha256Hex {
        &self.untracked_digest
    }

    /// Digest over the discovered instruction set.
    #[must_use]
    pub fn instructions_digest(&self) -> &Sha256Hex {
        &self.instructions_digest
    }

    /// The configuration generation this snapshot was taken under.
    #[must_use]
    pub const fn config_generation(&self) -> u64 {
        self.config_generation
    }

    /// The index generation this snapshot was taken against.
    #[must_use]
    pub const fn index_generation(&self) -> u64 {
        self.index_generation
    }

    /// When the capture happened.
    #[must_use]
    pub const fn captured_at(&self) -> OffsetDateTime {
        self.captured_at
    }

    /// The composite identity: every field except [`Self::id`] and
    /// [`Self::captured_at`].
    ///
    /// Capturing one unchanged workspace twice yields the same value. The two
    /// exclusions are exactly what a second capture must be allowed to change.
    #[must_use]
    pub fn digest(&self) -> SnapshotDigest {
        let mut writer = DigestWriter::new(DOMAIN_SNAPSHOT);
        writer
            .field(self.project_id.to_string().as_bytes())
            .field(self.repository_identity.as_bytes())
            .field(RepoPath::from_path(&self.worktree_root).as_bytes())
            .optional_field(self.head.as_deref().map(str::as_bytes))
            .optional_field(self.branch.as_deref().map(str::as_bytes))
            .field(self.index_digest.as_str().as_bytes())
            .field(self.tracked_dirty_digest.as_str().as_bytes())
            .field(self.untracked_digest.as_str().as_bytes())
            .field(self.instructions_digest.as_str().as_bytes())
            .integer(self.config_generation)
            .integer(self.index_generation);
        SnapshotDigest(writer.finish())
    }

    /// Refuses a snapshot whose identity is not the one expected.
    ///
    /// The check callers reach for before acting on a snapshot they were handed
    /// rather than one they captured.
    pub fn require_digest(&self, expected: &SnapshotDigest) -> Result<(), ContextDomainError> {
        let found = self.digest();
        if &found == expected {
            return Ok(());
        }
        Err(ContextDomainError::DigestMismatch {
            component: "workspace_snapshot",
            expected: expected.to_string(),
            found: found.to_string(),
        })
    }

    /// Reads the current state of `git`'s worktree.
    ///
    /// Blocking. `cancellation` is polled between status entries, while an
    /// untracked directory is walked, and between the blocks of one file's
    /// content, so a cancelled capture returns promptly whatever the size of the
    /// workspace — including a workspace that is one very large file. It returns
    /// [`ContextDomainError::SnapshotCancelled`] rather than a partial
    /// identity.
    ///
    /// One unreadable file does not fail a capture: it contributes
    /// [`ContentDigest::Unreadable`] and a line in the diagnostics reachable
    /// through [`Self::capture_with_diagnostics`].
    pub fn capture(
        request: &CaptureRequest,
        git: &GitService,
        probe: &dyn WorkspaceProbe,
        cancellation: &Cancellation,
    ) -> Result<Self, ContextDomainError> {
        Self::capture_with_diagnostics(request, git, probe, cancellation)
            .map(|capture| capture.snapshot)
    }

    /// Captures, and reports what reading the workspace involved.
    pub fn capture_with_diagnostics(
        request: &CaptureRequest,
        git: &GitService,
        probe: &dyn WorkspaceProbe,
        cancellation: &Cancellation,
    ) -> Result<Capture, ContextDomainError> {
        let started = Instant::now();
        let mut diagnostics = CaptureDiagnostics::default();
        let collected = collect(git, probe, cancellation, &mut diagnostics)
            .map_err(|failure| failure.into_capture_error(git.root()))?;
        diagnostics.duration = started.elapsed();
        Ok(Capture {
            snapshot: Self::assemble(
                SnapshotId::new(),
                request,
                collected.repository_identity,
                collected.worktree_root,
                collected.head,
                collected.branch,
                collected.files,
                OffsetDateTime::now_utc(),
            ),
            diagnostics,
        })
    }

    /// Recomputes the workspace's state and compares it with this snapshot.
    ///
    /// Never mutates anything and never hashes the whole tree: the cost is one
    /// `git status` plus hashing the paths Git reports as dirty or untracked, so
    /// it is bounded by the size of uncommitted work rather than by the
    /// repository.
    ///
    /// The instruction, configuration, and index-generation components are the
    /// caller's own knowledge rather than facts about the worktree, so they are
    /// carried across unchanged. A generation bump is a deliberate invalidation
    /// its owner already knows about, and it is compared by [`Self::digest`]
    /// equality; this method answers the question the caller cannot answer
    /// alone, which is whether the *files* moved.
    pub fn verify(
        &self,
        git: &GitService,
        probe: &dyn WorkspaceProbe,
        cancellation: &Cancellation,
    ) -> Result<FreshnessState, ContextDomainError> {
        self.verify_with_diagnostics(git, probe, cancellation)
            .map(|verification| verification.state)
    }

    /// Verifies, and reports what re-reading the workspace involved.
    pub fn verify_with_diagnostics(
        &self,
        git: &GitService,
        probe: &dyn WorkspaceProbe,
        cancellation: &Cancellation,
    ) -> Result<Verification, ContextDomainError> {
        let reading = WorkspaceReading::capture(git, probe, cancellation)?;
        Ok(Verification {
            state: self.verify_against(&reading),
            diagnostics: reading.diagnostics,
        })
    }

    /// Compares this snapshot with a workspace read somebody else performed.
    ///
    /// Pure: the reading is the only I/O, and it is already done. A caller
    /// holding several snapshots of one workspace — a projection of every
    /// recorded check, say — reads once and answers all of them, instead of
    /// running one `git status` and one hash of the dirty set per snapshot.
    #[must_use]
    pub fn verify_against(&self, reading: &WorkspaceReading) -> FreshnessState {
        let collected = match &reading.outcome {
            Ok(collected) => collected,
            Err(reason) => return FreshnessState::Unverifiable { reason: *reason },
        };

        let mut changed = Vec::new();
        if collected.repository_identity != self.repository_identity {
            changed.push(StalePath {
                path: None,
                component: SnapshotComponent::RepositoryIdentity,
                change: PathDivergence::Changed,
            });
        }
        if collected.worktree_root != self.worktree_root {
            changed.push(StalePath {
                path: None,
                component: SnapshotComponent::WorktreeRoot,
                change: PathDivergence::Changed,
            });
        }
        push_scalar_divergence(
            SnapshotComponent::Head,
            self.head.as_deref(),
            collected.head.as_deref(),
            &mut changed,
        );
        push_scalar_divergence(
            SnapshotComponent::Branch,
            self.branch.as_deref(),
            collected.branch.as_deref(),
            &mut changed,
        );
        push_set_divergence(
            SnapshotComponent::Index,
            self.files.staged(),
            collected.files.staged(),
            &mut changed,
        );
        push_set_divergence(
            SnapshotComponent::TrackedDirty,
            self.files.tracked_dirty(),
            collected.files.tracked_dirty(),
            &mut changed,
        );
        push_set_divergence(
            SnapshotComponent::Untracked,
            self.files.untracked(),
            collected.files.untracked(),
            &mut changed,
        );

        if changed.is_empty() {
            FreshnessState::Fresh
        } else {
            FreshnessState::Stale { changed }
        }
    }
}

/// One re-read of a workspace, reusable by every snapshot compared against it.
///
/// [`WorkspaceSnapshot::verify`] performs this read itself, which is right when
/// there is one snapshot and wasteful when there are many: the read is a `git
/// status` plus a hash of everything Git calls dirty or untracked, and it is the
/// same read for every snapshot of the same workspace at the same moment.
///
/// A failed read is a value here rather than an error, because verification
/// always owes a verdict — every snapshot compared against an unreadable
/// workspace is `Unverifiable` for the same reason.
#[derive(Debug)]
pub struct WorkspaceReading {
    outcome: Result<Collected, UnverifiableReason>,
    diagnostics: CaptureDiagnostics,
}

impl WorkspaceReading {
    /// Reads the workspace once.
    ///
    /// # Errors
    ///
    /// Returns [`ContextDomainError::HashingFailed`] when a path Git named could
    /// not be hashed. Every other failure to read is carried as an
    /// `Unverifiable` verdict rather than an error.
    pub fn capture(
        git: &GitService,
        probe: &dyn WorkspaceProbe,
        cancellation: &Cancellation,
    ) -> Result<Self, ContextDomainError> {
        let started = Instant::now();
        let mut diagnostics = CaptureDiagnostics::default();
        let outcome = match collect(git, probe, cancellation, &mut diagnostics) {
            Ok(collected) => Ok(collected),
            Err(CollectFailure::Hashing { path, reason }) => {
                return Err(ContextDomainError::HashingFailed {
                    path: path.display(),
                    reason,
                });
            }
            Err(failure) => Err(failure.into_unverifiable_reason()),
        };
        diagnostics.duration = started.elapsed();
        Ok(Self {
            outcome,
            diagnostics,
        })
    }

    /// What this read involved.
    #[must_use]
    pub const fn diagnostics(&self) -> &CaptureDiagnostics {
        &self.diagnostics
    }
}

/// Reports a present/absent/changed scalar component.
fn push_scalar_divergence(
    component: SnapshotComponent,
    before: Option<&str>,
    after: Option<&str>,
    changed: &mut Vec<StalePath>,
) {
    let change = match (before, after) {
        (Some(before), Some(after)) if before == after => return,
        (None, None) => return,
        (None, Some(_)) => PathDivergence::Added,
        (Some(_), None) => PathDivergence::Removed,
        (Some(_), Some(_)) => PathDivergence::Changed,
    };
    changed.push(StalePath {
        path: None,
        component,
        change,
    });
}

/// Merge-joins two sorted path sets and names every difference.
fn push_set_divergence(
    component: SnapshotComponent,
    before: &[FileDigestEntry],
    after: &[FileDigestEntry],
    changed: &mut Vec<StalePath>,
) {
    let (mut left, mut right) = (0, 0);
    let mut record = |path: &RepoPath, change| {
        changed.push(StalePath {
            path: Some(path.clone()),
            component,
            change,
        });
    };
    while left < before.len() && right < after.len() {
        match before[left].path.cmp(&after[right].path) {
            std::cmp::Ordering::Equal => {
                if before[left].digest != after[right].digest {
                    record(&before[left].path, PathDivergence::Changed);
                }
                left += 1;
                right += 1;
            }
            std::cmp::Ordering::Less => {
                record(&before[left].path, PathDivergence::Removed);
                left += 1;
            }
            std::cmp::Ordering::Greater => {
                record(&after[right].path, PathDivergence::Added);
                right += 1;
            }
        }
    }
    for entry in &before[left..] {
        record(&entry.path, PathDivergence::Removed);
    }
    for entry in &after[right..] {
        record(&entry.path, PathDivergence::Added);
    }
}

/// The workspace facts one read of the worktree produces.
#[derive(Debug)]
struct Collected {
    repository_identity: String,
    worktree_root: PathBuf,
    head: Option<String>,
    branch: Option<String>,
    files: SnapshotFiles,
}

/// Why one read of the worktree could not finish.
///
/// Capture and verification disagree about what to do with these, which is why
/// they are a private enum rather than a [`ContextDomainError`]: capture must not
/// yield a half-built identity, while verification always owes a verdict.
enum CollectFailure {
    RepositoryUnavailable(String),
    WorktreeRootMissing,
    StatusUnavailable(String),
    Cancelled,
    Hashing { path: RepoPath, reason: String },
}

impl CollectFailure {
    fn into_capture_error(self, root: &Path) -> ContextDomainError {
        match self {
            Self::RepositoryUnavailable(reason) => ContextDomainError::RepositoryUnavailable {
                path: root.to_path_buf(),
                reason,
            },
            Self::WorktreeRootMissing => ContextDomainError::WorktreeRootMissing {
                path: root.to_path_buf(),
            },
            Self::StatusUnavailable(reason) => ContextDomainError::RepositoryUnavailable {
                path: root.to_path_buf(),
                reason,
            },
            Self::Cancelled => ContextDomainError::SnapshotCancelled,
            Self::Hashing { path, reason } => ContextDomainError::HashingFailed {
                path: path.display(),
                reason,
            },
        }
    }

    fn into_unverifiable_reason(self) -> UnverifiableReason {
        match self {
            Self::RepositoryUnavailable(_) => UnverifiableReason::RepositoryUnavailable,
            Self::WorktreeRootMissing => UnverifiableReason::WorktreeRootMissing,
            Self::StatusUnavailable(_) => UnverifiableReason::StatusUnavailable,
            Self::Cancelled => UnverifiableReason::Cancelled,
            // Handled by the caller, which turns a fatal probe failure into an
            // error rather than a verdict.
            Self::Hashing { .. } => UnverifiableReason::StatusUnavailable,
        }
    }
}

/// Reads every workspace-derived component of an identity, once.
fn collect(
    git: &GitService,
    probe: &dyn WorkspaceProbe,
    cancellation: &Cancellation,
    diagnostics: &mut CaptureDiagnostics,
) -> Result<Collected, CollectFailure> {
    if cancellation.is_cancelled() {
        return Err(CollectFailure::Cancelled);
    }
    // Before anything is read, so a probe that caches cannot answer this read
    // from the last one.
    probe.begin_read();
    let root = git.root();
    let worktree_root =
        std::fs::canonicalize(root).map_err(|_| CollectFailure::WorktreeRootMissing)?;
    if !worktree_root.is_dir() {
        return Err(CollectFailure::WorktreeRootMissing);
    }
    let repository_identity = harkness_git::repository_identity(&worktree_root)
        .map_err(|error| CollectFailure::RepositoryUnavailable(error.to_string()))?;

    let head = head_commit(&worktree_root)?;
    let status = git
        .detailed_status(cancellation)
        .map_err(|error| match error {
            GitError::Cancelled => CollectFailure::Cancelled,
            error => CollectFailure::StatusUnavailable(error.to_string()),
        })?;
    let branch = branch_name(&status);

    let mut staged = Vec::new();
    let mut tracked_dirty = Vec::new();
    let mut untracked = Vec::new();
    for entry in &status.entries {
        if cancellation.is_cancelled() {
            return Err(CollectFailure::Cancelled);
        }
        collect_entry(
            probe,
            cancellation,
            diagnostics,
            entry,
            &mut staged,
            &mut tracked_dirty,
            &mut untracked,
        )?;
    }

    Ok(Collected {
        repository_identity,
        worktree_root,
        head,
        branch,
        files: SnapshotFiles::new(staged, tracked_dirty, untracked),
    })
}

/// Splits one status entry across the three path sets it can contribute to.
fn collect_entry(
    probe: &dyn WorkspaceProbe,
    cancellation: &Cancellation,
    diagnostics: &mut CaptureDiagnostics,
    entry: &StatusEntry,
    staged: &mut Vec<FileDigestEntry>,
    tracked_dirty: &mut Vec<FileDigestEntry>,
    untracked: &mut Vec<FileDigestEntry>,
) -> Result<(), CollectFailure> {
    let path = RepoPath::from_path(&entry.path);

    if entry.staged.is_some() {
        // A rename's source path leaves the index too, and a set that named only
        // the destination would compare equal to one where the source is still
        // staged under its old name.
        //
        // A *copy* carries the same `rename_source` and means the opposite: the
        // source is still in the index, unchanged. Recording it as `Absent`
        // would state a falsehood, and would give a staged copy the same
        // `index_digest` as a staged delete of the source beside a staged add of
        // the destination — two different index states, one identity. Git only
        // reports copies when `status.renames=copies` is configured, which is
        // exactly the sort of setting an identity must not quietly depend on.
        if entry.staged == Some(FileChange::Renamed)
            && let Some(source) = entry.rename_source.as_ref()
        {
            let source = RepoPath::from_path(source);
            staged.push(FileDigestEntry::new(source, ContentDigest::Absent));
        }
        let blob = probe_staged_blob(probe, diagnostics, &path)?;
        staged.push(FileDigestEntry::new(path.clone(), blob));
    }

    let is_untracked = entry.unstaged == Some(FileChange::Untracked);
    if is_untracked {
        let expanded = match probe.expand_untracked(&path, cancellation) {
            Ok(expanded) => expanded,
            Err(failure) => match refuse(&path, &failure) {
                Some(refusal) => return Err(refusal),
                // The candidate could not be read at all, so it is opaque: one
                // sentinel under its own name. A failure *inside* the tree never
                // reaches here — the probe reports those per sub-path, precisely
                // so that the rest of the tree keeps taking part in identity.
                None => {
                    // Stripped, so this route spells a directory the same way
                    // the per-sub-path route does. Otherwise a directory that
                    // switched failure mode between two captures would report a
                    // removed `node_modules/` beside an added `node_modules`
                    // while nothing moved.
                    let path = path.without_trailing_separator();
                    diagnostics.paths_skipped.push(SkippedPath {
                        path: path.clone(),
                        reason: failure.reason().to_owned(),
                    });
                    untracked.push(FileDigestEntry::new(path, ContentDigest::Unreadable));
                    return Ok(());
                }
            },
        };
        for candidate in expanded.paths {
            if cancellation.is_cancelled() {
                return Err(CollectFailure::Cancelled);
            }
            let digest = probe_content(probe, cancellation, diagnostics, &candidate)?;
            untracked.push(FileDigestEntry::new(candidate, digest));
        }
        for unreadable in expanded.unreadable {
            diagnostics.paths_skipped.push(SkippedPath {
                path: unreadable.path.clone(),
                reason: unreadable.reason,
            });
            untracked.push(FileDigestEntry::new(
                unreadable.path,
                ContentDigest::Unreadable,
            ));
        }
        return Ok(());
    }

    // A conflicted path has no single staged version, and it is work the model
    // may be asked to resolve, so it belongs to the tracked-dirty set even when
    // Git reports no unstaged change beside the conflict.
    if entry.unstaged.is_some() || entry.conflicted {
        let digest = probe_content(probe, cancellation, diagnostics, &path)?;
        tracked_dirty.push(FileDigestEntry::new(path, digest));
    }
    Ok(())
}

/// Translates a probe failure the read must not survive, or `None` for a skip.
fn refuse(path: &RepoPath, failure: &ProbeFailure) -> Option<CollectFailure> {
    if failure.is_cancelled() {
        return Some(CollectFailure::Cancelled);
    }
    failure.is_fatal().then(|| CollectFailure::Hashing {
        path: path.clone(),
        reason: failure.reason().to_owned(),
    })
}

/// Hashes one path, downgrading an ordinary read failure to the sentinel.
fn probe_content(
    probe: &dyn WorkspaceProbe,
    cancellation: &Cancellation,
    diagnostics: &mut CaptureDiagnostics,
    path: &RepoPath,
) -> Result<ContentDigest, CollectFailure> {
    match probe.hash_path(path, cancellation) {
        Ok(digest) => {
            diagnostics.paths_hashed += 1;
            Ok(digest)
        }
        Err(failure) => match refuse(path, &failure) {
            Some(refusal) => Err(refusal),
            None => {
                diagnostics.paths_skipped.push(SkippedPath {
                    path: path.clone(),
                    reason: failure.reason().to_owned(),
                });
                Ok(ContentDigest::Unreadable)
            }
        },
    }
}

/// Reads one staged blob id, downgrading an ordinary failure to the sentinel.
fn probe_staged_blob(
    probe: &dyn WorkspaceProbe,
    diagnostics: &mut CaptureDiagnostics,
    path: &RepoPath,
) -> Result<ContentDigest, CollectFailure> {
    match probe.staged_blob_id(path) {
        Ok(Some(id)) => Ok(ContentDigest::StagedBlob(id)),
        Ok(None) => Ok(ContentDigest::Absent),
        Err(failure) => match refuse(path, &failure) {
            Some(refusal) => Err(refusal),
            None => {
                diagnostics.paths_skipped.push(SkippedPath {
                    path: path.clone(),
                    reason: failure.reason().to_owned(),
                });
                Ok(ContentDigest::Unreadable)
            }
        },
    }
}

/// The branch `HEAD` names, or `None` when it is detached.
///
/// The three cases must stay distinguishable: an unborn branch has a name and no
/// commit, a detached head has a commit and no name, and a branch has both. That
/// is what keeps a detached checkout from comparing equal to the branch sitting
/// at the same commit.
fn branch_name(status: &DetailedStatus) -> Option<String> {
    match &status.head {
        HeadState::Unborn { branch } => branch.clone(),
        HeadState::Branch { name } => Some(name.clone()),
        HeadState::Detached { .. } => None,
    }
}

/// Resolves the commit `HEAD` points at, in process.
///
/// Porcelain status reports a commit only for a detached head, so a branch
/// checkout is resolved from the repository. Inspection only, which is what
/// libgit2 is used for here.
fn head_commit(root: &Path) -> Result<Option<String>, CollectFailure> {
    use harkness_git::git2::{ErrorCode, Repository};

    let repository = Repository::open(root)
        .map_err(|error| CollectFailure::RepositoryUnavailable(error.to_string()))?;
    match repository.head() {
        Ok(head) => Ok(head.target().map(|commit| commit.to_string())),
        Err(error) if matches!(error.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => {
            Ok(None)
        }
        Err(error) => Err(CollectFailure::RepositoryUnavailable(error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use harkness_core::ProjectId;
    use harkness_git::{Cancellation, FileChange, GitService, StatusEntry};
    use harkness_test_fixtures::{Fixture, commit_all, git, initialize_repository};

    use super::{
        CaptureRequest, FileDigestEntry, FreshnessState, PathDivergence, SnapshotComponent,
        SnapshotFiles, StalePath, UnverifiableReason, WorkspaceReading, WorkspaceSnapshot,
        path_set_digest,
    };
    use crate::digest::empty_path_set_digest;
    use crate::path::RepoPath;
    use crate::probe::{ContentDigest, FilesystemProbe, ProbeFailure, WorkspaceProbe};

    struct Workspace {
        fixture: Fixture,
        root: std::path::PathBuf,
        request: CaptureRequest,
    }

    impl Workspace {
        fn new(name: &str) -> Self {
            let fixture = Fixture::new();
            let root = fixture.directory(name);
            initialize_repository(&root);
            let request = CaptureRequest::new(ProjectId::new());
            Self {
                fixture,
                root,
                request,
            }
        }

        fn service(&self) -> GitService {
            GitService::new(&self.root, &self.fixture.data_dir)
        }

        fn capture(&self) -> WorkspaceSnapshot {
            let probe = FilesystemProbe::new(&self.root);
            WorkspaceSnapshot::capture(
                &self.request,
                &self.service(),
                &probe,
                &Cancellation::default(),
            )
            .unwrap()
        }

        fn verify(&self, snapshot: &WorkspaceSnapshot) -> FreshnessState {
            let probe = FilesystemProbe::new(&self.root);
            snapshot
                .verify(&self.service(), &probe, &Cancellation::default())
                .unwrap()
        }

        fn read(&self) -> WorkspaceReading {
            let probe = FilesystemProbe::new(&self.root);
            WorkspaceReading::capture(&self.service(), &probe, &Cancellation::default()).unwrap()
        }

        fn write(&self, relative: &str, content: &str) {
            let path = self.root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, content).unwrap();
        }
    }

    fn entry(path: &str, content: &[u8]) -> FileDigestEntry {
        FileDigestEntry::new(
            RepoPath::from_bytes(path.as_bytes().to_vec()),
            ContentDigest::of_content(content),
        )
    }

    #[test]
    fn the_four_workspace_states_produce_four_distinct_digests() {
        let workspace = Workspace::new("repo");
        let clean = workspace.capture();

        workspace.write("staged.txt", "staged\n");
        git(&workspace.root, ["add", "staged.txt"]);
        let staged_only = workspace.capture();

        workspace.write("tracked.txt", "modified\n");
        let dirty = workspace.capture();

        workspace.write("untracked.txt", "new\n");
        let dirty_and_untracked = workspace.capture();

        let digests = [
            clean.digest(),
            staged_only.digest(),
            dirty.digest(),
            dirty_and_untracked.digest(),
        ];
        let mut unique = digests.to_vec();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 4, "states collided: {digests:?}");

        assert!(clean.files().is_empty());
        assert_eq!(clean.index_digest(), &empty_path_set_digest());
        assert_eq!(staged_only.files().staged().len(), 1);
        assert_eq!(dirty.files().tracked_dirty().len(), 1);
        assert_eq!(dirty_and_untracked.files().untracked().len(), 1);
    }

    #[test]
    fn capturing_an_unchanged_workspace_twice_yields_one_digest_and_two_ids() {
        let workspace = Workspace::new("repo");
        let first = workspace.capture();
        let second = workspace.capture();
        assert_eq!(first.digest(), second.digest());
        assert_ne!(first.id(), second.id());
        assert!(first.require_digest(&second.digest()).is_ok());
    }

    #[test]
    fn two_clean_worktrees_at_one_commit_have_different_digests() {
        let workspace = Workspace::new("repo");
        let linked = workspace.fixture.root.path().join("linked");
        git(
            &workspace.root,
            [
                "worktree",
                "add",
                "-b",
                "linked",
                linked.to_str().unwrap(),
                "HEAD",
            ],
        );

        let main = workspace.capture();
        let linked_probe = FilesystemProbe::new(&linked);
        let linked_snapshot = WorkspaceSnapshot::capture(
            &workspace.request,
            &GitService::new(&linked, &workspace.fixture.data_dir),
            &linked_probe,
            &Cancellation::default(),
        )
        .unwrap();

        assert_eq!(
            main.repository_identity(),
            linked_snapshot.repository_identity(),
            "linked worktrees share one repository"
        );
        assert_eq!(
            main.head(),
            linked_snapshot.head(),
            "both sit at one commit"
        );
        assert!(main.files().is_empty() && linked_snapshot.files().is_empty());
        assert_ne!(
            main.digest(),
            linked_snapshot.digest(),
            "two clean worktrees at one commit must stay distinguishable"
        );

        // The branch names differ above because Git will not check one branch
        // out twice, so the root alone is varied here to show it is load-bearing
        // on its own.
        let relocated = WorkspaceSnapshot::assemble(
            main.id(),
            &CaptureRequest::new(main.project_id()),
            main.repository_identity().to_owned(),
            linked.clone(),
            main.head().map(str::to_owned),
            main.branch().map(str::to_owned),
            main.files().clone(),
            main.captured_at(),
        );
        assert_ne!(main.digest(), relocated.digest());
    }

    #[test]
    fn verification_is_fresh_until_one_byte_changes_and_then_names_the_path() {
        let workspace = Workspace::new("repo");
        workspace.write("tracked.txt", "modified\n");
        let snapshot = workspace.capture();
        assert_eq!(workspace.verify(&snapshot), FreshnessState::Fresh);

        workspace.write("tracked.txt", "modified!\n");
        let state = workspace.verify(&snapshot);
        assert_eq!(
            state,
            FreshnessState::Stale {
                changed: vec![StalePath {
                    path: Some(RepoPath::from_bytes(b"tracked.txt".to_vec())),
                    component: SnapshotComponent::TrackedDirty,
                    change: PathDivergence::Changed,
                }],
            }
        );
        assert!(!state.is_fresh());
    }

    #[test]
    fn a_new_untracked_file_makes_a_clean_snapshot_stale() {
        let workspace = Workspace::new("repo");
        let snapshot = workspace.capture();
        workspace.write("appeared.txt", "new\n");
        assert_eq!(
            workspace.verify(&snapshot),
            FreshnessState::Stale {
                changed: vec![StalePath {
                    path: Some(RepoPath::from_bytes(b"appeared.txt".to_vec())),
                    component: SnapshotComponent::Untracked,
                    change: PathDivergence::Added,
                }],
            }
        );
    }

    #[test]
    fn verification_is_unverifiable_once_the_worktree_root_is_removed() {
        let workspace = Workspace::new("repo");
        let snapshot = workspace.capture();
        fs::remove_dir_all(&workspace.root).unwrap();
        assert_eq!(
            workspace.verify(&snapshot),
            FreshnessState::Unverifiable {
                reason: UnverifiableReason::WorktreeRootMissing,
            }
        );
    }

    #[test]
    fn verification_is_unverifiable_once_the_repository_is_removed() {
        let workspace = Workspace::new("repo");
        let snapshot = workspace.capture();
        fs::remove_dir_all(workspace.root.join(".git")).unwrap();
        assert_eq!(
            workspace.verify(&snapshot),
            FreshnessState::Unverifiable {
                reason: UnverifiableReason::RepositoryUnavailable,
            }
        );
    }

    #[test]
    fn a_cancelled_verification_reports_a_verdict_rather_than_an_error() {
        let workspace = Workspace::new("repo");
        let snapshot = workspace.capture();
        let cancellation = Cancellation::default();
        cancellation.cancel();
        let probe = FilesystemProbe::new(&workspace.root);
        assert_eq!(
            snapshot
                .verify(&workspace.service(), &probe, &cancellation)
                .unwrap(),
            FreshnessState::Unverifiable {
                reason: UnverifiableReason::Cancelled,
            }
        );
    }

    #[test]
    fn a_cancelled_capture_returns_promptly_and_yields_no_snapshot() {
        let workspace = Workspace::new("repo");
        for index in 0..1_000 {
            workspace.write(&format!("bulk/file-{index}.txt"), "content\n");
        }
        let cancellation = Cancellation::default();
        cancellation.cancel();
        let probe = FilesystemProbe::new(&workspace.root);

        let error = WorkspaceSnapshot::capture(
            &workspace.request,
            &workspace.service(),
            &probe,
            &cancellation,
        )
        .unwrap_err();
        assert_eq!(error.kind(), "snapshot_cancelled");
    }

    #[test]
    fn capture_fails_when_the_worktree_root_is_gone() {
        let workspace = Workspace::new("repo");
        let service = workspace.service();
        let probe = FilesystemProbe::new(&workspace.root);
        fs::remove_dir_all(&workspace.root).unwrap();
        let error = WorkspaceSnapshot::capture(
            &workspace.request,
            &service,
            &probe,
            &Cancellation::default(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), "worktree_root_missing");
    }

    #[test]
    fn an_unborn_branch_captures_with_no_head_and_a_named_branch() {
        let fixture = Fixture::new();
        let root = fixture.directory("unborn");
        let repository = harkness_git::git2::Repository::init(&root).unwrap();
        repository.set_head("refs/heads/main").unwrap();
        let probe = FilesystemProbe::new(&root);
        let snapshot = WorkspaceSnapshot::capture(
            &CaptureRequest::new(ProjectId::new()),
            &GitService::new(&root, &fixture.data_dir),
            &probe,
            &Cancellation::default(),
        )
        .unwrap();

        assert_eq!(snapshot.head(), None);
        assert_eq!(snapshot.branch(), Some("main"));
    }

    #[test]
    fn a_detached_head_captures_with_a_commit_and_no_branch() {
        let workspace = Workspace::new("repo");
        let on_branch = workspace.capture();
        let commit = git(&workspace.root, ["rev-parse", "HEAD"])
            .trim()
            .to_owned();
        git(&workspace.root, ["checkout", "--detach", &commit]);
        let detached = workspace.capture();

        assert_eq!(detached.head(), Some(commit.as_str()));
        assert_eq!(detached.branch(), None);
        assert_eq!(on_branch.branch(), Some("main"));
        assert_ne!(
            on_branch.digest(),
            detached.digest(),
            "a detached checkout must not equal the branch at the same commit"
        );
    }

    #[test]
    fn moving_head_changes_the_identity() {
        let workspace = Workspace::new("repo");
        let before = workspace.capture();
        workspace.write("second.txt", "second\n");
        let repository = harkness_git::git2::Repository::open(&workspace.root).unwrap();
        commit_all(&repository, "second");
        let after = workspace.capture();
        assert_ne!(before.digest(), after.digest());
    }

    #[test]
    fn a_staged_change_that_is_then_committed_is_a_different_workspace() {
        let workspace = Workspace::new("repo");
        workspace.write("staged.txt", "staged\n");
        git(&workspace.root, ["add", "staged.txt"]);
        let staged = workspace.capture();
        let repository = harkness_git::git2::Repository::open(&workspace.root).unwrap();
        commit_all(&repository, "commit the staged file");
        let committed = workspace.capture();
        assert_ne!(staged.digest(), committed.digest());
    }

    #[test]
    fn path_set_digests_ignore_the_order_paths_arrive_in() {
        let forward = vec![
            entry("a.txt", b"a"),
            entry("b.txt", b"b"),
            entry("nested/c.txt", b"c"),
            entry("z.txt", b"z"),
        ];
        let mut shuffles = Vec::new();
        for rotation in 0..forward.len() {
            let mut order = forward.clone();
            order.rotate_left(rotation);
            shuffles.push(order);
        }
        let mut reversed = forward.clone();
        reversed.reverse();
        shuffles.push(reversed);

        let expected = path_set_digest(&super::canonicalize_entries(forward));
        for order in shuffles {
            let files = SnapshotFiles::new(order, Vec::new(), Vec::new());
            assert_eq!(path_set_digest(files.staged()), expected);
        }
    }

    #[test]
    fn every_snapshot_vocabulary_serializes_as_its_snake_case_spelling() {
        let components = [
            (SnapshotComponent::RepositoryIdentity, "repository_identity"),
            (SnapshotComponent::WorktreeRoot, "worktree_root"),
            (SnapshotComponent::Head, "head"),
            (SnapshotComponent::Branch, "branch"),
            (SnapshotComponent::Index, "index"),
            (SnapshotComponent::TrackedDirty, "tracked_dirty"),
            (SnapshotComponent::Untracked, "untracked"),
        ];
        for (component, spelling) in components {
            let json = serde_json::to_string(&component).unwrap();
            assert_eq!(json, format!("\"{spelling}\""));
            assert_eq!(
                serde_json::from_str::<SnapshotComponent>(&json).unwrap(),
                component
            );
            assert_eq!(component.as_str(), spelling);
            assert_eq!(component.to_string(), spelling);
        }

        for (divergence, spelling) in [
            (PathDivergence::Added, "added"),
            (PathDivergence::Removed, "removed"),
            (PathDivergence::Changed, "changed"),
        ] {
            let json = serde_json::to_string(&divergence).unwrap();
            assert_eq!(json, format!("\"{spelling}\""));
            assert_eq!(
                serde_json::from_str::<PathDivergence>(&json).unwrap(),
                divergence
            );
        }

        for (reason, spelling) in [
            (
                UnverifiableReason::RepositoryUnavailable,
                "repository_unavailable",
            ),
            (
                UnverifiableReason::WorktreeRootMissing,
                "worktree_root_missing",
            ),
            (UnverifiableReason::StatusUnavailable, "status_unavailable"),
            (UnverifiableReason::Cancelled, "cancelled"),
        ] {
            let json = serde_json::to_string(&reason).unwrap();
            assert_eq!(json, format!("\"{spelling}\""));
            assert_eq!(
                serde_json::from_str::<UnverifiableReason>(&json).unwrap(),
                reason
            );
        }
    }

    #[test]
    fn a_freshness_verdict_serializes_as_a_tagged_object() {
        assert_eq!(
            serde_json::to_string(&FreshnessState::Fresh).unwrap(),
            r#"{"state":"fresh"}"#
        );
        let stale = FreshnessState::Stale {
            changed: vec![StalePath {
                path: Some(RepoPath::from_bytes(b"a.txt".to_vec())),
                component: SnapshotComponent::Untracked,
                change: PathDivergence::Added,
            }],
        };
        assert_eq!(
            serde_json::to_string(&stale).unwrap(),
            r#"{"state":"stale","changed":[{"path":"a.txt","component":"untracked","change":"added"}]}"#
        );
        assert_eq!(
            serde_json::from_str::<FreshnessState>(&serde_json::to_string(&stale).unwrap())
                .unwrap(),
            stale
        );
        assert_eq!(
            serde_json::to_string(&FreshnessState::Unverifiable {
                reason: UnverifiableReason::Cancelled,
            })
            .unwrap(),
            r#"{"state":"unverifiable","reason":"cancelled"}"#
        );
    }

    #[test]
    fn deduplication_prefers_a_reading_over_the_absent_marker_either_way_round() {
        let blob = ContentDigest::StagedBlob("0123456789abcdef".to_owned());
        for order in [
            vec![
                FileDigestEntry::new(
                    RepoPath::from_bytes(b"a.txt".to_vec()),
                    ContentDigest::Absent,
                ),
                FileDigestEntry::new(RepoPath::from_bytes(b"a.txt".to_vec()), blob.clone()),
            ],
            vec![
                FileDigestEntry::new(RepoPath::from_bytes(b"a.txt".to_vec()), blob.clone()),
                FileDigestEntry::new(
                    RepoPath::from_bytes(b"a.txt".to_vec()),
                    ContentDigest::Absent,
                ),
            ],
        ] {
            let canonical = super::canonicalize_entries(order);
            assert_eq!(canonical.len(), 1);
            assert_eq!(canonical[0].digest, blob);
        }
    }

    #[test]
    fn a_repeated_path_counts_once() {
        let files = SnapshotFiles::new(
            vec![entry("a.txt", b"a"), entry("a.txt", b"a")],
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(files.staged().len(), 1);
    }

    #[test]
    fn an_unreadable_path_is_recorded_rather_than_failing_the_capture() {
        let workspace = Workspace::new("repo");
        // A directory Git reports as one untracked entry, containing a path the
        // probe refuses: the capture must still produce an identity.
        fs::create_dir(workspace.root.join("untracked")).unwrap();
        fs::create_dir(workspace.root.join("untracked/inner")).unwrap();
        workspace.write("untracked/inner/file.txt", "content\n");

        let probe = FilesystemProbe::new(&workspace.root);
        let capture = WorkspaceSnapshot::capture_with_diagnostics(
            &workspace.request,
            &workspace.service(),
            &probe,
            &Cancellation::default(),
        )
        .unwrap();
        assert_eq!(capture.diagnostics.paths_hashed, 1);
        assert!(capture.diagnostics.paths_skipped.is_empty());
        assert_eq!(
            capture
                .snapshot
                .files()
                .untracked()
                .iter()
                .map(|entry| entry.path.display())
                .collect::<Vec<_>>(),
            ["untracked/inner/file.txt"]
        );
    }

    #[test]
    fn a_symlink_changes_identity_without_its_target_being_read() {
        #[cfg(unix)]
        {
            let workspace = Workspace::new("repo");
            let outside = workspace.fixture.root.path().join("outside.txt");
            fs::write(&outside, "secret\n").unwrap();
            std::os::unix::fs::symlink(&outside, workspace.root.join("link")).unwrap();
            let with_link = workspace.capture();

            let untracked = with_link.files().untracked();
            assert_eq!(untracked.len(), 1);
            assert!(matches!(
                untracked[0].digest,
                ContentDigest::SymlinkTarget(_)
            ));
            assert_ne!(
                untracked[0].digest,
                ContentDigest::of_content(b"secret\n"),
                "the link target's content must never be hashed"
            );
        }
    }

    #[test]
    fn one_probe_reused_across_a_capture_and_a_verification_sees_the_new_index() {
        let workspace = Workspace::new("repo");
        // Held for the worktree's lifetime, which is the natural caller pattern
        // and the one a probe caching its index at construction gets wrong.
        let probe = FilesystemProbe::new(&workspace.root);

        workspace.write("tracked.txt", "staged once\n");
        git(&workspace.root, ["add", "tracked.txt"]);
        let snapshot = WorkspaceSnapshot::capture(
            &workspace.request,
            &workspace.service(),
            &probe,
            &Cancellation::default(),
        )
        .unwrap();

        workspace.write("tracked.txt", "staged twice\n");
        git(&workspace.root, ["add", "tracked.txt"]);
        let state = snapshot
            .verify(&workspace.service(), &probe, &Cancellation::default())
            .unwrap();

        assert_eq!(
            state,
            FreshnessState::Stale {
                changed: vec![StalePath {
                    path: Some(RepoPath::from_bytes(b"tracked.txt".to_vec())),
                    component: SnapshotComponent::Index,
                    change: PathDivergence::Changed,
                }],
            },
            "a staged change was invisible to a reused probe"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_branch_never_makes_the_rest_of_its_tree_invisible() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = Workspace::new("repo");
        fs::create_dir_all(workspace.root.join("build/open")).unwrap();
        fs::create_dir(workspace.root.join("build/closed")).unwrap();
        workspace.write("build/open/out.txt", "first\n");
        fs::write(workspace.root.join("build/closed/secret.txt"), "s\n").unwrap();

        let closed = workspace.root.join("build/closed");
        let mut permissions = fs::metadata(&closed).unwrap().permissions();
        permissions.set_mode(0o000);
        fs::set_permissions(&closed, permissions).unwrap();
        if fs::read_dir(&closed).is_ok() {
            // Running as root: the fixture cannot deny access to itself.
            return;
        }

        let snapshot = workspace.capture();
        assert_eq!(workspace.verify(&snapshot), FreshnessState::Fresh);

        // Collapsing the tree to one sentinel would freeze its digest, and every
        // edit under it would read as `Fresh` — the false negative that lets a
        // stale write land.
        workspace.write("build/open/out.txt", "second\n");
        assert_eq!(
            workspace.verify(&snapshot),
            FreshnessState::Stale {
                changed: vec![StalePath {
                    path: Some(RepoPath::from_bytes(b"build/open/out.txt".to_vec())),
                    component: SnapshotComponent::Untracked,
                    change: PathDivergence::Changed,
                }],
            }
        );

        workspace.write("build/open/new.txt", "appeared\n");
        assert!(matches!(
            workspace.verify(&snapshot),
            FreshnessState::Stale { .. }
        ));

        let mut permissions = fs::metadata(&closed).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&closed, permissions).unwrap();
    }

    /// A caller holding several snapshots of one workspace reads it once and
    /// answers all of them. `verify` reads for each snapshot it is asked about,
    /// which for a projection of every recorded check meant one `git status` and
    /// one hash of the dirty set per check, of the same workspace at the same
    /// moment.
    #[test]
    fn one_reading_answers_every_snapshot_taken_of_the_same_workspace() {
        let workspace = Workspace::new("repo");
        workspace.write("notes.txt", "first\n");
        let before = workspace.capture();
        workspace.write("notes.txt", "second\n");
        let after = workspace.capture();

        let reading = workspace.read();

        // One read, two different verdicts, each the one `verify` gives alone.
        assert!(matches!(
            before.verify_against(&reading),
            FreshnessState::Stale { .. }
        ));
        assert_eq!(after.verify_against(&reading), FreshnessState::Fresh);
        assert_eq!(before.verify_against(&reading), workspace.verify(&before));
        assert_eq!(after.verify_against(&reading), workspace.verify(&after));

        // The reading is a moment, not a subscription: the workspace moving
        // afterwards does not change what it already read.
        workspace.write("notes.txt", "third\n");
        assert_eq!(after.verify_against(&reading), FreshnessState::Fresh);
        assert!(matches!(
            workspace.verify(&after),
            FreshnessState::Stale { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_branch_is_named_in_the_capture_diagnostics() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = Workspace::new("repo");
        fs::create_dir_all(workspace.root.join("build/closed")).unwrap();
        workspace.write("build/out.txt", "first\n");
        let closed = workspace.root.join("build/closed");
        let mut permissions = fs::metadata(&closed).unwrap().permissions();
        permissions.set_mode(0o000);
        fs::set_permissions(&closed, permissions).unwrap();
        if fs::read_dir(&closed).is_ok() {
            return;
        }

        let probe = FilesystemProbe::new(&workspace.root);
        let capture = WorkspaceSnapshot::capture_with_diagnostics(
            &workspace.request,
            &workspace.service(),
            &probe,
            &Cancellation::default(),
        )
        .unwrap();

        assert_eq!(capture.diagnostics.paths_hashed, 1);
        assert_eq!(
            capture
                .diagnostics
                .paths_skipped
                .iter()
                .map(|skipped| skipped.path.display())
                .collect::<Vec<_>>(),
            ["build/closed"]
        );
        assert_eq!(
            capture
                .snapshot
                .files()
                .untracked()
                .iter()
                .map(|entry| (entry.path.display(), entry.digest.clone()))
                .collect::<Vec<_>>(),
            [
                ("build/closed".to_owned(), ContentDigest::Unreadable),
                (
                    "build/out.txt".to_owned(),
                    ContentDigest::of_content(b"first\n")
                ),
            ]
        );

        let mut permissions = fs::metadata(&closed).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&closed, permissions).unwrap();
    }

    /// Runs one status entry through the collector and returns the staged set.
    ///
    /// Driven directly rather than through a fixture repository because Git's
    /// own copy detection is reluctant — `status.renames=copies` does not make
    /// it emit a `C` record for an ordinary `cp` — while `harkness-git` parses
    /// and models those records regardless. The decision under test is which
    /// change kinds remove their source from the index, so it is made here.
    fn staged_set_for(entry: &StatusEntry) -> Vec<(String, ContentDigest)> {
        struct AbsentProbe;
        impl WorkspaceProbe for AbsentProbe {
            fn expand_untracked(
                &self,
                candidate: &RepoPath,
                _cancellation: &Cancellation,
            ) -> Result<crate::probe::UntrackedExpansion, ProbeFailure> {
                Ok(crate::probe::UntrackedExpansion::of_one(candidate.clone()))
            }

            fn hash_path(
                &self,
                _path: &RepoPath,
                _cancellation: &Cancellation,
            ) -> Result<ContentDigest, ProbeFailure> {
                Ok(ContentDigest::Unreadable)
            }

            fn staged_blob_id(&self, _path: &RepoPath) -> Result<Option<String>, ProbeFailure> {
                Ok(Some("0123456789abcdef".to_owned()))
            }
        }

        let mut staged = Vec::new();
        let mut tracked_dirty = Vec::new();
        let mut untracked = Vec::new();
        super::collect_entry(
            &AbsentProbe,
            &Cancellation::default(),
            &mut super::CaptureDiagnostics::default(),
            entry,
            &mut staged,
            &mut tracked_dirty,
            &mut untracked,
        )
        .unwrap_or_else(|_| panic!("the fixture probe never refuses"));
        super::canonicalize_entries(staged)
            .into_iter()
            .map(|entry| (entry.path.display(), entry.digest))
            .collect()
    }

    #[test]
    fn a_staged_copy_leaves_its_source_in_the_index() {
        let copied = staged_set_for(&StatusEntry {
            path: std::path::PathBuf::from("copy.txt"),
            staged: Some(FileChange::Copied),
            unstaged: None,
            rename_source: Some(std::path::PathBuf::from("original.txt")),
            conflicted: false,
        });
        let renamed = staged_set_for(&StatusEntry {
            path: std::path::PathBuf::from("copy.txt"),
            staged: Some(FileChange::Renamed),
            unstaged: None,
            rename_source: Some(std::path::PathBuf::from("original.txt")),
            conflicted: false,
        });

        // A copy leaves the source in the index untouched, so nothing may claim
        // it is gone. A rename really does remove it.
        assert_eq!(
            copied,
            [(
                "copy.txt".to_owned(),
                ContentDigest::StagedBlob("0123456789abcdef".to_owned())
            )]
        );
        assert_eq!(
            renamed,
            [
                (
                    "copy.txt".to_owned(),
                    ContentDigest::StagedBlob("0123456789abcdef".to_owned())
                ),
                ("original.txt".to_owned(), ContentDigest::Absent),
            ]
        );
        // The two index states must not share one identity: a staged copy is not
        // a staged delete of the source beside a staged add of the destination.
        assert_ne!(copied, renamed);
    }

    #[test]
    fn a_staged_rename_still_records_its_source_as_absent() {
        let workspace = Workspace::new("repo");
        git(&workspace.root, ["mv", "tracked.txt", "renamed.txt"]);
        let snapshot = workspace.capture();
        assert!(
            snapshot
                .files()
                .staged()
                .iter()
                .any(|entry| entry.path.display() == "tracked.txt"
                    && entry.digest == ContentDigest::Absent),
            "{:?}",
            snapshot.files().staged()
        );
    }

    /// Refuses to expand anything, the way the expansion bound does once a tree
    /// is too large to enumerate deterministically.
    struct OpaqueProbe;

    impl WorkspaceProbe for OpaqueProbe {
        fn expand_untracked(
            &self,
            _candidate: &RepoPath,
            _cancellation: &Cancellation,
        ) -> Result<crate::probe::UntrackedExpansion, ProbeFailure> {
            Err(ProbeFailure::skipped(
                "holds more than 10000 untracked files",
            ))
        }

        fn hash_path(
            &self,
            _path: &RepoPath,
            _cancellation: &Cancellation,
        ) -> Result<ContentDigest, ProbeFailure> {
            Ok(ContentDigest::Unreadable)
        }

        fn staged_blob_id(&self, _path: &RepoPath) -> Result<Option<String>, ProbeFailure> {
            Ok(None)
        }
    }

    #[test]
    fn an_opaque_directory_is_spelled_the_same_way_a_readable_one_is() {
        let workspace = Workspace::new("repo");
        fs::create_dir(workspace.root.join("build")).unwrap();
        workspace.write("build/out.txt", "first\n");

        // Git reports `build/`. A candidate the probe cannot expand at all —
        // the expansion bound, or a directory removed between status and the
        // walk — must still be recorded under the spelling every other path in
        // the identity uses, or one directory would have two names depending on
        // where it failed, and switching between them would read as a removal
        // beside an addition while nothing moved.
        let snapshot = WorkspaceSnapshot::capture(
            &workspace.request,
            &workspace.service(),
            &OpaqueProbe,
            &Cancellation::default(),
        )
        .unwrap();

        assert_eq!(
            snapshot
                .files()
                .untracked()
                .iter()
                .map(|entry| entry.path.display())
                .collect::<Vec<_>>(),
            ["build"],
            "the recorded path kept its directory separator"
        );

        // The same directory, expanded normally, names its contents beneath the
        // same prefix — so the two routes cannot disagree about the directory.
        let readable = workspace.capture();
        assert_eq!(
            readable
                .files()
                .untracked()
                .iter()
                .map(|entry| entry.path.display())
                .collect::<Vec<_>>(),
            ["build/out.txt"]
        );
    }

    #[test]
    fn diagnostics_report_what_a_capture_read() {
        let workspace = Workspace::new("repo");
        workspace.write("tracked.txt", "modified\n");
        workspace.write("new.txt", "new\n");
        let probe = FilesystemProbe::new(&workspace.root);
        let capture = WorkspaceSnapshot::capture_with_diagnostics(
            &workspace.request,
            &workspace.service(),
            &probe,
            &Cancellation::default(),
        )
        .unwrap();
        assert_eq!(capture.diagnostics.paths_hashed, 2);
        assert!(capture.diagnostics.paths_skipped.is_empty());
    }

    #[test]
    fn the_generations_and_instruction_digest_take_part_in_identity() {
        let workspace = Workspace::new("repo");
        let base = workspace.capture();

        let with_config = WorkspaceSnapshot::assemble(
            base.id(),
            &CaptureRequest::new(base.project_id()).with_config_generation(1),
            base.repository_identity().to_owned(),
            base.worktree_root().to_path_buf(),
            base.head().map(str::to_owned),
            base.branch().map(str::to_owned),
            base.files().clone(),
            base.captured_at(),
        );
        assert_ne!(base.digest(), with_config.digest());

        let with_index = WorkspaceSnapshot::assemble(
            base.id(),
            &CaptureRequest::new(base.project_id()).with_index_generation(7),
            base.repository_identity().to_owned(),
            base.worktree_root().to_path_buf(),
            base.head().map(str::to_owned),
            base.branch().map(str::to_owned),
            base.files().clone(),
            base.captured_at(),
        );
        assert_ne!(base.digest(), with_index.digest());
        assert_ne!(with_config.digest(), with_index.digest());

        let with_instructions = WorkspaceSnapshot::assemble(
            base.id(),
            &CaptureRequest::new(base.project_id())
                .with_instructions_digest(crate::digest::Sha256Hex::of(b"AGENTS.md")),
            base.repository_identity().to_owned(),
            base.worktree_root().to_path_buf(),
            base.head().map(str::to_owned),
            base.branch().map(str::to_owned),
            base.files().clone(),
            base.captured_at(),
        );
        assert_ne!(base.digest(), with_instructions.digest());
    }

    #[test]
    fn the_digest_ignores_the_capture_id_and_time() {
        let workspace = Workspace::new("repo");
        let base = workspace.capture();
        let restamped = WorkspaceSnapshot::assemble(
            crate::ids::SnapshotId::new(),
            &CaptureRequest::new(base.project_id()),
            base.repository_identity().to_owned(),
            base.worktree_root().to_path_buf(),
            base.head().map(str::to_owned),
            base.branch().map(str::to_owned),
            base.files().clone(),
            base.captured_at() + std::time::Duration::from_secs(3600),
        );
        assert_eq!(base.digest(), restamped.digest());
        assert_ne!(base.id(), restamped.id());
    }

    #[test]
    fn require_digest_names_both_sides_of_a_mismatch() {
        let workspace = Workspace::new("repo");
        let snapshot = workspace.capture();
        workspace.write("new.txt", "new\n");
        let moved = workspace.capture();
        let error = snapshot.require_digest(&moved.digest()).unwrap_err();
        assert_eq!(error.kind(), "digest_mismatch");
        assert!(error.to_string().contains(&moved.digest().to_string()));
    }

    #[test]
    fn a_repository_root_that_is_a_file_is_not_a_worktree() {
        let fixture = Fixture::new();
        let root = fixture.root.path().join("not-a-directory");
        fs::write(&root, b"file").unwrap();
        let probe = FilesystemProbe::new(&root);
        let error = WorkspaceSnapshot::capture(
            &CaptureRequest::new(ProjectId::new()),
            &GitService::new(&root, &fixture.data_dir),
            &probe,
            &Cancellation::default(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), "worktree_root_missing");
        assert!(!Path::new(&root).is_dir());
    }
}
