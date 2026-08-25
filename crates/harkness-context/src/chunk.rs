//! Stable, bounded units of eligible repository content.
//!
//! Chunk identity deliberately excludes byte and line positions. A structural
//! anchor and the chunk's own bytes decide the identity, so moving an unchanged
//! function down a file refreshes its location without invalidating the cached
//! content. [`CHUNKING_VERSION`] names the complete set of rules in this module.

use std::fmt;
use std::sync::Arc;

use harkness_git::Cancellation;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    ByteRange, ChunkId, FileClass, FileVersionId, InventoryEntry, RepoPath, Sensitivity, Sha256Hex,
    SnapshotId, SymbolId,
};

type EncodingBoundaries = Arc<[(usize, usize)]>;
type DecodedText = (String, ContentEncoding, Option<EncodingBoundaries>);

// Anchor serialization is part of ChunkId's frozen identity contract. It is
// deliberately independent of CHUNKING_VERSION: changing boundary selection
// invalidates cached rows without renaming an otherwise identical chunk.
const ANCHOR_ENCODING_VERSION: u32 = 1;

/// Version of the anchor encoding and chunk-boundary rules.
pub const CHUNKING_VERSION: u32 = 1;
/// Hard upper bound for the text represented by one chunk.
pub const MAX_CHUNK_BYTES: usize = 16 * 1024;
/// Preferred size for ordinary chunks.
pub const TARGET_CHUNK_BYTES: usize = 4 * 1024;
/// Files no larger than this become one whole-file chunk.
pub const MIN_WHOLE_FILE_BYTES: usize = 2 * 1024;
/// Line overlap used only by the source fallback.
pub const CHUNK_OVERLAP_LINES: usize = 8;
/// Maximum number of real content chunks returned for one file.
pub const MAX_CHUNKS_PER_FILE: usize = 512;

/// A bounded language identifier that parser adapters may populate later.
///
/// The value is metadata, not identity. It therefore has no effect on a
/// [`ChunkId`] and changing language detection cannot churn cached chunks.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Language(String);

impl Language {
    /// Creates a lowercase ASCII language identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, ChunkError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(ChunkError::InvalidLanguage { value });
        }
        Ok(Self(value))
    }

    /// The stable identifier spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Language {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for Language {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// How the exact file bytes are interpreted for chunk boundaries.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ContentEncoding {
    /// Exact bytes are valid UTF-8.
    Utf8,
    /// Exact bytes carry a little-endian UTF-16 byte-order mark.
    Utf16Le,
    /// Exact bytes carry a big-endian UTF-16 byte-order mark.
    Utf16Be,
}

impl ContentEncoding {
    /// Whether prompt text was transcoded from the file's original bytes.
    #[must_use]
    pub const fn is_transcoded(self) -> bool {
        !matches!(self, Self::Utf8)
    }
}

/// One exact, eligible working-tree file version supplied to a chunker.
#[derive(Clone, Debug)]
pub struct FileVersion {
    path: RepoPath,
    id: FileVersionId,
    content_sha256: Sha256Hex,
    class: FileClass,
    language: Option<Language>,
    snapshot: SnapshotId,
    sensitivity: Sensitivity,
    bytes: Arc<[u8]>,
    text: Arc<str>,
    encoding: ContentEncoding,
    utf16_boundaries: Option<EncodingBoundaries>,
}

impl FileVersion {
    /// Validates and captures bytes for one eligible inventory entry.
    ///
    /// The inventory size is rechecked so metadata from an earlier walk cannot
    /// silently be paired with different content. A caller that observes a
    /// mismatch must refresh the inventory before retrying.
    pub fn new(
        entry: &InventoryEntry,
        snapshot: SnapshotId,
        bytes: Arc<[u8]>,
        cancellation: &Cancellation,
    ) -> Result<Self, ChunkError> {
        if cancellation.is_cancelled() {
            return Err(ChunkError::Cancelled);
        }
        if !entry.eligible() {
            return Err(ChunkError::UnsupportedClass {
                path: entry.path.display(),
                class: entry.class,
            });
        }
        if entry.byte_size != u64::try_from(bytes.len()).unwrap_or(u64::MAX) {
            return Err(ChunkError::FileChanged {
                path: entry.path.display(),
                expected: entry.byte_size,
                found: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            });
        }

        let (text, encoding, utf16_boundaries) = decode_text(&bytes, cancellation)?;
        let content_sha256 = Sha256Hex::of(&bytes);
        let id = FileVersionId::from_content_digest(&entry.path, &content_sha256);
        Ok(Self {
            path: entry.path.clone(),
            id,
            content_sha256,
            class: entry.class,
            language: None,
            snapshot,
            sensitivity: Sensitivity::Normal,
            bytes,
            text: text.into(),
            encoding,
            utf16_boundaries,
        })
    }

    /// Attaches language metadata without changing content identity.
    #[must_use]
    pub fn with_language(mut self, language: Language) -> Self {
        self.language = Some(language);
        self
    }

    /// Attaches a sensitivity decision made above chunking.
    #[must_use]
    pub fn with_sensitivity(mut self, sensitivity: Sensitivity) -> Self {
        self.sensitivity = sensitivity;
        self
    }

    /// Repository-relative, byte-exact path.
    #[must_use]
    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    /// Identity of the exact original bytes at this path.
    #[must_use]
    pub fn id(&self) -> &FileVersionId {
        &self.id
    }

    /// SHA-256 of the exact original bytes.
    #[must_use]
    pub fn content_sha256(&self) -> &Sha256Hex {
        &self.content_sha256
    }

    /// Inventory classification.
    #[must_use]
    pub const fn class(&self) -> FileClass {
        self.class
    }

    /// Optional language metadata.
    #[must_use]
    pub fn language(&self) -> Option<&Language> {
        self.language.as_ref()
    }

    /// Snapshot capture this read belongs to.
    #[must_use]
    pub const fn snapshot(&self) -> SnapshotId {
        self.snapshot
    }

    /// Exact original file bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// UTF-8 text chunking operates on.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Encoding detected from the complete bytes.
    #[must_use]
    pub const fn encoding(&self) -> ContentEncoding {
        self.encoding
    }

    fn original_range(&self, start: usize, end: usize) -> Result<(u64, u64), ChunkError> {
        if self.encoding == ContentEncoding::Utf8 {
            return Ok((to_u64(start)?, to_u64(end)?));
        }
        let boundaries = self.utf16_boundaries.as_deref().unwrap_or_default();
        let start_original = if start == 0 {
            0
        } else {
            boundary_lookup(boundaries, start).ok_or(ChunkError::EncodingBoundary)?
        };
        let end_original = if end == self.text.len() {
            self.bytes.len()
        } else {
            boundary_lookup(boundaries, end).ok_or(ChunkError::EncodingBoundary)?
        };
        Ok((to_u64(start_original)?, to_u64(end_original)?))
    }

