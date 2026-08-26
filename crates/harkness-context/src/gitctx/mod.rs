//! Snapshot-bound, inventory-gated Git context retrieval.
//!
//! This module is an adapter over [`harkness_git::GitService`], never another
//! Git implementation. Diff, status, history, worktree and blame bytes all
//! cross that service first; the work here is bounding, eligibility filtering,
//! projection into context-owned records, provenance stamping, and the final
//! snapshot guard that prevents a mixed-moment answer.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Instant;

use harkness_git::{
    BlameCommit, BlameLineRange, Cancellation, CommitInfo, DiffLine, DiffOmission, DiffOptions,
    DiffTarget, FileChange, FileDiff, GitError, GitService, HeadState, LogCursor, LogOptions,
    PendingOperation, UpstreamStatus,
};
use thiserror::Error;

use crate::UnverifiableReason;
use crate::{
    ContextDomainError, FileClass, FileInventory, FileSample, FilesystemProbe, FreshnessState,
    InventoryEntry, InventoryError, Provenance, RepoPath, RetrievalSource, SelectionReason,
    SelectionReasonKind, Sensitivity, SnapshotId, WorkspaceSnapshot,
};

/// Context-engine diff budget, deliberately tighter than `harkness-git`'s UI budget.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GitContextBudget {
    /// Combined hunk bytes returned by one retrieval.
    pub max_total_bytes: u64,
    /// Files whose content may be carried before summary-only records begin.
    pub max_files: usize,
    /// Commits one history request may inspect.
    pub max_commits: usize,
}

impl Default for GitContextBudget {
    fn default() -> Self {
        Self {
            max_total_bytes: DEFAULT_CONTEXT_DIFF_BYTES,
            max_files: DEFAULT_CONTEXT_DIFF_FILES,
            max_commits: DEFAULT_CONTEXT_COMMITS,
        }
    }
}

/// Default combined hunk budget for Git context.
pub const DEFAULT_CONTEXT_DIFF_BYTES: u64 = 1024 * 1024;
/// Default file budget for Git context.
pub const DEFAULT_CONTEXT_DIFF_FILES: usize = 200;
/// Default commit page and scan budget for Git context.
pub const DEFAULT_CONTEXT_COMMITS: usize = 50;
/// Hard cap inherited from the history contract.
pub const MAX_CONTEXT_COMMITS: usize = 1_000;

/// Which immutable comparison a projected diff represents.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DiffComparison {
    /// `HEAD` (or the empty tree) against the index.
    Staged,
    /// The index against the working tree.
    WorkingTree,
    /// The captured head against its pinned merge-base with `base`.
    BranchAgainstBase {
        /// Base revision expression the caller supplied.
        base: String,
        /// Resolved merge-base object ID.
        merge_base: String,
        /// Captured head object ID, never a live branch name.
        head: String,
    },
}

/// A Git identity sufficient to re-address or audit one diff item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffAnchor {
    /// Old-side blob object ID, including the all-zero absent marker.
    pub old_blob_id: String,
    /// New-side blob object ID or verified working-tree hash.
    pub new_blob_id: String,
    /// Pinned old revision when the comparison has one.
    pub old_revision: Option<String>,
    /// Pinned new revision when the comparison has one.
    pub new_revision: Option<String>,
}

/// Why one projected file carries no hunk bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum GitDiffOmission {
    /// The Git-layer file-size bound fired.
    FileTooLarge {
        /// Byte limit.
        limit: u64,
    },
    /// The index has no resolved two-sided form.
    Unmerged,
    /// The combined context byte budget was spent.
    ContentBudgetExhausted {
        /// Combined byte limit.
        limit: u64,
    },
    /// The context file budget was spent.
    FileBudgetExhausted {
        /// File limit.
        limit: usize,
    },
    /// Git produced a delta shape the structured model cannot carry.
    Unrepresentable {
        /// Bounded diagnostic.
        detail: String,
    },
    /// The inventory recorded a class whose content is not retrievable.
    IneligibleClass {
        /// Inventory classification that refused content.
        class: FileClass,
    },
}

/// One projected unified-diff hunk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffContextHunk {
    /// First old-side line.
    pub old_start: u32,
    /// Old-side line count.
    pub old_lines: u32,
    /// First new-side line.
    pub new_start: u32,
    /// New-side line count.
    pub new_lines: u32,
    /// Raw hunk header.
    pub header: Vec<u8>,
    /// Byte-preserving lines in patch order.
    pub lines: Vec<DiffLine>,
}

/// One inventory-approved file in a diff context response.
#[derive(Clone, Debug, PartialEq)]
pub struct DiffContextFile {
    /// Change classification.
    pub change: FileChange,
    /// Old repository-relative path, absent for an addition.
    pub old_path: Option<RepoPath>,
    /// New repository-relative path, absent for a deletion.
    pub new_path: Option<RepoPath>,
    /// Immutable Git identity for both sides.
    pub anchor: DiffAnchor,
    /// Whether Git classified the inspected bytes as binary.
    pub binary: bool,
    /// Named reason hunk bytes are absent.
    pub omission: Option<GitDiffOmission>,
    /// Bounded hunks.
    pub hunks: Vec<DiffContextHunk>,
    /// Snapshot, digest, source, reason and trust marking for the bytes above.
    pub provenance: Provenance,
}

/// One bounded, snapshot-bound diff response.
#[derive(Clone, Debug, PartialEq)]
pub struct DiffContext {
    /// Capture every item is stamped with.
    pub snapshot_id: SnapshotId,
    /// Comparison that produced the files.
    pub comparison: DiffComparison,
    /// Inventory-approved files in Git's deterministic order.
    pub files: Vec<DiffContextFile>,
    /// Count of paths excluded by inventory policy, without recording names.
    pub withheld_files: usize,
}

/// Coherent staged and unstaged views computed against one open index.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceDiffContext {
    /// `HEAD` to index.
    pub staged: DiffContext,
    /// Index to working tree.
    pub working: DiffContext,
}

/// One byte-preserving commit signature.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitSignatureContext {
    /// Name bytes from the commit object.
    pub name: Vec<u8>,
    /// Email bytes from the commit object.
    pub email: Vec<u8>,
    /// Unix epoch seconds.
    pub time_seconds: i64,
    /// Recorded timezone offset in minutes.
    pub offset_minutes: i32,
}

