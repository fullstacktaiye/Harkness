//! What a search answers with, including everything it did not answer.
//!
//! The shape follows `harkness-git`'s diff rather than a conventional search
//! API, and for the same reason: **truncation is part of the answer**. A result
//! list that stopped at a budget and a repository that holds exactly that many
//! matches are otherwise one value, and the first reads as the second. Every
//! bound that fires therefore puts a [`SearchOmission`] in the success payload —
//! not in a log, not in a warning a caller may ignore — so "no more matches" is
//! a statement the response can actually make.
//!
//! The second rule is that **text is bounded and self-describing**. Repository
//! content decides neither how many bytes a response carries nor whether it is
//! valid UTF-8, so every excerpt is a [`BoundedText`]: clamped to a published
//! limit, marked when it was clamped, and carrying the encoding needed to get
//! the exact bytes back.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as STANDARD_BASE64;

use crate::digest::Sha256Hex;
use crate::ids::{ContextQueryId, SnapshotId};
use crate::path::RepoPath;
use crate::provenance::Provenance;
use crate::text::floor_char_boundary;

use super::cursor::SearchCursor;

/// Most omissions one response carries before it starts counting them instead.
///
/// A stale index over a rewritten tree can produce one omission per file, and a
/// response is assembled in memory. Past this the entries stop and
/// [`SearchResponse::dropped_omissions`] counts what did not fit, which is the
/// same bargain [`FileInventory`] makes with its diagnostics.
///
/// [`FileInventory`]: crate::FileInventory
pub const MAX_SEARCH_OMISSIONS: usize = 256;

/// How a piece of repository text is spelled in a response.
///
/// Two values rather than the three [`ContentEncoding`] has, because this is
/// about *transport* and that one is about how a file's bytes are interpreted.
/// The convention is the one the diff payload already publishes: valid UTF-8
/// travels as itself, anything else as Base64, and a consumer reconstructs the
/// exact bytes either way.
///
/// [`ContentEncoding`]: crate::ContentEncoding
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum TextEncoding {
    /// The bytes are valid UTF-8 and travel as themselves.
    #[default]
    Utf8,
    /// The bytes are arbitrary and travel Base64-encoded.
    Base64,
}

impl TextEncoding {
    /// Every encoding in its stable declaration order.
    pub const ALL: &'static [Self] = &[Self::Utf8, Self::Base64];

    /// The stable spelling a payload carries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Utf8 => "utf8",
            Self::Base64 => "base64",
        }
    }
}

impl std::fmt::Display for TextEncoding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One excerpt of repository text, clamped and self-describing.
///
/// `Debug` is derived, unlike [`FileVersion`]'s: what is here is already
/// bounded by a published limit, so a `{:?}` in a panic message costs kilobytes
/// rather than a repository.
///
/// [`FileVersion`]: crate::FileVersion
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BoundedText {
    text: String,
    encoding: TextEncoding,
    truncated: bool,
    source_bytes: u64,
}

impl BoundedText {
    /// Clamps `bytes` to `limit` and spells it in whichever encoding is exact.
    ///
    /// The encoding is decided by the *whole* source rather than by the clamped
    /// prefix, which matters at a boundary: clamping valid UTF-8 mid-character
    /// and then noticing that the prefix does not decode would flip a line from
    /// `utf8` to `base64` because of where the limit fell. The clamp walks back
    /// off the character instead, exactly as every other bound in this crate
    /// does.
    #[must_use]
    pub fn clamped(bytes: &[u8], limit: u64) -> Self {
        let source_bytes = bytes.len() as u64;
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        match std::str::from_utf8(bytes) {
            Ok(text) => {
                let end = floor_char_boundary(text, limit);
                Self {
                    text: text[..end].to_owned(),
                    encoding: TextEncoding::Utf8,
                    truncated: end < text.len(),
                    source_bytes,
                }
            }
            Err(_) => {
                let end = bytes.len().min(limit);
                Self {
                    text: STANDARD_BASE64.encode(&bytes[..end]),
                    encoding: TextEncoding::Base64,
                    truncated: end < bytes.len(),
                    source_bytes,
                }
            }
        }
    }

    /// The text as it travels, in [`encoding`](Self::encoding).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// How the text is spelled.
    #[must_use]
    pub const fn encoding(&self) -> TextEncoding {
        self.encoding
    }

    /// Whether the source was longer than the limit.
    #[must_use]
    pub const fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// How many bytes the source held, before any clamping.
    #[must_use]
    pub const fn source_bytes(&self) -> u64 {
        self.source_bytes
    }

