//! Bounded, read-only change attribution for one diff target.
//!
//! A reviewer opening a branch review asks two questions of every file: what
//! changed, and what produced it. The diff answers the first. This module
//! answers the second, and answers it from the repository alone — the commits
//! between the two sides of the comparison, the identities those commits
//! record, and the `agent/<slug>` branch convention Harkness uses for a
//! worktree it runs an agent in.
//!
//! Four properties are load-bearing.
//!
//! **One pass over the range, never a walk per file.** The range is walked
//! once, every commit in it is compared with its first parent once, and each
//! delta is recorded against the paths it names. A thousand-file review costs
//! the same walk a one-file review costs, and that cost is a function of the
//! *range* rather than of the repository's history. Nothing here follows a
//! rename backwards, because that is a per-file history walk by another name.
//!
//! **Nothing is inferred that cannot be read.** A producer is an identity a
//! commit actually recorded: its `author`, or a `Co-Authored-By` trailer, which
//! is where Harkness's own commit convention names the model that helped write
//! a change. Neither is classified as human or machine, because the repository
//! does not say and a guess would be worse than the question. The one
//! Harkness-specific reading is the branch convention, and it is reported as
//! exactly what it is — a slug taken from a reference name, recorded on the
//! range rather than on a file.
//!
//! **Absence is an answer.** Every requested path appears in the result, and a
//! path no commit in the range touched carries a [`ProvenanceGap`] naming why
//! rather than an empty field a reader has to interpret. A repository Harkness
//! has never run anything in resolves successfully with every file unknown,
//! which is the common case and must be the calm one.
//!
//! **It is advisory.** Attribution decides nothing: no staging, discarding, or
//! diffing behaviour may read it. A wrong attribution is a cosmetic error, and
//! that is the licence this module takes for skipping merges and for not
//! following renames.
//!
//! Like history and diff inspection, everything here is libgit2 in process. It
//! takes no repository lock, spawns no process, and contacts no remote.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use git2::{Delta, Oid, Repository, Sort};

use crate::{
    Cancellation, CommitSignature, DiffTarget, FileDiff, GitError, commit, diff::os_str_bytes,
    history,
};

/// The default number of commits one attribution will walk.
///
/// The bound exists because a range is caller-named and can reach the root of a
/// repository's history. Reaching it degrades the answer to a named
/// [`ProvenanceTruncation`] rather than failing the read or spending unbounded
/// time on the path that opens a panel.
pub const DEFAULT_MAX_PROVENANCE_COMMITS: usize = 1_000;

/// The most `Co-Authored-By` trailers read from one commit message.
///
/// A commit message is repository content and therefore untrusted input. The
/// producer table is keyed by identity and would otherwise grow with whatever a
/// message chose to write into its last paragraph.
pub const MAX_CO_AUTHORS_PER_COMMIT: usize = 16;

/// The reference-name prefix Harkness gives a branch it runs an agent on.
pub const AGENT_BRANCH_PREFIX: &str = "agent/";

/// How one identity was named by the commits it appears in.
///
/// The distinction is evidential rather than a claim about what kind of thing
/// the producer is: Git records an author and it records trailers, and this
/// says which of the two was read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProducerKind {
    /// The identity is the `author` of at least one commit in the range.
    Author,
    /// The identity appears only in `Co-Authored-By` trailers. Harkness's own
    /// commit convention names the model that helped write a change here.
    CoAuthor,
}

impl ProducerKind {
    /// The stable wire spelling shared by the CLI envelope and the panel.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Author => "author",
            Self::CoAuthor => "co_author",
        }
    }
}

/// One distinct identity that contributed to a comparison.
///
/// Producers are addressed by their index in [`ChangeProvenance::producers`].
/// That index is the grouping key a front end tints by: two files whose
/// producer sets differ are two files a reviewer should be able to tell apart
/// without reading a name.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct Producer {
    /// How this identity was named. [`ProducerKind::Author`] wins when an
    /// identity appears both ways.
    pub kind: ProducerKind,
    /// Name bytes exactly as the commit recorded them.
    pub name: Vec<u8>,
    /// Email bytes exactly as the commit recorded them, empty when a trailer
    /// carried none.
    pub email: Vec<u8>,
}

/// One commit that contributed content to a comparison.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct CommitAttribution {
    /// Full object ID of the commit.
    pub id: Oid,
    /// Original author signature.
    pub author: CommitSignature,
    /// Committer signature. It differs from the author after a rebase, a
    /// cherry-pick, or a patch applied on someone else's behalf.
    pub committer: CommitSignature,
    /// First line of the raw commit message, retained as bytes.
    pub summary: Vec<u8>,
    /// Indices into [`ChangeProvenance::producers`]: the author first, then any
    /// co-authors in the order their trailers appeared.
    pub producers: Vec<usize>,
}

/// Why a changed file carries no commit attribution.
///
/// Every reason is named rather than reported by failing the read, for the same
/// reason [`crate::DiffOmission`] names every reason a file carries no hunks:
/// one path Harkness cannot attribute must not cost a reviewer the other
/// thousand it can.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProvenanceGap {
    /// The comparison has no commit range at all. A working-tree or index
    /// target compares content nothing has committed, so no commit produced it.
    Uncommitted,
    /// The range is empty because both of its sides name one commit.
    EmptyRange,
    /// The range was walked and no commit in it names this path. A path reached
    /// only through a rename Harkness did not follow, and a path whose only
    /// change is on the old side of a two-revision comparison, both land here.
    NotInRange,
    /// The walk stopped at its commit budget before this path was reached.
    /// Raise [`ProvenanceOptions::max_commits`] to see further.
    CommitBudgetExhausted {
        /// The budget that was reached.
        limit: usize,
    },
}

impl ProvenanceGap {
    /// The stable wire spelling shared by the CLI envelope and the panel.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Uncommitted => "uncommitted",
            Self::EmptyRange => "empty_range",
            Self::NotInRange => "not_in_range",
            Self::CommitBudgetExhausted { .. } => "commit_budget_exhausted",
        }
    }
}

/// What produced one file within one comparison.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct FileProvenance {
    /// The path attribution was requested for, byte-for-byte.
    pub path: PathBuf,
    /// Indices into [`ChangeProvenance::commits`], newest first.
    ///
    /// A file may well name several, and that is the multi-contributor case a
    /// branch review exists to show: it is never collapsed to the most recent.
    pub commits: Vec<usize>,
    /// Indices into [`ChangeProvenance::producers`], in order of first
    /// contribution starting from the newest commit.
    pub producers: Vec<usize>,
    /// Why [`Self::commits`] is empty. `None` whenever it is not.
    pub gap: Option<ProvenanceGap>,
}

impl FileProvenance {
    /// Whether this file names anything at all.
    ///
    /// The negative case is the common one and must render as *unknown* rather
    /// than as blank.
    #[must_use]
    pub fn is_attributed(&self) -> bool {
        !self.commits.is_empty()
    }
}

