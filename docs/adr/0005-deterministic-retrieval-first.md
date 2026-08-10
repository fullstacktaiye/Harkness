# ADR-0005: Deterministic retrieval is the foundation; embeddings are optional

- **Status**: Accepted
- **Date**: 2026-08-10
- **Deciders**: Taiye Babatope
- **Implemented by**: [#116](https://github.com/fullstacktaiye/harkness/issues/116), [#117](https://github.com/fullstacktaiye/harkness/issues/117), [#118](https://github.com/fullstacktaiye/harkness/issues/118), [#119](https://github.com/fullstacktaiye/harkness/issues/119), [#120](https://github.com/fullstacktaiye/harkness/issues/120), [#121](https://github.com/fullstacktaiye/harkness/issues/121), [#137](https://github.com/fullstacktaiye/harkness/issues/137)
- **Builds on**: ADR-0004, [#87](https://github.com/fullstacktaiye/harkness/issues/87) (the tool contract context tools register into), [#94](https://github.com/fullstacktaiye/harkness/issues/94) (read-only inspection tools), [#113](https://github.com/fullstacktaiye/harkness/issues/113) (chunking), [#114](https://github.com/fullstacktaiye/harkness/issues/114) (index store)

## Context

Retrieval for coding assistants is commonly built embeddings-first: chunk the
repository, embed every chunk, store the vectors, and retrieve by cosine
similarity. That approach makes an embedding model a hard prerequisite for the
product working at all, adds a vector store to the dependency set, and produces
results that cannot be explained to a user beyond "it was similar".

Harkness has properties that change the calculation. It sits on top of Git and
already has cheap, exact answers to most of the questions that actually get
asked: which files changed in this branch, which file defines this symbol, which
test covers this module, what does the repository's `AGENTS.md` say. Its
retrieval must also be *auditable* — every context item carries provenance with
a selection reason and a rank explanation
([#109](https://github.com/fullstacktaiye/harkness/issues/109)) — and "the
vectors were close" is not a reason a user can check.

It must also be *testable*. Retrieval quality is measured against a known-answer
corpus with thresholds asserted in CI
([#137](https://github.com/fullstacktaiye/harkness/issues/137)). A pipeline
whose first stage is a neural model cannot produce byte-identical results across
machines, which makes those thresholds unenforceable and every regression
arguable.

## Decision

Deterministic retrieval is the **required foundation**. Five sources, all exact
and all explainable:

1. **Structure** — the repository map and file inventory
   ([#112](https://github.com/fullstacktaiye/harkness/issues/112),
   [#118](https://github.com/fullstacktaiye/harkness/issues/118)).
2. **Lexical** — filename and content search over the index, using in-process
   search libraries rather than a spawned `grep`
   ([#116](https://github.com/fullstacktaiye/harkness/issues/116)).
3. **Symbols** — language-aware definition and reference lookup via tree-sitter
   ([#117](https://github.com/fullstacktaiye/harkness/issues/117)).
4. **Git** — diffs, changed files, merge-base ranges, and bounded history via
   the existing `GitService`
   ([#119](https://github.com/fullstacktaiye/harkness/issues/119)).
5. **Instructions** — discovered repository instruction files, scoped and
   precedence-ordered ([#120](https://github.com/fullstacktaiye/harkness/issues/120)).

Ranking over those sources is deterministic and explainable: every score carries
a serializable per-signal `RankExplanation`
([#121](https://github.com/fullstacktaiye/harkness/issues/121)). The same query
against the same snapshot and the same index generation returns the same items
in the same order, on every machine.

**Semantic retrieval is an optional strategy behind an interface.** The
retrieval trait admits an embedding-backed source, and nothing in ranking or
pack assembly assumes its absence. But:

- **No embedding provider and no vector database is a v0.4 dependency.**
- **The default automated test suite requires no embedding provider and no
  vector database.** Every retrieval-quality threshold in
  [#137](https://github.com/fullstacktaiye/harkness/issues/137) is met by
  deterministic sources alone.
- **Correctness never depends on semantic retrieval.** If a semantic strategy is
  absent, disabled, or fails, retrieval degrades in *quality*, never in
  correctness: no missing embedding may cause a wrong file to be edited, a stale
  chunk to be served, or a sensitive path to be included.
- A semantic strategy that ships later is additive — a new `RetrievalSource`
  variant and a new ranking signal — and arrives with its own ADR covering where
  the vectors live, what leaves the machine to produce them, and how its
  nondeterminism is kept out of the deterministic test corpus.

## Consequences

- Harkness is useful offline, on a machine with no model at all. Context
  inspection, search, the repository map, and instruction discovery work with no
  provider configured.
- Every context item can answer "why were you selected?" with a reason a user
  can verify against the repository. That is what makes the pack inspector
  ([#133](https://github.com/fullstacktaiye/harkness/issues/133)) worth opening.
- Retrieval regressions are catchable. A ranking change moves measured
  Recall@K and MRR by a number, in CI, deterministically.
- Nothing is embedded, so nothing is sent to an embedding service. The privacy
  story for indexing is short: indexing is local, always.
- Purely semantic queries are weaker than in an embeddings-first system.
  "Where is the retry logic?" resolves through symbol names, lexical matches,
  and structure rather than meaning, and will sometimes miss a match that a
  vector search would find. That is the accepted cost, mitigated by the model's
  ability to issue follow-up context tool calls
  ([#123](https://github.com/fullstacktaiye/harkness/issues/123)) — an agentic
  search loop, not a single shot.
- Deterministic sources need their own quality work: ranking, deduplication,
  diversity, and crowd-out control
  ([#121](https://github.com/fullstacktaiye/harkness/issues/121)) are real
  engineering, not a fallback.
- `tree-sitter` becomes a dependency with per-language grammars, and a language
  without a grammar degrades to lexical-only retrieval rather than failing.

## Alternatives considered

**Embeddings-first, deterministic sources as filters.** The mainstream design,
and better at vague natural-language queries. Rejected: it makes an embedding
model a prerequisite for the product functioning, makes CI thresholds
unenforceable, sends repository content to a service or requires a local
embedding runtime, and produces provenance no user can check. Every one of those
is a v0.4 requirement it fails.

**Hybrid from day one — deterministic plus embeddings, fused.** Best quality on
paper. Rejected as sequencing, not as direction: fusing two rankers before
either is measured means neither can be attributed when quality moves. Build and
measure the deterministic pipeline first; the seam is reserved.

**A required local embedding model** (small, bundled, no network). Solves the
privacy objection. Rejected: it still makes model inference a prerequisite for
retrieval, adds a runtime and model weights to the install, and reintroduces
cross-machine nondeterminism into the test corpus.

**Spawning `ripgrep` for lexical search.** Reuses the hermetic-subprocess
precedent from `harkness-git`. Rejected: that precedent exists because Git is
genuinely an external program with process semantics worth controlling. Search
is a library call; spawning it adds a runtime binary dependency, a second
output-parsing surface, and no isolation benefit.