    /// The exact bytes this carries, whichever encoding it used.
    ///
    /// Round-trips: a Base64 excerpt decodes back to the bytes it was built
    /// from, clamped at the same place.
    #[must_use]
    pub fn bytes(&self) -> Vec<u8> {
        match self.encoding {
            TextEncoding::Utf8 => self.text.clone().into_bytes(),
            // Written by this type and never parsed from a caller, so a decode
            // failure is unreachable; answering with no bytes rather than
            // panicking keeps a corrupted value from taking a process down.
            TextEncoding::Base64 => STANDARD_BASE64.decode(&self.text).unwrap_or_default(),
        }
    }

    /// How many bytes of a response budget this excerpt costs.
    #[must_use]
    pub(crate) fn budget_cost(&self) -> u64 {
        self.text.len() as u64
    }
}

/// One match, positioned and attributed.
///
/// Content matches are reported one per *matching line*, positioned at the
/// first occurrence on it. That is what makes ordering a strict total order:
/// two matches can never share a `(path, byte_offset)` pair, so two runs over
/// the same bytes cannot disagree about which came first. Reporting every
/// occurrence separately would give a line with three hits three entries
/// against one budget, and reporting a line with no position would make the
/// order depend on the sort's stability.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct SearchMatch {
    /// Canonical repository-relative path, byte-exact.
    pub path: RepoPath,
    /// Absolute byte offset of the match within the file.
    ///
    /// For a filename match this is `0`: the whole path matched, and there is
    /// no file position to name.
    pub byte_offset: u64,
    /// One-based line the match is on, absent for a filename match.
    pub line_number: Option<u64>,
    /// The matched line, or the path itself for a filename match.
    pub line: BoundedText,
    /// Up to [`SearchLimits::context_lines`] lines before the match.
    ///
    /// In file order, so the last entry is the line immediately above.
    ///
    /// [`SearchLimits::context_lines`]: crate::SearchLimits::context_lines
    pub before: Vec<BoundedText>,
    /// Up to [`SearchLimits::context_lines`] lines after the match, in file
    /// order.
    ///
    /// [`SearchLimits::context_lines`]: crate::SearchLimits::context_lines
    pub after: Vec<BoundedText>,
    /// Digest of the file version that was searched.
    ///
    /// The bytes actually read, not the digest the index recorded: a file that
    /// moved between indexing and reading is searched as it is now and stamped
    /// with what it is now, so provenance never names a version nothing held.
    /// Absent for a filename match, which reads no content at all.
    pub content_sha256: Option<Sha256Hex>,
    /// Where this came from and why it was returned.
    ///
    /// Its own `content_sha256` covers the excerpt as emitted — after clamping
    /// — which is what
    /// [`Provenance::new`](crate::Provenance::new) documents it to mean, and is
    /// deliberately not the same digest as [`content_sha256`](Self::content_sha256).
    pub provenance: Provenance,
}

impl SearchMatch {
    /// The position this match occupies in the response's total order.
    #[must_use]
    pub fn position(&self) -> (&RepoPath, u64) {
        (&self.path, self.byte_offset)
    }

    /// How many bytes of a response budget this match costs.
    pub(crate) fn budget_cost(&self) -> u64 {
        let context: u64 = self
            .before
            .iter()
            .chain(self.after.iter())
            .map(BoundedText::budget_cost)
            .sum();
        self.line.budget_cost().saturating_add(context)
    }
}