/// The commit range one attribution was resolved over.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ProvenanceRange {
    /// The newest commit in the range.
    pub head: Oid,
    /// The commit the range starts *after*. `None` means it reaches the root of
    /// history.
    pub base: Option<Oid>,
    /// The revision expression the target named the head side by, as written.
    pub head_revision: String,
    /// The reference the caller resolved the head side from, when it said.
    ///
    /// A front end that pins a branch review to object IDs — which the panel
    /// does, so a branch advancing under a reader cannot move the review — has
    /// a target whose revision is a hexadecimal id and a reviewer who
    /// nonetheless asked for a branch by name. This is that name.
    pub head_reference: Option<String>,
    /// The slug of the head reference, or of [`Self::head_revision`] when the
    /// caller named none, if it follows the `agent/<slug>` branch convention.
    ///
    /// This is a fact about the reference rather than about any one commit —
    /// every file in the range came off that branch — so it belongs on the
    /// header of a review and not on a row of its file list.
    pub agent_slug: Option<String>,
}

/// Why an attribution describes less of its range than the range holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProvenanceTruncation {
    /// [`ProvenanceOptions::max_commits`] was reached with commits left to
    /// walk.
    CommitBudgetExhausted {
        /// The budget that was reached.
        limit: usize,
    },
}

impl ProvenanceTruncation {
    /// The stable wire spelling shared by the CLI envelope and the panel.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::CommitBudgetExhausted { .. } => "commit_budget_exhausted",
        }
    }
}

/// Which paths one attribution is about.
///
/// This is one value rather than a list whose emptiness has to be interpreted,
/// because the two things an empty list could mean are opposites: a caller
/// asking about a whole range and a caller narrowing to a file list that came
/// back with nothing in it. Inferring one from the other would make a review
/// with no changed files walk its entire range and report every path in it.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProvenancePaths {
    /// Every path the range touched, ordered by path bytes.
    All,
    /// Exactly these, in this order. Empty means exactly none.
    ///
    /// One entry is produced per element, repeats included, so a result always
    /// pairs with the list that asked for it by index. A repeated path answers
    /// the same both times rather than reading as unattributed the second.
    Only(Vec<PathBuf>),
}

impl ProvenancePaths {
    /// Whether this asks about nothing, and the walk can therefore be skipped.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Only(paths) if paths.is_empty())
    }
}

/// Bounds and narrows one attribution.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ProvenanceOptions {
    /// The paths to attribute, in the order the result should report them.
    ///
    /// Pass the paths of the diff being reviewed, so the result answers about
    /// the review in front of the caller and nothing else.
    pub paths: ProvenancePaths,
    /// The most commits the walk will visit.
    pub max_commits: usize,
    /// The reference the caller resolved the target's head side from.
    ///
    /// Only a caller that resolved a name into an object id itself can supply
    /// this, and only such a caller needs to: a target that still names a
    /// branch carries the name already.
    pub head_reference: Option<String>,
}

impl Default for ProvenanceOptions {
    fn default() -> Self {
        Self {
            paths: ProvenancePaths::All,
            max_commits: DEFAULT_MAX_PROVENANCE_COMMITS,
            head_reference: None,
        }
    }
}

impl ProvenanceOptions {
    /// Attributes the paths one diff reported, one entry per record and in the
    /// diff's own order.
    ///
    /// A renamed or deleted file is requested under the path the diff shows it
    /// at: its new-side path where it has one, and its old-side path otherwise.
    /// Repeats are kept rather than folded, so the result pairs with the file
    /// list by index — which is what a front end holding both actually needs,
    /// and a path named twice answers the same both times.
    ///
    /// Pass the records of **one** target. Each target has its own attribution,
    /// and a multi-target list would ask one of them about another's paths.
    #[must_use]
    pub fn for_files(files: &[FileDiff]) -> Self {
        let paths = files
            .iter()
            .filter_map(|file| file.new_path.as_ref().or(file.old_path.as_ref()))
            .cloned()
            .collect();
        Self {
            paths: ProvenancePaths::Only(paths),
            ..Self::default()
        }
    }

    /// Attributes only these paths, in this order.
    ///
    /// An empty iterator narrows to nothing and is not a request for
    /// everything: use [`ProvenancePaths::All`] for that.
    #[must_use]
    pub fn with_paths<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.paths = ProvenancePaths::Only(paths.into_iter().map(Into::into).collect());
        self
    }

    /// Replaces the commit budget.
    #[must_use]
    pub fn with_max_commits(mut self, max_commits: usize) -> Self {
        self.max_commits = max_commits;
        self
    }

    /// Names the reference the caller resolved the head side from.
    ///
    /// This changes no walk and no attribution. It only gives the reference
    /// conventions — the `agent/<slug>` one — something to read, which a
    /// target pinned to an object id otherwise takes away.
    #[must_use]
    pub fn with_head_reference(mut self, reference: impl Into<String>) -> Self {
        self.head_reference = Some(reference.into());
        self
    }
}

/// What produced each file of one comparison.
///
/// The record is total: [`Self::files`] holds one entry per requested path
/// whether or not anything could be attributed to it.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ChangeProvenance {
    /// The comparison this describes.
    pub target: DiffTarget,
    /// The commit range walked, absent for a target that has none.
    pub range: Option<ProvenanceRange>,
    /// Every distinct identity that contributed, in order of first appearance.
    pub producers: Vec<Producer>,
    /// Every commit that contributed to a requested path, newest first.
    ///
    /// A commit the walk visited that touched nothing being attributed is not
    /// here, so a narrowed request never carries commits no file references.
    /// [`Self::walked_commits`] is what says how far the walk went.
    pub commits: Vec<CommitAttribution>,
    /// One entry per requested path.
    pub files: Vec<FileProvenance>,
    /// How many commits the walk visited, merges included.
    pub walked_commits: usize,
    /// How many merge commits the walk passed over.
    ///
    /// A merge introduces content of its own only where it resolved a conflict,
    /// and comparing one with its first parent would attribute everything the
    /// merged branch did to whoever ran the merge. The commits it merged are
    /// themselves in the range, so skipping loses a conflict resolution and
    /// nothing else. The count is reported rather than hidden, because that
    /// loss is real.
    pub skipped_merges: usize,
    /// Why the walk describes less than its range holds.
    pub truncation: Option<ProvenanceTruncation>,
}

impl ChangeProvenance {
    /// Whether no commit contributed to any requested path.
    ///
    /// This is the answer for every working-tree comparison, for an empty
    /// range, and for a request narrowed to paths the range never touched. It
    /// says nothing about whether Harkness produced the work: an ordinary
    /// branch review of a repository Harkness has never run in has commits, and
    /// answers `false`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.commits.is_empty()
    }
}

/// The commit walk one target implies.
enum Walk {
    /// The comparison has no commit range: its content is uncommitted.
    Uncommitted,
    /// Exactly one commit, compared with the parent the target named.
    Single { commit: Oid, parent: Option<Oid> },
    /// Every commit reachable from `head` and not from `base`.
    Range { head: Oid, base: Option<Oid> },
}

