//! Where a piece of context came from, and why it was chosen.
//!
//! Provenance is attached to every retrieved item, and every field of it exists
//! to make a specific later claim checkable rather than asserted:
//!
//! - [`Provenance::snapshot_id`] names the workspace state the content was read
//!   from, so a run inspected afterwards can say which bytes existed then.
//! - [`Provenance::content_sha256`] is the digest of the bytes the model was
//!   actually shown — after truncation, not before — so [#138] can prove what
//!   left the machine without storing a second copy of it.
//! - [`Provenance::reason`] is a typed variant beside its human-readable text.
//!   A free-form string alone cannot be asserted on; a typed variant alone
//!   cannot be shown to a user.
//! - [`Provenance::sensitivity`] records that content was marked untrusted or
//!   redacted, so a test can require that suspicious content was marked and
//!   sensitive content excluded.
//!
//! Every text field is length-bounded. These records become columns beside the
//! run store's 64 KiB inline limit, and a bound applied only at the persistence
//! layer would mean a value that constructs cleanly and then makes the record
//! of its own use unpersistable.
//!
//! [#138]: https://github.com/fullstacktaiye/harkness/issues/138

use serde::{Deserialize, Serialize};

use crate::digest::Sha256Hex;
use crate::error::ContextDomainError;
use crate::ids::{SnapshotId, SymbolId};
use crate::path::RepoPath;

/// The longest a caller-supplied provenance string may be.
///
/// Well below the run store's inline limit, because one context pack carries
/// many of these and the budget is per row rather than per field.
pub const MAX_PROVENANCE_TEXT_BYTES: usize = 4096;

/// Which retrieval path produced a piece of context.
///
/// Distinct from *why* it was chosen: the source says which machinery found it,
/// and [`SelectionReason`] says what made it worth including.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RetrievalSource {
    /// The structural map of the repository.
    RepositoryMap,
    /// A search over path names.
    FilenameSearch,
    /// A search over file contents.
    LexicalSearch,
    /// A lookup in the symbol index.
    SymbolIndex,
    /// A diff between two workspace states.
    GitDiff,
    /// Commit history.
    GitHistory,
    /// The discovered instruction set.
    Instructions,
    /// A person named it explicitly.
    UserSelected,
    /// It came back from a tool call.
    ToolResult,
}

impl RetrievalSource {
    /// Every retrieval source in its stable declaration order.
    pub const ALL: &'static [Self] = &[
        Self::RepositoryMap,
        Self::FilenameSearch,
        Self::LexicalSearch,
        Self::SymbolIndex,
        Self::GitDiff,
        Self::GitHistory,
        Self::Instructions,
        Self::UserSelected,
        Self::ToolResult,
    ];

    /// Returns the stable persisted spelling of this source.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RepositoryMap => "repository_map",
            Self::FilenameSearch => "filename_search",
            Self::LexicalSearch => "lexical_search",
            Self::SymbolIndex => "symbol_index",
            Self::GitDiff => "git_diff",
            Self::GitHistory => "git_history",
            Self::Instructions => "instructions",
            Self::UserSelected => "user_selected",
            Self::ToolResult => "tool_result",
        }
    }
}

impl std::fmt::Display for RetrievalSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The typed half of why one item was included.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SelectionReasonKind {
    /// A person asked for this exact path or symbol.
    ExplicitRequest,
    /// The content matched the query.
    QueryMatch,
    /// It defines a symbol the query or another item names.
    SymbolDefinition,
    /// It references a symbol another selected item defines.
    SymbolReference,
    /// A selected item depends on it.
    DependencyOfSelected,
    /// It changed recently, or is part of the change under discussion.
    RecentlyChanged,
    /// It is part of the discovered instruction set.
    InstructionFile,
    /// It is the structural summary of the repository.
    RepositoryOverview,
    /// It came back from a tool the model called.
    ToolOutput,
}

