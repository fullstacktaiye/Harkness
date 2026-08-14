# ADR-0019: Change provenance is derived from Git, behind a read interface a recorded source can join

- **Status**: Accepted
- **Date**: 2026-08-13
- **Deciders**: Taiye Babatope
- **Implemented by**: [#82](https://github.com/fullstacktaiye/harkness/issues/82)
- **Builds on**: [#75](https://github.com/fullstacktaiye/harkness/issues/75) (one diff surface), [#56](https://github.com/fullstacktaiye/harkness/issues/56) (review is read-only), ADR-0004, ADR-0017

## Context

The review surface can say what changed and cannot say what produced it. Open a
branch review of forty files and every row is anonymous: the same flat list
whether the work came from one focused run or from four overlapping ones. The
question a reviewer asks first — *what produced this file, and was it the same
thing that produced the file next to it* — had no answer anywhere in the model.

Two sources were available, and [#82](https://github.com/fullstacktaiye/harkness/issues/82)
was written design-first precisely because choosing between them is the whole
decision.

**Deriving from Git** costs no new storage. Harkness's own conventions already
carry attribution: branches are named `agent/<slug>`, commits carry
`Co-Authored-By` trailers naming the model, and commit author, committer and
time are already read by the history walker (`crates/harkness-git/src/history.rs:481`).
It works retroactively on history that already exists and survives a Harkness
reinstall. It is also coarse: it attributes at commit granularity, and it cannot
distinguish two steps of one run.

**Recording it** is precise and per-step — and, unlike when #82 was written, the
run model it needs now exists. `harkness-runtime` has durable task, run, step and
tool-call records, an append-only `run_events` log, and an artifact store
(`crates/harkness-runtime/src/domain`, `.../store`). What it does *not* have is
any link from a run, a step, or a tool call to a path or a commit: no record type
carries one, and no table has a column for one. A recorded source is therefore
not a matter of reading rows that already exist — it is a `runtime.db` migration,
a new record shape, and a frozen fixture, and it would still answer nothing about
a repository whose history predates Harkness, which is every repository a user
imports.

The layering makes the choice sharper than it looks. `harkness-core` and
`harkness-runtime` are siblings: core does not depend on the runtime, and both
front ends reach `harkness-git` directly (`ProjectService::git` hands out a
`GitService` and core wraps no diff of its own). So a Git-derived source can live
below everything, while a recorded source can only be composed above the runtime.

Two other constraints bound the shape rather than the source. #56 fixed that
review is read-only — no repository lock, no spawned process — and #82 adds that a
thousand-file review must not become a thousand history walks. And provenance is
evidence for a human: nothing about staging, discarding or diffing may read it.

## Decision

**Change provenance is derived from Git, and lives in `harkness-git`.**

`crates/harkness-git/src/provenance.rs` owns the vocabulary — `ChangeProvenance`,
`FileProvenance`, `CommitAttribution`, `Producer`, `ProvenanceGap`,
`ProvenanceRange`, `ProvenanceTruncation` — and `GitService::provenance` is the
one entry point. It walks the range a `DiffTarget` implies exactly once, compares
each commit with its first parent exactly once, and records each delta against
the paths it names. **No path is ever walked on its own**, and following a rename
backwards is forbidden for that reason: it is a per-file history walk under
another name.

The record is **total and advisory**. Every requested path appears in the result,
and a path nothing could be attributed to carries a named `ProvenanceGap` —
`Uncommitted`, `EmptyRange`, `NotInRange`, `CommitBudgetExhausted` — rather than
an empty field a reader has to interpret. No staging, discarding, or diffing
decision may read any of it. That licence is what pays for the two known
inaccuracies below.

**Only what a commit records is reported.** A producer is a Git `author` or a
`Co-Authored-By` trailer, and `ProducerKind` says which of the two was read.
Neither is classified as human or machine, because the repository does not say
and a guess dressed as a fact is exactly what ADR-0017 forbids. The single
Harkness-specific reading is the `agent/<slug>` branch convention, and it is
reported on the *range* rather than on a file, because that is what it describes.
A front end that pinned a branch review to object ids — the panel does, so a
branch advancing cannot move a review under its reader — passes the name it
resolved as `ProvenanceOptions::head_reference`; that changes what the convention
reads and changes no walk.

**Merges are skipped in a range and attributed on their own.** Comparing a merge
with its first parent would attribute everything the merged branch did to whoever
ran the merge, and the commits it merged are already in the range. The count is
reported as `skipped_merges` rather than hidden, because the conflict resolutions
this loses are real. A single-commit target is the exception and attributes the
merge itself, because that is the comparison the diff beside it shows.

**No new persisted format lands, and none may be added to `harkness-git`.**
Provenance is recomputed from the repository on every read. There is no
provenance file, no provenance table, and no provenance column: the frozen-fixture
and version-probe obligations in `AGENTS.md` are satisfied by there being nothing
durable to freeze.

**A recorded source joins behind this interface, and never beside it.** When
[#83](https://github.com/fullstacktaiye/harkness/issues/83) or a later issue makes
the runtime record which paths a step touched, it composes above
`harkness-runtime` and enriches a `ChangeProvenance` — adding run and step
attribution to the commits already there. It does not introduce a second read
interface, a second vocabulary, or a second call from a front end, and
`harkness-git` never learns that runs exist. A recorded source that lands as a
parallel path for the panel to choose between is the outcome this ADR exists to
prevent.

## Consequences

Attribution works today, on every repository, with no migration, no store, and
nothing to prune — including on the forty-file review of an agent's work that
motivated the issue, and on repositories whose history predates Harkness
entirely.

The cost is granularity, and it is permanent for this source. Two steps of one
run that commit together are one commit and therefore one attribution. Work an
agent did and did not commit is `Uncommitted` — correct, and unhelpful, and the
common answer for a working-tree review. Anything finer waits for the recorded
source.

Two inaccuracies are accepted by name, and both are the kind that costs a label
rather than a correctness property. A file whose range history crosses a rename
is attributed only from the rename forward. A merge that resolved a conflict has
that resolution attributed to nobody.

Contributors gain three obligations. A read added here must stay lock-free and
process-free, like every other read on `GitService`. Anything reported must be
something a commit records, or must say what it inferred and from what. And
provenance must stay unreadable to any code that stages, discards or diffs: a
call site that branches on attribution is a review-blocking defect, not a
feature.

Front ends pay for what they ask for. `harkness git diff --provenance` is opt-in
because it walks a range, and the panel resolves once per review load rather than
per file. Both are bounded by `DEFAULT_MAX_PROVENANCE_COMMITS`, and reaching it
degrades to `ProvenanceTruncation::CommitBudgetExhausted` rather than failing a
read or spending unbounded time on the path that opens a panel.

## Alternatives considered

**Record it first, in a new versioned file beside `projects.json`.** This was the
alternative the issue named, and it loses on coverage before it loses on cost:
every repository a user imports has a history Harkness did not observe, so a
record-only source renders the common review entirely unknown while a derived
source answers it. It is also the more expensive of the two to get wrong — a new
durable user-data format carries version probing, additive-field rules, frozen
fixtures and pruning, all of it owed before the first row is useful — and #83
will need a *step*-to-path link that no shape invented here would have matched.
Deriving first and recording later inverts nothing: the read interface is the
same either way.

**Record it into `runtime.db` immediately, as an additive migration.** Cheaper
than a new file, and it reuses machinery that already exists. It still answers
nothing for an unobserved history, and it would put the resolver above
`harkness-runtime` where neither front end's diff path currently reaches — every
review would have to open a run store to render a file list, including the many
that have no runs at all. The composition point is right; the timing is not.

**Attribute from `git blame` per file.** The most precise per-line answer
available, and the one thing the issue's performance requirement rules out
outright: it is a history walk per file, which is a thousand of them on the
review that motivated this. It is also the wrong shape — a reviewer asks who
produced a *file within this range*, and blame answers who last touched a line in
all of history.

**Classify a producer as human or agent.** Tempting, because "which agent wrote
this" is the question in the issue's title. Nothing in a commit says. The
available signals — an email host, a name that looks like a model — are guesses,
and ADR-0017 fixes that a claim is never rendered as an observation. Reporting
`author` and `co_author` as what they are lets a reader who knows the convention
draw the conclusion, without Harkness asserting it.

**Follow renames with `find_similar` per commit.** It would attribute a renamed
file across its whole range instead of from the rename forward. It costs content
hashing once per commit in the range, on the path that opens a panel, to improve
an advisory label. If a future measurement says the cost is negligible, it is an
additive change to this same interface.
