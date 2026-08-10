# ADR-0004: Run evidence is durable; the context index is a disposable cache

- **Status**: Accepted
- **Date**: 2026-08-10
- **Deciders**: Taiye Babatope
- **Implemented by**: [#110](https://github.com/fullstacktaiye/harkness/issues/110), [#114](https://github.com/fullstacktaiye/harkness/issues/114), [#115](https://github.com/fullstacktaiye/harkness/issues/115), [#122](https://github.com/fullstacktaiye/harkness/issues/122)
- **Builds on**: [#86](https://github.com/fullstacktaiye/harkness/issues/86) (store and migrations), [#88](https://github.com/fullstacktaiye/harkness/issues/88) (events and artifacts), [#109](https://github.com/fullstacktaiye/harkness/issues/109) (snapshot types)

## Context

The data directory today holds `projects.json`, `projects.lock`, `runtime.db`
(plus `-wal`/`-shm`), `artifacts/`, `locks/`, `repositories/`, and `worktrees/`,
all under the `HARKNESS_DATA_DIR` override. `runtime.db` is the evidence store:
its migrations are numbered and never edited once released, its `run_events`
table contains no `UPDATE` and no `DELETE` by construction, and every row is
rebuilt into its wire record and re-validated on load so an impossible record
cannot enter the process. Artifact content lives at
`artifacts/<run_id>/<artifact_id>`; finalization is write, sync, rename, then
insert, so the only crash residue is an orphan file.

v0.4 adds a second body of data with the opposite character: a file inventory,
chunk boundaries, symbol tables, and search structures derived entirely from
repository content. It is expensive to build, cheap to rebuild, invalidated by
things that have nothing to do with the repository — a tree-sitter upgrade, a
chunking-rule change, a ranking-formula change — and useless to keep after any
of them.

Storing both in `runtime.db` would make every parser upgrade a schema migration
and would put a multi-hundred-megabyte index in the file whose backup procedure
is documented in `AGENTS.md`. Worse, it would blur which rows are *evidence* —
what Harkness actually showed a model, and why — and which are merely a
derivation that can be recomputed. The first cache eviction would then destroy
run provenance.

## Decision

Two stores, split by whether the data is evidence or derivation.

**Evidence lives in `runtime.db` and `artifacts/`.** Workspace snapshots,
context packs and their items, provenance, agent turns, provider requests,
messages, verification outcomes, and summaries arrive as **additive migrations**
through the existing framework
([#86](https://github.com/fullstacktaiye/harkness/issues/86)) and **additive
event kinds** through the existing log
([#88](https://github.com/fullstacktaiye/harkness/issues/88)). Large content —
rendered prompts, oversized payloads — spills to artifacts. All existing store
invariants apply unchanged.

**The index is a disposable cache** at
`<data_dir>/context/<repository-key>/index.db`, a sibling of `repositories/`,
`worktrees/`, `locks/`, and `artifacts/`, covered by the same
`HARKNESS_DATA_DIR` override. `<repository-key>` is the v5 UUID already used as
the repository-lock key (`REPOSITORY_LOCK_NAMESPACE` over the canonical Git
common directory, `crates/harkness-git/src/lock.rs:18`), so every linked
worktree of one repository maps to one cache root and per-worktree state is
isolated inside it ([#115](https://github.com/fullstacktaiye/harkness/issues/115)).
The path is derived, never user-supplied.

**Deleting `<data_dir>/context/` at any time loses no run evidence.** It costs
warm-up time and nothing else: no run history, no provenance, no approval
record, no artifact, and no context pack is stored there. This is the property
the split exists to guarantee, and it is asserted by an integration test
spanning both stores
([#110](https://github.com/fullstacktaiye/harkness/issues/110)).

The cache carries its own version metadata in a single-row `index_meta` table
with four version fields plus a generation counter:

| Field | Meaning | Mismatch behavior |
| --- | --- | --- |
| `schema_version` | the cache's own table layout | quarantine and recreate empty |
| `parser_version` | language grammars and symbol extraction | invalidate symbol-derived rows, reconcile incrementally |
| `chunking_version` | chunk boundary rules | invalidate chunk-derived rows, reconcile incrementally |
| `ranking_version` | scoring formula | invalidate cached scores |
| `index_generation` | bumped on every recreate or full rebuild | feeds `WorkspaceSnapshot.index_generation` |

A cache written by a newer build is refused (`cache_version_conflict`) and left
byte-identical rather than downgraded, mirroring the store's `schema_too_new`
refusal. A corrupt cache is quarantined to `index.db.corrupt-<timestamp>` (at
most two kept, oldest deleted first), recreated empty, and the generation
bumped. Neither path touches `runtime.db`.

`index_generation` is a component of the workspace snapshot digest
(ADR-0008), so a pack built against a rebuilt index is not mistaken for one
built against the index that produced it.

The cache layer never writes evidence. A pack is persisted by its caller in
`harkness-runtime` ([#122](https://github.com/fullstacktaiye/harkness/issues/122),
[#123](https://github.com/fullstacktaiye/harkness/issues/123)); the engine
returns typed values and stores nothing on the evidence side — which ADR-0001's
dependency direction makes structurally true rather than merely intended.

## Consequences

- Deleting the cache is a supported recovery action a user can be told to take,
  and "reclaim disk" and "fix a weird index" are the same command.
- A parser or chunking upgrade is a constant bump, not a migration. Nothing in
  the released-migration discipline is disturbed by improving retrieval.
- Evidence is complete on its own. A run inspected a year later can say what the
  model was shown and why, from `runtime.db` and `artifacts/` alone, with the
  index long since discarded.
- Provenance is duplicated by design: a context item's path, byte range, and
  content hash are copied into the evidence store rather than referenced into
  the cache. That costs bytes and buys an audit trail that survives eviction.
- Two SQLite databases means two connection disciplines. The cache follows the
  same pragmas (WAL, `foreign_keys=ON`, `busy_timeout`, `synchronous=NORMAL`)
  but is a separate handle, and the engine's index-writer lock is **leaf-level**:
  it is never held while acquiring the repository lock or the catalog lock, so
  the repository-then-catalog ordering is unchanged.
- Two processes (GUI and CLI) share one cache. WAL and the busy timeout make
  concurrent readers safe; writer contention ends in a typed error after the
  timeout, never a lock-free write.
- A cold cache is a real cost on first use of a large repository. The engine
  reports index status so the UI can say so
  ([#133](https://github.com/fullstacktaiye/harkness/issues/133)) rather than
  appearing hung.

## Alternatives considered

**One database for both.** One connection, one backup procedure, foreign keys
from pack items straight to chunk rows. Rejected: it makes every parser upgrade
a `runtime.db` migration, puts a rebuildable index inside the file that must
survive, and — decisively — makes it possible for provenance to be stored only
as a reference into rows that a rebuild deletes. The referential integrity that
looks like a benefit is what destroys the evidence.

**Index in memory only, rebuilt per process start.** No second store, no
versioning, no corruption handling. Rejected: a 100k-file repository cannot be
re-walked and re-parsed on every launch, and the first thing anyone would add is
a disk cache — unversioned, because it started as an optimization.

**Index inside the repository's `.git/` directory.** Naturally per-repository and
travels with the checkout. Rejected: Harkness never writes inside a user's
`.git` — the same rule that puts repository locks under `locks/` in the data
directory. A tool that leaves state in a user's repository is a tool that gets
blamed for it.

**Cache keyed by worktree path.** Simpler derivation. Rejected: linked worktrees
of one repository would each build a full index of nearly identical content, and
path-derived identity is already a known weakness
([#63](https://github.com/fullstacktaiye/harkness/issues/63)). Keying by the
common directory shares the expensive content-addressed work and isolates only
what is genuinely per-worktree.