impl SelectionReasonKind {
    /// Every selection reason in its stable declaration order.
    pub const ALL: &'static [Self] = &[
        Self::ExplicitRequest,
        Self::QueryMatch,
        Self::SymbolDefinition,
        Self::SymbolReference,
        Self::DependencyOfSelected,
        Self::RecentlyChanged,
        Self::InstructionFile,
        Self::RepositoryOverview,
        Self::ToolOutput,
    ];

    /// Returns the stable persisted spelling of this reason.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitRequest => "explicit_request",
            Self::QueryMatch => "query_match",
            Self::SymbolDefinition => "symbol_definition",
            Self::SymbolReference => "symbol_reference",
            Self::DependencyOfSelected => "dependency_of_selected",
            Self::RecentlyChanged => "recently_changed",
            Self::InstructionFile => "instruction_file",
            Self::RepositoryOverview => "repository_overview",
            Self::ToolOutput => "tool_output",
        }
    }
}

impl std::fmt::Display for SelectionReasonKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Why one item was included, in both machine and human form.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionReason {
    /// The typed reason, which tests and policies compare against.
    pub kind: SelectionReasonKind,
    /// The explanation shown to a person, bounded by
    /// [`MAX_PROVENANCE_TEXT_BYTES`].
    pub detail: String,
}

impl SelectionReason {
    /// Records a reason, truncating the detail to the published bound.
    #[must_use]
    pub fn new(kind: SelectionReasonKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: clamp(detail.into()),
        }
    }
}

/// One signal that contributed to an item's rank.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RankSignal {
    /// What the signal measures, bounded by [`MAX_PROVENANCE_TEXT_BYTES`].
    pub name: String,
    /// The signal's own value.
    pub value: f64,
    /// How heavily the ranker weighted it.
    pub weight: f64,
}

impl RankSignal {
    /// Records one contributing signal.
    #[must_use]
    pub fn new(name: impl Into<String>, value: f64, weight: f64) -> Self {
        Self {
            name: clamp(name.into()),
            value,
            weight,
        }
    }
}

/// Where an item ranked, and what put it there.
///
/// The scores are `f64`, so these types carry [`PartialEq`] and not [`Eq`]. A
/// non-finite score is refused when a record is decoded rather than stored: a
/// `NaN` score compares unequal to itself, so a ranking holding one silently
/// stops being an ordering.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RankExplanation {
    /// The item's final score.
    pub score: f64,
    /// Its zero-based position among the candidates considered.
    pub position: u32,
    /// How many candidates it was ranked against.
    pub candidates: u32,
    /// The signals that produced the score.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signals: Vec<RankSignal>,
}

impl RankExplanation {
    /// Records a rank with no signal breakdown.
    #[must_use]
    pub fn new(score: f64, position: u32, candidates: u32) -> Self {
        Self {
            score,
            position,
            candidates,
            signals: Vec::new(),
        }
    }

    /// Records the signals that produced the score.
    #[must_use]
    pub fn with_signals(mut self, signals: Vec<RankSignal>) -> Self {
        self.signals = signals;
        self
    }

    /// Refuses a ranking that is not an ordering.
    pub(crate) fn validate(&self) -> Result<(), ContextDomainError> {
        let invalid = |reason: String| ContextDomainError::InvalidProvenanceWire { reason };
        if !self.score.is_finite() {
            return Err(invalid(format!("rank score {} is not finite", self.score)));
        }
        // Unconditionally, including `candidates == 0`. Guarding the comparison
        // on a non-empty candidate set would exempt `position: 7, candidates: 0`
        // — an impossible pair, and precisely the one this check exists for.
        if self.position >= self.candidates {
            return Err(invalid(format!(
                "rank position {} is not within {} candidates",
                self.position, self.candidates
            )));
        }
        for signal in &self.signals {
            if !signal.value.is_finite() || !signal.weight.is_finite() {
                return Err(invalid(format!(
                    "rank signal '{}' carries a non-finite value or weight",
                    signal.name
                )));
            }
        }
        Ok(())
    }
}

/// How a piece of content must be treated.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Sensitivity {
    /// Ordinary repository content.
    Normal,
    /// Content that matched an injection or untrusted-content rule and was
    /// marked before being shown.
    Suspicious {
        /// The marker that was applied, bounded by
        /// [`MAX_PROVENANCE_TEXT_BYTES`].
        marker: String,
    },
    /// Content a redaction rule removed.
    ///
    /// Carries the rule's name and never the removed value, mirroring the
    /// `«redacted:<rule>»` marker the run store writes.
    Redacted {
        /// The rule that fired, bounded by [`MAX_PROVENANCE_TEXT_BYTES`].
        rule: String,
    },
}