    fn logical_offset(&self, original: u64) -> Option<usize> {
        let original = usize::try_from(original).ok()?;
        if self.encoding == ContentEncoding::Utf8 {
            return self.text.is_char_boundary(original).then_some(original);
        }
        if original == 0 {
            return Some(0);
        }
        if original == self.bytes.len() {
            return Some(self.text.len());
        }
        self.utf16_boundaries
            .as_deref()?
            .iter()
            .find_map(|(logical, source)| (*source == original).then_some(*logical))
    }
}

/// Stable structural position of one chunk.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Anchor {
    /// Qualified structural path supplied by a parser outline.
    Symbol {
        /// Qualified names from outermost to innermost.
        path: Vec<String>,
    },
    /// Case-preserved Markdown heading path.
    Heading {
        /// Heading titles from document root to this section.
        path: Vec<String>,
    },
    /// Configuration table or key path.
    ConfigKey {
        /// Table or key components from outermost to innermost.
        path: Vec<String>,
    },
    /// Honest positional fallback.
    LineWindow {
        /// Zero-based fallback window number.
        index: u32,
    },
    /// Entire tiny file.
    WholeFile,
}

impl Anchor {
    fn identity_key(&self, ordinal: u32) -> String {
        let mut key = format!("v{ANCHOR_ENCODING_VERSION};");
        match self {
            Self::Symbol { path } => encode_path(&mut key, "symbol", path),
            Self::Heading { path } => encode_path(&mut key, "heading", path),
            Self::ConfigKey { path } => encode_path(&mut key, "config", path),
            Self::LineWindow { index } => {
                key.push_str("line;");
                key.push_str(&index.to_string());
                key.push(';');
            }
            Self::WholeFile => key.push_str("whole;"),
        }
        key.push_str("part;");
        key.push_str(&ordinal.to_string());
        key
    }
}

/// One parser-supplied, non-overlapping structural span.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutlineNode {
    /// Qualified stable structural path.
    pub anchor_path: Vec<String>,
    /// Half-open range in the original file bytes.
    pub byte_range: std::ops::Range<u64>,
    /// Stable parser-owned kind such as `function` or `type`.
    pub kind: String,
    /// Symbol identity when extraction has one.
    pub symbol: Option<SymbolId>,
}

/// Parser projection used only to choose chunk boundaries.
///
/// Nodes must be non-overlapping. A parser with a nested symbol tree projects
/// the leaf or otherwise chosen chunk boundaries here instead of passing the
/// overlapping symbol inventory wholesale.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StructuralOutline {
    /// Nodes in any order; validation orders a copy by byte range.
    pub nodes: Vec<OutlineNode>,
    /// Language that produced the outline.
    pub language: Option<Language>,
}

/// Why a bounded result does not cover the remainder of a file.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ChunkTruncation {
    /// The per-file count bound was reached.
    ChunkBudgetExhausted {
        /// Maximum number of content chunks returned.
        limit: usize,
    },
}

/// One stable, bounded chunk of a file version.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChunkRecord {
    /// Content-derived chunk identity.
    pub id: ChunkId,
    /// Exact file version this range belongs to.
    pub file: FileVersionId,
    /// Structural identity, independent of position.
    pub anchor: Anchor,
    /// Continuation number beneath the anchor.
    pub ordinal: u32,
    /// Original-file half-open bytes and one-based line hints.
    pub byte_range: ByteRange,
    /// SHA-256 of the UTF-8 text represented by the chunk.
    pub chunk_sha256: Sha256Hex,
    /// Optional detected language.
    pub language: Option<Language>,
    /// Inventory class propagated unchanged.
    pub class: FileClass,
    /// Optional symbol associated by an outline.
    pub symbol: Option<SymbolId>,
    /// Capture this chunk was read under.
    pub snapshot: SnapshotId,
    /// Sensitivity decision propagated from the file version.
    pub sensitivity: Sensitivity,
    /// Whether the represented text was transcoded from UTF-16.
    pub transcoded: bool,
    /// Chunking rules that produced the record.
    pub chunking_version: u32,
}

/// Bounded chunk output and an explicit explanation when it is partial.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChunkSet {
    /// Real content chunks, never synthetic marker rows.
    pub chunks: Vec<ChunkRecord>,
    /// Why content remains after the returned records.
    pub truncation: Option<ChunkTruncation>,
    /// Strategy selected for the file.
    pub strategy: ChunkStrategy,
}

/// Strategy that produced a chunk set.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ChunkStrategy {
    /// Tiny whole file.
    WholeFile,
    /// Source outline or source line fallback.
    Source,
    /// Markdown heading sections.
    Markdown,
    /// Top-level configuration sections.
    Configuration,
}

/// A deterministic chunking strategy.
pub trait Chunker: Send + Sync {
    /// Splits one validated file, observing cancellation during bounded work.
    fn chunk(
        &self,
        file: &FileVersion,
        outline: Option<&StructuralOutline>,
        cancellation: &Cancellation,
    ) -> Result<ChunkSet, ChunkError>;
}

/// Structural source chunker with a line-window fallback.
#[derive(Clone, Copy, Debug, Default)]
pub struct SourceChunker;
/// Markdown heading-path chunker.
#[derive(Clone, Copy, Debug, Default)]
pub struct MarkdownChunker;
/// Top-level configuration chunker.
#[derive(Clone, Copy, Debug, Default)]
pub struct ConfigChunker;
/// Whole-file chunker used for tiny files.
#[derive(Clone, Copy, Debug, Default)]
pub struct WholeFileChunker;

/// Selects and runs the deterministic strategy for `file`.
pub fn chunk_file(
    file: &FileVersion,
    outline: Option<&StructuralOutline>,
    cancellation: &Cancellation,
) -> Result<ChunkSet, ChunkError> {
    if cancellation.is_cancelled() {
        return Err(ChunkError::Cancelled);
    }
    let started = std::time::Instant::now();
    let span = tracing::debug_span!(
        target: "harkness_context",
        "context.chunk",
        path = %file.path().display()
    );
    let _entered = span.enter();
    let (strategy, result) = if file.text.len() <= MIN_WHOLE_FILE_BYTES {
        (
            ChunkStrategy::WholeFile,
            WholeFileChunker.chunk(file, None, cancellation),
        )
    } else if is_markdown(file.path()) {
        (
            ChunkStrategy::Markdown,
            MarkdownChunker.chunk(file, None, cancellation),
        )
    } else if matches!(
        file.class(),
        FileClass::Configuration | FileClass::BuildManifest | FileClass::Lockfile
    ) {
        (
            ChunkStrategy::Configuration,
            ConfigChunker.chunk(file, None, cancellation),
        )
    } else {
        (
            ChunkStrategy::Source,
            SourceChunker.chunk(file, outline, cancellation),
        )
    };
    let result = result?;
    tracing::debug!(
        target: "harkness_context",
        path = %file.path().display(),
        strategy = ?strategy,
        chunks = result.chunks.len(),
        truncated = result.truncation.is_some(),
        duration_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
        "chunking complete"
    );
    Ok(result)
}

