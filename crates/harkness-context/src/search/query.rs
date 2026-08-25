//! What a caller may ask for, and the bounds it is asked within.
//!
//! Three things about the shape here are decisions rather than conveniences.
//!
//! **The universe is not a parameter.** A query names a pattern, a narrowing,
//! and a size — never a root, a walk policy, or an ignore rule. What may be
//! searched is decided once, by the inventory that produced the index rows, and
//! a request that could widen it would be caller-supplied input deciding how
//! much of a repository Harkness reads (ADR-0006).
//!
//! **The regular-expression capability is separate from the pattern.** A
//! [`SearchPattern::Regex`] can always be *expressed*; it is refused unless the
//! query also carries [`SearchQuery::permitting_regex`]. If the capability were
//! folded into the pattern's constructor there would be nothing left to refuse,
//! and the gate would exist only in prose. Deciding whether it was granted is
//! the policy engine's, which lives above this crate — what is here is the
//! seam that makes the decision have to be made.
//!
//! **Limits are clamped, never rejected.** A caller asking for ten thousand
//! results gets a thousand and a cursor; asking for zero gets the default. The
//! alternative is a namespace of `invalid_limit` refusals for values that have
//! an obvious, safe reading, and a query that fails on a number is a query a
//! front end has to validate twice.

use std::collections::BTreeSet;
use std::path::{Component, Path};

use crate::classify::FileClass;
use crate::digest::{DOMAIN_SEARCH_QUERY, DigestWriter, Sha256Hex};
use crate::path::RepoPath;

use super::cursor::SearchCursor;
use super::error::SearchError;

/// Matches a query returns when the caller names no bound.
pub const DEFAULT_MAX_RESULTS: usize = 200;

/// Most matches one response may carry, whatever the caller asks for.
///
/// A response is assembled in memory and handed to a model or a window, both of
/// which have budgets of their own. Past this a caller wants a cursor rather
/// than a bigger page.
pub const MAX_RESULTS_CAP: usize = 1_000;

/// Bytes of match text a response carries when the caller names no bound.
pub const DEFAULT_MAX_RESPONSE_BYTES: u64 = 256 * 1024;

/// Most bytes of match text one response may carry, whatever the caller asks.
///
/// Every bound in this crate is capped and not merely defaulted, for the reason
/// [`MAX_PATTERN_BYTES`] states: caller-supplied input may not decide how much
/// memory Harkness holds. Without this, a thousand matches each carrying eleven
/// megabyte-long lines is a response a caller can ask for in one call.
pub const MAX_RESPONSE_BYTES_CAP: u64 = 8 * 1024 * 1024;

/// Most context lines a match may carry on either side.
pub const MAX_CONTEXT_LINES: u32 = 5;

/// Bytes of one match line a response carries when the caller names no bound.
///
/// A minified bundle is one line of a megabyte, and a repository is entitled to
/// contain one. The bound is per line rather than per file so the other matches
/// in such a file still arrive.
pub const DEFAULT_MAX_LINE_BYTES: u64 = 8 * 1024;

/// Most bytes of one line a response may carry, whatever the caller asks.
///
/// A match carries up to `1 + 2 *` [`MAX_CONTEXT_LINES`] lines, so this and
/// [`MAX_RESPONSE_BYTES_CAP`] together are what bound a page: the byte budget
/// stops the page, and this stops the single match the byte budget is allowed
/// to let through when nothing else has fitted yet.
pub const MAX_LINE_BYTES_CAP: u64 = 64 * 1024;

/// Largest compiled program a pattern may produce.
///
/// The linear-time engine has no backtracking to blow up, but a pattern can
/// still ask for an automaton too large to hold. Refused at compile time with
/// [`SearchError::InvalidPattern`], before a file is opened.
pub const MAX_REGEX_SIZE_BYTES: usize = 10 * 1024 * 1024;

/// Longest pattern a caller may supply.
///
/// Caller text is bounded everywhere in this crate for the same reason: none of
/// it may decide how much work Harkness does or how long a message is.
pub const MAX_PATTERN_BYTES: usize = 4096;

/// Most path prefixes one query may narrow to.
///
/// Each is a separate ordered read of the index that the merge holds a page of,
/// so the count is paid in memory as well as in queries. A caller with more
/// subtrees than this wants their common ancestor.
pub const MAX_PATH_PREFIXES: usize = 64;