impl Sensitivity {
    /// Marks content as suspicious.
    #[must_use]
    pub fn suspicious(marker: impl Into<String>) -> Self {
        Self::Suspicious {
            marker: clamp(marker.into()),
        }
    }

    /// Records that a redaction rule fired.
    #[must_use]
    pub fn redacted(rule: impl Into<String>) -> Self {
        Self::Redacted {
            rule: clamp(rule.into()),
        }
    }

    /// Whether this content is anything other than ordinary.
    #[must_use]
    pub const fn is_normal(&self) -> bool {
        matches!(self, Self::Normal)
    }
}

/// A half-open byte range within a file, with derived line hints.
///
/// Bytes are authoritative: they are what a digest covers and what an edit is
/// applied against. The line numbers are a convenience for display and may be
/// absent for content with no line structure.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ByteRange {
    /// First byte, inclusive.
    pub start: u64,
    /// One past the last byte.
    pub end: u64,
    /// One-based line the range starts on, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_line: Option<u32>,
    /// One-based line the range ends on, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_line: Option<u32>,
}

impl ByteRange {
    /// Records a half-open byte range with no line hints.
    #[must_use]
    pub const fn new(start: u64, end: u64) -> Self {
        Self {
            start,
            end,
            first_line: None,
            last_line: None,
        }
    }

    /// Adds the one-based line numbers the range spans.
    #[must_use]
    pub const fn with_lines(mut self, first: u32, last: u32) -> Self {
        self.first_line = Some(first);
        self.last_line = Some(last);
        self
    }

    /// How many bytes the range covers.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    /// Whether the range covers nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.end <= self.start
    }

    /// Refuses a range that cannot describe any content.
    pub(crate) fn validate(&self) -> Result<(), ContextDomainError> {
        let invalid = |reason: String| ContextDomainError::InvalidProvenanceWire { reason };
        if self.end < self.start {
            return Err(invalid(format!(
                "byte range end {} precedes its start {}",
                self.end, self.start
            )));
        }
        if let (Some(first), Some(last)) = (self.first_line, self.last_line)
            && last < first
        {
            return Err(invalid(format!(
                "line range end {last} precedes its start {first}"
            )));
        }
        Ok(())
    }
}

/// The symbol a piece of context describes.
///
/// The language and kind vocabularies belong to [#117]; this type fixes only how
/// a symbol is referenced and that its identity is content-derived.
///
/// [#117]: https://github.com/fullstacktaiye/harkness/issues/117
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SymbolRef {
    /// Stable identity of the symbol.
    pub id: SymbolId,
    /// Fully qualified name, bounded by [`MAX_PROVENANCE_TEXT_BYTES`].
    pub qualified_name: String,
    /// The symbol's kind, bounded by [`MAX_PROVENANCE_TEXT_BYTES`].
    pub kind: String,
    /// The language it is written in, bounded by [`MAX_PROVENANCE_TEXT_BYTES`].
    pub language: String,
}

impl SymbolRef {
    /// References a symbol, deriving its identity from its declaration.
    #[must_use]
    pub fn new(
        path: &RepoPath,
        language: impl Into<String>,
        qualified_name: impl Into<String>,
        kind: impl Into<String>,
    ) -> Self {
        let language = clamp(language.into());
        let qualified_name = clamp(qualified_name.into());
        let kind = clamp(kind.into());
        Self {
            id: SymbolId::derive(path, &language, &qualified_name, &kind),
            qualified_name,
            kind,
            language,
        }
    }
}

/// Everything known about where one piece of context came from.
///
/// Serialization goes through [`ProvenanceWire`](crate::ProvenanceWire), which
/// carries the schema version and revalidates the record on the way in, so
/// there is exactly one durable spelling of it.
#[derive(Clone, Debug, PartialEq)]
pub struct Provenance {
    /// Which retrieval path produced it.
    pub source: RetrievalSource,
    /// Repository-relative path, absent for content that has none.
    pub path: Option<RepoPath>,
    /// The byte range within that path, when the item is part of a file.
    pub range: Option<ByteRange>,
    /// The symbol the item describes, when it describes one.
    pub symbol: Option<SymbolRef>,
    /// Digest of the exact bytes the model was shown, after any truncation.
    pub content_sha256: Sha256Hex,
    /// The workspace state the content was read from.
    pub snapshot_id: SnapshotId,
    /// Why the item was included.
    pub reason: SelectionReason,
    /// Where it ranked, when ranking produced it.
    pub rank: Option<RankExplanation>,
    /// Whether the shown bytes are a prefix of the source content.
    pub truncated: bool,
    /// How the content must be treated.
    pub sensitivity: Sensitivity,
}