pub(crate) fn resolve(
    root: &Path,
    target: &DiffTarget,
    options: &ProvenanceOptions,
    cancellation: &Cancellation,
) -> Result<ChangeProvenance, GitError> {
    refuse_cancelled(cancellation)?;
    let repository = commit::open(root)?;
    let (walk, range) = plan(&repository, root, target, options.head_reference.as_deref())?;

    let mut builder = Builder::new(options);
    // Narrowed to no paths at all: there is nothing the walk could answer, so
    // the range is still reported and not one commit is read.
    if options.paths.is_empty() {
        return Ok(builder.finish(target.clone(), range));
    }
    match walk {
        Walk::Uncommitted => builder.without_range(ProvenanceGap::Uncommitted),
        Walk::Single { commit, parent } => {
            builder.visit(&repository, root, commit, parent, cancellation)?;
        }
        Walk::Range { head, base } => {
            builder.walk_range(&repository, root, head, base, cancellation)?;
        }
    }

    Ok(builder.finish(target.clone(), range))
}

/// Resolves the range a target implies, without walking it.
fn plan(
    repository: &Repository,
    root: &Path,
    target: &DiffTarget,
    head_reference: Option<&str>,
) -> Result<(Walk, Option<ProvenanceRange>), GitError> {
    match target {
        // Every working-tree and index comparison is uncommitted content by
        // construction. Resolving a range for one would attribute a file to the
        // commit that last touched it, which is a different question from the
        // one the diff in front of the reviewer is asking.
        DiffTarget::Staged | DiffTarget::Unstaged | DiffTarget::RevisionAgainstWorktree { .. } => {
            Ok((Walk::Uncommitted, None))
        }
        DiffTarget::Commit { revision, parent } => {
            let head = history::require_commit(repository, root, revision)?;
            let commit = repository
                .find_commit(head)
                .map_err(|source| inspection(root, source))?;
            let base = match parent {
                Some(parent) => {
                    let parent_id = history::require_commit(repository, root, parent)?;
                    if !commit.parent_ids().any(|candidate| candidate == parent_id) {
                        return Err(GitError::RevisionNotParent {
                            revision: revision.clone(),
                            parent: parent.clone(),
                        });
                    }
                    Some(parent_id)
                }
                None => commit.parent_ids().next(),
            };
            Ok((
                Walk::Single {
                    commit: head,
                    parent: base,
                },
                Some(describe_range(head, base, revision, head_reference)),
            ))
        }
        DiffTarget::Revisions {
            old_revision,
            new_revision,
        } => {
            let head = history::require_commit(repository, root, new_revision)?;
            let base = history::require_commit(repository, root, old_revision)?;
            Ok((
                Walk::Range {
                    head,
                    base: Some(base),
                },
                Some(describe_range(
                    head,
                    Some(base),
                    new_revision,
                    head_reference,
                )),
            ))
        }
        DiffTarget::BranchAgainstBase {
            branch,
            base_branch,
        } => {
            // Resolve both moving names once, exactly as the diff does, so the
            // attribution describes the snapshot its file list describes.
            let head = history::require_commit(repository, root, branch)?;
            let base_id = history::require_commit(repository, root, base_branch)?;
            let base =
                history::merge_base_ids(repository, root, head, base_id, branch, base_branch)?;
            Ok((
                Walk::Range {
                    head,
                    base: Some(base),
                },
                Some(describe_range(head, Some(base), branch, head_reference)),
            ))
        }
    }
}

fn describe_range(
    head: Oid,
    base: Option<Oid>,
    head_revision: &str,
    head_reference: Option<&str>,
) -> ProvenanceRange {
    ProvenanceRange {
        head,
        base,
        head_revision: head_revision.to_owned(),
        head_reference: head_reference.map(ToOwned::to_owned),
        // The reference wins when the caller named one: it is the spelling a
        // reviewer chose, and the revision beside it may be the object id that
        // spelling was pinned to.
        agent_slug: agent_slug(head_reference.unwrap_or(head_revision)),
    }
}

/// Reads the `agent/<slug>` convention off a reference name.
///
/// A fully qualified `refs/heads/` or `refs/remotes/<remote>/` name is accepted
/// alongside the short form, because a caller naming a branch either way means
/// the same branch. An empty slug is not a slug.
fn agent_slug(revision: &str) -> Option<String> {
    let short = revision
        .strip_prefix("refs/heads/")
        .or_else(|| {
            revision
                .strip_prefix("refs/remotes/")
                .and_then(|rest| rest.split_once('/'))
                .map(|(_remote, rest)| rest)
        })
        .unwrap_or(revision);
    let slug = short.strip_prefix(AGENT_BRANCH_PREFIX)?;
    (!slug.is_empty()).then(|| slug.to_owned())
}

/// Accumulates one attribution across a range.
struct Builder<'options> {
    options: &'options ProvenanceOptions,
    /// The requested paths in caller order, or `None` when every path the range
    /// touched was asked for.
    requested: Option<Vec<PathBuf>>,
    /// The same set, for a delta lookup that must not scale with the file list.
    wanted: HashSet<PathBuf>,
    /// Producer index by folded identity key.
    producer_index: HashMap<Vec<u8>, usize>,
    producers: Vec<Producer>,
    commits: Vec<CommitAttribution>,
    /// Commit indices per path, newest first and free of repeats.
    per_path: HashMap<PathBuf, Vec<usize>>,
    /// Paths discovered by the walk, when the caller requested none.
    discovered: Vec<PathBuf>,
    walked_commits: usize,
    skipped_merges: usize,
    truncation: Option<ProvenanceTruncation>,
    /// The gap every unattributed path reports.
    default_gap: ProvenanceGap,
}

impl<'options> Builder<'options> {
    fn new(options: &'options ProvenanceOptions) -> Self {
        let requested = match &options.paths {
            ProvenancePaths::All => None,
            ProvenancePaths::Only(paths) => Some(paths.clone()),
        };
        Self {
            options,
            wanted: requested.iter().flatten().cloned().collect(),
            requested,
            producer_index: HashMap::new(),
            producers: Vec::new(),
            commits: Vec::new(),
            per_path: HashMap::new(),
            discovered: Vec::new(),
            walked_commits: 0,
            skipped_merges: 0,
            truncation: None,
            default_gap: ProvenanceGap::NotInRange,
        }
    }

    fn without_range(&mut self, gap: ProvenanceGap) {
        self.default_gap = gap;
    }

    fn walk_range(
        &mut self,
        repository: &Repository,
        root: &Path,
        head: Oid,
        base: Option<Oid>,
        cancellation: &Cancellation,
    ) -> Result<(), GitError> {
        let mut walk = repository
            .revwalk()
            .map_err(|source| inspection(root, source))?;
        walk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME)
            .map_err(|source| inspection(root, source))?;
        walk.push(head).map_err(|source| inspection(root, source))?;
        if let Some(base) = base {
            walk.hide(base).map_err(|source| inspection(root, source))?;
        }

        let mut empty = true;
        while self.walked_commits < self.options.max_commits {
            refuse_cancelled(cancellation)?;
            let Some(id) = walk.next() else {
                if empty {
                    self.default_gap = ProvenanceGap::EmptyRange;
                }
                return Ok(());
            };
            empty = false;
            let id = id.map_err(|source| inspection(root, source))?;
            self.walked_commits += 1;
            let commit = repository
                .find_commit(id)
                .map_err(|source| inspection(root, source))?;
            if commit.parent_count() > 1 {
                self.skipped_merges += 1;
                continue;
            }
            let parent = commit.parent_ids().next();
            self.record(repository, root, &commit, parent)?;
        }