/// What to look for, and by which machinery.
///
/// The three shapes are not three spellings of one thing. Exact and regular
/// expression both scan file *content* and differ only in how a line is
/// matched; filename reads no content at all and answers from the index's own
/// path rows, which is why it is an order of magnitude faster and why its
/// matches carry [`RetrievalSource::FilenameSearch`].
///
/// [`RetrievalSource::FilenameSearch`]: crate::RetrievalSource::FilenameSearch
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SearchPattern {
    /// Literal text, matched byte for byte.
    ///
    /// Compiled through the same engine as [`Regex`](Self::Regex) with every
    /// metacharacter escaped, so `a.b` finds `a.b` and never `axb`. That is
    /// what makes an exact search safe to expose without a capability: nothing
    /// a caller writes is interpreted.
    Exact(String),
    /// A regular expression, run by the linear-time engine.
    ///
    /// Accepted only when the query carries
    /// [`permitting_regex`](SearchQuery::permitting_regex).
    Regex(String),
    /// A substring of a repository-relative path.
    ///
    /// Matched against the exact path bytes, so it finds a directory segment,
    /// a file stem, or an extension without three spellings of the query.
    ///
    /// It answers over the same universe as a content query, and that is a
    /// decision rather than an accident: an oversized file, a binary, a symlink
    /// and a repository boundary are all outside it, even though no content is
    /// read here and their *names* would be harmless to return. One universe
    /// for both shapes means a name a search offers is a file a search can then
    /// read — the alternative is a caller finding `db/schema.sql` by name and
    /// being told there is nothing in it.
    Filename(String),
}

impl SearchPattern {
    /// The stable spelling of this shape, for diagnostics and refusals.
    ///
    /// The *kind* is what a log line carries. The pattern itself never is: a
    /// query is what a person was looking for, and a diagnostic log outlives
    /// the session that produced it.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Exact(_) => "exact",
            Self::Regex(_) => "regex",
            Self::Filename(_) => "filename",
        }
    }

    /// The raw text, whichever shape this is.
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            Self::Exact(text) | Self::Regex(text) | Self::Filename(text) => text,
        }
    }

    /// Whether this shape reads file content.
    #[must_use]
    pub const fn reads_content(&self) -> bool {
        matches!(self, Self::Exact(_) | Self::Regex(_))
    }
}

/// Which of the eligible files a query is willing to look at.
///
/// Every filter narrows. There is no field here that can add a file to the
/// universe, because the universe is the index and the index holds only what
/// the inventory's four exclusion layers already allowed — a denied path, a
/// secret-classified file and an ignored one are not rows to be filtered out
/// but rows that were never written. Exclusion by construction is the whole
/// security story of this module, and it is why there is no post-filter here to
/// forget.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct SearchFilters {
    prefixes: Vec<RepoPath>,
    classes: BTreeSet<FileClass>,
}

