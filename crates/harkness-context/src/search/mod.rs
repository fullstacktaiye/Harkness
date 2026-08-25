//! Deterministic filename and lexical search over the eligible-file inventory.
//!
//! This is the retrieval layer ADR-0005 puts first: find every occurrence of an
//! identifier, a string, or a path, in an order that does not move, with
//! everything that was left out said out loud. No embeddings, no scoring, no
//! subprocess — a query is answered the same way today, tomorrow, and on
//! somebody else's machine, which is what makes retrieval quality something a
//! test can measure rather than something a demonstration can suggest.
//!
//! ```no_run
//! use harkness_context::{ContextEngine, ContextEngineConfig, SearchQuery};
//! use harkness_core::ProjectId;
//! use harkness_git::Cancellation;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let (root, data_dir) = (std::path::Path::new("/repo"), std::path::Path::new("/data"));
//! let cancellation = Cancellation::default();
//! let engine = ContextEngine::open(
//!     ContextEngineConfig::new(ProjectId::new(), root, data_dir),
//!     &cancellation,
//! )?;
//! engine.reindex(&cancellation)?;
//!
//! // One capture for the whole paging session, not one per page: a capture
//! // reads the whole workspace, and five pages stamped with five captures
//! // would claim five workspace states for one moment.
//! let snapshot = engine.snapshot(&cancellation)?;
//! let mut query = SearchQuery::exact("TODO");
//! loop {
//!     let page = engine.search_under(&snapshot, &query, &cancellation)?;
//!     for found in &page.matches {
//!         println!("{}:{:?}", found.path.display(), found.line_number);
//!     }
//!     let Some(cursor) = page.next_cursor else { break };
//!     query = query.continuing(cursor);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Five rules, and what each one prevents
//!
//! **The universe is the index and never a walk.** Every file a scan opens came
//! out of a `files` row, and every row came out of an inventory whose four
//! exclusion layers had already refused a denied path, a secret-classified
//! file, a binary and an ignored one. Exclusion is therefore by *construction*:
//! there is no post-filter here that a later change could forget to apply, and
//! a `.env` is not a row waiting to be filtered but a row that was never
//! written. The corollary is that a worktree the cache has never seen is
//! [`SearchError::IndexUnavailable`] and not an empty result — "no match" and
//! "I did not look" are different answers and a caller acts differently on
//! each.
//!
//! **Ordering is a total order over positions.** Path bytes ascending, then
//! absolute byte offset ascending, and nothing else. Both come out of the scan
//! already ordered, so there is no sort whose stability could decide anything.
//! A content match is reported once per matching line, positioned at the first
//! occurrence on it, which is what makes the pair unique — two matches sharing
//! a position would be two matches no cursor could sit between.
//!
//! **A cursor is a position, not an offset.** "The first match after this one"
//! is well defined however the surrounding results moved; "skip the first N"
//! is not. [`SearchCursor`] is opaque, versioned, bound to the index generation
//! it was minted against and to the query it belongs to, and refuses rather
//! than silently continuing something else.
//!
//! **Truncation is part of the answer.** Every bound that fires puts a
//! [`SearchOmission`] in the *success* payload. A result list stopped by a
//! budget and a repository holding exactly that many matches are otherwise one
//! value, and reading the first as the second is how a bounded search quietly
//! becomes a wrong one. The bound is checked against the offered match rather
//! than the stored one, so a page that fills exactly is a complete answer and
//! not a truncated-looking one. Every bound is also *capped* and not merely
//! defaulted — [`MAX_RESULTS_CAP`], [`MAX_RESPONSE_BYTES_CAP`],
//! [`MAX_CONTEXT_LINES`], [`MAX_LINE_BYTES_CAP`] — because caller-supplied
//! numbers may not decide how much memory one response holds.
//!
//! **A regular expression is a capability.** [`SearchPattern::Exact`] escapes
//! every metacharacter, so nothing a caller writes is interpreted and no
//! capability is needed. [`SearchPattern::Regex`] runs a small program a caller
//! supplied over the repository, which is a policy decision — so the engine
//! refuses unless the query carries [`SearchQuery::permitting_regex`], and
//! deciding whether to add it belongs to the policy engine above this crate
//! ([#123]).
//!
//! # Why there is no subprocess
//!
//! The ripgrep libraries run in process. Shelling out to `rg` would put a
//! caller-supplied pattern on an argv, and the defence against that is a
//! quoting rule somebody has to keep getting right. There is no argv here, so
//! there is nothing to escape from — the same reasoning `harkness-git`'s
//! runner applies to Git, arrived at by removing the process rather than by
//! hardening it.
//!
//! # What is not here
//!
//! Ranking and scoring are [#121]: matches come back in canonical order and the
//! engine expresses no opinion about which is better. Symbol-aware lookup is
//! [#117], the repository map is [#118], and the tool and policy wiring that
//! exposes any of it to a model is [#123]. A **language filter** is deliberately
//! absent rather than accepted-and-ignored: nothing in this build populates a
//! language, since that vocabulary belongs to [#117]'s parser adapters, and a
//! filter over a column no row carries would answer "no match" for a repository
//! full of matches.
//!
//! `docs/context-search.md` is the reference.
//!
//! [#117]: https://github.com/fullstacktaiye/harkness/issues/117
//! [#118]: https://github.com/fullstacktaiye/harkness/issues/118
//! [#121]: https://github.com/fullstacktaiye/harkness/issues/121
//! [#123]: https://github.com/fullstacktaiye/harkness/issues/123

mod cursor;
mod error;
mod query;
mod result;
mod scan;

pub use cursor::SearchCursor;
pub use error::{CursorRefusal, SearchError};
pub use query::{
    DEFAULT_MAX_LINE_BYTES, DEFAULT_MAX_RESPONSE_BYTES, DEFAULT_MAX_RESULTS, MAX_CONTEXT_LINES,
    MAX_LINE_BYTES_CAP, MAX_PATH_PREFIXES, MAX_PATTERN_BYTES, MAX_REGEX_SIZE_BYTES,
    MAX_RESPONSE_BYTES_CAP, MAX_RESULTS_CAP, SearchFilters, SearchLimits, SearchPattern,
    SearchQuery,
};
pub use result::{
    BoundedText, MAX_SEARCH_OMISSIONS, SearchMatch, SearchOmission, SearchResponse, SearchStats,
    TextEncoding,
};

pub(crate) use scan::Scan;

#[cfg(test)]
mod tests;