        refuse_cancelled(cancellation)?;
        if walk.next().is_some() {
            let limit = self.options.max_commits;
            self.truncation = Some(ProvenanceTruncation::CommitBudgetExhausted { limit });
            self.default_gap = ProvenanceGap::CommitBudgetExhausted { limit };
        } else if empty {
            self.default_gap = ProvenanceGap::EmptyRange;
        }
        Ok(())
    }

    /// Attributes exactly one commit, for a single-commit target.
    ///
    /// A merge is deliberately *not* skipped here: the caller asked about this
    /// commit against this parent, which is precisely the comparison the diff
    /// in front of them shows.
    fn visit(
        &mut self,
        repository: &Repository,
        root: &Path,
        id: Oid,
        parent: Option<Oid>,
        cancellation: &Cancellation,
    ) -> Result<(), GitError> {
        refuse_cancelled(cancellation)?;
        if self.options.max_commits == 0 {
            self.truncation = Some(ProvenanceTruncation::CommitBudgetExhausted { limit: 0 });
            self.default_gap = ProvenanceGap::CommitBudgetExhausted { limit: 0 };
            return Ok(());
        }
        let commit = repository
            .find_commit(id)
            .map_err(|source| inspection(root, source))?;
        self.walked_commits += 1;
        self.record(repository, root, &commit, parent)
    }

    /// Compares one commit with one parent and records the paths it names.
    fn record(
        &mut self,
        repository: &Repository,
        root: &Path,
        commit: &git2::Commit<'_>,
        parent: Option<Oid>,
    ) -> Result<(), GitError> {
        let new_tree = commit.tree().map_err(|source| inspection(root, source))?;
        let old_tree = parent
            .map(|parent| {
                repository
                    .find_commit(parent)
                    .and_then(|parent| parent.tree())
                    .map_err(|source| inspection(root, source))
            })
            .transpose()?;

        // No content is read: a delta names its paths, and nothing here needs a
        // hunk, a blob, or a similarity score. Rename detection is deliberately
        // absent — `find_similar` would pair a delete with an add this record
        // already names on both paths, at the cost of hashing content once per
        // commit in the range.
        let mut native = git2::DiffOptions::new();
        native
            .context_lines(0)
            .interhunk_lines(0)
            .include_typechange(true)
            .skip_binary_check(true);
        let diff = repository
            .diff_tree_to_tree(old_tree.as_ref(), Some(&new_tree), Some(&mut native))
            .map_err(|source| inspection(root, source))?;

        // The deltas decide whether this commit is recorded at all, so they are
        // scanned before it is interned. A commit that touched nothing the
        // caller asked about contributed nothing to the comparison in front of
        // them, and recording it anyway would put commits in the result that no
        // file references, people in the producer list whose work is not being
        // reviewed, and a `false` in `is_empty` for a result where every file
        // came back unknown. It still counts as walked, because it was.
        if !diff.deltas().any(|delta| {
            !matches!(delta.status(), Delta::Unmodified | Delta::Ignored)
                && (self.wants(delta.new_file().path()) || self.wants(delta.old_file().path()))
        }) {
            return Ok(());
        }

        let index = self.push_commit(commit);
        for delta in diff.deltas() {
            if matches!(delta.status(), Delta::Unmodified | Delta::Ignored) {
                continue;
            }
            for path in [delta.new_file().path(), delta.old_file().path()]
                .into_iter()
                .flatten()
            {
                self.attribute(path, index);
            }
        }
        Ok(())
    }

    /// Whether one delta side is a path this attribution is about.
    fn wants(&self, path: Option<&Path>) -> bool {
        let Some(path) = path else {
            return false;
        };
        self.requested.is_none() || self.wanted.contains(path)
    }

    /// Records one path as touched by one commit.
    fn attribute(&mut self, path: &Path, commit: usize) {
        if !self.wants(Some(path)) {
            return;
        }
        if let Some(entry) = self.per_path.get_mut(path) {
            if entry.last() != Some(&commit) {
                entry.push(commit);
            }
            return;
        }
        if self.requested.is_none() {
            self.discovered.push(path.to_path_buf());
        }
        self.per_path.insert(path.to_path_buf(), vec![commit]);
    }

    fn push_commit(&mut self, commit: &git2::Commit<'_>) -> usize {
        let author = signature(commit.author());
        let mut producers = vec![self.producer(ProducerKind::Author, &author.name, &author.email)];
        for (name, email) in co_authors(commit.message_raw_bytes()) {
            let index = self.producer(ProducerKind::CoAuthor, &name, &email);
            if !producers.contains(&index) {
                producers.push(index);
            }
        }
        let message = commit.message_raw_bytes();
        let summary = message
            .split(|byte| *byte == b'\n')
            .next()
            .unwrap_or_default()
            .to_vec();
        self.commits.push(CommitAttribution {
            id: commit.id(),
            author,
            committer: signature(commit.committer()),
            summary,
            producers,
        });
        self.commits.len() - 1
    }

    /// Interns one identity, promoting a co-author to an author when it turns
    /// out to have written a commit of its own.
    fn producer(&mut self, kind: ProducerKind, name: &[u8], email: &[u8]) -> usize {
        let key = identity_key(name, email);
        if let Some(index) = self.producer_index.get(&key).copied() {
            if kind == ProducerKind::Author {
                self.producers[index].kind = ProducerKind::Author;
            }
            return index;
        }
        self.producers.push(Producer {
            kind,
            name: name.to_vec(),
            email: email.to_vec(),
        });
        let index = self.producers.len() - 1;
        self.producer_index.insert(key, index);
        index
    }

    fn finish(mut self, target: DiffTarget, range: Option<ProvenanceRange>) -> ChangeProvenance {
        let paths = self.requested.take().unwrap_or_else(|| {
            let mut discovered = std::mem::take(&mut self.discovered);
            discovered.sort_by(|left, right| {
                os_str_bytes(left.as_os_str()).cmp(os_str_bytes(right.as_os_str()))
            });
            discovered
        });

        let files = paths
            .into_iter()
            .map(|path| {
                // Read rather than removed: a caller may legitimately request
                // one path twice, and the second copy must answer the same as
                // the first instead of reading as unattributed.
                let commits = self.per_path.get(&path).cloned().unwrap_or_default();
                let mut producers: Vec<usize> = Vec::new();
                for commit in &commits {
                    for producer in &self.commits[*commit].producers {
                        if !producers.contains(producer) {
                            producers.push(*producer);
                        }
                    }
                }
                let gap = commits.is_empty().then_some(self.default_gap);
                FileProvenance {
                    path,
                    commits,
                    producers,
                    gap,
                }
            })
            .collect();

        ChangeProvenance {
            target,
            range,
            producers: self.producers,
            commits: self.commits,
            files,
            walked_commits: self.walked_commits,
            skipped_merges: self.skipped_merges,
            truncation: self.truncation,
        }
    }
}

/// The key two spellings of one identity must share.
///
/// Email decides when there is one, because a display name changes between
/// machines while an address does not. Case is folded on ASCII only: an address
/// is case-insensitive by convention, and folding beyond ASCII would need a
/// locale this layer has no business choosing.
fn identity_key(name: &[u8], email: &[u8]) -> Vec<u8> {
    let source = if email.is_empty() { name } else { email };
    source.to_ascii_lowercase()
}