impl SearchFilters {
    /// Filters nothing: every eligible file of the worktree.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Narrows to one directory and everything beneath it.
    ///
    /// Containment requires the separator, exactly as
    /// [`RepoPath::contains`] states it: `src` covers `src` and `src/main.rs`
    /// and never `src-generated.rs`. An empty prefix is the worktree root and
    /// is dropped rather than stored, since it narrows nothing.
    ///
    /// Narrowing is idempotent and absorbing: naming one directory twice, or
    /// naming a directory already covered by another, adds nothing. The
    /// normalization happens here rather than only at
    /// [`prefixes`](Self::prefixes), so the count that
    /// [`MAX_PATH_PREFIXES`] bounds is the number of *distinct subtrees* the
    /// merge will actually stream rather than the number of times a caller
    /// spelled one.
    ///
    /// # Errors
    ///
    /// [`SearchError::ForbiddenPath`] for an absolute path, a path leaving the
    /// worktree through `..`, or a path with a platform prefix. The filter
    /// never reaches the filesystem — it is compared against stored path bytes
    /// — so this is about what a caller may *express* rather than about what it
    /// could reach.
    ///
    /// [`SearchError::TooManyFilters`] past [`MAX_PATH_PREFIXES`] distinct
    /// subtrees. Deliberately not a `forbidden_path`: a size limit and a path
    /// outside the workspace lead a front end to say opposite things, and
    /// telling somebody their sixty-fifth perfectly ordinary `src/` filter left
    /// the worktree is the worse of the two.
    pub fn under(mut self, prefix: impl AsRef<Path>) -> Result<Self, SearchError> {
        let prefix = prefix.as_ref();
        let forbidden = |reason: &'static str| SearchError::ForbiddenPath {
            path: prefix.display().to_string(),
            reason,
        };
        // Built from the *validated components* rather than from the raw bytes,
        // which is the difference between accepting a spelling and understanding
        // it. `Path` normalizes nothing, so `src//`, `src/./x` and `src/` all
        // reach `from_path` verbatim — and a stored `src/` ranges over
        // `src//`..`src/0`, matching not one file in the subtree it names. `.`
        // is likewise a spelling of the worktree root and not an escape from it.
        let mut segments: Vec<&std::ffi::OsStr> = Vec::new();
        for component in prefix.components() {
            match component {
                Component::Normal(segment) => segments.push(segment),
                // `.` contributes nothing, exactly as it does to a filesystem.
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(forbidden("'..' would leave the workspace"));
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(forbidden("an absolute path is not repository-relative"));
                }
            }
        }
        // Joined with `/` whatever the platform's separator is, because that is
        // the byte the index stores paths under — so a filter written
        // `src\inner` on Windows narrows to the rows it plainly means.
        let mut bytes: Vec<u8> = Vec::new();
        for segment in segments {
            if !bytes.is_empty() {
                bytes.push(b'/');
            }
            bytes.extend_from_slice(RepoPath::from_path(Path::new(segment)).as_bytes());
        }
        let path = RepoPath::from_bytes(bytes);
        if path.is_empty() {
            return Ok(self);
        }
        self.prefixes.push(path);
        self.prefixes = normalize(&self.prefixes);
        if self.prefixes.len() > MAX_PATH_PREFIXES {
            return Err(SearchError::TooManyFilters {
                limit: MAX_PATH_PREFIXES,
            });
        }
        Ok(self)
    }

    /// Narrows to files of `class`.
    ///
    /// Naming any class at all excludes every other, so a query asking for
    /// [`FileClass::Source`] does not also read documentation. Naming none is
    /// the default and means every class the index holds content for.
    #[must_use]
    pub fn in_class(mut self, class: FileClass) -> Self {
        self.classes.insert(class);
        self
    }

    /// The subtrees this query is narrowed to, sorted and made disjoint.
    ///
    /// Sorted so a merge over them yields one global path order, and disjoint
    /// so no file can be visited twice: two overlapping prefixes would stream
    /// one file down two paths, and the merge would then emit its matches
    /// twice — two matches sharing a `(path, byte_offset)` position, which is
    /// exactly the total order a cursor has to be able to sit inside.
    ///
    /// A parent always sorts before its own children, but it is **not**
    /// necessarily adjacent to them: `src-gen` falls between `src` and
    /// `src/inner`, because `-` is `0x2d` and the separator is `0x2f`. Every
    /// kept prefix is therefore consulted rather than only the last one — the
    /// list is bounded by [`MAX_PATH_PREFIXES`], so the scan costs nothing
    /// worth optimizing away.
    #[must_use]
    pub fn prefixes(&self) -> Vec<RepoPath> {
        debug_assert_eq!(
            self.prefixes,
            normalize(&self.prefixes),
            "`under` is the only way in and it leaves the list normalized"
        );
        self.prefixes.clone()
    }

    /// The classes this query is narrowed to; empty means every class.
    #[must_use]
    pub fn classes(&self) -> &BTreeSet<FileClass> {
        &self.classes
    }

    /// Whether `class` passes this filter.
    #[must_use]
    pub fn admits(&self, class: FileClass) -> bool {
        self.classes.is_empty() || self.classes.contains(&class)
    }
}