/// Something a search did not return, and why.
///
/// Every bound that can fire is named here rather than reported by failing the
/// query or by shrinking the answer quietly. One unreadable file must not cost
/// a caller the ten thousand that were readable, and a budget that stopped a
/// scan must not look like a repository that ran out of matches.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SearchOmission {
    /// The response reached [`SearchLimits::max_results`].
    ///
    /// [`SearchLimits::max_results`]: crate::SearchLimits::max_results
    ResultBudgetExhausted {
        /// The limit that fired.
        limit: usize,
    },
    /// The response reached [`SearchLimits::max_bytes`].
    ///
    /// [`SearchLimits::max_bytes`]: crate::SearchLimits::max_bytes
    ByteBudgetExhausted {
        /// The limit that fired.
        limit: u64,
    },
    /// A matched line was longer than [`SearchLimits::max_line_bytes`].
    ///
    /// The match is still returned, clamped and marked; this says that what a
    /// caller was shown is a prefix of the line rather than the line. Other
    /// matches in the same file are unaffected.
    ///
    /// [`SearchLimits::max_line_bytes`]: crate::SearchLimits::max_line_bytes
    LineTooLong {
        /// The file the line is in.
        path: RepoPath,
        /// Where the match sits in that file.
        byte_offset: u64,
        /// The limit that fired.
        limit: u64,
    },
    /// A file the index lists could not be read.
    ///
    /// Deleted, permission-denied, or replaced by something that is not a
    /// regular file since it was indexed. The scan continues.
    FileUnreadable {
        /// The file that could not be read.
        path: RepoPath,
    },
    /// A file's bytes are not the ones the index recorded.
    ///
    /// The file *was* searched — as it is on disk now — and its matches carry
    /// the digest observed rather than the one stored. This says the index is
    /// behind, which is a fact about freshness rather than about the answer.
    FileChangedSinceIndex {
        /// The file that moved.
        path: RepoPath,
    },
    /// A file is text in an encoding this scan does not read.
    ///
    /// Byte offsets are what provenance and every later edit are anchored to,
    /// so a UTF-16 file is *not* transcoded and then searched: the offsets that
    /// came back would name positions in a decoded stream nothing on disk
    /// holds. Saying the file was not searched is the honest answer; returning
    /// no match from it would be indistinguishable from a file that does not
    /// contain the pattern. The chunker reads these files and this scan does
    /// not, which is a difference worth stating rather than hiding.
    EncodingNotSearchable {
        /// The file that was not searched.
        path: RepoPath,
        /// The encoding its byte-order mark declared.
        encoding: crate::chunk::ContentEncoding,
    },
    /// Binary content was met inside a file the index classified as text.
    ///
    /// Defense in depth: classification already keeps binaries out of the
    /// universe, so reaching this means a file changed kind since it was
    /// indexed. Scanning stops at the first NUL and whatever preceded it is
    /// reported, because bytes after one are not lines.
    BinaryContentDetected {
        /// The file that turned out to hold binary content.
        path: RepoPath,
        /// Where the first NUL byte sits.
        byte_offset: u64,
    },
}

impl SearchOmission {
    /// The stable spelling of this omission, for payloads and diagnostics.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::ResultBudgetExhausted { .. } => "result_budget_exhausted",
            Self::ByteBudgetExhausted { .. } => "byte_budget_exhausted",
            Self::LineTooLong { .. } => "line_too_long",
            Self::FileUnreadable { .. } => "file_unreadable",
            Self::FileChangedSinceIndex { .. } => "file_changed_since_index",
            Self::EncodingNotSearchable { .. } => "encoding_not_searchable",
            Self::BinaryContentDetected { .. } => "binary_content_detected",
        }
    }
}

/// What one search cost, in numbers a repository decides rather than a clock.
///
/// Deliberately no duration: two runs over an unchanged worktree return equal
/// responses, and a field that cannot be equal twice would make that
/// untestable. Timing goes to the diagnostic span, where it belongs.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct SearchStats {
    /// Index rows the query considered, before filters.
    pub paths_examined: u64,
    /// Files whose content was opened and scanned.
    pub files_scanned: u64,
    /// Bytes of file content scanned.
    pub bytes_scanned: u64,
}

/// One page of a search.
///
/// [`query_id`](Self::query_id) and [`snapshot_id`](Self::snapshot_id) name the
/// *call*; everything else names the *answer*. Two runs of one query over an
/// unchanged worktree produce one answer and two captures — equal matches,
/// omissions, statistics and cursor, and two ids. That is the same distinction
/// [`SnapshotId`] draws everywhere else in this crate: capturing one unchanged
/// workspace twice yields two ids and one digest, because an id names the
/// reading and the digest names what was read. The one place it shows through
/// is [`SearchMatch::provenance`], whose `snapshot_id` names the capture that
/// match was read under, so a determinism check compares matches against the
/// capture rather than across two of them.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct SearchResponse {
    /// Identifies this call, for correlation with a run and a tool call.
    pub query_id: ContextQueryId,
    /// The workspace state the matches were read from.
    pub snapshot_id: SnapshotId,
    /// The index generation the universe was read from.
    pub index_generation: u64,
    /// The matches, in canonical order.
    pub matches: Vec<SearchMatch>,
    /// Every bound that fired, at most [`MAX_SEARCH_OMISSIONS`] of them.
    pub omissions: Vec<SearchOmission>,
    /// Omissions past [`MAX_SEARCH_OMISSIONS`] that were counted, not carried.
    pub dropped_omissions: usize,
    /// Where to continue, when a budget stopped the scan short.
    pub next_cursor: Option<SearchCursor>,
    /// What the scan cost.
    pub stats: SearchStats,
}

impl SearchResponse {
    /// Whether a budget stopped this page before the repository ran out.
    #[must_use]
    pub const fn is_truncated(&self) -> bool {
        self.next_cursor.is_some()
    }
}