/// Reads `Co-Authored-By` identities out of a commit message's trailer block.
///
/// Only the last paragraph is considered, which is Git's own rule and the one
/// thing that stops a trailer quoted inside a message body from being read as a
/// contributor. The result is bounded by [`MAX_CO_AUTHORS_PER_COMMIT`], because
/// a commit message is repository content and nothing else bounds it.
fn co_authors(message: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    const TRAILER: &[u8] = b"co-authored-by";

    let lines: Vec<&[u8]> = message.split(|byte| *byte == b'\n').collect();
    let end = lines
        .iter()
        .rposition(|line| !line.iter().all(u8::is_ascii_whitespace))
        .map_or(0, |position| position + 1);
    let start = lines[..end]
        .iter()
        .rposition(|line| line.iter().all(u8::is_ascii_whitespace))
        .map_or(0, |position| position + 1);

    let mut found = Vec::new();
    for line in &lines[start..end] {
        if found.len() == MAX_CO_AUTHORS_PER_COMMIT {
            break;
        }
        let Some(separator) = line.iter().position(|byte| *byte == b':') else {
            continue;
        };
        let (token, value) = line.split_at(separator);
        if !trim_ascii(token).eq_ignore_ascii_case(TRAILER) {
            continue;
        }
        let Some(identity) = parse_identity(&value[1..]) else {
            continue;
        };
        found.push(identity);
    }
    found
}

/// Splits `Name <email>` into its two halves, keeping the bytes of each.
fn parse_identity(value: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let value = trim_ascii(value);
    if value.is_empty() {
        return None;
    }
    let (Some(open), Some(close)) = (
        value.iter().rposition(|byte| *byte == b'<'),
        value.iter().rposition(|byte| *byte == b'>'),
    ) else {
        return Some((value.to_vec(), Vec::new()));
    };
    if close < open {
        return Some((value.to_vec(), Vec::new()));
    }
    let name = trim_ascii(&value[..open]).to_vec();
    let email = trim_ascii(&value[open + 1..close]).to_vec();
    if name.is_empty() && email.is_empty() {
        return None;
    }
    Some((name, email))
}

fn trim_ascii(value: &[u8]) -> &[u8] {
    let start = value
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(value.len());
    let end = value
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |position| position + 1);
    &value[start..end]
}

fn signature(signature: git2::Signature<'_>) -> CommitSignature {
    CommitSignature {
        name: signature.name_bytes().to_vec(),
        email: signature.email_bytes().to_vec(),
        time: signature.when(),
    }
}

fn refuse_cancelled(cancellation: &Cancellation) -> Result<(), GitError> {
    if cancellation.is_cancelled() {
        Err(GitError::Cancelled)
    } else {
        Ok(())
    }
}