/// Sorts `prefixes` and drops every one another already covers.
///
/// A parent sorts before its own children, so one pass suffices — but it is
/// *not* adjacent to them, which is the whole reason every kept prefix is
/// consulted rather than only the most recent: `src-gen` sorts between `src`
/// and `src/inner`, because `-` is `0x2d` and the separator is `0x2f`.
fn normalize(prefixes: &[RepoPath]) -> Vec<RepoPath> {
    let mut sorted = prefixes.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut disjoint: Vec<RepoPath> = Vec::with_capacity(sorted.len());
    for candidate in sorted {
        if disjoint.iter().any(|kept| kept.contains(&candidate)) {
            continue;
        }
        disjoint.push(candidate);
    }
    disjoint
}

/// How much one response may hold.
///
/// Every value is clamped on the way in, so the accessors are the effective
/// numbers rather than the requested ones and nothing downstream has to clamp
/// again.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SearchLimits {
    max_results: usize,
    max_bytes: u64,
    context_lines: u32,
    max_line_bytes: u64,
}

impl Default for SearchLimits {
    fn default() -> Self {
        Self {
            max_results: DEFAULT_MAX_RESULTS,
            max_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            context_lines: 0,
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
        }
    }
}

impl SearchLimits {
    /// The published defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets how many matches a response may carry, clamped to
    /// [`MAX_RESULTS_CAP`]. Zero is read as the default.
    #[must_use]
    pub const fn with_max_results(mut self, results: usize) -> Self {
        self.max_results = if results == 0 {
            DEFAULT_MAX_RESULTS
        } else if results > MAX_RESULTS_CAP {
            MAX_RESULTS_CAP
        } else {
            results
        };
        self
    }

    /// Sets how many bytes of match text a response may carry, clamped to
    /// [`MAX_RESPONSE_BYTES_CAP`]. Zero is read as the default.
    #[must_use]
    pub const fn with_max_bytes(mut self, bytes: u64) -> Self {
        self.max_bytes = if bytes == 0 {
            DEFAULT_MAX_RESPONSE_BYTES
        } else if bytes > MAX_RESPONSE_BYTES_CAP {
            MAX_RESPONSE_BYTES_CAP
        } else {
            bytes
        };
        self
    }

    /// Sets how many lines of context each match carries on either side,
    /// clamped to [`MAX_CONTEXT_LINES`].
    #[must_use]
    pub const fn with_context_lines(mut self, lines: u32) -> Self {
        self.context_lines = if lines > MAX_CONTEXT_LINES {
            MAX_CONTEXT_LINES
        } else {
            lines
        };
        self
    }

    /// Sets how many bytes of one line a match may carry, clamped to
    /// [`MAX_LINE_BYTES_CAP`]. Zero is read as the default.
    #[must_use]
    pub const fn with_max_line_bytes(mut self, bytes: u64) -> Self {
        self.max_line_bytes = if bytes == 0 {
            DEFAULT_MAX_LINE_BYTES
        } else if bytes > MAX_LINE_BYTES_CAP {
            MAX_LINE_BYTES_CAP
        } else {
            bytes
        };
        self
    }

    /// Matches this response may carry.
    #[must_use]
    pub const fn max_results(&self) -> usize {
        self.max_results
    }

    /// Bytes of match text this response may carry.
    #[must_use]
    pub const fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Context lines each match carries on either side.
    #[must_use]
    pub const fn context_lines(&self) -> u32 {
        self.context_lines
    }

    /// Bytes of one line a match may carry.
    #[must_use]
    pub const fn max_line_bytes(&self) -> u64 {
        self.max_line_bytes
    }
}

/// One search, whole.
///
/// Built by naming a pattern and narrowing from there. A query is a value with
/// no interior state: running the same one twice against an unchanged worktree
/// produces the same matches in the same order, which is what makes retrieval
/// quality something a test can measure ([#137]).
///
/// ```
/// use harkness_context::{SearchLimits, SearchQuery};
///
/// let query = SearchQuery::exact("fn main")
///     .with_limits(SearchLimits::new().with_context_lines(2));
/// assert_eq!(query.pattern().kind(), "exact");
/// assert_eq!(query.limits().context_lines(), 2);
/// assert!(!query.regex_permitted());
/// ```
///
/// [#137]: https://github.com/fullstacktaiye/harkness/issues/137
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct SearchQuery {
    pattern: SearchPattern,
    filters: SearchFilters,
    limits: SearchLimits,
    cursor: Option<SearchCursor>,
    regex_permitted: bool,
}

