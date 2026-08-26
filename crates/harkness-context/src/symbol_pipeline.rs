//! Shared extraction-to-chunking policy for cold builds and reconciliation.

use harkness_git::Cancellation;

use crate::chunk::{ChunkError, ChunkSet, FileVersion, chunk_file};
use crate::path::RepoPath;
use crate::symbols::{ExtractionSkipReason, FileSymbols, ParseHealth, SymbolSource};

/// Extracts symbols from the decoded text that chunking sees.
///
/// Tree-sitter byte offsets describe the input passed to it. UTF-16 input is
/// decoded by [`FileVersion`], so parsing those UTF-8 bytes would produce ranges
/// in a different coordinate space from the original file. Until adapters can
/// translate every range back, the honest answer is a named skip.
pub(crate) fn extract_file_symbols(
    source: &dyn SymbolSource,
    path: &RepoPath,
    version: &FileVersion,
    cancellation: &Cancellation,
) -> FileSymbols {
    if version.encoding().is_transcoded() {
        return source.skipped(
            path,
            version.text().as_bytes(),
            ExtractionSkipReason::TranscodedInput,
        );
    }

    source.extract(path, version.text().as_bytes(), cancellation)
}

/// Chunks a file from a complete, validated outline or its line fallback.
///
/// Structural extraction is advisory. A malformed adapter result must not make
/// readable source disappear from the index, and failed/skipped extraction has
/// no complete outline to offer.
pub(crate) fn chunk_with_symbol_outline(
    version: &FileVersion,
    extracted: &FileSymbols,
    cancellation: &Cancellation,
) -> Result<ChunkSet, ChunkError> {
    let structurally_usable = matches!(
        extracted.health,
        ParseHealth::Complete | ParseHealth::Partial { .. }
    );
    let outline =
        (structurally_usable && !extracted.outline.nodes.is_empty()).then_some(&extracted.outline);

    match chunk_file(version, outline, cancellation) {
        Err(ChunkError::OutlineMismatch { .. }) if outline.is_some() => {
            chunk_file(version, None, cancellation)
        }
        result => result,
    }
}