/// One commit projected into context.
#[derive(Clone, Debug, PartialEq)]
pub struct CommitContextItem {
    /// Full commit object ID.
    pub id: String,
    /// Parent object IDs in recorded order.
    pub parent_ids: Vec<String>,
    /// Original author.
    pub author: CommitSignatureContext,
    /// Committer.
    pub committer: CommitSignatureContext,
    /// First message line, byte-preserving.
    pub summary: Vec<u8>,
    /// Complete raw commit message.
    pub message: Vec<u8>,
    /// Provenance of the untrusted message bytes.
    pub provenance: Provenance,
}

/// A named reason history is a bounded prefix rather than a complete answer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum HistoryOmission {
    /// The request or remaining range exceeded the commit budget.
    CommitBudgetExhausted {
        /// Commit bound that fired.
        limit: usize,
    },
}

/// One bounded history page or file-history scan.
#[derive(Clone, Debug, PartialEq)]
pub struct HistoryContext {
    /// Capture every item is stamped with.
    pub snapshot_id: SnapshotId,
    /// Projected commits.
    pub commits: Vec<CommitContextItem>,
    /// Cursor for a recent-history page; file-history scans do not paginate.
    pub next_cursor: Option<LogCursor>,
    /// Why the response is incomplete.
    pub omission: Option<HistoryOmission>,
    /// Commits inspected to produce this response.
    pub inspected_commits: usize,
}

/// One inventory-approved changed path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangedFile {
    /// Current repository-relative path.
    pub path: RepoPath,
    /// Index change, when any.
    pub staged: Option<FileChange>,
    /// Working-tree change, when any.
    pub unstaged: Option<FileChange>,
    /// Source of a rename or copy, preserved when the destination is eligible.
    pub rename_source: Option<RepoPath>,
    /// Whether the path is unresolved.
    pub conflicted: bool,
}

/// Bounded changed-file projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangedFilesContext {
    /// Capture this status was checked against.
    pub snapshot_id: SnapshotId,
    /// Eligible paths.
    pub files: Vec<ChangedFile>,
    /// Ineligible paths counted without names.
    pub withheld_files: usize,
}

/// Merge-conflict and pending-operation state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictContext {
    /// Capture this status was checked against.
    pub snapshot_id: SnapshotId,
    /// Whether any eligible or withheld path is unresolved.
    pub has_conflicts: bool,
    /// Eligible unresolved paths.
    pub paths: Vec<RepoPath>,
    /// Unresolved paths withheld by inventory policy.
    pub withheld_paths: usize,
    /// Multi-step operation Git reports, when any.
    pub pending: Option<PendingOperation>,
}

/// One checkout in the repository's worktree list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeContextEntry {
    /// Checkout root.
    pub root: PathBuf,
    /// Checked-out branch, absent when detached.
    pub branch: Option<String>,
    /// Whether Git locked the worktree.
    pub locked: bool,
    /// Non-empty lock reason.
    pub lock_reason: Option<String>,
    /// Whether its administrative record is prunable.
    pub prunable: bool,
}

/// Checked-out state and every sibling worktree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeContext {
    /// Capture this answer is bound to.
    pub snapshot_id: SnapshotId,
    /// Current checked-out head shape.
    pub head: HeadState,
    /// Locally known upstream divergence.
    pub upstream: Option<UpstreamStatus>,
    /// Pending multi-step operation.
    pub pending: Option<PendingOperation>,
    /// Main and linked checkouts.
    pub worktrees: Vec<WorktreeContextEntry>,
}

/// Explicit request gate for blame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlameRequest {
    /// Repository-relative file.
    pub path: RepoPath,
    /// Required inclusive range; `None` is a typed refusal.
    pub range: Option<BlameLineRange>,
    /// Must be true; no implicit retrieval may invoke blame.
    pub explicit: bool,
}

impl BlameRequest {
    /// Builds an explicit, ranged request.
    #[must_use]
    pub fn explicit(path: RepoPath, range: BlameLineRange) -> Self {
        Self {
            path,
            range: Some(range),
            explicit: true,
        }
    }
}

/// Commit identity on one blame run.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum BlameContextCommit {
    /// Immutable commit ID.
    Commit(String),
    /// Working-tree-only content.
    Uncommitted,
}

/// One consecutive blame run with provenance.
#[derive(Clone, Debug, PartialEq)]
pub struct BlameContextEntry {
    /// Commit or the explicit uncommitted marker.
    pub commit: BlameContextCommit,
    /// Original path on the commit side.
    pub original_path: RepoPath,
    /// First original line.
    pub original_start_line: u32,
    /// First final line.
    pub final_start_line: u32,
    /// Consecutive line count.
    pub line_count: u32,
    /// Author epoch seconds.
    pub author_time: Option<i64>,
    /// Provenance for this attribution record.
    pub provenance: Provenance,
}

/// Explicit, bounded blame response.
#[derive(Clone, Debug, PartialEq)]
pub struct BlameContext {
    /// Capture every entry is stamped with.
    pub snapshot_id: SnapshotId,
    /// Requested path.
    pub path: RepoPath,
    /// Requested range.
    pub range: BlameLineRange,
    /// Consecutive attribution runs.
    pub entries: Vec<BlameContextEntry>,
}

/// Failures specific to Git-to-context adaptation.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum GitContextError {
    /// A Git read failed beneath the adapter.
    #[error(transparent)]
    Git(#[from] GitError),
    /// Snapshot verification could not be performed.
    #[error(transparent)]
    Domain(#[from] ContextDomainError),
    /// The eligibility walk failed before any Git content was projected.
    #[error(transparent)]
    Inventory(InventoryError),
    /// The workspace no longer matches the capture.
    #[error("workspace snapshot {expected} is stale: {state:?}")]
    StaleSnapshot {
        /// Capture that was expected.
        expected: SnapshotId,
        /// Named divergence or unverifiable reason.
        state: FreshnessState,
    },
    /// An inventory from another capture was paired with this service.
    #[error("inventory belongs to snapshot {found}, not {expected}")]
    ForeignInventory {
        /// Service snapshot.
        expected: SnapshotId,
        /// Inventory snapshot.
        found: SnapshotId,
    },
    /// The inventory and snapshot name different worktrees.
    #[error("inventory describes '{}', not '{}'", found.display(), expected.display())]
    ForeignWorktree {
        /// Snapshot root.
        expected: PathBuf,
        /// Inventory root.
        found: PathBuf,
    },
    /// A zero budget could never return a useful result.
    #[error("Git context {field} budget must be greater than zero")]
    InvalidBudget {
        /// Budget field.
        field: &'static str,
    },
    /// Blame was reached without an explicit tool/user request.
    #[error("blame is available only through an explicit request")]
    BlameNotExplicit,
    /// Blame was requested without a bounded line range.
    #[error("blame requires an explicit line range")]
    BlameRangeRequired,
    /// An explicit file is absent from the eligible inventory.
    #[error("the requested path is not eligible for context retrieval")]
    PathNotEligible {
        /// Caller-supplied path.
        path: RepoPath,
    },
}

impl GitContextError {
    /// Stable machine-readable discriminant.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Git(error) => error.kind(),
            Self::Domain(error) => error.kind(),
            Self::Inventory(error) => error.kind(),
            Self::StaleSnapshot { .. } => "stale_snapshot",
            Self::ForeignInventory { .. } => "foreign_inventory",
            Self::ForeignWorktree { .. } => "foreign_worktree",
            Self::InvalidBudget { .. } => "invalid_git_context_budget",
            Self::BlameNotExplicit => "blame_not_explicit",
            Self::BlameRangeRequired => "blame_range_required",
            Self::PathNotEligible { .. } => "path_not_eligible",
        }
    }
}