impl Chunker for WholeFileChunker {
    fn chunk(
        &self,
        file: &FileVersion,
        _outline: Option<&StructuralOutline>,
        cancellation: &Cancellation,
    ) -> Result<ChunkSet, ChunkError> {
        let mut builder = ChunkSetBuilder::new(file, ChunkStrategy::WholeFile, cancellation);
        builder.push_span(0, file.text.len(), Anchor::WholeFile, None)?;
        builder.finish()
    }
}

impl Chunker for SourceChunker {
    fn chunk(
        &self,
        file: &FileVersion,
        outline: Option<&StructuralOutline>,
        cancellation: &Cancellation,
    ) -> Result<ChunkSet, ChunkError> {
        let Some(outline) = outline else {
            return line_window_set(file, ChunkStrategy::Source, cancellation);
        };
        let nodes = validate_outline(file, outline)?;
        if nodes.is_empty() {
            return line_window_set(file, ChunkStrategy::Source, cancellation);
        }

        let mut builder = ChunkSetBuilder::new(file, ChunkStrategy::Source, cancellation);
        if builder.file.language.is_none() {
            builder.language = outline.language.clone();
        }
        let mut cursor = 0;
        let mut gap = 0_u32;
        let mut seen = std::collections::BTreeMap::<Vec<String>, u32>::new();
        for (start, end, node) in nodes {
            if cursor < start {
                builder.push_span(cursor, start, Anchor::LineWindow { index: gap }, None)?;
                gap = gap.saturating_add(1);
            }
            let anchor_path = deduplicated_path(&node.anchor_path, &mut seen);
            builder.push_span(
                start,
                end,
                Anchor::Symbol { path: anchor_path },
                node.symbol.clone(),
            )?;
            cursor = end;
        }
        if cursor < file.text.len() {
            builder.push_span(
                cursor,
                file.text.len(),
                Anchor::LineWindow { index: gap },
                None,
            )?;
        }
        builder.finish()
    }
}

impl Chunker for MarkdownChunker {
    fn chunk(
        &self,
        file: &FileVersion,
        _outline: Option<&StructuralOutline>,
        cancellation: &Cancellation,
    ) -> Result<ChunkSet, ChunkError> {
        let headings = markdown_headings(file.text());
        if headings.is_empty() {
            return line_window_set(file, ChunkStrategy::Markdown, cancellation);
        }
        let mut builder = ChunkSetBuilder::new(file, ChunkStrategy::Markdown, cancellation);
        if headings[0].0 > 0 {
            builder.push_span(0, headings[0].0, Anchor::Heading { path: Vec::new() }, None)?;
        }
        let mut stack: Vec<String> = Vec::new();
        let mut seen = std::collections::BTreeMap::<Vec<String>, u32>::new();
        for (index, (start, level, title)) in headings.iter().enumerate() {
            stack.truncate(level.saturating_sub(1));
            while stack.len() < level.saturating_sub(1) {
                stack.push(String::new());
            }
            stack.push(title.clone());
            let occurrence = seen.entry(stack.clone()).or_default();
            *occurrence = occurrence.saturating_add(1);
            let mut path = stack.clone();
            if *occurrence > 1
                && let Some(last) = path.last_mut()
            {
                last.push_str(" #");
                last.push_str(&occurrence.to_string());
            }
            let end = headings
                .get(index + 1)
                .map_or(file.text.len(), |heading| heading.0);
            builder.push_span(*start, end, Anchor::Heading { path }, None)?;
        }
        builder.finish()
    }
}

impl Chunker for ConfigChunker {
    fn chunk(
        &self,
        file: &FileVersion,
        _outline: Option<&StructuralOutline>,
        cancellation: &Cancellation,
    ) -> Result<ChunkSet, ChunkError> {
        let sections = config_sections(file)?;
        if sections.is_empty() {
            return line_window_set(file, ChunkStrategy::Configuration, cancellation);
        }
        let mut builder = ChunkSetBuilder::new(file, ChunkStrategy::Configuration, cancellation);
        if sections[0].0 > 0 {
            builder.push_span(
                0,
                sections[0].0,
                Anchor::ConfigKey { path: Vec::new() },
                None,
            )?;
        }
        let mut seen = std::collections::BTreeMap::<Vec<String>, u32>::new();
        for (index, (start, path)) in sections.iter().enumerate() {
            let end = sections
                .get(index + 1)
                .map_or(file.text.len(), |section| section.0);
            let path = deduplicated_path(path, &mut seen);
            builder.push_span(*start, end, Anchor::ConfigKey { path }, None)?;
        }
        builder.finish()
    }
}

/// Failures that prevent an honest chunk set.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ChunkError {
    /// Inventory metadata says content must not be chunked.
    #[error("'{path}' has ineligible class {class}")]
    UnsupportedClass {
        /// File path in display form.
        path: String,
        /// Class that forbids chunking.
        class: FileClass,
    },
    /// Bytes no longer have the size the inventory captured.
    #[error("'{path}' changed after inventory: expected {expected} bytes, found {found}")]
    FileChanged {
        /// File path in display form.
        path: String,
        /// Size captured by the inventory.
        expected: u64,
        /// Size of the supplied bytes.
        found: u64,
    },
    /// Complete bytes are neither supported UTF-8 nor valid marked UTF-16.
    #[error("file content uses an unsupported encoding")]
    UnsupportedEncoding,
    /// A structural range is outside, inverted, overlapping, or not on a text boundary.
    #[error("structural outline does not match the file: {reason}")]
    OutlineMismatch {
        /// Stable human-readable mismatch explanation.
        reason: String,
    },
    /// A language identifier was outside its stable grammar.
    #[error("'{value}' is not a valid language identifier")]
    InvalidLanguage {
        /// Rejected identifier spelling.
        value: String,
    },
    /// A platform-sized offset could not be represented in the public range.
    #[error("a chunk offset exceeds the supported range")]
    OffsetOverflow,
    /// A decoded/original mapping did not land on a character boundary.
    #[error("a chunk boundary does not map to the original encoding")]
    EncodingBoundary,
    /// Cancellation was observed before a complete bounded answer existed.
    #[error("chunking was cancelled")]
    Cancelled,
}

