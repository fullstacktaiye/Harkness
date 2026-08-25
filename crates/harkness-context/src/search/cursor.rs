//! The opaque continuation a paged search resumes from.
//!
//! A cursor is a *position in a total order*, not an offset into a result list.
//! That distinction is the whole reason paging here cannot duplicate or skip a
//! match: an offset means "skip the first N matches", which is a different set
//! of matches every time the repository moves, while a position means "the
//! first match strictly after this one", which is well defined however the
//! surrounding results changed. `harkness-git`'s [`LogCursor`] is anchored for
//! the same reason and this is deliberately the same shape.
//!
//! It is opaque because what it holds is an implementation detail that must
//! stay changeable: a token round-trips through a front end, a tool result, and
//! possibly a model's context, and none of them may come to depend on its
//! fields. The wire form is versioned so a future representation refuses an old
//! token by name rather than misreading it.
//!
//! Three things are bound into it and every one of them refuses a continuation
//! rather than silently answering the wrong question — the index generation, so
//! a rebuilt cache cannot have two generations' rows in one result set; the
//! query identity, so a token cannot be replayed against a different pattern;
//! and the version, so a token this build does not understand is not guessed at.
//!
//! [`LogCursor`]: harkness_git::LogCursor

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as URL_BASE64;
use serde::{Deserialize, Serialize};

use crate::digest::Sha256Hex;
use crate::path::RepoPath;

use super::error::{CursorRefusal, SearchError};

/// Wire version of the token this build mints.
const CURSOR_VERSION: u8 = 1;

/// Where one page of a search ended.
///
/// Compare with [`SearchCursor::token`] and rebuild with
/// [`SearchCursor::parse`]; the fields are deliberately not public.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchCursor {
    generation: u64,
    query: Sha256Hex,
    path: RepoPath,
    byte_offset: u64,
}

/// The versioned document a token base64-encodes.
///
/// The path travels base64-encoded *inside* the document because a
/// repository-relative path is a byte string and need not be UTF-8 — spelling
/// it through a lossy conversion would produce a token that resumes at a
/// different file than the one the page ended on.
#[derive(Deserialize, Serialize)]
struct SearchCursorWire {
    v: u8,
    generation: u64,
    query: String,
    path: String,
    byte_offset: u64,
}

impl SearchCursor {
    /// Records the position one page ended at.
    pub(crate) fn new(generation: u64, query: Sha256Hex, path: RepoPath, byte_offset: u64) -> Self {
        Self {
            generation,
            query,
            path,
            byte_offset,
        }
    }

    /// The opaque token a caller carries between pages.
    #[must_use]
    pub fn token(&self) -> String {
        let wire = SearchCursorWire {
            v: CURSOR_VERSION,
            generation: self.generation,
            query: self.query.as_str().to_owned(),
            path: URL_BASE64.encode(self.path.as_bytes()),
            byte_offset: self.byte_offset,
        };
        // Infallible in practice: every field is a plain scalar or a `String`,
        // and `serde_json` fails only on a map key that is not a string or on
        // a float that is not a number. A refusal here would be a bug in this
        // module rather than something a caller can cause, so it is spelled as
        // an empty token that `parse` then refuses as malformed rather than as
        // a `Result` on every call site.
        serde_json::to_vec(&wire).map_or_else(|_| String::new(), |bytes| URL_BASE64.encode(bytes))
    }

    /// Reads a token back, refusing anything this build did not mint.
    ///
    /// # Errors
    ///
    /// [`SearchError::StaleCursor`] carrying [`CursorRefusal::Malformed`] for a
    /// token that is not base64, is not the document this build writes, or
    /// carries a version it does not read.
    pub fn parse(token: &str) -> Result<Self, SearchError> {
        let malformed = || SearchError::StaleCursor {
            refusal: CursorRefusal::Malformed,
        };
        let bytes = URL_BASE64.decode(token).map_err(|_| malformed())?;
        let wire: SearchCursorWire = serde_json::from_slice(&bytes).map_err(|_| malformed())?;
        if wire.v != CURSOR_VERSION {
            return Err(malformed());
        }
        let query: Sha256Hex = wire.query.parse().map_err(|_| malformed())?;
        let path = URL_BASE64.decode(&wire.path).map_err(|_| malformed())?;
        Ok(Self {
            generation: wire.generation,
            query,
            path: RepoPath::from_bytes(path),
            byte_offset: wire.byte_offset,
        })
    }

    /// The index generation this cursor may continue against.
    #[must_use]
    pub const fn index_generation(&self) -> u64 {
        self.generation
    }

    /// Refuses a cursor that cannot continue `query` against `generation`.
    ///
    /// # Errors
    ///
    /// [`SearchError::StaleCursor`] carrying
    /// [`CursorRefusal::GenerationChanged`] or
    /// [`CursorRefusal::DifferentQuery`].
    pub(crate) fn admits(&self, generation: u64, query: &Sha256Hex) -> Result<(), SearchError> {
        if self.generation != generation {
            return Err(SearchError::StaleCursor {
                refusal: CursorRefusal::GenerationChanged,
            });
        }
        if &self.query != query {
            return Err(SearchError::StaleCursor {
                refusal: CursorRefusal::DifferentQuery,
            });
        }
        Ok(())
    }

    /// The file the previous page ended in.
    ///
    /// Rows strictly after it are the continuation; that file's *own* row is
    /// fetched by name, because the page may have stopped in the middle of it
    /// and the rest of it is the next page's first matches.
    pub(crate) const fn path(&self) -> &RepoPath {
        &self.path
    }

    /// Whether a match at `(path, offset)` was already returned.
    pub(crate) fn precedes(&self, path: &RepoPath, offset: u64) -> bool {
        (path, offset) <= (&self.path, self.byte_offset)
    }
}