impl From<InventoryError> for GitContextError {
    fn from(error: InventoryError) -> Self {
        match error {
            InventoryError::Cancelled => Self::Git(GitError::Cancelled),
            carried => Self::Inventory(carried),
        }
    }
}

/// Git-aware retrieval bound to one capture and one eligible inventory.
#[derive(Clone, Debug)]
pub struct GitContextService {
    git: GitService,
    snapshot: WorkspaceSnapshot,
    inventory: FileInventory,
    historical_paths: BTreeSet<RepoPath>,
}

impl GitContextService {
    /// Pairs the existing Git service, capture and inventory.
    pub fn new(
        git: &GitService,
        snapshot: &WorkspaceSnapshot,
        inventory: FileInventory,
    ) -> Result<Self, GitContextError> {
        if inventory.snapshot() != snapshot.id() {
            return Err(GitContextError::ForeignInventory {
                expected: snapshot.id(),
                found: inventory.snapshot(),
            });
        }
        if inventory.worktree_root() != snapshot.worktree_root() {
            return Err(GitContextError::ForeignWorktree {
                expected: snapshot.worktree_root().to_path_buf(),
                found: inventory.worktree_root().to_path_buf(),
            });
        }
        Ok(Self {
            git: git.clone(),
            snapshot: snapshot.clone(),
            inventory,
            historical_paths: BTreeSet::new(),
        })
    }

    pub(crate) fn with_historical_paths(mut self, paths: BTreeSet<RepoPath>) -> Self {
        self.historical_paths = paths;
        self
    }

    /// Capture this service stamps every item with.
    #[must_use]
    pub const fn snapshot(&self) -> &WorkspaceSnapshot {
        &self.snapshot
    }

    /// Computes staged and unstaged views against one open index.
    pub fn workspace_diff(
        &self,
        budget: &GitContextBudget,
        cancellation: &Cancellation,
    ) -> Result<WorkspaceDiffContext, GitContextError> {
        validate_budget(budget)?;
        refuse_cancelled(cancellation)?;
        let started = Instant::now();
        let files = self.git.diff_snapshot(
            &[DiffTarget::Staged, DiffTarget::Unstaged],
            &diff_options(budget),
        )?;
        let mut staged = Vec::new();
        let mut working = Vec::new();
        for file in files {
            match file.target {
                DiffTarget::Staged => staged.push(file),
                DiffTarget::Unstaged => working.push(file),
                _ => unreachable!("the requested targets fix both variants"),
            }
        }
        let staged = self.project_diff(DiffComparison::Staged, staged);
        let working = self.project_diff(DiffComparison::WorkingTree, working);
        self.guard(cancellation)?;
        tracing::info!(
            retrieval = "workspace_diff",
            snapshot_id = %self.snapshot.id(),
            max_total_bytes = budget.max_total_bytes,
            max_files = budget.max_files,
            staged_files = staged.files.len(),
            working_files = working.files.len(),
            withheld_files = staged.withheld_files + working.withheld_files,
            duration_ms = started.elapsed().as_millis() as u64,
            "Git context retrieval completed"
        );
        Ok(WorkspaceDiffContext { staged, working })
    }

    /// Returns the working-tree half of a coherent workspace diff.
    pub fn working_diff(
        &self,
        budget: &GitContextBudget,
        cancellation: &Cancellation,
    ) -> Result<DiffContext, GitContextError> {
        self.workspace_diff(budget, cancellation)
            .map(|view| view.working)
    }

    /// Returns the staged half of a coherent workspace diff.
    pub fn staged_diff(
        &self,
        budget: &GitContextBudget,
        cancellation: &Cancellation,
    ) -> Result<DiffContext, GitContextError> {
        self.workspace_diff(budget, cancellation)
            .map(|view| view.staged)
    }

    /// Diffs the captured head against a merge-base resolved once from `base`.
    pub fn branch_diff(
        &self,
        base: &str,
        budget: &GitContextBudget,
        cancellation: &Cancellation,
    ) -> Result<DiffContext, GitContextError> {
        validate_budget(budget)?;
        refuse_cancelled(cancellation)?;
        let started = Instant::now();
        let Some(head) = self.snapshot.head() else {
            return Err(GitError::UnbornBranch {
                path: self.git.root().to_path_buf(),
                branch: self.snapshot.branch().unwrap_or("HEAD").to_owned(),
            }
            .into());
        };
        let merge_base = self.git.merge_base(base, head)?.to_string();
        let target = DiffTarget::Revisions {
            old_revision: merge_base.clone(),
            new_revision: head.to_owned(),
        };
        let files = self.git.diff(target, &diff_options(budget))?;
        let context = self.project_diff(
            DiffComparison::BranchAgainstBase {
                base: base.to_owned(),
                merge_base,
                head: head.to_owned(),
            },
            files,
        );
        self.guard(cancellation)?;
        trace("branch_diff", &context, budget, started);
        Ok(context)
    }