impl ChunkError {
    /// Stable caller-facing error namespace.
    pub const KINDS: &'static [&'static str] = &[
        "unsupported_class",
        "file_changed",
        "unsupported_encoding",
        "outline_mismatch",
        "invalid_language",
        "offset_overflow",
        "encoding_boundary",
        "cancelled",
    ];

    /// Stable discriminant for this failure.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::UnsupportedClass { .. } => "unsupported_class",
            Self::FileChanged { .. } => "file_changed",
            Self::UnsupportedEncoding => "unsupported_encoding",
            Self::OutlineMismatch { .. } => "outline_mismatch",
            Self::InvalidLanguage { .. } => "invalid_language",
            Self::OffsetOverflow => "offset_overflow",
            Self::EncodingBoundary => "encoding_boundary",
            Self::Cancelled => "cancelled",
        }
    }
}

struct ChunkSetBuilder<'a> {
    file: &'a FileVersion,
    strategy: ChunkStrategy,
    cancellation: &'a Cancellation,
    line_starts: Vec<usize>,
    chunks: Vec<ChunkRecord>,
    language: Option<Language>,
    truncated: bool,
}

impl<'a> ChunkSetBuilder<'a> {
    fn new(file: &'a FileVersion, strategy: ChunkStrategy, cancellation: &'a Cancellation) -> Self {
        Self {
            file,
            strategy,
            cancellation,
            line_starts: line_starts(file.text()),
            chunks: Vec::new(),
            language: file.language.clone(),
            truncated: false,
        }
    }

    fn push_span(
        &mut self,
        start: usize,
        end: usize,
        anchor: Anchor,
        symbol: Option<SymbolId>,
    ) -> Result<(), ChunkError> {
        if start > end || end > self.file.text.len() {
            return Err(ChunkError::OutlineMismatch {
                reason: format!("range {start}..{end} is outside the decoded content"),
            });
        }
        if start == end {
            return self.push_piece(start, end, anchor, 0, symbol);
        }
        let mut cursor = start;
        let mut ordinal = 0_u32;
        while cursor < end {
            if self.cancellation.is_cancelled() {
                return Err(ChunkError::Cancelled);
            }
            if self.chunks.len() == MAX_CHUNKS_PER_FILE {
                self.truncated = true;
                return Ok(());
            }
            let next = if end.saturating_sub(cursor) <= MAX_CHUNK_BYTES {
                end
            } else {
                let limit = cursor.saturating_add(MAX_CHUNK_BYTES).min(end);
                let target = cursor.saturating_add(TARGET_CHUNK_BYTES).min(limit);
                preferred_boundary(self.file.text(), cursor, target, limit, end)
            };
            self.push_piece(cursor, next, anchor.clone(), ordinal, symbol.clone())?;
            cursor = next;
            ordinal = ordinal.saturating_add(1);
        }
        Ok(())
    }

    fn push_piece(
        &mut self,
        start: usize,
        end: usize,
        anchor: Anchor,
        ordinal: u32,
        symbol: Option<SymbolId>,
    ) -> Result<(), ChunkError> {
        if self.chunks.len() == MAX_CHUNKS_PER_FILE {
            self.truncated = true;
            return Ok(());
        }
        let content = &self.file.text[start..end];
        let chunk_sha256 = Sha256Hex::of(content.as_bytes());
        let id = ChunkId::from_content_digest(
            self.file.path(),
            &anchor.identity_key(ordinal),
            &chunk_sha256,
        );
        let (original_start, original_end) = self.file.original_range(start, end)?;
        let first_line = line_number(&self.line_starts, start)?;
        let last_offset = if end > start { end - 1 } else { start };
        let last_line = line_number(&self.line_starts, last_offset)?;
        self.chunks.push(ChunkRecord {
            id,
            file: self.file.id.clone(),
            anchor,
            ordinal,
            byte_range: ByteRange::new(original_start, original_end)
                .with_lines(first_line, last_line),
            chunk_sha256,
            language: self.language.clone(),
            class: self.file.class,
            symbol,
            snapshot: self.file.snapshot,
            sensitivity: self.file.sensitivity.clone(),
            transcoded: self.file.encoding.is_transcoded(),
            chunking_version: CHUNKING_VERSION,
        });
        Ok(())
    }

    fn finish(self) -> Result<ChunkSet, ChunkError> {
        if self.cancellation.is_cancelled() {
            return Err(ChunkError::Cancelled);
        }
        Ok(ChunkSet {
            chunks: self.chunks,
            truncation: self
                .truncated
                .then_some(ChunkTruncation::ChunkBudgetExhausted {
                    limit: MAX_CHUNKS_PER_FILE,
                }),
            strategy: self.strategy,
        })
    }
}

fn line_window_set(
    file: &FileVersion,
    strategy: ChunkStrategy,
    cancellation: &Cancellation,
) -> Result<ChunkSet, ChunkError> {
    let starts = line_starts(file.text());
    let mut builder = ChunkSetBuilder::new(file, strategy, cancellation);
    if file.text.is_empty() {
        builder.push_span(0, 0, Anchor::WholeFile, None)?;
        return builder.finish();
    }
    let mut line = 0_usize;
    let mut index = 0_u32;
    while line < starts.len() {
        if cancellation.is_cancelled() {
            return Err(ChunkError::Cancelled);
        }
        let start = starts[line];
        if start == file.text.len() {
            break;
        }
        let mut end_line = line + 1;
        while end_line < starts.len()
            && starts[end_line].saturating_sub(start) <= TARGET_CHUNK_BYTES
        {
            end_line += 1;
        }
        let end = starts
            .get(end_line)
            .copied()
            .unwrap_or(file.text.len())
            .min(file.text.len());
        builder.push_span(start, end, Anchor::LineWindow { index }, None)?;
        if end == file.text.len() {
            break;
        }
        let next = end_line.saturating_sub(CHUNK_OVERLAP_LINES).max(line + 1);
        line = next;
        index = index.saturating_add(1);
    }
    builder.finish()
}

