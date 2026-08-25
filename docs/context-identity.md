# Stable file and chunk identity

The context engine names repository content without treating a line number as
an identity. A file version says which exact bytes existed at one exact
repository-relative path. A chunk says which bounded UTF-8 text belonged under
one structural anchor. Byte and line ranges remain location hints: inserting a
line above an unchanged function moves those hints and does not rename the
function's chunk.

The implementation is `crates/harkness-context/src/chunk.rs`. The workspace
identity containing the snapshot named by each chunk is defined separately in
ADR-0008.

## Derivation

Every digest uses `DigestWriter`: the domain is the first field, and every field
is encoded as an eight-byte little-endian length followed by its bytes. Paths
use `RepoPath::as_bytes`, never a lossy display spelling.

`FileVersionId` is derived from:

1. `harkness.context.file_version.v1`
2. exact repository-relative path bytes
3. lowercase hexadecimal SHA-256 of the original file bytes

`ChunkId` is derived from:

1. `harkness.context.chunk.v1`
2. exact repository-relative path bytes
3. the canonical v1 anchor key, including the continuation ordinal
4. lowercase hexadecimal SHA-256 of the chunk's represented UTF-8 text

The canonical anchor key starts with its frozen encoding version, names the
anchor kind, length-frames every structural path component in lowercase
hexadecimal, and ends with the continuation ordinal. It is unambiguous even
when repository content contains separators or non-ASCII text. Fixed
hexadecimal vectors in the module's tests freeze both derivations.

Changing an identity domain or the canonical anchor encoding is a breaking
identity change. It requires a new domain/version and new fixtures, never an
edit that silently changes what an existing identifier means.

## Stability and its limits

A structural `ChunkId` changes when its own represented text changes, its
anchor changes, its continuation ordinal changes, or its file path changes. It
does not change merely because the byte or line range moved. Separate structural
anchors are never merged: merging two small functions would make changing one
invalidate the other.

Markdown headings use their case-preserved heading path. Repeated paths receive
a deterministic ` #N` suffix. Configuration files use conservative top-level
table or key paths. A malformed or ambiguous configuration file falls back to
line windows rather than claiming structure the heuristic did not establish.

`LineWindow` is an honest weaker identity. Its index and represented bytes both
participate, so an edit above a window can move or replace it. Consumers must not
present fallback windows as symbol-stable provenance.

## Boundaries and encoding

`ByteRange` always addresses the original file bytes and is half-open. Its line
hints are one-based display metadata. Chunk hashes cover the UTF-8 text a model
would receive:

- UTF-8 boundaries are character boundaries in the original bytes.
- Marked, valid UTF-16 is transcoded; records retain original-byte ranges and
  set `transcoded`.
- An unsupported encoding or an inventory entry that is not eligible produces
  a typed refusal and no chunk record. Inventory metadata is already the honest
  representation of excluded content.

No content is stored in a `ChunkRecord`; retrieval re-reads the file and checks
the recorded file identity before materializing text.

## Strategies and bounds

Files at or below 2 KiB use one whole-file anchor. Source outlines use
non-overlapping parser-projected nodes and line-window chunks for uncovered
gaps. Without an outline, source uses approximately 4 KiB line windows with an
overlap of up to eight lines. The overlap never takes more than a quarter of the
window it trails, so a file of long lines advances by whole windows rather than
one line at a time; a flat line count would repeat such a file many times over
and exhaust the chunk budget long before its end. Markdown uses ATX heading
sections. TOML, YAML, and JSON use top-level tables or keys where conservative
recognition succeeds.

No represented chunk exceeds 16 KiB. A long line falls back to character-safe
byte splitting, and a long structural node becomes ordinal continuations under
the same anchor. At most 512 real chunks are returned. If content remains,
`ChunkSet.truncation` reports `ChunkBudgetExhausted`; no synthetic empty chunk is
inserted into the content-addressed records.

`CHUNKING_VERSION` covers strategy selection and boundary rules. A change to
either increments it so the disposable index can invalidate chunk-derived rows
while leaving run evidence untouched. It is recorded on each chunk but excluded
from `ChunkId`: bumping cache policy must not rename an otherwise identical
path, anchor, ordinal, and content hash. Changing the anchor encoding itself is
an identity-format migration and requires a new frozen anchor encoding version
and identifier domain.

## What proves this

| Contract | Package | Test |
| --- | --- | --- |
| Fixed file and chunk identities | `harkness-context` | `chunk::tests::identity_vectors_are_frozen` |
| Structural edit locality | `harkness-context` | `chunk::tests::structural_ids_survive_an_edit_above_them` |
| UTF-8 and UTF-16 ranges | `harkness-context` | `chunk::tests::utf8_boundaries_and_utf16_original_ranges_are_honest` |
| Bounds and huge-line backstop | `harkness-context` | `chunk::tests::one_huge_line_stays_bounded_and_reports_the_chunk_budget` |