    /// Returns one bounded, cursor-paged commit history.
    pub fn recent_commits(
        &self,
        options: &LogOptions,
        budget: &GitContextBudget,
        cancellation: &Cancellation,
    ) -> Result<HistoryContext, GitContextError> {
        validate_budget(budget)?;
        refuse_cancelled(cancellation)?;
        let started = Instant::now();
        let requested = options.limit;
        let limit = requested.min(budget.max_commits).min(MAX_CONTEXT_COMMITS);
        let mut bounded = options.clone();
        bounded.limit = limit;
        let page = self.git.log(&bounded, cancellation)?;
        let incomplete = requested > limit || page.next_cursor.is_some();
        let inspected_commits = page.commits.len();
        let commits = page
            .commits
            .iter()
            .map(|commit| project_commit(self.snapshot.id(), commit))
            .collect();
        self.guard(cancellation)?;
        tracing::info!(
            retrieval = "recent_commits",
            snapshot_id = %self.snapshot.id(),
            max_commits = limit,
            result_count = inspected_commits,
            incomplete,
            duration_ms = started.elapsed().as_millis() as u64,
            "Git context retrieval completed"
        );
        Ok(HistoryContext {
            snapshot_id: self.snapshot.id(),
            commits,
            next_cursor: page.next_cursor,
            omission: incomplete.then_some(HistoryOmission::CommitBudgetExhausted { limit }),
            inspected_commits,
        })
    }

    /// Finds commits in a bounded recent-history scan that changed `path`.
    ///
    /// Rename following is deliberately absent. Each inspected commit asks the
    /// existing diff service about this one literal path; reaching the scan
    /// budget is reported rather than interpreted as complete history.
    pub fn file_history(
        &self,
        path: &RepoPath,
        limit: usize,
        budget: &GitContextBudget,
        cancellation: &Cancellation,
    ) -> Result<HistoryContext, GitContextError> {
        validate_budget(budget)?;
        if limit == 0 {
            return Err(GitContextError::InvalidBudget {
                field: "file history",
            });
        }
        self.require_path(path)?;
        refuse_cancelled(cancellation)?;
        let started = Instant::now();
        let Some(head) = self.snapshot.head() else {
            return Ok(HistoryContext {
                snapshot_id: self.snapshot.id(),
                commits: Vec::new(),
                next_cursor: None,
                omission: None,
                inspected_commits: 0,
            });
        };
        let scan_limit = budget.max_commits.min(MAX_CONTEXT_COMMITS);
        let page = self
            .git
            .log(&LogOptions::new(head, scan_limit), cancellation)?;
        let mut matches = Vec::new();
        let path_buf = self.snapshot.worktree_root().join(path.to_path_buf());
        for commit in &page.commits {
            refuse_cancelled(cancellation)?;
            let options = DiffOptions::default()
                .with_max_file_size(0)
                .with_max_total_bytes(0)
                .with_max_files(1)
                .with_paths([path_buf.clone()]);
            if !self
                .git
                .diff(
                    DiffTarget::Commit {
                        revision: commit.id.to_string(),
                        parent: None,
                    },
                    &options,
                )?
                .is_empty()
            {
                matches.push(project_commit(self.snapshot.id(), commit));
            }
        }
        let more_matches = matches.len() > limit;
        matches.truncate(limit);
        let incomplete = more_matches || page.next_cursor.is_some();
        let inspected_commits = page.commits.len();
        self.guard(cancellation)?;
        tracing::info!(
            retrieval = "file_history",
            snapshot_id = %self.snapshot.id(),
            max_commits = scan_limit,
            result_count = matches.len(),
            inspected_commits,
            incomplete,
            duration_ms = started.elapsed().as_millis() as u64,
            "Git context retrieval completed"
        );
        Ok(HistoryContext {
            snapshot_id: self.snapshot.id(),
            commits: matches,
            next_cursor: None,
            omission: incomplete
                .then_some(HistoryOmission::CommitBudgetExhausted { limit: scan_limit }),
            inspected_commits,
        })
    }

    /// Lists changed files with renames, excluding ineligible names entirely.
    pub fn changed_files(
        &self,
        cancellation: &Cancellation,
    ) -> Result<ChangedFilesContext, GitContextError> {
        refuse_cancelled(cancellation)?;
        let started = Instant::now();
        let status = self.git.detailed_status_in_process(cancellation)?;
        let (files, withheld_files) = self.project_status_entries(&status.entries);
        self.guard(cancellation)?;
        tracing::info!(
            retrieval = "changed_files",
            snapshot_id = %self.snapshot.id(),
            result_count = files.len(),
            withheld_files,
            duration_ms = started.elapsed().as_millis() as u64,
            "Git context retrieval completed"
        );
        Ok(ChangedFilesContext {
            snapshot_id: self.snapshot.id(),
            files,
            withheld_files,
        })
    }

    /// Reports conflicts without failing the surrounding diff retrieval.
    pub fn conflict_state(
        &self,
        cancellation: &Cancellation,
    ) -> Result<ConflictContext, GitContextError> {
        refuse_cancelled(cancellation)?;
        let started = Instant::now();
        let status = self.git.detailed_status_in_process(cancellation)?;
        let mut paths = Vec::new();
        let mut withheld_paths = 0;
        for entry in status.entries.iter().filter(|entry| entry.conflicted) {
            let path = RepoPath::from_path(&entry.path);
            if self.inventory_entry(&path).is_some() {
                paths.push(path);
            } else {
                withheld_paths += 1;
            }
        }
        self.guard(cancellation)?;
        tracing::info!(
            retrieval = "conflict_state",
            snapshot_id = %self.snapshot.id(),
            result_count = paths.len(),
            withheld_paths,
            has_conflicts = status.has_conflicts(),
            duration_ms = started.elapsed().as_millis() as u64,
            "Git context retrieval completed"
        );
        Ok(ConflictContext {
            snapshot_id: self.snapshot.id(),
            has_conflicts: status.has_conflicts(),
            paths,
            withheld_paths,
            pending: status.pending,
        })
    }

    /// Reports the checked-out head and all sibling worktrees.
    pub fn worktree_state(
        &self,
        cancellation: &Cancellation,
    ) -> Result<WorktreeContext, GitContextError> {
        refuse_cancelled(cancellation)?;
        let started = Instant::now();
        let status = self.git.detailed_status_in_process(cancellation)?;
        let worktrees: Vec<WorktreeContextEntry> = self
            .git
            .worktrees(cancellation)?
            .into_iter()
            .map(|worktree| WorktreeContextEntry {
                root: worktree.root().to_path_buf(),
                branch: worktree.branch().map(str::to_owned),
                locked: worktree.is_locked(),
                lock_reason: worktree.lock_reason().map(str::to_owned),
                prunable: worktree.is_prunable(),
            })
            .collect();
        self.guard(cancellation)?;
        tracing::info!(
            retrieval = "worktree_state",
            snapshot_id = %self.snapshot.id(),
            result_count = worktrees.len(),
            duration_ms = started.elapsed().as_millis() as u64,
            "Git context retrieval completed"
        );
        Ok(WorktreeContext {
            snapshot_id: self.snapshot.id(),
            head: status.head,
            upstream: status.upstream,
            pending: status.pending,
            worktrees,
        })
    }

