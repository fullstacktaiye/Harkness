//! What a search refuses, and why each refusal is its own discriminant.
//!
//! The table is deliberately small. A discriminant exists when a caller does
//! something *different* because of it — fix the pattern, ask policy for the
//! regular-expression capability, name a path inside the workspace, start the
//! query again, build the index, stop. Three unrelated things can be wrong with
//! a continuation cursor and all three lead to the same repair, so they share
//! [`SearchError::StaleCursor`] and are told apart by the [`CursorRefusal`] it
//! carries rather than by three kinds a caller would have to handle
//! identically.
//!
//! [`ContextEngineError`](crate::ContextEngineError) carries these whole, in the
//! same way it carries an inventory walk's failures: a caller that needs to tell
//! a malformed pattern from an unindexed worktree needs the discriminant the
//! search gave it. The one exception is `cancelled`, which the facade spells
//! once for every layer that can observe a token.

use thiserror::Error;

/// Why a continuation cursor could not be used.
///
/// Carried by [`SearchError::StaleCursor`] rather than published as three
/// kinds: every one of them means "this token cannot continue this query, run
/// it again from the start", and a namespace that spells one response three
/// ways makes a caller write the same arm three times.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CursorRefusal {
    /// The token is not a Harkness search cursor, or is of a version this
    /// build does not read.
    Malformed,
    /// The token was minted against an index generation that no longer exists.
    ///
    /// The rows it names may have been rebuilt from a different walk, so
    /// resuming would mix two generations into one result set — the thing an
    /// opaque cursor exists to make impossible.
    GenerationChanged,
    /// The token was minted for a different query.
    ///
    /// A cursor names a position in *one* query's total order. Continuing a
    /// different query from it would skip and repeat matches with nothing
    /// saying so, which is exactly the promise paging makes.
    DifferentQuery,
}

impl CursorRefusal {
    /// Every refusal in its stable declaration order.
    pub const ALL: &'static [Self] = &[
        Self::Malformed,
        Self::GenerationChanged,
        Self::DifferentQuery,
    ];

    /// The stable spelling this refusal reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::GenerationChanged => "generation_changed",
            Self::DifferentQuery => "different_query",
        }
    }
}

impl std::fmt::Display for CursorRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Failures raised while validating or running a search.
///
/// Every variant maps to a stable discriminant in [`SearchError::KINDS`], and
/// no kind here may collide with another published namespace —
/// [`ContextEngineError::kinds`](crate::ContextEngineError::kinds) publishes
/// their concatenation.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum SearchError {
    /// The pattern is empty, malformed, or compiles past its size limit.
    ///
    /// Raised before any file is opened, so a bad pattern costs no I/O. The
    /// `pattern_kind` is the *shape* that was refused and never the pattern
    /// itself, for the reason the module's diagnostics are written that way:
    /// what a person searched for is theirs.
    #[error("the {pattern_kind} pattern cannot be used: {reason}")]
    InvalidPattern {
        /// Stable spelling of the pattern shape that was refused.
        pattern_kind: &'static str,
        /// Stable human-readable explanation.
        reason: String,
    },

    /// A regular-expression query arrived without the capability permitting one.
    ///
    /// A regular expression is a small program a caller supplies, and the
    /// engine runs it over the repository. That is a policy decision rather
    /// than an engine one, so the engine refuses unless the caller has said in
    /// as many words that it was granted — see
    /// [`SearchQuery::permitting_regex`](crate::SearchQuery::permitting_regex).
    #[error(
        "a regular-expression search needs the regex capability, which this query does not carry"
    )]
    RegexNotPermitted,

    /// A path filter names something outside the worktree.
    ///
    /// Filters narrow the index rows a search reads and can never widen them,
    /// but a filter that escaped the worktree would still be a caller
    /// describing a location Harkness does not serve, and answering it at all
    /// would teach a caller that such a path is expressible.
    #[error("the path filter '{path}' is outside the workspace: {reason}")]
    ForbiddenPath {
        /// The rejected filter in its lossy display form.
        path: String,
        /// Stable human-readable explanation.
        reason: &'static str,
    },

    /// A query names more distinct subtrees than the merge will stream.
    ///
    /// Its own discriminant rather than a [`ForbiddenPath`](Self::ForbiddenPath)
    /// with a size-shaped message, because the two lead a front end to say
    /// opposite things: one means "that path is not somewhere Harkness will
    /// look", and this means "every one of those paths is fine, name fewer of
    /// them or name what contains them". The count is of subtrees after
    /// normalization, so naming one directory sixty-five times never reaches
    /// this.
    #[error("a query may narrow to at most {limit} distinct subtrees")]
    TooManyFilters {
        /// The limit that fired.
        limit: usize,
    },

    /// A continuation cursor could not be used.
    #[error("the search cursor cannot continue this query: {refusal}")]
    StaleCursor {
        /// Which of the three things was wrong with it.
        refusal: CursorRefusal,
    },

    /// The worktree has no index to search.
    ///
    /// Search reads the index and never walks the filesystem, so an unindexed
    /// worktree has no universe rather than an empty one. Answering "no match"
    /// would be indistinguishable from a repository that does not contain the
    /// pattern, which is the honesty rule the whole cache is written under.
    /// The repair is a build — `reindex` or a full `reconcile`.
    #[error("the worktree '{worktree}' has no index to search: {reason}")]
    IndexUnavailable {
        /// The worktree key the search was addressed to.
        worktree: String,
        /// Stable human-readable explanation.
        reason: &'static str,
    },

    /// The operation observed its cancellation token.
    ///
    /// No partial response: a caller that stopped a search did not ask for the
    /// prefix of one, and a partial answer under the same shape as a complete
    /// one is how a bounded result set becomes a wrong one.
    #[error("the search was cancelled")]
    Cancelled,
}

impl SearchError {
    /// Every stable discriminant this namespace defines.
    pub const KINDS: &'static [&'static str] = &[
        "invalid_pattern",
        "regex_not_permitted",
        "forbidden_path",
        "too_many_filters",
        "stale_search_cursor",
        "index_unavailable",
        "cancelled",
    ];

    /// Stable machine-readable discriminant for caller-facing error handling.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::InvalidPattern { .. } => "invalid_pattern",
            Self::RegexNotPermitted => "regex_not_permitted",
            Self::ForbiddenPath { .. } => "forbidden_path",
            Self::TooManyFilters { .. } => "too_many_filters",
            Self::StaleCursor { .. } => "stale_search_cursor",
            Self::IndexUnavailable { .. } => "index_unavailable",
            Self::Cancelled => "cancelled",
        }
    }
}