impl Provenance {
    /// Records the provenance of `content` retrieved under `snapshot`.
    ///
    /// The digest is taken here, over the bytes the caller is about to show, so
    /// it cannot describe a different version of them.
    #[must_use]
    pub fn new(
        source: RetrievalSource,
        snapshot_id: SnapshotId,
        content: &[u8],
        reason: SelectionReason,
    ) -> Self {
        Self {
            source,
            path: None,
            range: None,
            symbol: None,
            content_sha256: Sha256Hex::of(content),
            snapshot_id,
            reason,
            rank: None,
            truncated: false,
            sensitivity: Sensitivity::Normal,
        }
    }

    /// Names the path the content came from.
    #[must_use]
    pub fn at_path(mut self, path: RepoPath) -> Self {
        self.path = Some(path);
        self
    }

    /// Names the byte range the content occupies.
    #[must_use]
    pub fn in_range(mut self, range: ByteRange) -> Self {
        self.range = Some(range);
        self
    }

    /// Names the symbol the content describes.
    #[must_use]
    pub fn for_symbol(mut self, symbol: SymbolRef) -> Self {
        self.symbol = Some(symbol);
        self
    }

    /// Records where the item ranked.
    #[must_use]
    pub fn ranked(mut self, rank: RankExplanation) -> Self {
        self.rank = Some(rank);
        self
    }

    /// Records that the shown bytes are a prefix of the source content.
    #[must_use]
    pub const fn truncated(mut self) -> Self {
        self.truncated = true;
        self
    }

    /// Records how the content must be treated.
    #[must_use]
    pub fn with_sensitivity(mut self, sensitivity: Sensitivity) -> Self {
        self.sensitivity = sensitivity;
        self
    }

    /// Refuses a record whose fields cannot describe any retrieval.
    pub(crate) fn validate(&self) -> Result<(), ContextDomainError> {
        let invalid = |reason: String| ContextDomainError::InvalidProvenanceWire { reason };
        if let Some(range) = self.range.as_ref() {
            range.validate()?;
            if self.path.is_none() {
                return Err(invalid(
                    "a byte range names no file without a path".to_owned(),
                ));
            }
        }
        if let Some(rank) = self.rank.as_ref() {
            rank.validate()?;
        }
        check_bound("reason.detail", &self.reason.detail)?;
        if let Some(symbol) = self.symbol.as_ref() {
            check_bound("symbol.qualified_name", &symbol.qualified_name)?;
            check_bound("symbol.kind", &symbol.kind)?;
            check_bound("symbol.language", &symbol.language)?;
            // A symbol is declared in a file, and its identity derives from that
            // file's path, so a record naming a symbol and no path is one whose
            // identity cannot be checked — the single asserted-but-unverified
            // content identity left in the crate. The rule is the one `range`
            // already carries.
            let Some(path) = self.path.as_ref() else {
                return Err(invalid(
                    "a symbol names no declaration site without a path".to_owned(),
                ));
            };
            // Re-derived rather than trusted, for the reason a snapshot's
            // digests are: a content-derived identity a record merely asserts is
            // one nothing downstream re-checks, so a row could name a symbol its
            // own components do not produce.
            let derived =
                SymbolId::derive(path, &symbol.language, &symbol.qualified_name, &symbol.kind);
            if derived != symbol.id {
                return Err(invalid(format!(
                    "symbol.id is {} but its components derive {derived}",
                    symbol.id
                )));
            }
        }
        match &self.sensitivity {
            Sensitivity::Normal => {}
            Sensitivity::Suspicious { marker } => check_bound("sensitivity.marker", marker)?,
            Sensitivity::Redacted { rule } => check_bound("sensitivity.rule", rule)?,
        }
        if let Some(rank) = self.rank.as_ref() {
            for signal in &rank.signals {
                check_bound("rank.signals.name", &signal.name)?;
            }
        }
        Ok(())
    }
}