    /// Runs explicit, ranged blame and stamps each attribution run.
    pub fn blame(
        &self,
        request: &BlameRequest,
        cancellation: &Cancellation,
    ) -> Result<BlameContext, GitContextError> {
        if !request.explicit {
            return Err(GitContextError::BlameNotExplicit);
        }
        let range = request.range.ok_or(GitContextError::BlameRangeRequired)?;
        self.require_path(&request.path)?;
        refuse_cancelled(cancellation)?;
        let started = Instant::now();
        let path = self
            .snapshot
            .worktree_root()
            .join(request.path.to_path_buf());
        let blamed = self.git.blame_file(&path, range, cancellation)?;
        let entries = blamed
            .entries
            .into_iter()
            .map(|entry| {
                let commit = match entry.commit {
                    BlameCommit::Commit(id) => BlameContextCommit::Commit(id),
                    BlameCommit::Uncommitted => BlameContextCommit::Uncommitted,
                    _ => {
                        return Err(GitContextError::Git(GitError::MalformedBlame {
                            detail: "blame returned an attribution this context build does not understand"
                                .to_owned(),
                        }));
                    }
                };
                let original_path = RepoPath::from_path(&entry.original_path);
                let content = blame_identity(
                    &commit,
                    &original_path,
                    entry.original_start_line,
                    entry.final_start_line,
                    entry.line_count,
                );
                let provenance = Provenance::new(
                    RetrievalSource::GitHistory,
                    self.snapshot.id(),
                    &content,
                    SelectionReason::new(
                        SelectionReasonKind::ExplicitRequest,
                        "explicit line attribution request",
                    ),
                )
                .at_path(request.path.clone())
                .with_sensitivity(untrusted());
                Ok(BlameContextEntry {
                    commit,
                    original_path,
                    original_start_line: entry.original_start_line,
                    final_start_line: entry.final_start_line,
                    line_count: entry.line_count,
                    author_time: entry.author_time,
                    provenance,
                })
            })
            .collect::<Result<Vec<_>, GitContextError>>()?;
        self.guard(cancellation)?;
        tracing::info!(
            retrieval = "blame",
            snapshot_id = %self.snapshot.id(),
            result_count = entries.len(),
            requested_lines = range.line_count(),
            duration_ms = started.elapsed().as_millis() as u64,
            "Git context retrieval completed"
        );
        Ok(BlameContext {
            snapshot_id: self.snapshot.id(),
            path: request.path.clone(),
            range,
            entries,
        })
    }