impl SearchQuery {
    /// Searches file content for literal `text`.
    #[must_use]
    pub fn exact(text: impl Into<String>) -> Self {
        Self::with_pattern(SearchPattern::Exact(text.into()))
    }

    /// Searches file content with the regular expression `pattern`.
    ///
    /// Refused with [`SearchError::RegexNotPermitted`] unless the query also
    /// carries [`permitting_regex`](Self::permitting_regex).
    #[must_use]
    pub fn regex(pattern: impl Into<String>) -> Self {
        Self::with_pattern(SearchPattern::Regex(pattern.into()))
    }

    /// Searches repository-relative paths for the substring `text`.
    #[must_use]
    pub fn filename(text: impl Into<String>) -> Self {
        Self::with_pattern(SearchPattern::Filename(text.into()))
    }

    fn with_pattern(pattern: SearchPattern) -> Self {
        Self {
            pattern,
            filters: SearchFilters::new(),
            limits: SearchLimits::new(),
            cursor: None,
            regex_permitted: false,
        }
    }

    /// Records that the caller was granted the regular-expression capability.
    ///
    /// The engine takes this as the answer rather than as the question: it
    /// cannot see the policy engine, which sits above this crate. [#123] is
    /// what calls this, and only after [#91]'s policy said so.
    ///
    /// [#91]: https://github.com/fullstacktaiye/harkness/issues/91
    /// [#123]: https://github.com/fullstacktaiye/harkness/issues/123
    #[must_use]
    pub const fn permitting_regex(mut self) -> Self {
        self.regex_permitted = true;
        self
    }

    /// Narrows which files the query looks at.
    #[must_use]
    pub fn with_filters(mut self, filters: SearchFilters) -> Self {
        self.filters = filters;
        self
    }

    /// Bounds what the response may hold.
    #[must_use]
    pub const fn with_limits(mut self, limits: SearchLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Continues a previous response from its cursor.
    #[must_use]
    pub fn continuing(mut self, cursor: SearchCursor) -> Self {
        self.cursor = Some(cursor);
        self
    }

    /// What the query looks for.
    #[must_use]
    pub const fn pattern(&self) -> &SearchPattern {
        &self.pattern
    }

    /// Which files it is willing to look at.
    #[must_use]
    pub const fn filters(&self) -> &SearchFilters {
        &self.filters
    }

    /// How much the response may hold.
    #[must_use]
    pub const fn limits(&self) -> &SearchLimits {
        &self.limits
    }

    /// The continuation this query resumes from, when it resumes one.
    #[must_use]
    pub const fn cursor(&self) -> Option<&SearchCursor> {
        self.cursor.as_ref()
    }

    /// Whether the regular-expression capability was recorded.
    #[must_use]
    pub const fn regex_permitted(&self) -> bool {
        self.regex_permitted
    }

    /// The identity a cursor is bound to.
    ///
    /// Everything that decides *which* matches exist and *in what order*, and
    /// nothing that decides how much arrives at a time. The pattern and the
    /// filters decide the match set; the capability flag is absorbed with them,
    /// since a pattern that was refused and one that ran are not the same
    /// query.
    ///
    /// The four bounds are all deliberately outside it. Page size obviously is
    /// — a caller asking for a smaller second page is paging. So are
    /// `context_lines` and `max_line_bytes`, which is less obvious and is the
    /// same rule: they decide how much *text* each match carries and not which
    /// matches there are, so a surface that lets somebody expand the context
    /// around a result and keep paging is asking one question, not two.
    #[must_use]
    pub(crate) fn identity(&self) -> Sha256Hex {
        let mut writer = DigestWriter::new(DOMAIN_SEARCH_QUERY);
        writer.field(self.pattern.kind().as_bytes());
        writer.field(self.pattern.text().as_bytes());
        writer.field(if self.regex_permitted {
            b"granted"
        } else {
            b"withheld"
        });
        let prefixes = self.filters.prefixes();
        writer.integer(prefixes.len() as u64);
        for prefix in &prefixes {
            writer.field(prefix.as_bytes());
        }
        writer.integer(self.filters.classes.len() as u64);
        for class in &self.filters.classes {
            writer.field(class.as_str().as_bytes());
        }
        writer.finish()
    }
}
