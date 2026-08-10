# ADR-0008: Workspace identity is a composite digest, never `HEAD` alone

- **Status**: Accepted
- **Date**: 2026-08-10
- **Deciders**: Taiye Babatope
- **Implemented by**: [#109](https://github.com/fullstacktaiye/harkness/issues/109), [#115](https://github.com/fullstacktaiye/harkness/issues/115), [#128](https://github.com/fullstacktaiye/harkness/issues/128), [#130](https://github.com/fullstacktaiye/harkness/issues/130)
- **Builds on**: ADR-0004, ADR-0006, [#85](https://github.com/fullstacktaiye/harkness/issues/85) (ID-newtype and wire conventions), [#86](https://github.com/fullstacktaiye/harkness/issues/86) (the migration discipline a persisted snapshot inherits), [#63](https://github.com/fullstacktaiye/harkness/issues/63) (path-derived worktree identity)

## Context

Every claim the context engine makes is relative to a workspace state: this
chunk came from this file, at this hash, with this content. If "the workspace
state" is not precisely identified, none of those claims can be checked later,
and a mutation applied against a state that has since moved silently overwrites
somebody's work.

The obvious identity is the commit. It is wrong in ways that are ordinary rather
than exotic:

- A developer edits a file in their editor. `HEAD` does not move. The context
  Harkness gathered a minute ago describes bytes that no longer exist.
- Two linked worktrees sit at the same commit with different uncommitted work.
  By commit alone they are the same workspace, so context from one would be
  served for the other.
- A file is staged but not committed. `HEAD` does not move; the working tree
  the model will edit is not what `HEAD` describes.
- A `.gitignore`d generated file appears or disappears. No commit changes.
- The index is rebuilt under new chunking rules. Every `ChunkId` shifts, and a
  pack referencing the old ones points at boundaries that no longer exist.

`harkness_git::GitStatus` (`crates/harkness-git/src/status.rs:37`) reports
branch, dirtiness, and counts, but it is a display summary: two genuinely
different dirty states can produce equal `GitStatus` values. It is not an
identity and was never meant to be one.

## Decision

**Workspace identity is a composite digest over ten components.** `HEAD` alone
is never accepted as identity, anywhere, by anything.

`WorkspaceSnapshot` ([#109](https://github.com/fullstacktaiye/harkness/issues/109))
captures:

| Component | Why it is in the identity |
| --- | --- |
| **Repository identity** | `harkness_git::repository_identity` — the v5 UUID of the canonical common directory; separates unrelated repositories that happen to share paths or commits |
| **Worktree root** (canonicalized) | separates two linked worktrees of one repository at the same commit |
| **`HEAD`** | the committed base; `None` on an unborn branch |
| **Branch** | separates a detached checkout from a branch at the same commit |
| **Index digest** | staged paths plus staged blob ids — staged work is not in `HEAD` |
| **Tracked-dirty digest** | modified tracked paths plus content hashes — the case `HEAD` misses most often |
| **Untracked digest** | untracked eligible paths plus content hashes — new files the model will read |
| **Instruction-set digest** | the ordered discovered instruction set and its contents ([#120](https://github.com/fullstacktaiye/harkness/issues/120)); guidance changing mid-run is a state change |
| **Config generation** | bumped when context-relevant configuration changes; different exclusions mean a different workspace view |
| **Index generation** | from `index_meta` (ADR-0004); a rebuilt index invalidates chunk-level references |

The digest covers every component above and excludes only the snapshot's own id
and capture timestamp — capturing the same state twice must yield the same
digest. Digests are order-independent (sorted path lists), byte-exact, and
identical across platforms, which is asserted by a fixture test on the existing
three-OS matrix.

**Staleness is checked before mutations.** `verify()` recomputes cheaply —
Git status plus hashing only the files reported dirty or untracked, never a full
rehash — and returns `Fresh`, `Stale { changed }` naming the diverged paths, or
`Unverifiable { reason }` when the repository or root is gone. Every mutating
tool re-verifies the snapshot **and** the per-file base content hash before
writing ([#128](https://github.com/fullstacktaiye/harkness/issues/128),
[#130](https://github.com/fullstacktaiye/harkness/issues/130)). A stale
workspace produces a structured failure and a bounded refresh path — never a
silent overwrite, and never a retry that reapplies an edit.

**Every context item names its snapshot.** Provenance carries `snapshot_id` and
the exact `content_sha256` of the bytes the model was shown
([#109](https://github.com/fullstacktaiye/harkness/issues/109)), so a run
inspected later can prove what state it described.

Capture tolerates a moving workspace rather than fighting it: a file that
changes mid-hash contributes the bytes that were read, an unreadable file
contributes a sentinel and is listed in capture diagnostics without failing the
capture, and `verify` reports the divergence afterwards. A snapshot is an
honest record of what was read, not a lock on the filesystem.

Snapshots contain **hashes and paths only, never file contents**, so they are
safe to persist in `runtime.db` and to display. The only absolute path is
`worktree_root`.

## Consequences

- Stale-context overwrites become detectable and are refused with a reason
  naming the changed path, instead of landing as a plausible-looking bad diff.
- Two worktrees of one repository are distinguishable even at the same commit
  and equally clean, which is what makes per-worktree isolation
  ([#115](https://github.com/fullstacktaiye/harkness/issues/115)) implementable
  on a shared content-addressed cache.
- A pack built before an index rebuild is not confused with one built after,
  because `index_generation` participates in identity.
- Verification is not free. It costs a Git status plus hashing the dirty and
  untracked set — bounded by the size of uncommitted work, not by the
  repository, with a p95 target under 200 ms warm on a ~10k-file repository.
- Identity is deliberately *sensitive*. Touching a file's mtime is not enough to
  change it (content is hashed), but any real edit is, including an edit to an
  ignored-but-eligible file. Refreshing is cheap; a false "fresh" is not.
- `worktree_root` is a canonicalized absolute path, which inherits the known
  weakness that a re-created checkout at the same path is indistinguishable from
  the original ([#63](https://github.com/fullstacktaiye/harkness/issues/63)).
  Recording it as a known limitation is deliberate; fixing it belongs to
  [#63](https://github.com/fullstacktaiye/harkness/issues/63), and the composite
  digest narrows the blast radius because content digests still diverge.
- Symlinks are hashed as their link target *path string* and never followed for
  content, so a symlink pointing outside the worktree changes identity without
  reading anything outside it.
- The wire form is frozen by test at
  [#109](https://github.com/fullstacktaiye/harkness/issues/109) because it
  becomes a persisted column in
  [#110](https://github.com/fullstacktaiye/harkness/issues/110). Adding a
  component to the identity later is a `runtime.db` migration plus a new frozen
  fixture, exactly as `AGENTS.md` requires for any persisted format.

## Alternatives considered

**`HEAD` alone.** Free to compute, trivially stable. Rejected: it is wrong for
every uncommitted edit, which is the normal state of a workspace being worked
on, and it cannot distinguish two worktrees. It is the failure this ADR exists
to prevent.

**`HEAD` plus a dirty boolean** (roughly what `GitStatus` offers). Cheaper.
Rejected: "dirty" does not say *what* is dirty, so two different dirty states
compare equal and staleness is undetectable at the granularity that matters.

**A full working-tree content digest.** Maximally precise. Rejected: it requires
hashing every eligible file on every capture *and* every verify, which is
seconds on a large repository and is charged before every mutation. The
committed state is already content-addressed by Git; only the delta needs
hashing.

**Filesystem mtimes instead of content hashes.** Much faster. Rejected: mtimes
lie in both directions — a checkout rewrites them without changing content, and
coarse timestamp granularity hides same-second edits. They are a fine
*optimization hint* for deciding what to rehash ([#115](https://github.com/fullstacktaiye/harkness/issues/115))
and an unacceptable basis for identity.

**Per-file hashes only, with no composite digest.** Sufficient for precondition
checks on individual writes. Rejected: it gives no single value to record on a
pack, correlate events by, or compare in one operation, so "is this the same
workspace?" would have no answer short of a full comparison.