    fn project_diff(&self, comparison: DiffComparison, files: Vec<FileDiff>) -> DiffContext {
        let (old_revision, new_revision) = comparison_revisions(&comparison);
        let mut projected = Vec::with_capacity(files.len());
        let mut withheld_files = 0;
        for file in files {
            let Some((path, class)) = self.file_eligibility(&file) else {
                withheld_files += 1;
                continue;
            };
            let retrievable = class.is_retrievable();
            let omission = if retrievable {
                file.omission.as_ref().map(project_omission)
            } else {
                Some(GitDiffOmission::IneligibleClass { class })
            };
            let hunks = if retrievable {
                file.hunks
                    .iter()
                    .map(|hunk| DiffContextHunk {
                        old_start: hunk.old_start,
                        old_lines: hunk.old_lines,
                        new_start: hunk.new_start,
                        new_lines: hunk.new_lines,
                        header: hunk.header.clone(),
                        lines: hunk.lines.clone(),
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            let content = hunks
                .iter()
                .flat_map(|hunk| hunk.lines.iter())
                .flat_map(|line| line.content.iter().copied())
                .collect::<Vec<_>>();
            let provenance = Provenance::new(
                RetrievalSource::GitDiff,
                self.snapshot.id(),
                &content,
                SelectionReason::new(
                    SelectionReasonKind::RecentlyChanged,
                    "file participates in the requested Git comparison",
                ),
            )
            .at_path(path)
            .with_sensitivity(untrusted());
            projected.push(DiffContextFile {
                change: file.change,
                old_path: file.old_path.as_deref().map(RepoPath::from_path),
                new_path: file.new_path.as_deref().map(RepoPath::from_path),
                anchor: DiffAnchor {
                    old_blob_id: file.old_blob_id,
                    new_blob_id: file.new_blob_id,
                    old_revision: old_revision.clone(),
                    new_revision: new_revision.clone(),
                },
                binary: file.binary,
                omission,
                hunks,
                provenance,
            });
        }
        DiffContext {
            snapshot_id: self.snapshot.id(),
            comparison,
            files: projected,
            withheld_files,
        }
    }

    fn file_eligibility(&self, file: &FileDiff) -> Option<(RepoPath, FileClass)> {
        let path = file.new_path.as_deref().or(file.old_path.as_deref())?;
        let path = RepoPath::from_path(path);
        let mut class = self.path_class(&path)?;
        for candidate in [file.old_path.as_deref(), file.new_path.as_deref()]
            .into_iter()
            .flatten()
        {
            let candidate = RepoPath::from_path(candidate);
            let candidate_class = self.path_class(&candidate)?;
            if !candidate_class.is_retrievable() {
                class = candidate_class;
            }
        }
        Some((path, class))
    }

    fn inventory_entry(&self, path: &RepoPath) -> Option<&InventoryEntry> {
        self.inventory
            .entries()
            .binary_search_by(|entry| entry.path.cmp(path))
            .ok()
            .map(|index| &self.inventory.entries()[index])
    }

    fn path_class(&self, path: &RepoPath) -> Option<FileClass> {
        self.inventory_entry(path)
            .map(|entry| entry.class)
            .or_else(|| {
                self.historical_paths
                    .contains(path)
                    .then(|| FileSample::new(path, 0).classify())
            })
    }

    fn require_path(&self, path: &RepoPath) -> Result<(), GitContextError> {
        if self
            .inventory_entry(path)
            .is_some_and(|entry| entry.class.is_retrievable())
        {
            Ok(())
        } else {
            Err(GitContextError::PathNotEligible { path: path.clone() })
        }
    }

    fn project_status_entries(
        &self,
        entries: &[harkness_git::StatusEntry],
    ) -> (Vec<ChangedFile>, usize) {
        let mut files = Vec::new();
        let mut withheld = 0;
        for entry in entries {
            let path = RepoPath::from_path(&entry.path);
            let rename_source = entry.rename_source.as_deref().map(RepoPath::from_path);
            if self.path_class(&path).is_none()
                || rename_source
                    .as_ref()
                    .is_some_and(|source| self.path_class(source).is_none())
            {
                withheld += 1;
                continue;
            }
            files.push(ChangedFile {
                path,
                staged: entry.staged,
                unstaged: entry.unstaged,
                rename_source,
                conflicted: entry.conflicted,
            });
        }
        (files, withheld)
    }

    fn guard(&self, cancellation: &Cancellation) -> Result<(), GitContextError> {
        refuse_cancelled(cancellation)?;
        let probe = FilesystemProbe::new(self.snapshot.worktree_root());
        let state = self.snapshot.verify(&self.git, &probe, cancellation)?;
        if matches!(
            state,
            FreshnessState::Unverifiable {
                reason: UnverifiableReason::Cancelled
            }
        ) {
            return Err(GitError::Cancelled.into());
        }
        if state.is_fresh() {
            Ok(())
        } else {
            Err(GitContextError::StaleSnapshot {
                expected: self.snapshot.id(),
                state,
            })
        }
    }
}

fn validate_budget(budget: &GitContextBudget) -> Result<(), GitContextError> {
    for (field, zero) in [
        ("max_total_bytes", budget.max_total_bytes == 0),
        ("max_files", budget.max_files == 0),
        ("max_commits", budget.max_commits == 0),
    ] {
        if zero {
            return Err(GitContextError::InvalidBudget { field });
        }
    }
    Ok(())
}

fn refuse_cancelled(cancellation: &Cancellation) -> Result<(), GitContextError> {
    if cancellation.is_cancelled() {
        Err(GitError::Cancelled.into())
    } else {
        Ok(())
    }
}

fn diff_options(budget: &GitContextBudget) -> DiffOptions {
    DiffOptions::default()
        .with_max_total_bytes(budget.max_total_bytes)
        .with_max_files(budget.max_files)
}

fn comparison_revisions(comparison: &DiffComparison) -> (Option<String>, Option<String>) {
    match comparison {
        DiffComparison::Staged | DiffComparison::WorkingTree => (None, None),
        DiffComparison::BranchAgainstBase {
            merge_base, head, ..
        } => (Some(merge_base.clone()), Some(head.clone())),
    }
}

fn project_omission(omission: &DiffOmission) -> GitDiffOmission {
    match omission {
        DiffOmission::FileTooLarge { limit } => GitDiffOmission::FileTooLarge { limit: *limit },
        DiffOmission::Unmerged => GitDiffOmission::Unmerged,
        DiffOmission::ContentBudgetExhausted { limit } => {
            GitDiffOmission::ContentBudgetExhausted { limit: *limit }
        }
        DiffOmission::FileBudgetExhausted { limit } => {
            GitDiffOmission::FileBudgetExhausted { limit: *limit }
        }
        DiffOmission::Unrepresentable { detail } => GitDiffOmission::Unrepresentable {
            detail: detail.clone(),
        },
        _ => GitDiffOmission::Unrepresentable {
            detail: "Git returned an omission this context build does not understand".to_owned(),
        },
    }
}

fn project_commit(snapshot: SnapshotId, commit: &CommitInfo) -> CommitContextItem {
    let signature = |value: &harkness_git::CommitSignature| CommitSignatureContext {
        name: value.name.clone(),
        email: value.email.clone(),
        time_seconds: value.time.seconds(),
        offset_minutes: value.time.offset_minutes(),
    };
    let provenance = Provenance::new(
        RetrievalSource::GitHistory,
        snapshot,
        &commit.message,
        SelectionReason::new(
            SelectionReasonKind::RecentlyChanged,
            "commit participates in the requested history range",
        ),
    )
    .with_sensitivity(untrusted());
    CommitContextItem {
        id: commit.id.to_string(),
        parent_ids: commit.parent_ids.iter().map(ToString::to_string).collect(),
        author: signature(&commit.author),
        committer: signature(&commit.committer),
        summary: commit.summary.clone(),
        message: commit.message.clone(),
        provenance,
    }
}

fn blame_identity(
    commit: &BlameContextCommit,
    path: &RepoPath,
    original_start: u32,
    final_start: u32,
    count: u32,
) -> Vec<u8> {
    let mut bytes = match commit {
        BlameContextCommit::Commit(id) => id.as_bytes().to_vec(),
        BlameContextCommit::Uncommitted => b"uncommitted".to_vec(),
    };
    bytes.push(0);
    bytes.extend_from_slice(path.as_bytes());
    for value in [original_start, final_start, count] {
        bytes.extend_from_slice(&value.to_be_bytes());
    }
    bytes
}

fn untrusted() -> Sensitivity {
    Sensitivity::suspicious("untrusted_repository_content")
}

fn trace(
    retrieval: &'static str,
    context: &DiffContext,
    budget: &GitContextBudget,
    started: Instant,
) {
    let omitted_files = context
        .files
        .iter()
        .filter(|file| file.omission.is_some())
        .count();
    tracing::info!(
        retrieval,
        snapshot_id = %context.snapshot_id,
        max_total_bytes = budget.max_total_bytes,
        max_files = budget.max_files,
        result_count = context.files.len(),
        omitted_files,
        withheld_files = context.withheld_files,
        duration_ms = started.elapsed().as_millis() as u64,
        "Git context retrieval completed"
    );
}

#[cfg(test)]
mod tests {
    use std::fs;

    use harkness_core::ProjectId;
    use harkness_git::{
        Cancellation, DiffTarget, FileContextRequest, FileSide, GitService, LogOptions,
    };
    use harkness_test_fixtures::{Fixture, commit_all, git, initialize_repository};

    use super::{
        BlameContextCommit, BlameRequest, DiffComparison, GitContextBudget, GitContextError,
        GitDiffOmission,
    };
    use crate::{ContextEngine, ContextEngineConfig, RepoPath, Sensitivity};

    fn open_engine(fixture: &Fixture, root: &std::path::Path) -> ContextEngine {
        ContextEngine::open(
            ContextEngineConfig::new(ProjectId::new(), root, &fixture.data_dir),
            &Cancellation::default(),
        )
        .unwrap()
    }

    #[test]
    fn staged_and_working_diffs_share_one_index_snapshot_and_blob_ids_remain_readable() {
        let fixture = Fixture::new();
        let root = fixture.directory("repository");
        let repository = initialize_repository(&root);
        fs::write(root.join("file.txt"), "base\n").unwrap();
        commit_all(&repository, "base");
        fs::write(root.join("file.txt"), "staged\n").unwrap();
        git(&root, ["add", "file.txt"]);
        fs::write(root.join("file.txt"), "working\n").unwrap();

        let engine = open_engine(&fixture, &root);
        let snapshot = engine.snapshot(&Cancellation::default()).unwrap();
        let context = engine
            .git_context_under(&snapshot, &Cancellation::default())
            .unwrap();
        let view = context
            .workspace_diff(&GitContextBudget::default(), &Cancellation::default())
            .unwrap();
        assert_eq!(view.staged.files.len(), 1);
        assert_eq!(view.working.files.len(), 1);
        assert!(matches!(view.staged.comparison, DiffComparison::Staged));
        assert!(matches!(
            view.working.comparison,
            DiffComparison::WorkingTree
        ));
        let staged_blob = view.staged.files[0].anchor.new_blob_id.clone();

        fs::write(root.join("file.txt"), "moved again\n").unwrap();
        let response = GitService::new(&root, &fixture.data_dir)
            .file_context(&FileContextRequest::full_blob(staged_blob, FileSide::New))
            .unwrap();
        let bytes = response
            .lines
            .iter()
            .flat_map(|line| line.content.iter().copied())
            .collect::<Vec<_>>();
        assert_eq!(bytes, b"staged\n");
    }

    #[test]
    fn branch_diff_pins_the_merge_base_and_excludes_base_only_commits() {
        let fixture = Fixture::new();
        let root = fixture.directory("repository");
        let repository = initialize_repository(&root);
        fs::write(root.join("shared.txt"), "base\n").unwrap();
        commit_all(&repository, "base");
        git(&root, ["branch", "feature"]);
        fs::write(root.join("base-only.txt"), "main moved\n").unwrap();
        commit_all(&repository, "base advanced");
        git(&root, ["switch", "feature"]);
        fs::write(root.join("feature-only.txt"), "feature moved\n").unwrap();
        commit_all(&repository, "feature work");

        let engine = open_engine(&fixture, &root);
        let snapshot = engine.snapshot(&Cancellation::default()).unwrap();
        let context = engine
            .git_context_under(&snapshot, &Cancellation::default())
            .unwrap();
        let diff = context
            .branch_diff(
                "main",
                &GitContextBudget::default(),
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(diff.files.len(), 1);
        assert_eq!(
            diff.files[0].new_path.as_ref().unwrap().as_bytes(),
            b"feature-only.txt"
        );
        let DiffComparison::BranchAgainstBase {
            merge_base, head, ..
        } = diff.comparison
        else {
            panic!("branch comparison was not recorded");
        };
        assert_ne!(merge_base, head);
    }

    #[test]
    fn moving_head_after_capture_is_a_typed_stale_snapshot() {
        let fixture = Fixture::new();
        let root = fixture.directory("repository");
        let repository = initialize_repository(&root);
        fs::write(root.join("file.txt"), "base\n").unwrap();
        commit_all(&repository, "base");
        let engine = open_engine(&fixture, &root);
        let snapshot = engine.snapshot(&Cancellation::default()).unwrap();
        let context = engine
            .git_context_under(&snapshot, &Cancellation::default())
            .unwrap();

        fs::write(root.join("later.txt"), "later\n").unwrap();
        commit_all(&repository, "later");
        assert!(matches!(
            context.changed_files(&Cancellation::default()),
            Err(GitContextError::StaleSnapshot { .. })
        ));
    }

    #[test]
    fn renames_keep_their_source_and_secret_paths_never_enter_diff_items() {
        let fixture = Fixture::new();
        let root = fixture.directory("repository");
        let repository = initialize_repository(&root);
        fs::write(root.join("old.txt"), "rename me\n").unwrap();
        fs::write(root.join(".env"), "TOKEN=old\n").unwrap();
        commit_all(&repository, "base");
        git(&root, ["mv", "old.txt", "new.txt"]);
        fs::write(root.join(".env"), "TOKEN=secret\n").unwrap();

        let engine = open_engine(&fixture, &root);
        let snapshot = engine.snapshot(&Cancellation::default()).unwrap();
        let context = engine
            .git_context_under(&snapshot, &Cancellation::default())
            .unwrap();
        let changed = context.changed_files(&Cancellation::default()).unwrap();
        let renamed = changed
            .files
            .iter()
            .find(|file| file.path.as_bytes() == b"new.txt")
            .unwrap();
        assert_eq!(
            renamed.rename_source.as_ref().unwrap().as_bytes(),
            b"old.txt"
        );
        assert_eq!(changed.withheld_files, 1);

        let diff = context
            .workspace_diff(&GitContextBudget::default(), &Cancellation::default())
            .unwrap();
        assert!(
            diff.staged
                .files
                .iter()
                .chain(&diff.working.files)
                .all(|file| {
                    file.old_path.as_ref().map(RepoPath::as_bytes) != Some(b".env")
                        && file.new_path.as_ref().map(RepoPath::as_bytes) != Some(b".env")
                })
        );
        assert_eq!(diff.working.withheld_files, 1);
    }

    #[test]
    fn a_rename_crossing_the_inventory_boundary_withholds_both_names() {
        let fixture = Fixture::new();
        let root = fixture.directory("repository");
        let repository = initialize_repository(&root);
        fs::write(root.join(".env"), "TOKEN=secret\n").unwrap();
        commit_all(&repository, "secret source");
        git(&root, ["mv", ".env", "visible.txt"]);

        let engine = open_engine(&fixture, &root);
        let snapshot = engine.snapshot(&Cancellation::default()).unwrap();
        let context = engine
            .git_context_under(&snapshot, &Cancellation::default())
            .unwrap();
        let changed = context.changed_files(&Cancellation::default()).unwrap();
        assert!(changed.files.is_empty());
        assert_eq!(changed.withheld_files, 1);

        let diff = context
            .staged_diff(&GitContextBudget::default(), &Cancellation::default())
            .unwrap();
        assert!(diff.files.is_empty());
        assert_eq!(diff.withheld_files, 1);
    }

    #[test]
    fn file_budget_keeps_every_eligible_identity_and_names_omissions() {
        let fixture = Fixture::new();
        let root = fixture.directory("repository");
        let repository = initialize_repository(&root);
        for name in ["a.txt", "b.txt", "c.txt"] {
            fs::write(root.join(name), "base\n").unwrap();
        }
        commit_all(&repository, "base");
        for name in ["a.txt", "b.txt", "c.txt"] {
            fs::write(root.join(name), "changed\n").unwrap();
        }
        let engine = open_engine(&fixture, &root);
        let snapshot = engine.snapshot(&Cancellation::default()).unwrap();
        let context = engine
            .git_context_under(&snapshot, &Cancellation::default())
            .unwrap();
        let budget = GitContextBudget {
            max_files: 1,
            ..GitContextBudget::default()
        };
        let diff = context
            .working_diff(&budget, &Cancellation::default())
            .unwrap();
        assert_eq!(diff.files.len(), 3);
        assert_eq!(
            diff.files
                .iter()
                .filter(|file| matches!(
                    file.omission,
                    Some(GitDiffOmission::FileBudgetExhausted { limit: 1 })
                ))
                .count(),
            2
        );
    }

    #[test]
    fn blame_is_explicit_ranged_and_marks_dirty_lines_uncommitted() {
        let fixture = Fixture::new();
        let root = fixture.directory("repository");
        let repository = initialize_repository(&root);
        fs::write(root.join("file.txt"), "base\nsecond\n").unwrap();
        commit_all(&repository, "base");
        fs::write(root.join("file.txt"), "dirty\nsecond\n").unwrap();
        let engine = open_engine(&fixture, &root);
        let snapshot = engine.snapshot(&Cancellation::default()).unwrap();
        let context = engine
            .git_context_under(&snapshot, &Cancellation::default())
            .unwrap();

        let path = RepoPath::from_path(std::path::Path::new("file.txt"));
        let refused = BlameRequest {
            path: path.clone(),
            range: Some(harkness_git::BlameLineRange::new(1, 2).unwrap()),
            explicit: false,
        };
        assert!(matches!(
            context.blame(&refused, &Cancellation::default()),
            Err(GitContextError::BlameNotExplicit)
        ));
        let missing_range = BlameRequest {
            path: path.clone(),
            range: None,
            explicit: true,
        };
        assert!(matches!(
            context.blame(&missing_range, &Cancellation::default()),
            Err(GitContextError::BlameRangeRequired)
        ));

        let blamed = context
            .blame(
                &BlameRequest::explicit(path, harkness_git::BlameLineRange::new(1, 2).unwrap()),
                &Cancellation::default(),
            )
            .unwrap();
        assert!(
            blamed
                .entries
                .iter()
                .any(|entry| entry.commit == BlameContextCommit::Uncommitted)
        );
    }

    #[test]
    fn recent_history_is_bounded_paged_and_marked_untrusted() {
        let fixture = Fixture::new();
        let root = fixture.directory("repository");
        let repository = initialize_repository(&root);
        for index in 0..3 {
            fs::write(root.join("file.txt"), format!("{index}\n")).unwrap();
            commit_all(&repository, &format!("commit {index}"));
        }
        let engine = open_engine(&fixture, &root);
        let snapshot = engine.snapshot(&Cancellation::default()).unwrap();
        let context = engine
            .git_context_under(&snapshot, &Cancellation::default())
            .unwrap();
        let history = context
            .recent_commits(
                &LogOptions::new("HEAD", 2),
                &GitContextBudget::default(),
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(history.commits.len(), 2);
        assert!(history.next_cursor.is_some());
        assert!(history.omission.is_some());
        assert!(matches!(
            history.commits[0].provenance.sensitivity,
            Sensitivity::Suspicious { .. }
        ));
    }

    #[test]
    fn file_history_is_literal_bounded_and_does_not_follow_other_paths() {
        let fixture = Fixture::new();
        let root = fixture.directory("repository");
        let repository = initialize_repository(&root);
        fs::write(root.join("target.txt"), "one\n").unwrap();
        commit_all(&repository, "target one");
        fs::write(root.join("other.txt"), "other\n").unwrap();
        commit_all(&repository, "other only");
        fs::write(root.join("target.txt"), "two\n").unwrap();
        commit_all(&repository, "target two");

        let engine = open_engine(&fixture, &root);
        let snapshot = engine.snapshot(&Cancellation::default()).unwrap();
        let context = engine
            .git_context_under(&snapshot, &Cancellation::default())
            .unwrap();
        let history = context
            .file_history(
                &RepoPath::from_path(std::path::Path::new("target.txt")),
                10,
                &GitContextBudget::default(),
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(history.commits.len(), 2);
        assert!(
            history
                .commits
                .iter()
                .all(|commit| commit.summary.starts_with(b"target"))
        );
    }

    #[test]
    fn conflict_state_succeeds_and_names_the_unmerged_path() {
        let fixture = Fixture::new();
        let root = fixture.directory("repository");
        let repository = initialize_repository(&root);
        fs::write(root.join("file.txt"), "base\n").unwrap();
        commit_all(&repository, "base");
        let base = repository.head().unwrap().target().unwrap();
        let base_commit = repository.find_commit(base).unwrap();
        repository.branch("side", &base_commit, false).unwrap();
        drop(base_commit);

        fs::write(root.join("file.txt"), "main\n").unwrap();
        commit_all(&repository, "main edit");
        let main = repository.head().unwrap().target().unwrap();
        git(&root, ["switch", "side"]);
        fs::write(root.join("file.txt"), "side\n").unwrap();
        commit_all(&repository, "side edit");
        let annotated = repository.find_annotated_commit(main).unwrap();
        repository.merge(&[&annotated], None, None).unwrap();
        assert!(repository.index().unwrap().has_conflicts());

        let engine = open_engine(&fixture, &root);
        let snapshot = engine.snapshot(&Cancellation::default()).unwrap();
        let context = engine
            .git_context_under(&snapshot, &Cancellation::default())
            .unwrap();
        let conflicts = context.conflict_state(&Cancellation::default()).unwrap();
        assert!(conflicts.has_conflicts);
        assert_eq!(conflicts.paths.len(), 1);
        assert_eq!(conflicts.paths[0].as_bytes(), b"file.txt");

        let raw = GitService::new(&root, &fixture.data_dir)
            .diff(DiffTarget::Unstaged, &harkness_git::DiffOptions::default())
            .unwrap();
        assert!(
            raw.iter()
                .any(|file| matches!(file.omission, Some(harkness_git::DiffOmission::Unmerged)))
        );
    }
}