fn validate_outline<'a>(
    file: &FileVersion,
    outline: &'a StructuralOutline,
) -> Result<Vec<(usize, usize, &'a OutlineNode)>, ChunkError> {
    let mut nodes = Vec::with_capacity(outline.nodes.len());
    for node in &outline.nodes {
        if node.byte_range.end < node.byte_range.start
            || node.byte_range.end > u64::try_from(file.bytes.len()).unwrap_or(u64::MAX)
        {
            return Err(ChunkError::OutlineMismatch {
                reason: format!(
                    "{} range {}..{} is outside {} bytes",
                    node.kind,
                    node.byte_range.start,
                    node.byte_range.end,
                    file.bytes.len()
                ),
            });
        }
        let start = file.logical_offset(node.byte_range.start).ok_or_else(|| {
            ChunkError::OutlineMismatch {
                reason: format!("{} start is not a character boundary", node.kind),
            }
        })?;
        let end = file.logical_offset(node.byte_range.end).ok_or_else(|| {
            ChunkError::OutlineMismatch {
                reason: format!("{} end is not a character boundary", node.kind),
            }
        })?;
        nodes.push((start, end, node));
    }
    nodes.sort_by_key(|(start, end, _)| (*start, *end));
    for pair in nodes.windows(2) {
        if pair[0].1 > pair[1].0 {
            return Err(ChunkError::OutlineMismatch {
                reason: "outline nodes overlap".to_owned(),
            });
        }
    }
    Ok(nodes)
}

fn decode_text(bytes: &[u8], cancellation: &Cancellation) -> Result<DecodedText, ChunkError> {
    const LE: &[u8] = &[0xff, 0xfe];
    const BE: &[u8] = &[0xfe, 0xff];
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Ok((text.to_owned(), ContentEncoding::Utf8, None));
    }
    let (body, encoding, big_endian) = if let Some(body) = bytes.strip_prefix(LE) {
        (body, ContentEncoding::Utf16Le, false)
    } else if let Some(body) = bytes.strip_prefix(BE) {
        (body, ContentEncoding::Utf16Be, true)
    } else {
        return Err(ChunkError::UnsupportedEncoding);
    };
    if !body.len().is_multiple_of(2) {
        return Err(ChunkError::UnsupportedEncoding);
    }
    let units = body.chunks_exact(2).map(|pair| {
        if big_endian {
            u16::from_be_bytes([pair[0], pair[1]])
        } else {
            u16::from_le_bytes([pair[0], pair[1]])
        }
    });
    let mut text = String::new();
    let mut boundaries = vec![(0, 2)];
    let mut source = 2_usize;
    let mut iter = units.peekable();
    while let Some(first) = iter.next() {
        if cancellation.is_cancelled() {
            return Err(ChunkError::Cancelled);
        }
        let (character, consumed) = if (0xd800..=0xdbff).contains(&first) {
            let Some(second) = iter.next() else {
                return Err(ChunkError::UnsupportedEncoding);
            };
            let mut decoded = char::decode_utf16([first, second]);
            let character = decoded
                .next()
                .and_then(Result::ok)
                .ok_or(ChunkError::UnsupportedEncoding)?;
            if decoded.next().is_some() {
                return Err(ChunkError::UnsupportedEncoding);
            }
            (character, 4)
        } else if (0xdc00..=0xdfff).contains(&first) {
            return Err(ChunkError::UnsupportedEncoding);
        } else {
            (
                char::from_u32(u32::from(first)).ok_or(ChunkError::UnsupportedEncoding)?,
                2,
            )
        };
        if character == '\0' {
            return Err(ChunkError::UnsupportedEncoding);
        }
        text.push(character);
        source += consumed;
        boundaries.push((text.len(), source));
    }
    Ok((text, encoding, Some(boundaries.into())))
}

fn preferred_boundary(text: &str, start: usize, target: usize, limit: usize, end: usize) -> usize {
    if limit == end {
        return end;
    }
    let mut candidate = target;
    while candidate > start && !text.is_char_boundary(candidate) {
        candidate -= 1;
    }
    if let Some(newline) = text[start..candidate].rfind('\n') {
        return start + newline + 1;
    }
    candidate = limit;
    while candidate > start && !text.is_char_boundary(candidate) {
        candidate -= 1;
    }
    candidate.max(start + text[start..].chars().next().map_or(1, char::len_utf8))
}

fn markdown_headings(text: &str) -> Vec<(usize, usize, String)> {
    let mut headings = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let body = line
            .strip_suffix('\n')
            .unwrap_or(line)
            .strip_suffix('\r')
            .unwrap_or(line.strip_suffix('\n').unwrap_or(line));
        let hashes = body.bytes().take_while(|byte| *byte == b'#').count();
        if (1..=6).contains(&hashes)
            && body
                .as_bytes()
                .get(hashes)
                .is_some_and(u8::is_ascii_whitespace)
        {
            let title = body[hashes..].trim().trim_end_matches('#').trim();
            headings.push((offset, hashes, title.to_owned()));
        }
        offset += line.len();
    }
    headings
}

fn config_sections(file: &FileVersion) -> Result<Vec<(usize, Vec<String>)>, ChunkError> {
    let name = file.path().as_bytes();
    let json = name.ends_with(b".json") || name.ends_with(b".jsonc");
    if json && serde_json::from_str::<serde_json::Value>(file.text()).is_err() {
        return Ok(Vec::new());
    }
    let mut sections = Vec::new();
    let mut offset = 0;
    let mut json_depth = 0_i32;
    let mut table_path = Vec::<String>::new();
    for line in file.text().split_inclusive('\n') {
        let body = line.trim();
        let anchor = if (body.starts_with("[[") && body.ends_with("]]"))
            || (body.starts_with('[') && body.ends_with(']'))
        {
            table_path = body
                .trim_matches(['[', ']'])
                .split('.')
                .map(|part| part.trim().trim_matches('"').to_owned())
                .collect();
            Some(table_path.clone())
        } else if json && json_depth == 1 {
            body.strip_prefix('"').and_then(|tail| {
                let end = tail.find('"')?;
                tail[end + 1..]
                    .trim_start()
                    .starts_with(':')
                    .then(|| vec![tail[..end].to_owned()])
            })
        } else if !json
            && line
                .as_bytes()
                .first()
                .is_some_and(|byte| !byte.is_ascii_whitespace())
        {
            body.split_once([':', '=']).and_then(|(key, _)| {
                let key = key.trim();
                (!key.is_empty() && !key.starts_with(['#', ';'])).then(|| {
                    let mut path = table_path.clone();
                    path.push(key.trim_matches('"').to_owned());
                    path
                })
            })
        } else {
            None
        };
        if let Some(path) = anchor {
            sections.push((offset, path));
        }
        if json {
            json_depth += json_brace_delta(line);
        }
        offset += line.len();
    }
    sections.dedup_by(|left, right| left.0 == right.0);
    Ok(sections)
}

