# Language detection and structural symbols

Harkness extracts a bounded structural inventory from eligible repository
files. This is syntax, not semantics: a symbol says “the Rust grammar parsed a
method declaration with these bytes and this structural parent.” It does not
say which overload a call resolves to, whether the file compiles, or where a
name mention is defined. Type resolution, cross-file go-to-definition, rename,
and call graphs remain future LSP-backed capabilities behind `SymbolSource`.

The implementation is `crates/harkness-context/src/symbols/`. Tree-sitter types
do not leave that module. Chunking, the cache, queries, and later ranking depend
only on `Symbol`, `SymbolReference`, `ParseHealth`, and `StructuralOutline`.

## Detection

`detect_language` applies the first signal that answers:

1. repository-relative filename or extension;
2. an interpreter shebang on the first line;
3. a bounded content heuristic over at most the first 4 KiB.

The result always records both the optional `Language` and the winning
`LanguageDetectionSource`. Detection covers common inventory languages even
when extraction does not: a Python file is detected as Python and gets
`Skipped { reason: UnsupportedLanguage }`, which is observably different from a
supported file that parsed successfully and happened to contain no symbols.

Rust, TOML, and Markdown are the initial extraction adapters. Extension wins
over contradictory content deliberately; guessing around a known filename
would make the same bytes parse differently after an unrelated edit.

## Symbol and health contract

A declaration records its stable `SymbolId`, typed kind, bare and qualified
names, duplicate ordinal, byte-exact definition range, structural parent,
bounded signature line, test flag, and whether its stored name is a lossy view
of invalid identifier bytes. Signatures are capped at 512 bytes. Symbol and
reference counts are capped per file.

`SymbolId` follows the landed identity contract: exact path bytes, language,
qualified name, and kind are domain-separated and length-framed. Content does
not participate, so editing one function body leaves every declaration ID
unchanged. The ordinary duplicate ordinal is zero and preserves that derivation;
only a residual duplicate qualified declaration adds a private
`#duplicate:<ordinal>` suffix to the qualified identity input. The displayed
qualified name never includes it.

Parse health is one of:

- `Complete`: no tree-sitter error or missing node;
- `Partial { error_ranges }`: error nodes were present, while symbols outside
  them remain usable;
- `Failed { reason }`: the adapter could not produce a structural answer or
  panicked;
- `Skipped { reason }`: the language is unknown or has no registered adapter.

An adapter panic is caught around one file and becomes `adapter_panicked`.
Indexing continues with the next file. Unsupported files stay in the inventory
and lexical index; they are not mislabeled as successfully parsed empty files.

## Indexing and invalidation

Extraction runs only after the inventory admitted the file. Built-in denied
paths never become inventory entries, and ineligible `SecretSensitive`,
`Binary`, `Oversized`, symlink, boundary, and unreadable entries are never
parsed. The file's detected language is attached to its exact `FileVersionId`.

For supported source, the adapter projects the leaf declarations into a
non-overlapping `StructuralOutline` before chunking. Chunks therefore align to
symbol ranges and carry the associated `SymbolId`; an unavailable or unsupported
adapter falls back to the existing line-window behavior.

The disposable index schema is version 4. It adds typed symbol metadata,
`symbol_references`, `parse_health`, and `parser_versions`. The shared parser
component version covers detection and identity projection. Grammar versions
are separate rows keyed by language. When one changes, Harkness deletes only
that language's derived rows and nulls only those file versions' parser marker;
the reconciler then re-extracts those files. Other languages' rows survive
byte-for-byte.

Lookups are worktree-scoped and never query a content table without joining
through that worktree's visible file rows. Exact bare-name and qualified-suffix
answers order by qualified name bytes, path bytes, then start offset. Per-file
listings order by start offset, then qualified name. Every answer is bounded and
reports `more`; an unindexed worktree is `index_unavailable`, not an empty
success.

## Adding a language adapter

1. Add the grammar crate only to `harkness-context` through the root workspace
   dependency table.
2. Implement `LanguageAdapter` inside `symbols/`; do not export tree-sitter
   nodes, queries, parsers, or grammar types.
3. Compile every query in the adapter constructor and register the adapter in
   `LanguageRegistry::built_in`. A malformed query must fail engine startup as
   `symbol_adapter_unavailable`, never wait for the first matching file.
4. Give the adapter a language-local `grammar_version`. Bump it for a grammar
   or language-specific extraction change. Bump `SYMBOL_EXTRACTION_VERSION`
   only for detection, identity projection, or a shared contract change that
   really invalidates every language.
5. Add a versioned source fixture and frozen expected inventory covering kinds,
   qualified names, exact ranges, parents, test flags, partial parsing, and ID
   stability. Add a language-local invalidation test beside the store tests.
6. Measure release extraction throughput and warm lookup latency. A file at or
   below 256 KiB must extract in under 200 ms, aggregate extraction must sustain
   at least 5 MiB/s, and warm lookup p95 must remain under 100 ms on the medium
   profile.

## Recorded release benchmark

On 2026-08-26, the ignored release tests ran on x86-64 Fedora Linux
7.1.10-200.fc44 with rustc 1.97.1 and LLVM 22.1.6. The 256 KiB Rust profile
(548 declarations) extracted in 6.35 ms, or 39.38 MiB/s. One hundred warm exact
lookups over 2,500 declarations measured 0.056 ms at p95. The committed tests
assert the 200 ms, 5 MiB/s, and 100 ms limits in release builds:

```text
cargo test -p harkness-context --release rust_extraction_meets_the_single_file_throughput_target -- --ignored --nocapture
cargo test -p harkness-context --release warm_symbol_lookup_meets_the_latency_target -- --ignored --nocapture
```

## What proves this

| Guarantee | Package | Test |
| --- | --- | --- |
| Detection applies extension, then shebang, then heuristic | `harkness-context` | `symbols::tests::detection_precedence_is_extension_then_shebang_then_heuristic` |
| The versioned Rust fixture has exact kinds, names, ranges and parents | `harkness-context` | `symbols::tests::the_versioned_rust_fixture_has_an_exact_symbol_inventory` |
| Syntax errors remain visible while surrounding declarations survive | `harkness-context` | `symbols::tests::syntax_errors_are_partial_and_do_not_hide_surrounding_symbols` |
| Unsupported detection differs from a supported empty extraction | `harkness-context` | `symbols::tests::unsupported_language_is_not_an_extracted_empty_file` |
| Adapter panic is contained to one file | `harkness-context` | `symbols::tests::adapter_panic_degrades_exactly_one_file` |
| Unrelated body edits preserve other declaration IDs | `harkness-context` | `symbols::tests::unrelated_body_edits_preserve_other_symbol_ids` |
| Rust, TOML and Markdown all register through adapters | `harkness-context` | `symbols::tests::toml_and_markdown_are_adapters_not_core_special_cases` |
| Cold indexing persists symbols, health, lookup, and chunk association | `harkness-context` | `engine::tests::reindexing_extracts_queries_and_explains_symbol_health` |
| A grammar bump invalidates only that language | `harkness-context` | `index::store_tests::a_grammar_bump_invalidates_only_that_languages_rows` |

## Where to read next

- [`docs/context-inventory.md`](context-inventory.md) — the eligibility and
  denial boundary every adapter receives.
- [`docs/context-identity.md`](context-identity.md) — file, symbol, and chunk
  identity derivation.
- [`docs/context-index.md`](context-index.md) — disposable storage, worktree
  isolation, batches, and component invalidation.
- [`docs/context-search.md`](context-search.md) — the lexical retrieval surface
  that remains available when symbol extraction is skipped or partial.