fn inspection(path: &Path, source: git2::Error) -> GitError {
    GitError::Inspection {
        path: path.to_path_buf(),
        source: source.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use git2::{Oid, Repository, Signature, Time, build::CheckoutBuilder};

    use super::{
        ChangeProvenance, DEFAULT_MAX_PROVENANCE_COMMITS, ProducerKind, ProvenanceGap,
        ProvenanceOptions, ProvenanceTruncation, agent_slug, co_authors,
    };
    use crate::{
        Cancellation, DiffOptions, DiffTarget, GitError, GitService,
        testing::{
            COMMIT_EPOCH_SECONDS, Fixture, PROCESS_PROJECT_ROOT_ENV, PROCESS_READY_FILE_ENV,
            initialize_repository, spawn_child, wait_for_child_signal,
        },
    };

    /// The one property the whole issue turns on: opening a branch review says
    /// what produced each of its files, and says it in one pass.
    #[test]
    fn a_branch_review_attributes_each_file_to_the_commits_that_touched_it() {
        let fixture = Fixture::new();
        let root = fixture.directory("branch-review");
        let repository = initialize_repository(&root);
        branch_from_head(&repository, "feature");
        checkout(&repository, "feature");

        let first = commit(
            &repository,
            &root,
            &[("alpha.txt", "one\n"), ("beta.txt", "one\n")],
            "add alpha and beta",
            ("Ada", "ada@example.invalid"),
            1,
        );
        let second = commit(
            &repository,
            &root,
            &[("beta.txt", "two\n")],
            "revise beta",
            ("Ada", "ada@example.invalid"),
            2,
        );

        let provenance = review(&fixture, &root);
        assert_eq!(provenance.walked_commits, 2);
        assert_eq!(provenance.skipped_merges, 0);
        assert_eq!(provenance.truncation, None);
        assert!(!provenance.is_empty());

        let range = provenance.range.as_ref().expect("a branch has a range");
        assert_eq!(range.head, second);
        assert_eq!(range.head_revision, "feature");
        assert_eq!(range.agent_slug, None);

        // Newest first, and a file touched twice keeps both rather than
        // collapsing onto the most recent.
        assert_eq!(commits_for(&provenance, "beta.txt"), vec![second, first]);
        assert_eq!(commits_for(&provenance, "alpha.txt"), vec![first]);
        for file in &provenance.files {
            assert_eq!(file.gap, None, "{}", file.path.display());
            assert!(file.is_attributed());
        }
    }

    /// The second acceptance criterion, and the reason attribution is a set
    /// rather than a single name.
    #[test]
    fn a_file_touched_by_two_authors_in_the_range_names_both() {
        let fixture = Fixture::new();
        let root = fixture.directory("two-authors");
        let repository = initialize_repository(&root);
        branch_from_head(&repository, "feature");
        checkout(&repository, "feature");
        commit(
            &repository,
            &root,
            &[("shared.txt", "one\n")],
            "first pass",
            ("Ada", "ada@example.invalid"),
            1,
        );
        commit(
            &repository,
            &root,
            &[("shared.txt", "two\n")],
            "second pass",
            ("Grace", "grace@example.invalid"),
            2,
        );

        let provenance = review(&fixture, &root);
        let file = file_for(&provenance, "shared.txt");
        assert_eq!(file.commits.len(), 2);
        assert_eq!(file.producers.len(), 2);
        let names = file
            .producers
            .iter()
            .map(|index| provenance.producers[*index].name.clone())
            .collect::<Vec<_>>();
        assert_eq!(names, vec![b"Grace".to_vec(), b"Ada".to_vec()]);
        assert!(
            provenance
                .producers
                .iter()
                .all(|producer| producer.kind == ProducerKind::Author)
        );
    }

    /// The common case, and the one that has to be calmest: a repository whose
    /// history Harkness had nothing to do with, and a working tree whose
    /// content no commit has yet claimed.
    #[test]
    fn a_repository_with_no_attribution_renders_every_file_unknown() {
        let fixture = Fixture::new();
        let root = fixture.directory("unattributed");
        initialize_repository(&root);
        fs::write(root.join("scratch.txt"), "written by nobody\n").unwrap();
        let service = GitService::new(&root, &fixture.data_dir);

        for target in [DiffTarget::Unstaged, DiffTarget::Staged] {
            let files = service
                .diff(target.clone(), &DiffOptions::default())
                .unwrap();
            let provenance = service
                .provenance(
                    &target,
                    &ProvenanceOptions::for_files(&files),
                    &Cancellation::default(),
                )
                .unwrap();
            assert!(provenance.is_empty(), "{target:?}");
            assert_eq!(provenance.range, None, "{target:?}");
            assert_eq!(provenance.producers, Vec::new(), "{target:?}");
            assert_eq!(provenance.walked_commits, 0, "{target:?}");
            for file in &provenance.files {
                assert!(!file.is_attributed());
                assert_eq!(file.gap, Some(ProvenanceGap::Uncommitted));
            }
        }

        // A branch with nothing on it resolves too, and says so by name.
        let empty = service
            .provenance(
                &DiffTarget::BranchAgainstBase {
                    branch: "main".to_owned(),
                    base_branch: "main".to_owned(),
                },
                &ProvenanceOptions::default().with_paths(["tracked.txt"]),
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(empty.files[0].gap, Some(ProvenanceGap::EmptyRange));
    }

    /// The performance criterion, asserted structurally rather than by a clock:
    /// the walk is a function of the range, and a file the range never touched
    /// costs nothing but its own row.
    #[test]
    fn attribution_walks_the_range_once_however_many_files_it_reviews() {
        let fixture = Fixture::new();
        let root = fixture.directory("many-files");
        let repository = initialize_repository(&root);
        branch_from_head(&repository, "feature");
        checkout(&repository, "feature");
        let files = (0..200)
            .map(|index| (format!("file-{index:03}.txt"), "content\n".to_owned()))
            .collect::<Vec<_>>();
        let borrowed = files
            .iter()
            .map(|(path, content)| (path.as_str(), content.as_str()))
            .collect::<Vec<_>>();
        let only = commit(
            &repository,
            &root,
            &borrowed,
            "add two hundred files",
            ("Ada", "ada@example.invalid"),
            1,
        );

        let mut requested = files
            .iter()
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        requested.push("never-existed.txt".to_owned());
        let provenance = GitService::new(&root, &fixture.data_dir)
            .provenance(
                &DiffTarget::BranchAgainstBase {
                    branch: "feature".to_owned(),
                    base_branch: "main".to_owned(),
                },
                &ProvenanceOptions::default().with_paths(requested.clone()),
                &Cancellation::default(),
            )
            .unwrap();

        assert_eq!(provenance.walked_commits, 1);
        assert_eq!(provenance.commits.len(), 1);
        assert_eq!(provenance.files.len(), requested.len());
        // The result zips with the request, which is what lets a front end pair
        // it with a file list without matching on paths.
        assert_eq!(
            provenance
                .files
                .iter()
                .map(|file| file.path.display().to_string())
                .collect::<Vec<_>>(),
            requested
        );
        assert_eq!(commits_for(&provenance, "file-000.txt"), vec![only]);
        let missing = file_for(&provenance, "never-existed.txt");
        assert!(!missing.is_attributed());
        assert_eq!(missing.gap, Some(ProvenanceGap::NotInRange));
    }

    /// Harkness's own commit convention names the model in a trailer, and the
    /// branch convention names the agent in a reference. Both are read as what
    /// they are, and neither is turned into a claim the repository did not make.
    #[test]
    fn harkness_conventions_are_read_without_being_guessed_at() {
        let fixture = Fixture::new();
        let root = fixture.directory("conventions");
        let repository = initialize_repository(&root);
        branch_from_head(&repository, "agent/extract-harkness-git");
        checkout(&repository, "agent/extract-harkness-git");
        commit(
            &repository,
            &root,
            &[("worked.txt", "one\n")],
            "Extract the Git crate\n\nBody text.\n\nCo-Authored-By: Some Model <model@example.invalid>\n",
            ("Ada", "ada@example.invalid"),
            1,
        );

        let provenance = GitService::new(&root, &fixture.data_dir)
            .provenance(
                &DiffTarget::BranchAgainstBase {
                    branch: "agent/extract-harkness-git".to_owned(),
                    base_branch: "main".to_owned(),
                },
                &ProvenanceOptions::default(),
                &Cancellation::default(),
            )
            .unwrap();

        assert_eq!(
            provenance
                .range
                .as_ref()
                .and_then(|range| range.agent_slug.clone()),
            Some("extract-harkness-git".to_owned())
        );
        assert_eq!(provenance.producers.len(), 2);
        assert_eq!(provenance.producers[0].kind, ProducerKind::Author);
        assert_eq!(provenance.producers[0].name, b"Ada");
        assert_eq!(provenance.producers[1].kind, ProducerKind::CoAuthor);
        assert_eq!(provenance.producers[1].name, b"Some Model");
        assert_eq!(provenance.producers[1].email, b"model@example.invalid");
        assert_eq!(file_for(&provenance, "worked.txt").producers, vec![0, 1]);
        // Nothing was requested, so the walk reports what it found in path order.
        assert_eq!(provenance.files.len(), 1);
    }

    /// A budget is reached by degrading to a named answer, never by failing the
    /// read and never by walking on regardless.
    #[test]
    fn the_commit_budget_degrades_to_a_named_truncation() {
        let fixture = Fixture::new();
        let root = fixture.directory("budget");
        let repository = initialize_repository(&root);
        branch_from_head(&repository, "feature");
        checkout(&repository, "feature");
        for index in 0..4 {
            commit(
                &repository,
                &root,
                &[(&format!("file-{index}.txt"), &format!("content {index}\n"))],
                &format!("commit {index}"),
                ("Ada", "ada@example.invalid"),
                index + 1,
            );
        }

        let provenance = GitService::new(&root, &fixture.data_dir)
            .provenance(
                &DiffTarget::BranchAgainstBase {
                    branch: "feature".to_owned(),
                    base_branch: "main".to_owned(),
                },
                &ProvenanceOptions::default()
                    .with_paths(["file-0.txt", "file-3.txt"])
                    .with_max_commits(2),
                &Cancellation::default(),
            )
            .unwrap();

        assert_eq!(provenance.walked_commits, 2);
        assert_eq!(
            provenance.truncation,
            Some(ProvenanceTruncation::CommitBudgetExhausted { limit: 2 })
        );
        assert!(file_for(&provenance, "file-3.txt").is_attributed());
        assert_eq!(
            file_for(&provenance, "file-0.txt").gap,
            Some(ProvenanceGap::CommitBudgetExhausted { limit: 2 })
        );
        assert_eq!(DEFAULT_MAX_PROVENANCE_COMMITS, 1_000);
    }

    /// A merge is passed over in a range and attributed in a single-commit
    /// target, because the two comparisons are different questions.
    #[test]
    fn a_merge_is_skipped_in_a_range_and_attributed_on_its_own() {
        let fixture = Fixture::new();
        let root = fixture.directory("merges");
        let repository = initialize_repository(&root);
        branch_from_head(&repository, "side");
        branch_from_head(&repository, "feature");
        checkout(&repository, "feature");
        let mainline = commit(
            &repository,
            &root,
            &[("mainline.txt", "one\n")],
            "mainline work",
            ("Ada", "ada@example.invalid"),
            1,
        );
        checkout(&repository, "side");
        let side = commit(
            &repository,
            &root,
            &[("side.txt", "one\n")],
            "side work",
            ("Grace", "grace@example.invalid"),
            2,
        );
        checkout(&repository, "feature");
        let merge = merge_commit(&repository, "merge side", 3, &[mainline, side]);

        let service = GitService::new(&root, &fixture.data_dir);
        let ranged = service
            .provenance(
                &DiffTarget::BranchAgainstBase {
                    branch: "feature".to_owned(),
                    base_branch: "main".to_owned(),
                },
                &ProvenanceOptions::default().with_paths(["mainline.txt", "side.txt"]),
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(ranged.skipped_merges, 1);
        assert_eq!(ranged.walked_commits, 3);
        assert_eq!(commits_for(&ranged, "mainline.txt"), vec![mainline]);
        assert_eq!(commits_for(&ranged, "side.txt"), vec![side]);
        assert!(ranged.commits.iter().all(|commit| commit.id != merge));

        // Asking about the merge itself is asking about the merge itself.
        let single = service
            .provenance(
                &DiffTarget::Commit {
                    revision: merge.to_string(),
                    parent: Some(mainline.to_string()),
                },
                &ProvenanceOptions::default(),
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(single.skipped_merges, 0);
        assert_eq!(single.commits.len(), 1);
        assert_eq!(single.commits[0].id, merge);
        assert_eq!(commits_for(&single, "side.txt"), vec![merge]);
    }

    /// Reading provenance is a read, so it must not wait behind the lock that
    /// mutations take, and must not need system Git to exist.
    #[test]
    fn provenance_reads_while_another_process_holds_the_repository_lock() {
        let fixture = Fixture::new();
        let root = fixture.directory("locked");
        let repository = initialize_repository(&root);
        branch_from_head(&repository, "feature");
        checkout(&repository, "feature");
        let only = commit(
            &repository,
            &root,
            &[("locked.txt", "one\n")],
            "add locked",
            ("Ada", "ada@example.invalid"),
            1,
        );

        let ready_file = fixture.root.path().join("provenance-lock-held");
        let mut holder = spawn_child(&fixture.data_dir, "hold-repository-lock")
            .env(PROCESS_PROJECT_ROOT_ENV, &root)
            .env(PROCESS_READY_FILE_ENV, &ready_file)
            .spawn()
            .unwrap();
        wait_for_child_signal(&mut holder, &ready_file);

        let provenance = GitService::new(&root, &fixture.data_dir)
            // A path no executable can be reached at: a spawned process would
            // fail rather than pass silently.
            .with_git_executable(root.join("must-not-run"))
            .provenance(
                &DiffTarget::BranchAgainstBase {
                    branch: "feature".to_owned(),
                    base_branch: "main".to_owned(),
                },
                &ProvenanceOptions::default().with_paths(["locked.txt"]),
                &Cancellation::default(),
            );

        holder.kill().unwrap();
        holder.wait().unwrap();
        assert_eq!(commits_for(&provenance.unwrap(), "locked.txt"), vec![only]);
    }

    #[test]
    fn invalid_revisions_and_cancellation_stay_typed() {
        let fixture = Fixture::new();
        let root = fixture.directory("typed-failures");
        let repository = initialize_repository(&root);
        let head = repository.head().unwrap().target().unwrap();
        let service = GitService::new(&root, &fixture.data_dir);
        let target = DiffTarget::BranchAgainstBase {
            branch: "feature".to_owned(),
            base_branch: "main".to_owned(),
        };

        assert!(matches!(
            service.provenance(&target, &ProvenanceOptions::default(), &Cancellation::default()),
            Err(GitError::RevisionNotFound { revision }) if revision == "feature"
        ));

        assert!(matches!(
            service.provenance(
                &DiffTarget::Commit {
                    revision: head.to_string(),
                    parent: Some(head.to_string()),
                },
                &ProvenanceOptions::default(),
                &Cancellation::default(),
            ),
            Err(GitError::RevisionNotParent { .. })
        ));

        let cancellation = Cancellation::default();
        cancellation.cancel();
        assert!(matches!(
            service.provenance(
                &DiffTarget::Commit {
                    revision: head.to_string(),
                    parent: None,
                },
                &ProvenanceOptions::default(),
                &cancellation,
            ),
            Err(GitError::Cancelled)
        ));
    }

    #[test]
    fn trailers_are_read_from_the_last_paragraph_only_and_stay_bytes() {
        assert_eq!(
            co_authors(b"subject\n\nCo-Authored-By: Ada <ada@example.invalid>\n"),
            vec![(b"Ada".to_vec(), b"ada@example.invalid".to_vec())]
        );
        // A trailer quoted in the body is not a contributor.
        assert_eq!(
            co_authors(b"subject\n\nCo-Authored-By: Quoted <q@example.invalid>\n\nbody\n"),
            Vec::new()
        );
        assert_eq!(
            co_authors(b"subject\n\nco-authored-by:  Name Only \n"),
            vec![(b"Name Only".to_vec(), Vec::new())]
        );
        assert_eq!(co_authors(b"subject\n\nCo-Authored-By:   \n"), Vec::new());
        assert_eq!(co_authors(b""), Vec::new());
        assert_eq!(
            co_authors(b"subject\n\nCo-Authored-By: \xff <b\xfe@example.invalid>\n"),
            vec![(b"\xff".to_vec(), b"b\xfe@example.invalid".to_vec())]
        );

        let flood = format!(
            "subject\n\n{}",
            "Co-Authored-By: A <a@example.invalid>\n".repeat(64)
        );
        assert_eq!(
            co_authors(flood.as_bytes()).len(),
            super::MAX_CO_AUTHORS_PER_COMMIT
        );
    }

    /// Narrowing the request narrows the whole record, not only its file list.
    /// A commit that touched nothing being asked about contributed nothing to
    /// the comparison in front of the reader, so it is neither reported nor
    /// counted among the people who produced the review.
    #[test]
    fn a_narrowed_request_carries_only_the_commits_its_paths_name() {
        let fixture = Fixture::new();
        let root = fixture.directory("narrowed");
        let repository = initialize_repository(&root);
        branch_from_head(&repository, "feature");
        checkout(&repository, "feature");
        commit(
            &repository,
            &root,
            &[("alpha.txt", "one\n")],
            "add alpha",
            ("Ada", "ada@example.invalid"),
            1,
        );
        let beta = commit(
            &repository,
            &root,
            &[("beta.txt", "one\n")],
            "add beta",
            ("Grace", "grace@example.invalid"),
            2,
        );
        commit(
            &repository,
            &root,
            &[("alpha.txt", "two\n")],
            "revise alpha",
            ("Ada", "ada@example.invalid"),
            3,
        );

        let provenance = GitService::new(&root, &fixture.data_dir)
            .provenance(
                &DiffTarget::BranchAgainstBase {
                    branch: "feature".to_owned(),
                    base_branch: "main".to_owned(),
                },
                &ProvenanceOptions::default().with_paths(["beta.txt"]),
                &Cancellation::default(),
            )
            .unwrap();

        // The walk still visited every commit; the record names only the one
        // that touched what was asked about.
        assert_eq!(provenance.walked_commits, 3);
        assert_eq!(commits_for(&provenance, "beta.txt"), vec![beta]);
        assert_eq!(provenance.commits.len(), 1);
        assert_eq!(provenance.producers.len(), 1);
        assert_eq!(provenance.producers[0].name, b"Grace");
        assert!(!provenance.is_empty());
    }

    /// Narrowing to no paths and asking about every path are opposite requests,
    /// and an empty list is the first rather than the second.
    #[test]
    fn narrowing_to_no_paths_walks_nothing_and_asking_for_all_walks_everything() {
        let fixture = Fixture::new();
        let root = fixture.directory("no-paths");
        let repository = initialize_repository(&root);
        branch_from_head(&repository, "feature");
        checkout(&repository, "feature");
        commit(
            &repository,
            &root,
            &[("alpha.txt", "one\n")],
            "add alpha",
            ("Ada", "ada@example.invalid"),
            1,
        );
        let service = GitService::new(&root, &fixture.data_dir);
        let target = DiffTarget::BranchAgainstBase {
            branch: "feature".to_owned(),
            base_branch: "main".to_owned(),
        };

        let none = service
            .provenance(
                &target,
                &ProvenanceOptions::default().with_paths(Vec::<PathBuf>::new()),
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(none.files, Vec::new());
        assert_eq!(none.commits, Vec::new());
        assert_eq!(none.producers, Vec::new());
        assert_eq!(none.walked_commits, 0);
        // The range is still reported: the caller asked about no paths, not
        // about no comparison.
        assert!(none.range.is_some());

        let all = service
            .provenance(
                &target,
                &ProvenanceOptions::default(),
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(all.walked_commits, 1);
        assert_eq!(all.files.len(), 1);
        assert_eq!(all.files[0].path, PathBuf::from("alpha.txt"));

        // One entry per element asked for, repeats included, so a caller
        // holding both lists may pair them by index. A repeat answers the same
        // as its first occurrence rather than reading as unattributed.
        let repeated = service
            .provenance(
                &target,
                &ProvenanceOptions::default().with_paths(["alpha.txt", "alpha.txt"]),
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(repeated.files.len(), 2);
        assert_eq!(repeated.files[0], repeated.files[1]);
        assert!(repeated.files[1].is_attributed());
    }

    /// A front end that pins a review to object ids keeps the reference the
    /// reviewer named, and the convention is read off that rather than off the
    /// hexadecimal the walk was actually given.
    #[test]
    fn a_named_head_reference_is_what_the_branch_convention_is_read_from() {
        let fixture = Fixture::new();
        let root = fixture.directory("pinned");
        let repository = initialize_repository(&root);
        branch_from_head(&repository, "agent/pinned");
        checkout(&repository, "agent/pinned");
        let base = repository
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        let head = commit(
            &repository,
            &root,
            &[("pinned.txt", "one\n")],
            "pinned work",
            ("Ada", "ada@example.invalid"),
            1,
        );

        let target = DiffTarget::Revisions {
            old_revision: base.to_string(),
            new_revision: head.to_string(),
        };
        let service = GitService::new(&root, &fixture.data_dir);

        let pinned = service
            .provenance(
                &target,
                &ProvenanceOptions::default(),
                &Cancellation::default(),
            )
            .unwrap();
        let range = pinned.range.expect("a two-revision target has a range");
        assert_eq!(range.head_revision, head.to_string());
        assert_eq!(range.head_reference, None);
        assert_eq!(range.agent_slug, None);

        let named = service
            .provenance(
                &target,
                &ProvenanceOptions::default().with_head_reference("agent/pinned"),
                &Cancellation::default(),
            )
            .unwrap();
        let range = named.range.expect("a two-revision target has a range");
        // The walk is unchanged; only what the convention had to read is.
        assert_eq!(range.head_revision, head.to_string());
        assert_eq!(range.head_reference.as_deref(), Some("agent/pinned"));
        assert_eq!(range.agent_slug.as_deref(), Some("pinned"));
        assert_eq!(named.commits, pinned.commits);
        assert_eq!(named.files, pinned.files);
    }

    #[test]
    fn the_agent_branch_convention_is_read_in_every_spelling() {
        assert_eq!(
            agent_slug("agent/catalog-v2"),
            Some("catalog-v2".to_owned())
        );
        assert_eq!(
            agent_slug("refs/heads/agent/catalog-v2"),
            Some("catalog-v2".to_owned())
        );
        assert_eq!(
            agent_slug("refs/remotes/origin/agent/catalog-v2"),
            Some("catalog-v2".to_owned())
        );
        assert_eq!(agent_slug("agent/"), None);
        assert_eq!(agent_slug("feature/catalog-v2"), None);
        assert_eq!(agent_slug("main"), None);
    }

    fn review(fixture: &Fixture, root: &Path) -> ChangeProvenance {
        let service = GitService::new(root, &fixture.data_dir);
        let target = DiffTarget::BranchAgainstBase {
            branch: "feature".to_owned(),
            base_branch: "main".to_owned(),
        };
        let files = service
            .diff(target.clone(), &DiffOptions::default())
            .unwrap();
        service
            .provenance(
                &target,
                &ProvenanceOptions::for_files(&files),
                &Cancellation::default(),
            )
            .unwrap()
    }

    fn file_for<'provenance>(
        provenance: &'provenance ChangeProvenance,
        path: &str,
    ) -> &'provenance super::FileProvenance {
        provenance
            .files
            .iter()
            .find(|file| file.path == Path::new(path))
            .unwrap_or_else(|| panic!("{path} was not reported"))
    }

    fn commits_for(provenance: &ChangeProvenance, path: &str) -> Vec<Oid> {
        file_for(provenance, path)
            .commits
            .iter()
            .map(|index| provenance.commits[*index].id)
            .collect()
    }

    fn branch_from_head(repository: &Repository, name: &str) {
        let head = repository.head().unwrap().peel_to_commit().unwrap();
        repository.branch(name, &head, true).unwrap();
    }

    fn checkout(repository: &Repository, branch: &str) {
        repository
            .set_head(&format!("refs/heads/{branch}"))
            .unwrap();
        repository
            .checkout_head(Some(CheckoutBuilder::new().force()))
            .unwrap();
    }

    fn commit(
        repository: &Repository,
        root: &Path,
        files: &[(&str, &str)],
        message: &str,
        author: (&str, &str),
        offset: i64,
    ) -> Oid {
        let mut index = repository.index().unwrap();
        for (path, content) in files {
            fs::write(root.join(path), content).unwrap();
            index.add_path(Path::new(path)).unwrap();
        }
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repository.find_tree(tree_id).unwrap();
        let parent = repository.head().unwrap().peel_to_commit().unwrap();
        let signature = Signature::new(
            author.0,
            author.1,
            &Time::new(COMMIT_EPOCH_SECONDS + offset, 0),
        )
        .unwrap();
        repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                message,
                &tree,
                &[&parent],
            )
            .unwrap()
    }

    fn merge_commit(repository: &Repository, message: &str, offset: i64, parents: &[Oid]) -> Oid {
        let parents = parents
            .iter()
            .map(|id| repository.find_commit(*id).unwrap())
            .collect::<Vec<_>>();
        // Take the second parent's tree so the merge genuinely introduces the
        // side branch's file relative to its first parent.
        let tree = parents[1].tree().unwrap();
        let signature = Signature::new(
            "Merger",
            "merger@example.invalid",
            &Time::new(COMMIT_EPOCH_SECONDS + offset, 0),
        )
        .unwrap();
        repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                message,
                &tree,
                &parents.iter().collect::<Vec<_>>(),
            )
            .unwrap()
    }
}