fn json_brace_delta(line: &str) -> i32 {
    let mut delta = 0;
    let mut quoted = false;
    let mut escaped = false;
    for character in line.chars() {
        if quoted {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                quoted = false;
            }
        } else if character == '"' {
            quoted = true;
        } else if character == '{' || character == '[' {
            delta += 1;
        } else if character == '}' || character == ']' {
            delta -= 1;
        }
    }
    delta
}

fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(
        text.match_indices('\n')
            .map(|(index, _)| index + 1)
            .filter(|start| *start < text.len()),
    );
    starts
}

fn line_number(starts: &[usize], offset: usize) -> Result<u32, ChunkError> {
    let zero_based = starts
        .partition_point(|start| *start <= offset)
        .saturating_sub(1);
    u32::try_from(zero_based + 1).map_err(|_| ChunkError::OffsetOverflow)
}

fn boundary_lookup(boundaries: &[(usize, usize)], logical: usize) -> Option<usize> {
    boundaries
        .binary_search_by_key(&logical, |(decoded, _)| *decoded)
        .ok()
        .map(|index| boundaries[index].1)
}

fn encode_path(key: &mut String, kind: &str, path: &[String]) {
    use fmt::Write as _;
    key.push_str(kind);
    key.push(';');
    let _ = write!(key, "{};", path.len());
    for part in path {
        let _ = write!(key, "{}:", part.len());
        for byte in part.as_bytes() {
            let _ = write!(key, "{byte:02x}");
        }
        key.push(';');
    }
}

fn deduplicated_path(
    path: &[String],
    seen: &mut std::collections::BTreeMap<Vec<String>, u32>,
) -> Vec<String> {
    let occurrence = seen.entry(path.to_vec()).or_default();
    *occurrence = occurrence.saturating_add(1);
    let mut unique = path.to_vec();
    if *occurrence > 1 {
        if let Some(last) = unique.last_mut() {
            last.push_str(" #");
            last.push_str(&occurrence.to_string());
        } else {
            unique.push(format!("#{occurrence}"));
        }
    }
    unique
}

fn is_markdown(path: &RepoPath) -> bool {
    let bytes = path.as_bytes();
    bytes.ends_with(b".md") || bytes.ends_with(b".markdown")
}