/// Truncates a caller string to the published bound on a character boundary.
fn clamp(mut text: String) -> String {
    if text.len() <= MAX_PROVENANCE_TEXT_BYTES {
        return text;
    }
    let mut boundary = MAX_PROVENANCE_TEXT_BYTES;
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text.truncate(boundary);
    text
}

/// Refuses a decoded string that exceeds the published bound.
///
/// Construction clamps; decoding refuses. A record that arrived from outside
/// Harkness oversized must fail to load rather than be silently shortened, which
/// is the position the run store takes on every caller-controlled column.
fn check_bound(field: &'static str, value: &str) -> Result<(), ContextDomainError> {
    if value.len() > MAX_PROVENANCE_TEXT_BYTES {
        return Err(ContextDomainError::InvalidProvenanceWire {
            reason: format!(
                "{field} is {} bytes, over the {MAX_PROVENANCE_TEXT_BYTES} byte limit",
                value.len()
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ByteRange, MAX_PROVENANCE_TEXT_BYTES, Provenance, RankExplanation, RankSignal,
        RetrievalSource, SelectionReason, SelectionReasonKind, Sensitivity, SymbolRef,
    };
    use crate::ids::SnapshotId;
    use crate::path::RepoPath;

    fn path() -> RepoPath {
        RepoPath::from_bytes(b"src/main.rs".to_vec())
    }

    fn provenance() -> Provenance {
        Provenance::new(
            RetrievalSource::LexicalSearch,
            SnapshotId::new(),
            b"fn main() {}",
            SelectionReason::new(SelectionReasonKind::QueryMatch, "matched 'main'"),
        )
        .at_path(path())
        .in_range(ByteRange::new(0, 12).with_lines(1, 1))
    }

    #[test]
    fn every_vocabulary_serializes_as_its_snake_case_spelling() {
        for source in RetrievalSource::ALL {
            let json = serde_json::to_string(source).unwrap();
            assert_eq!(json, format!("\"{}\"", source.as_str()));
            assert_eq!(
                &serde_json::from_str::<RetrievalSource>(&json).unwrap(),
                source
            );
        }
        for kind in SelectionReasonKind::ALL {
            let json = serde_json::to_string(kind).unwrap();
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
            assert_eq!(
                &serde_json::from_str::<SelectionReasonKind>(&json).unwrap(),
                kind
            );
        }
        assert_eq!(RetrievalSource::ALL.len(), 9);
        assert_eq!(SelectionReasonKind::ALL.len(), 9);
    }

    #[test]
    fn sensitivity_serializes_as_a_tagged_object_naming_only_the_rule() {
        assert_eq!(
            serde_json::to_string(&Sensitivity::Normal).unwrap(),
            r#"{"kind":"normal"}"#
        );
        let redacted = Sensitivity::redacted("github_token");
        assert_eq!(
            serde_json::to_string(&redacted).unwrap(),
            r#"{"kind":"redacted","rule":"github_token"}"#
        );
        assert_eq!(
            serde_json::from_str::<Sensitivity>(r#"{"kind":"redacted","rule":"github_token"}"#)
                .unwrap(),
            redacted
        );
        assert!(Sensitivity::Normal.is_normal());
        assert!(!Sensitivity::suspicious("«untrusted»").is_normal());
    }

    #[test]
    fn a_range_without_a_path_names_no_file() {
        let mut record = provenance();
        record.path = None;
        assert_eq!(
            record.validate().unwrap_err().kind(),
            "invalid_provenance_wire"
        );
    }

    #[test]
    fn an_inverted_range_is_refused() {
        let mut record = provenance();
        record.range = Some(ByteRange::new(20, 10));
        assert_eq!(
            record.validate().unwrap_err().kind(),
            "invalid_provenance_wire"
        );

        record.range = Some(ByteRange::new(0, 10).with_lines(9, 2));
        assert_eq!(
            record.validate().unwrap_err().kind(),
            "invalid_provenance_wire"
        );
    }

    #[test]
    fn an_empty_range_is_allowed_and_reports_its_length() {
        let range = ByteRange::new(7, 7);
        assert!(range.is_empty());
        assert_eq!(range.len(), 0);
        assert!(range.validate().is_ok());
        assert_eq!(ByteRange::new(4, 10).len(), 6);
    }

    #[test]
    fn a_non_finite_rank_is_refused_because_it_is_not_an_ordering() {
        let record = provenance().ranked(RankExplanation::new(f64::NAN, 0, 3));
        assert_eq!(
            record.validate().unwrap_err().kind(),
            "invalid_provenance_wire"
        );

        let record = provenance().ranked(
            RankExplanation::new(1.0, 0, 1).with_signals(vec![RankSignal::new(
                "recency",
                f64::INFINITY,
                0.5,
            )]),
        );
        assert_eq!(
            record.validate().unwrap_err().kind(),
            "invalid_provenance_wire"
        );
    }

    #[test]
    fn a_rank_outside_its_candidate_set_is_refused() {
        for (position, candidates) in [(5, 3), (7, 0), (0, 0)] {
            let record = provenance().ranked(RankExplanation::new(1.0, position, candidates));
            assert_eq!(
                record.validate().unwrap_err().kind(),
                "invalid_provenance_wire",
                "accepted position {position} of {candidates}"
            );
        }
        assert!(
            provenance()
                .ranked(RankExplanation::new(1.0, 2, 3))
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn a_symbol_without_a_path_is_refused_because_its_identity_cannot_be_checked() {
        let mut record =
            provenance().for_symbol(SymbolRef::new(&path(), "rust", "main", "function"));
        record.path = None;
        record.range = None;
        let error = record.validate().unwrap_err();
        assert_eq!(error.kind(), "invalid_provenance_wire");
        assert!(error.to_string().contains("without a path"), "{error}");
    }

    #[test]
    fn a_symbol_identity_that_its_components_do_not_derive_is_refused() {
        let mut record =
            provenance().for_symbol(SymbolRef::new(&path(), "rust", "main", "function"));
        let symbol = record.symbol.as_mut().expect("the fixture names a symbol");
        symbol.qualified_name = "renamed_after_the_fact".to_owned();
        let error = record.validate().unwrap_err();
        assert_eq!(error.kind(), "invalid_provenance_wire");
        assert!(error.to_string().contains("symbol.id"), "{error}");
    }

    #[test]
    fn construction_clamps_text_and_decoding_refuses_it() {
        let long = "x".repeat(MAX_PROVENANCE_TEXT_BYTES * 2);
        let reason = SelectionReason::new(SelectionReasonKind::QueryMatch, long.clone());
        assert_eq!(reason.detail.len(), MAX_PROVENANCE_TEXT_BYTES);

        let mut record = provenance();
        record.reason.detail = long;
        assert_eq!(
            record.validate().unwrap_err().kind(),
            "invalid_provenance_wire"
        );
    }

    #[test]
    fn clamping_never_splits_a_character() {
        // A three-byte character straddling the bound must be dropped whole.
        let text = format!("{}\u{4e00}", "x".repeat(MAX_PROVENANCE_TEXT_BYTES - 1));
        let reason = SelectionReason::new(SelectionReasonKind::QueryMatch, text);
        assert_eq!(reason.detail.len(), MAX_PROVENANCE_TEXT_BYTES - 1);
        assert!(reason.detail.chars().all(|character| character == 'x'));
    }

    #[test]
    fn a_symbol_reference_derives_its_identity_from_its_declaration() {
        let symbol = SymbolRef::new(&path(), "rust", "main", "function");
        let record = provenance().for_symbol(symbol.clone());
        assert!(record.validate().is_ok());
        assert_eq!(
            symbol.id,
            crate::ids::SymbolId::derive(&path(), "rust", "main", "function")
        );
    }

    #[test]
    fn a_truncated_record_says_so_and_digests_what_was_shown() {
        let shown = b"fn main";
        let record = Provenance::new(
            RetrievalSource::LexicalSearch,
            SnapshotId::new(),
            shown,
            SelectionReason::new(SelectionReasonKind::QueryMatch, "matched"),
        )
        .truncated()
        .with_sensitivity(Sensitivity::suspicious("«untrusted»"));
        assert!(record.truncated);
        assert_eq!(record.content_sha256, crate::digest::Sha256Hex::of(shown));
        assert_ne!(
            record.content_sha256,
            crate::digest::Sha256Hex::of(b"fn main() {}"),
            "the digest must describe the bytes shown, not the bytes retrieved"
        );
    }
}