fn to_u64(value: usize) -> Result<u64, ChunkError> {
    u64::try_from(value).map_err(|_| ChunkError::OffsetOverflow)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use harkness_git::Cancellation;

    use super::*;
    use crate::InventoryEntry;

    fn entry(path: &str, bytes: &[u8], class: FileClass) -> InventoryEntry {
        InventoryEntry {
            path: RepoPath::from_bytes(path.as_bytes().to_vec()),
            byte_size: bytes.len() as u64,
            mtime_ns: None,
            class,
            symlink: false,
            boundary: None,
            unreadable: false,
        }
    }

    fn file(path: &str, text: &str, class: FileClass) -> FileVersion {
        let bytes: Arc<[u8]> = Arc::from(text.as_bytes());
        FileVersion::new(
            &entry(path, &bytes, class),
            SnapshotId::new(),
            bytes,
            &Cancellation::default(),
        )
        .unwrap()
    }

    #[test]
    fn error_kinds_are_exact_and_unique() {
        let mut kinds = ChunkError::KINDS.to_vec();
        kinds.sort_unstable();
        kinds.dedup();
        assert_eq!(kinds.len(), ChunkError::KINDS.len());
        let errors = [
            ChunkError::UnsupportedClass {
                path: "x".into(),
                class: FileClass::Binary,
            },
            ChunkError::FileChanged {
                path: "x".into(),
                expected: 1,
                found: 2,
            },
            ChunkError::UnsupportedEncoding,
            ChunkError::OutlineMismatch { reason: "x".into() },
            ChunkError::InvalidLanguage { value: "X".into() },
            ChunkError::OffsetOverflow,
            ChunkError::EncodingBoundary,
            ChunkError::Cancelled,
        ];
        assert_eq!(
            errors.iter().map(ChunkError::kind).collect::<Vec<_>>(),
            ChunkError::KINDS
        );
    }

    #[test]
    fn identity_vectors_are_frozen() {
        let file = file("src/lib.rs", "fn main() {}\n", FileClass::Source);
        let set = chunk_file(&file, None, &Cancellation::default()).unwrap();
        assert_eq!(
            file.id().to_string(),
            "sha256:d5f83c2d2d59cae839dd2dc42f089b7980f0a10820e1b501365b88ae7852f87f"
        );
        assert_eq!(
            set.chunks[0].id.to_string(),
            "sha256:d402b1cf70490781d2fe043a72de23ab30060e1914b72d95f150be084ea71da2"
        );
    }

    #[test]
    fn structural_ids_survive_an_edit_above_them() {
        let a_before = format!("fn a() {{ /* {} */ 1 }}\n", "a".repeat(700));
        let a_after = format!("fn a() {{ /* {} */ 9 }}\n", "a".repeat(700));
        let b = format!("fn b() {{ /* {} */ 2 }}\n", "b".repeat(700));
        let c = format!("fn c() {{ /* {} */ 3 }}\n", "c".repeat(700));
        let before = format!("{a_before}{b}{c}");
        let after = format!("// inserted\n{a_after}{b}{c}");
        let outline = |text: &str| StructuralOutline {
            nodes: ["a", "b", "c"]
                .into_iter()
                .map(|name| {
                    let start = text.find(&format!("fn {name}")).unwrap();
                    let end = text[start..]
                        .find('\n')
                        .map_or(text.len(), |at| start + at + 1);
                    OutlineNode {
                        anchor_path: vec![format!("fn {name}")],
                        byte_range: start as u64..end as u64,
                        kind: "function".into(),
                        symbol: None,
                    }
                })
                .collect(),
            language: Language::new("rust").ok(),
        };
        let before_file = file("src/lib.rs", &before, FileClass::Source);
        let after_file = file("src/lib.rs", &after, FileClass::Source);
        let before_set = chunk_file(
            &before_file,
            Some(&outline(&before)),
            &Cancellation::default(),
        )
        .unwrap();
        let after_set = chunk_file(
            &after_file,
            Some(&outline(&after)),
            &Cancellation::default(),
        )
        .unwrap();
        fn structural<'a>(set: &'a ChunkSet, name: &str) -> &'a ChunkRecord {
            set.chunks
                .iter()
                .find(|chunk| {
                    chunk.anchor
                        == Anchor::Symbol {
                            path: vec![format!("fn {name}")],
                        }
                })
                .unwrap()
        }
        assert_eq!(
            structural(&before_set, "b").id,
            structural(&after_set, "b").id
        );
        assert_eq!(
            structural(&before_set, "c").id,
            structural(&after_set, "c").id
        );
        assert_ne!(
            structural(&before_set, "b").byte_range.start,
            structural(&after_set, "b").byte_range.start
        );
        assert_ne!(
            structural(&before_set, "a").id,
            structural(&after_set, "a").id
        );
        assert_eq!(
            structural(&after_set, "b")
                .language
                .as_ref()
                .map(Language::as_str),
            Some("rust")
        );
    }

    #[test]
    fn renaming_an_anchor_or_file_changes_the_identity() {
        let text = format!(
            "fn stable() {{ /* {} */ }}\n",
            "x".repeat(MIN_WHOLE_FILE_BYTES)
        );
        let source = file("src/lib.rs", &text, FileClass::Source);
        let moved = file("src/moved.rs", &text, FileClass::Source);
        let outline = |name: &str| StructuralOutline {
            nodes: vec![OutlineNode {
                anchor_path: vec![name.to_owned()],
                byte_range: 0..text.len() as u64,
                kind: "function".into(),
                symbol: None,
            }],
            language: None,
        };
        let original = SourceChunker
            .chunk(
                &source,
                Some(&outline("fn stable")),
                &Cancellation::default(),
            )
            .unwrap();
        let renamed = SourceChunker
            .chunk(
                &source,
                Some(&outline("fn renamed")),
                &Cancellation::default(),
            )
            .unwrap();
        let moved = SourceChunker
            .chunk(
                &moved,
                Some(&outline("fn stable")),
                &Cancellation::default(),
            )
            .unwrap();
        assert_ne!(original.chunks[0].id, renamed.chunks[0].id);
        assert_ne!(original.chunks[0].id, moved.chunks[0].id);
        assert_ne!(original.chunks[0].file, moved.chunks[0].file);
    }

    #[test]
    fn markdown_records_preamble_nested_and_duplicate_headings() {
        let text = format!(
            "preamble {}\n# A\none\n## B\ntwo\n## B\nthree\n",
            "p".repeat(MIN_WHOLE_FILE_BYTES)
        );
        let set = chunk_file(
            &file("README.md", &text, FileClass::Documentation),
            None,
            &Cancellation::default(),
        )
        .unwrap();
        let anchors = set
            .chunks
            .iter()
            .map(|chunk| chunk.anchor.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            anchors,
            vec![
                Anchor::Heading { path: vec![] },
                Anchor::Heading {
                    path: vec!["A".into()]
                },
                Anchor::Heading {
                    path: vec!["A".into(), "B".into()]
                },
                Anchor::Heading {
                    path: vec!["A".into(), "B #2".into()]
                },
            ]
        );
    }

    #[test]
    fn malformed_json_falls_back_to_line_windows() {
        let text = format!("{{\n  \"a\": \"{}\"\n", "x".repeat(MIN_WHOLE_FILE_BYTES));
        let set = chunk_file(
            &file("x.json", &text, FileClass::Configuration),
            None,
            &Cancellation::default(),
        )
        .unwrap();
        assert!(
            set.chunks
                .iter()
                .all(|chunk| matches!(chunk.anchor, Anchor::LineWindow { .. }))
        );
    }

    #[test]
    fn configuration_sections_use_top_level_anchors() {
        let text = format!(
            "preamble = true\n[workspace]\nmembers = [\"{}\"]\n[dependencies]\nserde = \"1\"\n",
            "member".repeat(400)
        );
        let set = chunk_file(
            &file("Cargo.toml", &text, FileClass::BuildManifest),
            None,
            &Cancellation::default(),
        )
        .unwrap();
        assert_eq!(set.strategy, ChunkStrategy::Configuration);
        assert_eq!(
            set.chunks
                .iter()
                .map(|chunk| chunk.anchor.clone())
                .collect::<Vec<_>>(),
            vec![
                Anchor::ConfigKey {
                    path: vec!["preamble".into()]
                },
                Anchor::ConfigKey {
                    path: vec!["workspace".into()]
                },
                Anchor::ConfigKey {
                    path: vec!["workspace".into(), "members".into()]
                },
                Anchor::ConfigKey {
                    path: vec!["dependencies".into()]
                },
                Anchor::ConfigKey {
                    path: vec!["dependencies".into(), "serde".into()]
                },
            ]
        );
    }

    #[test]
    fn utf8_boundaries_and_utf16_original_ranges_are_honest() {
        let text = format!("{}é\n", "a".repeat(TARGET_CHUNK_BYTES));
        let utf8 = file("src/lib.rs", &text, FileClass::Source);
        let chunks = chunk_file(&utf8, None, &Cancellation::default())
            .unwrap()
            .chunks;
        for chunk in chunks {
            let start = chunk.byte_range.start as usize;
            let end = chunk.byte_range.end as usize;
            assert!(text.is_char_boundary(start) && text.is_char_boundary(end));
            assert!(std::str::from_utf8(&utf8.bytes()[start..end]).is_ok());
        }

        let mut bytes = vec![0xff, 0xfe];
        for unit in "hello é\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let bytes: Arc<[u8]> = bytes.into();
        let version = FileVersion::new(
            &entry("notes.txt", &bytes, FileClass::Documentation),
            SnapshotId::new(),
            bytes.clone(),
            &Cancellation::default(),
        )
        .unwrap();
        let chunk = chunk_file(&version, None, &Cancellation::default())
            .unwrap()
            .chunks
            .remove(0);
        assert!(chunk.transcoded);
        assert_eq!(chunk.byte_range.start, 0);
        assert_eq!(chunk.byte_range.end, bytes.len() as u64);
        assert_eq!(chunk.chunk_sha256, Sha256Hex::of("hello é\n"));
    }

    #[test]
    fn invalid_or_ineligible_content_is_refused() {
        let bad: Arc<[u8]> = Arc::from([0xff, 0x00, 0x01].as_slice());
        assert_eq!(
            FileVersion::new(
                &entry("x.txt", &bad, FileClass::Documentation),
                SnapshotId::new(),
                bad,
                &Cancellation::default()
            )
            .unwrap_err()
            .kind(),
            "unsupported_encoding"
        );
        let binary: Arc<[u8]> = Arc::from([0_u8].as_slice());
        assert_eq!(
            FileVersion::new(
                &entry("x.bin", &binary, FileClass::Binary),
                SnapshotId::new(),
                binary,
                &Cancellation::default()
            )
            .unwrap_err()
            .kind(),
            "unsupported_class"
        );
    }

    #[test]
    fn a_zero_byte_file_is_one_stable_empty_chunk() {
        let set = chunk_file(
            &file("empty.txt", "", FileClass::UnknownText),
            None,
            &Cancellation::default(),
        )
        .unwrap();
        assert_eq!(set.chunks.len(), 1);
        assert!(set.chunks[0].byte_range.is_empty());
    }

    #[test]
    fn one_huge_line_stays_bounded_and_reports_the_chunk_budget() {
        let text = "é".repeat(10 * 1024 * 1024);
        let set = SourceChunker
            .chunk(
                &file("src/huge.rs", &text, FileClass::Source),
                None,
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(set.chunks.len(), MAX_CHUNKS_PER_FILE);
        assert_eq!(
            set.truncation,
            Some(ChunkTruncation::ChunkBudgetExhausted {
                limit: MAX_CHUNKS_PER_FILE
            })
        );
        assert!(
            set.chunks
                .iter()
                .all(|chunk| chunk.byte_range.len() <= MAX_CHUNK_BYTES as u64)
        );
    }

    #[test]
    fn outline_mismatch_and_cancellation_return_no_partial_answer() {
        let version = file(
            "src/lib.rs",
            &"x".repeat(MIN_WHOLE_FILE_BYTES + 1),
            FileClass::Source,
        );
        let outline = StructuralOutline {
            nodes: vec![OutlineNode {
                anchor_path: vec!["x".into()],
                byte_range: 0..999_999,
                kind: "function".into(),
                symbol: None,
            }],
            language: None,
        };
        assert_eq!(
            SourceChunker
                .chunk(&version, Some(&outline), &Cancellation::default())
                .unwrap_err()
                .kind(),
            "outline_mismatch"
        );
        let cancellation = Cancellation::default();
        cancellation.cancel();
        assert_eq!(
            chunk_file(&version, None, &cancellation)
                .unwrap_err()
                .kind(),
            "cancelled"
        );
    }

    #[test]
    fn duplicate_structural_anchors_never_produce_duplicate_ids() {
        let body = format!("fn same() {{ /* {} */ }}\n", "x".repeat(1100));
        let text = format!("{body}{body}");
        let outline = StructuralOutline {
            nodes: vec![
                OutlineNode {
                    anchor_path: vec!["fn same".into()],
                    byte_range: 0..body.len() as u64,
                    kind: "function".into(),
                    symbol: None,
                },
                OutlineNode {
                    anchor_path: vec!["fn same".into()],
                    byte_range: body.len() as u64..text.len() as u64,
                    kind: "function".into(),
                    symbol: None,
                },
            ],
            language: None,
        };
        let set = SourceChunker
            .chunk(
                &file("src/lib.rs", &text, FileClass::Source),
                Some(&outline),
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(set.chunks.len(), 2);
        assert_ne!(set.chunks[0].id, set.chunks[1].id);
        assert_eq!(
            set.chunks[1].anchor,
            Anchor::Symbol {
                path: vec!["fn same #2".into()]
            }
        );
    }

    #[test]
    fn seven_thousand_line_source_is_deterministic_with_or_without_an_outline() {
        let mut text = String::new();
        let mut nodes = Vec::new();
        for item in 0..70 {
            let start = text.len();
            for line in 0..100 {
                text.push_str(&format!("let item_{item}_{line} = {line};\n"));
            }
            nodes.push(OutlineNode {
                anchor_path: vec![format!("item {item}")],
                byte_range: start as u64..text.len() as u64,
                kind: "item".into(),
                symbol: None,
            });
        }
        assert_eq!(text.lines().count(), 7_000);
        let version = file("src/large.rs", &text, FileClass::Source);
        let outline = StructuralOutline {
            nodes,
            language: Language::new("rust").ok(),
        };
        let structural = SourceChunker
            .chunk(&version, Some(&outline), &Cancellation::default())
            .unwrap();
        let structural_again = SourceChunker
            .chunk(&version, Some(&outline), &Cancellation::default())
            .unwrap();
        let fallback = SourceChunker
            .chunk(&version, None, &Cancellation::default())
            .unwrap();
        assert_eq!(structural, structural_again);
        assert_eq!(structural.chunks.len(), 70);
        assert!(structural.truncation.is_none());
        assert!(fallback.truncation.is_none());
        assert!(
            structural
                .chunks
                .iter()
                .all(|chunk| matches!(chunk.anchor, Anchor::Symbol { .. }))
        );
        assert!(
            fallback
                .chunks
                .iter()
                .all(|chunk| matches!(chunk.anchor, Anchor::LineWindow { .. }))
        );
    }

    #[test]
    fn generated_multibyte_inputs_are_deterministic_sorted_and_unique() {
        for seed in 0..64 {
            let mut text = String::new();
            for index in 0..(seed * 13 + 1) {
                text.push(if index % 7 == 0 {
                    '界'
                } else if index % 5 == 0 {
                    'é'
                } else {
                    'a'
                });
                if index % 31 == 0 {
                    text.push('\n');
                }
            }
            while text.len() <= MIN_WHOLE_FILE_BYTES {
                text.push('x');
            }
            let version = file("src/generated.rs", &text, FileClass::Source);
            let left = chunk_file(&version, None, &Cancellation::default()).unwrap();
            let right = chunk_file(&version, None, &Cancellation::default()).unwrap();
            assert_eq!(left, right);
            let mut ids = left
                .chunks
                .iter()
                .map(|chunk| chunk.id.clone())
                .collect::<Vec<_>>();
            let count = ids.len();
            ids.sort();
            ids.dedup();
            assert_eq!(ids.len(), count);
            assert!(
                left.chunks
                    .windows(2)
                    .all(|pair| pair[0].byte_range.start <= pair[1].byte_range.start)
            );
        }
    }

    #[test]
    #[ignore = "latency target; meaningful only in a release build"]
    fn chunking_one_megabyte_meets_the_latency_target() {
        let mut text = String::with_capacity(1024 * 1024);
        while text.len() < 1024 * 1024 {
            text.push_str("fn generated() { let value = 1; }\n");
        }
        let version = file("src/generated.rs", &text, FileClass::Source);
        let started = std::time::Instant::now();
        let result = SourceChunker
            .chunk(&version, None, &Cancellation::default())
            .unwrap();
        let elapsed = started.elapsed();
        assert!(!result.chunks.is_empty());
        assert!(result.truncation.is_none());
        harkness_test_fixtures::latency::record(
            "context::chunk_one_megabyte",
            elapsed,
            std::time::Duration::from_millis(20),
        );
    }
}
