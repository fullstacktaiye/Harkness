# Repository Guidelines

## Project Structure & Module Organization

Harkness is a Rust 2024 workspace split into thirteen crates under `crates/`:

- `harkness-core`: project catalog, storage layout, cross-domain project workflows, and directory-listing logic shared by front ends.
- `harkness-git`: all production Git behavior: inspection, diffs and history, change provenance, file context and hunk staging, branch and worktree mutation, commits, clone and synchronization, hermetic process execution, and repository locking.
- `harkness-context`: the context engine's typed vocabulary — workspace snapshot identity, stable identifiers, provenance, and file classification.
- `harkness-provider`: the provider-neutral model contract, the streaming turn assembler, and the deterministic scripted provider. Every concrete model adapter lives here and keeps its wire types private; nothing above learns what an endpoint's JSON looks like.
- `harkness-transport`: the shared subprocess JSON-RPC engine both protocol adapters run on — hermetic allowlisted spawn, newline-delimited framing, request correlation, bounded messages, and the close-stdin/`SIGTERM`/`SIGKILL` teardown. Below every adapter and above nothing but `harkness-git`.
- `harkness-acp`: the Agent Client Protocol client — the wire vocabulary, the `initialize` handshake, protocol-version and capability negotiation, and gated authentication. Sessions, mediation, and the rest of the v0.5 ACP surface land here beside them.
- `harkness-mcp`, `harkness-forge`, `harkness-recipe`: the remaining v0.5 external-integration adapters — the Model Context Protocol client, forge-neutral contracts with the GitHub REST adapter, and the workflow-recipe source format and compiler. Currently skeletons; see the invariants below.
- `harkness-test-fixtures`: hermetic repository, filesystem, and process fixtures shared only by crate tests.
- `harkness-runtime`: typed task, run, step, and tool-call records, the typed tool contract and registry every executable operation implements, the execution contracts shared by front ends, and the SQLite run store that makes those records durable.
- `harkness-cli`: the `harkness` command and its integration tests in `tests/`.
- `harkness-gui`: the Qt 6/KDE Kirigami application. Rust/CXX-Qt bindings live in `src/` and `cxx/`; UI components live in `qml/`. Run, timeline, and approval bridge code lives outside `backend.rs`, in `src/run_list_model.rs`, `src/run_timeline_model.rs`, `src/approval_model.rs`, and `src/runs_backend.rs`, sharing `src/reconcile.rs` and `cxx/listmodelbase.h`; `backend.rs` stays the Git and catalog surface and gains no run or approval members. A front-end read takes the run store, never the coordinator: building one takes this process's lease and runs the recovery sweep, which writes, so it belongs to deciding to drive work rather than to looking at what was recorded.

Desktop integration assets are in `data/`. The root `CMakeLists.txt` provides release build and local installation support; Cargo remains the primary development interface.

Architecture Decision Records live in `docs/adr/`; read them before proposing a change to a crate boundary, a dependency direction, a persisted format, a trust boundary, or the concurrency model. This file states the invariants; an ADR states why an alternative was refused.

## Build, Test, and Development Commands

- `cargo run -p harkness-cli` runs the command-line binary.
- `cargo run -p harkness-gui` launches the Kirigami application; Qt 6, KDE Frameworks 6, and `qmake` must be available.
- `cargo test --workspace` runs all unit and integration tests.
- `cargo fmt --check` verifies Rust formatting without changing files.
- `cargo clippy --workspace --all-targets` checks all targets for common Rust issues.
- `cmake -S . -B build -DCMAKE_BUILD_TYPE=Release` followed by `cmake --build build` creates the locked release workspace used for installation.

## Coding Style & Naming Conventions

Use standard `rustfmt` output (four-space indentation) and keep Clippy clean. Follow Rust conventions: `snake_case` for functions and modules, `PascalCase` for types, and `SCREAMING_SNAKE_CASE` for constants. Prefer explicit error propagation and typed domain errors over panics in production paths. Name QML components in `PascalCase.qml`, assign lower camel-case IDs/properties, and keep UI state transitions documented when they are not obvious.

## Testing Guidelines

Place focused unit tests in a `#[cfg(test)] mod tests` beside the implementation. Put executable-level behavior in crate-level `tests/*.rs`; use descriptive names such as `json_empty_project_list_is_exact`. Add regression coverage for catalog locking, Git process handling, filesystem safety, and navigation changes. Run the full test, format, and Clippy commands before submitting.

## Catalog Schema & Worktree Invariants

`projects.json` is a versioned user-data format guarded by the stable
`projects.lock` inode. Probe `version` before deserializing the body so a newer
schema produces an upgrade message rather than a corruption message. Persist
the oldest version that can represent the current entries: ordinary local and
managed projects remain v1-compatible, the first worktree requires v2, global
editor configuration requires v3, and the first explicit project check list
requires v4.
Read-only operations must never rewrite the file.

Additive optional fields must deserialize missing values to a safe default and
must be omitted when absent. Any new `ProjectSource` variant or other data an
older build cannot preserve requires a catalog version bump and a frozen JSON
fixture. Same-version unknown fields are rejected instead of being silently
dropped on the next write.

New durable JSON formats use explicit schema versions and RFC 3339 UTC
timestamps. The project catalog's human-readable `time` encoding is a legacy
exception that remains frozen until a future catalog migration; do not copy it
into new formats. JSON-backed path fields currently require UTF-8, so
persisting a runtime task with a non-UTF-8 workspace path is a known Unix
limitation and must surface as a serialization error rather than lossy data.

Each durable runtime record probes `schema_version` before parsing its strict
body. Adding a field or persisted state spelling requires a version bump and an
updated frozen fixture; current-version unknown fields remain errors. Keep the
owned deserialization type and borrowing serialization type byte-compatible.

Every worktree must name an existing parent; self-parenting, dangling parents,
and parent cycles are invalid. Parent removal and worktree insertion both need
the exclusive catalog lock. Worktree creation acquires the repository lock
first, then the catalog lock, and re-checks the parent under that catalog lock
before inserting the worktree. Removal keeps the repository lock while Git
deletes the checkout, but never holds the global catalog lock during that
potentially long operation. Remove worktrees only through Git so the checkout
and `.git/worktrees` administration disappear together; reconciliation must be
selective and must not prune external worktree records.

## Diff Whitespace & Selection Invariants

A diff computed with anything other than `Whitespace::EXACT` is a view and
never a source of truth. Its hunks omit lines that genuinely differ on disk, so
a patch rendered from them describes a file nobody has. `FileDiff::exact` is the
only way to obtain the `ExactFileDiff` token `HunkSelection::new` and
`LineSelection::new` accept, which makes building a selection from a relaxed
record a compile error rather than a wrong-bytes apply found later. A selection
rebuilt from a wire form carries its own `whitespace`, and every staging,
unstaging and discard entry point refuses a non-exact one by name *before* it
takes the repository lock — never after recomputing, because an exact
recomputation can match relaxed coordinates by coincidence on a hunk whose
interior differs.

Whitespace handling is recorded per file for the same reason `context_lines` is:
hunk coordinates mean nothing without both. Every revalidation under the lock
goes through `DiffOptions::unbounded`, which is exact by construction and must
stay that way.

Staging from a relaxed *view* is still legitimate, and `remap_to_exact` is the
one seam that serves it: the caller re-requests the same target and path at
`Whitespace::EXACT` and the region is re-expressed in that model. The mapping
verifies rather than assumes — the candidate exact hunks' changed lines must be
exactly the changed lines the view displayed, kinds, numbers and bytes alike —
and refuses with `HiddenWhitespaceChanges` when they are not, because the
alternative is applying content the reader was never shown.

Any wire form that carries hunk coordinates must carry the whitespace they were
taken under, and `harkness git stage --hunk` requires *both* `--whitespace` and
`--ignore-blank-lines` for exactly that reason — the second as a written value
rather than a bare switch, because an unstated switch cannot be told from
`false`, and `false` is the spelling that claims the coordinates are appliable.
Revalidation matches on blob IDs and coordinates and never on hunk interior, and
a relaxed hunk can carry the *identical* coordinates of an exact hunk that also
holds the change the relaxed view was hiding — whenever that change sits inside
a region bounded by real changes. Defaulting an absent setting to its exact
value there would turn a coordinate coincidence into a silent apply.

The document form defaults an absent `whitespace` to exact and the flag form
refuses to default one, which is a deliberate asymmetry rather than an
oversight. A file record is copied wholesale out of `harkness git diff`, which
has always emitted the field since it existed, so a record without one comes
from a producer that predates it and was necessarily exact — the inference is
sound. A missing *flag* only says the caller did not type it, which is equally
true of a current caller reading a relaxed diff, so the same inference is not
available. A mistyped value is refused in both, and never read as exact.

Adding a whitespace setting means extending `Whitespace`, not adding a loose
boolean beside it: `is_exact` is the single question staging asks, and it must
not be able to go stale.

Revealing whitespace runs the other way and stays entirely inside the front end.
A renderer may draw bytes differently — a tinted trailing run, a glyph standing
in for a tab, the name of a line terminator — and may never alter them: no
segment text is rewritten, no diff is recomputed, and what a copy puts on the
clipboard is the line the model carried, terminator included — which is why the
copy goes through a clipboard writer of our own rather than through QtQuick's,
whose only one carries the text through a text document and turns every CRLF
into an LF on the way out. Classification
happens on the bytes the Git layer already segmented, never on a re-decoded
string, and every run boundary lands on an ASCII space or tab, so a boundary can
never fall inside a multi-byte character. A revealed glyph keeps the advance
width of what it stands for — one column for a space, four for a tab — because
side-by-side alignment belongs to the model and must not become a property of a
display setting. A line ending is carried as a name on the row rather than left
in the segment text, so the reveal never has to decide what a carriage return
looks like mid-run.

## Change Provenance Invariants

Change provenance is derived from the repository and persisted nowhere. There is
no provenance file, table, or column, and adding one to `harkness-git` is not how
a recorded source lands: it composes above `harkness-runtime` and enriches the
same `ChangeProvenance` a Git-derived read already returns. ADR-0019 records why,
and a second read interface for the panel to choose between is the outcome it
exists to prevent.

**Attribution is advisory and nothing may act on it.** No staging, discarding, or
diffing decision may read a `FileProvenance`, a `Producer`, or a
`ProvenanceGap`. A wrong attribution must stay a cosmetic error, and that licence
is what pays for skipping merges and for not following renames.

**One walk per range, never one per file.** The range a `DiffTarget` implies is
walked once and each commit compared with its first parent once. Nothing may add
a per-path history walk — `git blame` and rename following are both that — because
a thousand-file review must open no slower than a one-file one. Reaching
`ProvenanceOptions::max_commits` degrades to a named
`ProvenanceTruncation`, never to a failed read.

**Every requested path is reported, and absence is a named answer.** A path
nothing could be attributed to carries a `ProvenanceGap` rather than an empty
field, because most repositories have no Harkness runs and every working-tree
comparison is uncommitted content. Blank is not an answer and a guess is worse
than one.

**A narrowed request narrows the whole record.** A commit that touched none of
the requested paths is not in `commits` and its author is not in `producers`: a
result must not carry commits no file references or count people whose work is
not being reviewed. `walked_commits` is what says how far the walk went.

**Which paths are asked about is a `ProvenancePaths`, never a list whose
emptiness is interpreted.** `All` and `Only(vec![])` are opposite requests — a
whole range, and nothing — and inferring one from an empty `Vec` would make a
review with no changed files walk its entire history. Do not reintroduce a bare
path list beside it.

**Only what a commit records is reported.** A producer is a Git `author` or a
`Co-Authored-By` trailer and `ProducerKind` says which; neither is classified as
human or machine, for the reason ADR-0017 gives. The `agent/<slug>` reading is
the one Harkness-specific inference, it is recorded on the range rather than on a
file, and a caller that pinned a review to object ids supplies the reference it
resolved through `ProvenanceOptions::head_reference` — which changes what the
convention reads and never changes a walk. Commit messages and identities are
repository content: trailer parsing is bounded by `MAX_CO_AUTHORS_PER_COMMIT`,
and a producer name reaching a surface is collapsed to one line in both front
ends — `single_line` in the CLI, `collapse_whitespace` in the panel — so it
cannot forge a column or decide how tall a row is. In the panel a name is
rendered only by a `Text.PlainText` label and never reaches a tool tip, whose
style-supplied label renders `Text.AutoText` and would treat a name shaped like
markup as markup; a tool tip reports the *number* of producers instead.

**Provenance reads take no repository lock and spawn no process**, exactly as
every other read on `GitService` does not.

## Run Store Schema & Connection Invariants

Run history lives in `runtime.db` beside `projects.json`, never inside it. It is
the *evidence* half of ADR-0004's split: the context index cache under
`<data_dir>/context/` is disposable and this is not, so nothing derived belongs
in these tables and nothing recorded belongs in that one. The
two stores are versioned the same way and for the same reason: `PRAGMA
user_version` is probed before any statement runs, so a database written by a
newer build produces an upgrade message rather than a corruption message, and
the refusal happens before the connection requests WAL so the file it declined
is left byte-identical. Migrations are numbered, applied in ascending order, and
each shares one `BEGIN IMMEDIATE` transaction with the `user_version` bump it
establishes. That version is re-read under the write lock and the step is
skipped if it moved, because a version read outside the lock only describes the
past: two processes opening one new database would otherwise both replay the
same `CREATE TABLE`. Adding a table, a column, or a persisted spelling requires
a new numbered migration and a frozen fixture database; never edit an
already-released migration.

Every connection applies `journal_mode=WAL`, `foreign_keys=ON`,
`busy_timeout=5000`, and `synchronous=NORMAL`. All writes serialize through one
mutex-guarded connection, and every read-modify-write runs inside one `BEGIN
IMMEDIATE` transaction so a lifecycle change cannot be split by another process.
No transaction is ever held across a user wait: approval-gated work persists the
request, commits, and only then waits. The store takes neither the repository
lock nor the catalog lock, and no caller may hold a store transaction while
acquiring either, so the existing repository-before-catalog ordering is
unchanged.

Timestamps are RFC 3339 UTC written at fixed nanosecond precision so byte order
and chronological order agree; run listing pages by `(created_at, id)` keyset and
never by offset. That one spelling is the only one a stored row may hold: a
column carrying another valid RFC 3339 form is refused on load, because a
variable-width or offset-bearing timestamp does not fail, it just stops sorting
chronologically. Encoding normalizes to UTC rather than trusting its caller. A
continuation token is the one place that stays lenient — it has travelled
through a front end's transport — so it accepts any RFC 3339 spelling and
normalizes it to UTC before it becomes a key. A run cursor is a position, not a
claim that a row exists: it is never validated against the anchor, so paging
still works after a prune.

No column holds more than 64 KiB of caller data — the inline threshold is a
named constant, and it binds every caller-controlled column, not tool payloads
alone: titles, workspace paths, tool identifiers, failure detail, and the
approval history are each held to it in both directions, so a row that arrived
from outside Harkness oversized also fails to load. A caller whose failure
message exceeds the threshold is refused with the record left in its previous
state and must summarize and retry; truncating silently would store something
the caller never wrote. The approval history is the one column a caller can
overflow without ever supplying an oversized value, so its refusal has to land
on the append rather than on some later transition that merely rewrites the
column: a record whose history has filled up can still be cancelled, failed, or
interrupted, and is never stranded awaiting a decision it cannot record. Every stored row is rebuilt into its wire record and
re-validated by the domain on load, so a hand-edited row fails to load instead
of entering the process as an impossible record, and its `schema_version` is
probed before any other column is decoded so a future row reads as an upgrade
request rather than as a corrupt column.

`owner_pid` is audit context and decides nothing. A process identifier is
reused, so a row naming a live pid is not evidence that the process holding it
is the one that wrote the row; `runs.lease_id` and the advisory lock file behind
it are what answer that. `runtime_leases` carries no foreign key from `runs`
either: a lease is the process that drove a run rather than a containment
parent, and a run must stay loadable after its claim is collected.

A WAL database is three files. Backups must copy `runtime.db`, `runtime.db-wal`,
and `runtime.db-shm` together, or checkpoint first and copy `runtime.db` alone —
and check that the checkpoint returned success. A checkpoint reports an
incomplete fold in its result row instead of failing, so a reader on another
connection can leave frames behind; the store reads that row and refuses rather
than letting a backup be taken on a checkpoint that never finished.

## Workspace Trust & Process Safety Invariants

Workspace trust binds a decision to both `ProjectId` and the canonical root.
Neither identity alone is a grant: no row, a moved or unavailable checkout, and
a path reused by another catalog entry all resolve as `Untrusted`. Trust lives
in its own versioned `runtime.db` table, not in `projects.json`, and a trust read
never repairs or rewrites either store.

Every tool-supplied filesystem path crosses `PathBoundary` before use. The
boundary canonicalizes the nearest existing ancestor, restores a missing tail,
and checks the result against the canonical workspace and explicit extra roots.
A symlink reached inside an allowed root that targets outside every allowed root
is refused by name. Downstream APIs accept `ContainedPath`; do not add a second
unchecked path route or turn it back into a public tuple field.

Arbitrary tool children are described only by `CommandSpec`: an executable, an
argv vector, a contained working directory, and an exact `AllowlistedEnv`. The
environment starts empty and admits present baseline variables plus validated
exact names published by the tool descriptor; no wildcard and no shell-string
API exists. This does not change `GitCommand`: Git is one known executable whose
credential integrations require its deliberately separate denylist model.

## Event Log & Artifact Store Invariants

`run_events` is append-only in the strict sense: the repository layer contains no
`UPDATE` and no `DELETE` against it, and must never gain one. The log is the
audit trail approvals are read out of and the timeline front ends render; a row
that can be rewritten afterwards is not evidence of anything. Nothing in the
`artifacts` table is updated either — `availability` records what was true at
finalization and reads probe the file and refine the answer without writing, so a
retention pass that later wants to mark a row `missing` would be the first
mutation in this area and should be reviewed as one.

A sequence number is per run, starts at one, and is allocated as `1 + MAX(seq)`
*inside* the transaction that inserts the row, so two appenders cannot be handed
the same number; `(run_id, seq)` is the primary key, so nothing reaching the
table another way can produce a duplicate either. Gaps are allowed and
monotonicity is not: pagination is `seq > last`, so a gap costs a reader nothing
while a repeated or reordered number silently changes what a timeline says
happened. `run_events` is `WITHOUT ROWID` so that key is the storage order.

A state change and the event describing it commit in one transaction. Everything
slow — redaction, encoding, and writing a spilled payload to disk — happens
before that transaction opens, so the pairing costs two inserts rather than a
filesystem round trip inside the write lock.

An event payload is the one caller value that is never refused for being too
large. Refusing it would lose history rather than protect anything, so a payload
over the 64 KiB inline threshold is written to an artifact and replaced inline by
a reference to it. Every other column keeps the bound in both directions as
before.

Event kinds are extensible the way the catalog is: `kind` is free text, a
spelling this build does not define decodes to an opaque entry rather than
failing the read, and adding a kind is never a migration. Do not turn the column
into a checked enumeration.

Every association an event or an artifact carries is composite with `run_id` —
`(step_id, run_id)`, `(tool_call_id, run_id)`, `(artifact_id, run_id)` — so the
database refuses one naming another run's record, exactly as it already refuses a
tool call whose denormalized run disagrees with its step. A NULL association
satisfies a composite key, so "no step" stays unconstrained while "that step"
must be this run's. A timeline naming another run's step is worse than a refused
write: nothing downstream re-checks it.

Artifacts live under `<data_dir>/artifacts/<run_id>/<artifact_id>`, a sibling of
`repositories/`, `worktrees/` and `locks/` and covered by the same
`HARKNESS_DATA_DIR` override. Finalization is **write, sync, rename, then insert
the row** — file first, always. Every other ordering can leave a metadata row
pointing at bytes that were never made durable, which a reader cannot tell from a
good row; this ordering can only leave an orphan file, which costs disk and
nothing else. Orphan collection is deliberately absent: a collector that ran
before the insert committed would delete live artifacts. That reasoning covers
crashes only — every path that fails for an ordinary reason removes what it
wrote, whether that is a refused metadata insert or a rejected event write
cleaning up its spill, because a caller retrying one must not leave a file per
attempt in a store with no collector.

An abandoned sink cleans up by *where the bytes currently are*, not by whether a
flag was set: before the rename it owes the `.tmp-` file, after the rename it
owes the destination, and once a sealed record has been handed to a caller it
owes nothing. A boolean cannot express that, and answering "the temporary file"
after a successful rename removes a path that no longer exists while orphaning
the artifact that does.

`storage_path` is derivable from `(run_id, id)` and is stored anyway so the
layout is legible from the database alone. It is compared against the derived
form on every read and a row that disagrees is refused by name; the path Harkness
actually opens is always rebuilt from the two identifiers, never joined from the
stored text. Artifact files are created `0600` and their directories `0700`,
because process output, Git stderr and tool errors all land here.

A missing or resized file degrades a read rather than failing one: metadata reads
probe the file and report `missing` or `size_mismatch`, and loading a run or
paging its events never opens an artifact at all, so deleting content cannot
break a run. Only asking for the content itself fails.

Every caller value that becomes durable here passes through a `Redactor` first —
payload string *values* and an artifact's label and media type through
`redact_text`, artifact content through `wrap_stream`, which sits above the
hasher so the recorded size and SHA-256 describe the bytes actually on disk. An
artifact's metadata is not exempt: a tool naming its artifact after the
credential it just leaked would otherwise persist it in the one place redaction
never looks. Object keys *are* exempt: a key is a published field name, and a
secret is a value. The v0.3 default changes nothing; the point of the hook is
that no write path bypasses it.

Each shape is redacted by its own method, exactly once. A spilled event payload
has already been through `redact_text`, so it is written in `Redaction::Applied`
mode and the stream wrapper does not run over it again. Passing the caller's
original through `wrap_stream` instead would be worse in both directions: a rule
implemented only in `redact_text` — which the trait permits — would scrub a
payload under the threshold and persist the same secret in the clear above it,
and a rule that did run would rewrite the object keys, so recovering a payload
would yield different field names depending only on its size. There is no public
route to `Redaction::Applied`; do not add one.

## Workspace Snapshot Identity Invariants

`HEAD` is never a workspace identity, anywhere. Identity is the composite digest
over the ten components ADR-0008 fixes, and it excludes exactly two things: the
snapshot's own id and its capture timestamp. Capturing one unchanged workspace
twice must yield two ids and one digest — the id names the capture, the digest
names the workspace, and the two are never used for each other's job.

Digests are domain-separated and length-framed. Every derivation absorbs a
constant naming what is being hashed and its version, and every component is
absorbed as its length followed by its bytes, so concatenation is injective and
a file-version hash cannot collide with a chunk hash over the same input. Path
sets are sorted by the exact path bytes and hold one entry per path, which is
what makes them order-independent; nothing may digest a path through its lossy
display form, because two names a lossy conversion folds together are two files.

A snapshot's three content digests are derived from its entry lists and never
stored independently of them. Loading a persisted snapshot re-derives all three
and the composite, and refuses the row by name when either disagrees, so a
hand-edited row cannot enter the process claiming an identity its own contents
do not support. The stored *order* is checked before anything is normalized, for
the same reason: sorting first would re-canonicalize an out-of-order list to the
digest it claims, and deduplicating first would drop a second row carrying a
different digest for a path already present. Every content-derived identity a
record merely asserts is re-derived on load too, `SymbolId` included.

A probe opens regular files and nothing else. `open(2)` on a FIFO with no writer
never returns and a character device never reaches end of file, so a capture that
opened one would hang with no way out — the cancellation token is polled between
files and inside the block-read loop, neither of which a blocked `open` reaches.
Anything that is not a regular file contributes a skip, decided from its metadata
before it is opened. A probe also resolves every path through a check that it has
no absolute or `..` component: the trait promises never to read outside the
worktree root, and `PathBuf::join` discards that root outright when handed an
absolute path, so the promise has to be enforced rather than assumed of callers.

Only a rename removes its source from the index. `StatusEntry::rename_source` is
populated for copies too, and a copy leaves its source in the index unchanged, so
recording it as absent would give a staged copy the same `index_digest` as a
staged delete of the source beside a staged add of the destination.

One path has one spelling. A directory recorded because it could not be expanded
and the same directory recorded because something inside it failed must not
differ by a trailing separator, or switching between the two reads as a removal
beside an addition while nothing moved.

An unreadable branch of the workspace must never make the rest of its tree
invisible. Collapsing a subtree to one sentinel freezes its digest, and a frozen
digest means every later edit beneath it verifies as `Fresh` — the exact false
negative this whole mechanism exists to prevent. A probe reports a failure inside
a tree per sub-path, so what was walked keeps taking part in identity and the
branch that failed is named on its own. A probe that caches anything about the
workspace invalidates it in `begin_read`, because a probe outlives a read and a
cached index served to a later verification answers from the past.

Capture and verification fail differently on purpose. Capture must never yield a
half-built identity, so cancellation is an error; verification always owes a
verdict, so a gone repository, a missing root, an unreadable status, and a
cancelled check all return `Unverifiable` with a reason. `Unverifiable` is not a
soft `Fresh` and must stop a mutation exactly as `Stale` does. An unreadable file
is neither: it contributes a stable sentinel and a capture diagnostic, because
one permission bit must not cost a whole snapshot.

Symlinks are hashed as their target *path string* and never followed, so a link
pointing outside the worktree changes identity without anything outside the
worktree being read. Snapshots hold hashes and paths only, never file contents.

The wire forms in `harkness-context` are frozen by committed fixtures because
they become persisted columns; changing a field, a variant spelling, or a
timestamp format after that is a `runtime.db` migration plus a new fixture.

## Context Engine & Index Cache Invariants

**The context engine persists nothing.** Every facade method returns a typed
value and writes no row anywhere; a snapshot becomes evidence only through
`Store::record_workspace_snapshot_for_run`, and that path is the sole producer
of `snapshot_captured`. ADR-0001's dependency direction makes it structural
rather than intended — `harkness-context` cannot name `harkness-runtime` — and
that is what keeps deleting the cache lossless. The corollary binds the other
way too: nothing in `harkness-context` may grow a route to `runtime.db`.

**`<data_dir>/context/` is disposable and `runtime.db` is not.** Deleting the
whole subtree at any moment costs warm-up time and no run history, provenance,
approval, or artifact. It is a supported recovery action, so "reclaim disk" and
"fix a weird index" are one command. The cache path is derived —
`<data_dir>/context/<repository-key>/index.db`, where the key is the v5 UUID
`harkness-git` already keys the repository lock by — never user-supplied, so
every linked worktree of one repository maps to one cache and there is no
traversal surface.

**A cache is read before it is written.** The metadata probe opens read-only, so
a cache written by a newer build is refused with `cache_version_conflict` and
left byte-identical rather than downgraded, mirroring the store's
`schema_too_new`. An older `schema_version` is quarantined and recreated —
the cache is disposable, so there is no downgrade path and none may be added.
A *component* version (parser, chunking, ranking) is the opposite: the file is
kept, the stored version is **not** rewritten, and the skew is reported for
incremental reconciliation, because overwriting it would erase exactly the
knowledge reconciliation needs.

**A busy cache is not a corrupt cache.** Contention, a permission bit, and an
exhausted descriptor table are all `cache_open_failed`; only a statement about
the file's *contents* may quarantine one. Reading a locked file as corruption
would let one front end destroy the other's index simply by being slow. A cache
recording a different repository identity is quarantined, because serving one
checkout's rows for another is the bleed the derived path exists to prevent.

**`index_generation` is a token, not a counter.** It is a component of the
snapshot digest, so a snapshot taken against a rebuilt index must never compare
equal to one taken against the index that produced it — and the counter lives in
the file being deleted, so a plain increment cannot promise that. It is seeded
from the wall clock in nanoseconds with `previous + 1` as a floor: a wiped
directory cannot reissue a number a stored snapshot already recorded, and a clock
that stepped backwards cannot either. The floor needs the previous value to be
readable, so a *corrupt* cache combined with a backwards clock is the residual
case where a generation could repeat; it is stated rather than closed, because
closing it means keeping the counter somewhere a user is invited to delete.
Quarantine keeps at most two files, named
with a fixed-width stamp so age order is name order, and takes the write-ahead
log and shared-memory sidecars with it rather than leaving a replacement to
recover somebody else's log.

**A cache that holds no connection says so.** A recreation closes the handle
before it unlinks the file, so a removal or a create that fails leaves the cache
open in name only. It reports `Unavailable` and generation `0` from that moment
— handing out the generation of a database that was just deleted would make a
capture record an identity nothing on disk supports — and the next `refresh`
reopens it. For the same reason an engine's remembered open failure is
*retried* by `refresh_index` and `dispose_index` rather than kept for the
engine's life: the commonest cause is a few seconds of contention, and an action
documented as the fix for a weird index has to be able to fix that one.

**One engine per project, one cache per repository, and neither lock is ever
held across the other's work.** The engine registry's mutex is not held while an
engine is opened, because opening one can wait out the cache's busy timeout. The
cache's connection lock is leaf-level: never held while the repository lock or
the catalog lock is acquired, so the repository-then-catalog ordering is
untouched. `index_status` takes neither and never blocks on indexing.

**Every facade method is blocking and cancellation-polled.** There is no async
runtime and this crate starts none, so a caller on a UI thread moves the call to
a worker itself. An already-cancelled token launches nothing. A method whose
implementation has not landed returns `not_yet_available` naming the feature —
a real, tested refusal, never a `todo!()` and never fabricated data.

**A `workspace_snapshots` row carries two independent version ladders.** Its
`schema_version` column is the runtime's and describes the envelope;
`payload_json` carries `harkness-context`'s own inside the document and is probed
against that ladder *before* its strict body is parsed, so a payload from a newer
build reads as an upgrade request rather than as a corrupt column. The payload is
not redacted, and must not be: it is bound by a digest the load path re-derives,
so rewriting a path inside it would refuse the very row the rewrite was meant to
protect. `id`, `project_id`, `snapshot_digest` and `captured_at` are lifted out
of the payload for queries and are *compared* against it on every read, exactly
as an artifact's `storage_path` is.
## Context Inventory & Classification Invariants

**One walk, four layers, first opinion wins.** Built-in denials, the global user
ignore file, the repository's own ignore file, then the repository's `.gitignore`
chain — and every later retrieval feature reads the inventory rather than the
filesystem, so two of them cannot disagree about whether a file exists. A layer
answers exclude, explicitly re-include, or nothing; an explicit re-inclusion
stops the descent, which is what gives the order meaning in both directions. The
repository layer may only tighten: its negations are discarded line by line and
reported, never merely outranked, because ADR-0006's rule is that repository
content narrows what Harkness reads and can never widen it. Nothing outranks
layer 1, which is checked against every parent directory as well as the path.

**A denied path is a count and never a name.** It is not an entry, not a
diagnostic, not a count keyed by path, and its content is never opened — the
whole point of denying at the walk is that no later stage has anything to
retrieve. A denied directory counts once and is not descended into, so
`denied_count` counts rules applied rather than files that exist. The test that
matters scans the *whole* rendered inventory rather than its entries. A symlink
is matched against layer 1 as both a file and a directory, because the walk will
not follow one to find out which it is and a directory-only denial would
otherwise let a link standing where a credential directory belongs be recorded
under its own name.

**A rule file is read on terms the repository does not choose.** The repository's
own file may not be *reached through* a symlink — neither the file nor any
directory on the way to it, because `lstat` on `.harkness/context-ignore`
resolves `.harkness` first and a check on the leaf alone answers about a file
outside the worktree while reporting that nothing was a link. That is how a
repository would aim the reader at `~/.ssh/id_rsa` and read the target back
through a diagnostic quoting the "pattern" that would not compile. The global
file, whose path the user supplied, is followed, and `.gitmodules` is read on the
same terms as a rule file even though its answer only picks a boundary spelling.
A `.gitignore` that is not a readable regular file is *reported*: dropping it on
its kind alone loses every rule it held with nobody told. The size bound is enforced on the bytes *read*
rather than on a stat, because a file grows between the two and procfs reports
zero for content that is not. A leading byte-order mark is stripped, since
leaving it in compiles a first pattern that matches nothing: a tightening rule
that silently stopped applying is the failure this whole layer exists to prevent,
and one reader serves all three configurable layers so the halves cannot drift.

**What a walk records is decided by the last stat, not the first.** A path listed
as a regular file and replaced by a symlink before it is opened is recorded as
the link it now is and never read — `File::open` follows one, and eight kilobytes
of somewhere else would otherwise reach a classification. The open is then
checked back against that stat, on Unix by inode and device, because narrowing a
race is not closing one; the platforms without a cheap identity keep the residual
and say so. A path that became a directory is reported rather than recorded,
since no entry about it would be true.

**A budget answers only while there is work left.** The walk asks whether its
stack is empty before it asks the clock, so an inventory that recorded every path
is never handed back marked truncated — a caller told to treat it as partial
would degrade a complete answer.

**The walk is ours and the rule engine is `ignore`'s**, and that split is
deliberate. `WalkBuilder` decides exclusion inside its own iterator, which would
hide from layers 1 to 3 whatever layer 4 removed, make `ignored_count`
unobservable, and let a `.gitignore` decide whether a credential was counted as
denied. Only `ignore::gitignore` is used, so glob semantics are not hand-rolled
either.

**The root comes from a captured snapshot, never from a caller's string.** There
is no entry point taking a bare path, so containment is answered once where the
workspace was read. Symlinks are recorded and never followed; a directory holding
its own `.git` is a boundary the walk stops at; the repository's own `.git` is
skipped rather than counted, because nothing excluded it.

**Truncation and cancellation fail differently, for the same reason capture and
verification do.** A file or time budget stops the walk and returns a *partial*
inventory carrying a typed truncation, which nothing may read as "the repository
has this many files". A cancelled walk returns `cancelled` and no inventory at
all, because a caller that stopped it did not ask for a subset.

**`eligible` is derived and never stored**, so it cannot go stale against the
class, the symlink flag, the boundary, or the unreadable flag it summarizes.
Classification itself is pure and total over exactly three inputs — a path, a
size, and at most `BINARY_SNIFF_BYTES` of opening bytes — so a persisted class
can be re-derived and compared rather than trusted. No file is read past that
window, and a file whose *name* already classifies it secret-sensitive is not
opened at all. `CLASSIFY_VERSION` covers the denial list and the classification
rules together; bumping it invalidates what was derived, and never silently
reclassifies evidence recorded under the old rules.

`docs/context-inventory.md` is the reference for the denial list, the class
precedence, and the bounds.

## Model Provider & Streaming Assembly Invariants

Three contracts, three names, and no type implements two of them: a **model
provider** streams text and tool-call *requests* and executes nothing, the
**native agent** owns the loop, and an **external coding agent** owns its own
and arrives behind the ACP milestone. ADR-0002 fixes that vocabulary and it is
enforced in rustdoc and in naming, which is weaker than a type check — a
proposal reintroducing the merged abstraction under a new name has to be
noticed by a reviewer. No public item in the workspace is named `AgentProvider`,
and `harkness-provider` carries a test over its own sources that says so.

`ModelProvider::stream` is **blocking and cancellation-polled**. It runs on the
caller's worker thread, polls the shared `harkness_git::Cancellation` at the
same 20 ms cadence every other blocking seam uses, returns kind `cancelled`,
and delivers nothing to the sink after the poll that observed it. An
already-cancelled token launches nothing at all, and that has to be polled
where the *work* is rather than only where events are: a turn that fails
without emitting anything would otherwise answer a cancelled run with a
provider failure, and a retry loop reading `rate_limited` off a run somebody
stopped would retry it. There is no async runtime and no HTTP client in this
crate; #125 introduces the client, and the manifest test naming that is what
makes its arrival deliberate.

`ProviderError` publishes exactly ten kinds and `RetryHint` classifies them
without policy: a hint answers *when* an identical request could be sent, never
*whether* to send one. `retry_hint` is `After` only when the provider named a
window, and `Never` for `cancelled` because the token that stopped the turn is
still cancelled — starting the work again is a new decision, not a retry.

**Assembly surfaces every call and drops none.** A call whose arguments do not
parse, one the provider never named, and one a disconnect cut short are all
recorded as `AssembledToolCall::Invalid` and never executed. A call the provider
left unnamed gets a deterministic turn-scoped id marked `Synthesized`, and a
call repeating an id an earlier call in the same turn used keeps both entries
with the second marked `duplicate_of` — merging them would run one call twice or
not at all. Provenance is recorded, never read back out of an id's spelling.

An index names one call within a turn: a second call at one index, a delta or
readiness for an index nothing started, and a second `TurnStarted` are each
`malformed_response`. Every accumulation is bounded — 1 MiB of arguments per
call, 8 MiB of text per turn, 256 calls per turn — and the bound is checked
against what *would* be held rather than after the fact. Exceeding one refuses
the turn by name; truncating instead would produce arguments the model never
wrote.

A turn that ends without the provider saying why is `disconnected` with the
partial turn attached, and one that produced no events at all is
`empty_response`; a sink answering `Stop` ends the turn as a *success* stopped
`AbortedBySink`, because Harkness stopped it. `AssistantTurn::stop` is what the
provider said and `TurnOutcome::stop` is what the call concluded, and the two
are deliberately not one field.

Every type that holds model-written text has a hand-written `Debug` that
previews its strings and bounds its list entries, so `{:?}` on a megabyte of
prompt stays under four kilobytes and cannot dump a conversation into a log
before #103's redaction applies. Bounding a list is not enough on its own: an
`AssembledToolCall` holds as much argument text as the per-call cap allows, so
it and its defect are previewed too — the turn a disconnect attaches to its
error is the rendering most likely to reach a log. Nothing in this crate can carry a credential:
there is no endpoint, header, key, or profile type in it at all.

Scripted scenarios are frozen v1 wire evidence. Their fixtures probe `v` before
a strict `deny_unknown_fields` body, the committed file is compared byte for
byte against the canonical pretty encoding, and changing a step, event, or
spelling means publishing a new version beside v1 rather than editing what v1
meant. Two replays of one scenario are identical down to `elapsed` and
`first_event_latency`, because a script advances its own clock instead of
reading a real one — a timing asserted with a sleep is not deterministic and a
timing measured from the machine makes an outcome unequal to itself.

## Tool Contract & Registry Invariants

A tool identifier and version are part of the published contract, not internal
names: they are persisted in `tool_calls.tool_id` and `tool_calls.tool_version`,
enumerated by `harkness contract`, and bound into approval scopes. Identifiers
use one narrow grammar — dot-separated lowercase ASCII segments, at least a
namespace and a verb — and versions are parsed as semantic versions so "the
latest version of an id" is decided by precedence rather than by string order.
Renaming either is a breaking change for every record that named it.

A registered `(id, version)` is immutable. The registry rejects a duplicate and
offers no way to replace or remove a registration, because a recorded call and an
approval both name a version and expect it to keep meaning what it meant.
Publishing a change means registering a new version beside the old one.
Descriptor enumeration is ordered by identifier and then by version precedence,
so any projection built from it is diff-stable regardless of registration order.

Build metadata is refused outright rather than accepted and ignored. The
specification requires precedence to disregard it, but `semver::Version` derives
`Eq` and `Ord` over it, so `1.0.0` and `1.0.0+hotfix` would be two registry keys
that compare unequal while denoting one version: the duplicate guard would not
fire, neither would look like a pre-release, and unversioned resolution would
silently move onto the build-tagged one while an approval bound to the plain
spelling still resolved to the original. An identity component the ordering must
ignore has no coherent meaning here; publishing a fix means bumping the patch.

Resolving without a version selects the highest *stable* version, falling back to
a pre-release only when nothing stable is registered. Raw semver precedence puts
`2.0.0-rc.1` above `1.10.0`, so an unfiltered "highest version" would mean that
registering a release candidate silently redirects every caller that named no
version — the documented default entry point — onto the candidate. Publishing a
pre-release must never change what production runs.

Both a tool id and a tool version are length-bounded, because both are persisted
in adjacent `tool_calls` columns held to the store's 64 KiB inline limit. Semver
places no limit on pre-release identifiers, so without the bound a version could
register cleanly and then make every record of its own calls unpersistable.

The same reasoning bounds everything a failure carries, and the bound belongs on
the projection rather than on the variants. `ToolError::as_failure` is how a
refusal gets recorded, so it clamps whatever it is handed; a message too large is
refused by the store as `payload_too_large` and leaves the call stuck in `running`
with nothing written about why it failed. Bounding variants alone leaves the
invariant one new variant away from breaking — and there are more sources than the
obvious one. A validator quotes the value it rejected. A JSON Pointer names the map
keys it traverses, so a caller choosing a long key chooses the pointer's length. A
tool flattening a `GitError` into `execution_failed` carries whatever stderr Git
produced. A panic payload can quote the tool's own input. Every field of a
`SchemaViolation` is truncated, its fields are private, and deserialization
re-applies the bound, so no construction path yields an unbounded one.

Cancellation is checked by the pipeline, not only by tools. `execute_json` gates
on the token before validating and again before the body, so a tool dispatched
after a cancel never starts even if it never polls. Stopping a call already in
flight still depends on the tool polling `check_cancelled`.

`ToolError::happened_before_execution` answers `true` for exactly one kind,
`invalid_input`, because that is the only one the pipeline itself raises before
calling the body. Do not widen it. `forbidden_path` in particular is raised by
`ExecutionContext::resolve`, which tools call mid-body, so treating it as
pre-execution would licence a retry that double-applies an earlier write.

`#[non_exhaustive]` on an enum does not seal its variants, so a tool *can*
construct `invalid_input` itself and return it after doing work. The erasure
boundary therefore re-attributes a schema error raised by the body to
`execution_failed`, keeping the detail in the message. Only the pipeline's own gate
may produce a kind that promises nothing ran.

`jsonschema` is built with `default-features = false`, which drops `resolve-http`
and `resolve-file`. A test asserts each refusal *names the missing feature*, not
merely that compilation failed — an unreachable host or a malformed file fails
either way, so a weaker assertion would stay green if Cargo feature unification
restored retrieval because some other workspace member depended on `jsonschema`
with default features. Note that the draft meta-schemas ship inside the crate and
resolve from its built-in registry; that is local resolution, not retrieval.

`ErasedTool` is sealed: `erase` is the only way to produce one. Without the seal a
hand-written implementation reachable through the public `register_erased` could
publish a descriptor unrelated to what it deserializes and skip the cancellation
gate, the panic boundary, and both validation gates, while `harkness contract`
still advertised it as validated — every guarantee in this section would be on the
honour system.

Tool output is re-serialized through `serde_json::Value` and every object key is then sorted by its
exact bytes, so a delivered result has canonical key order whatever order the tool declares its
fields in. Approval and provenance hashing depends on that stability. The sort is explicit rather
than inherited from the map type — see the canonical-key-order section below for why that
distinction is load-bearing.

Schemas are generated from the `Input` and `Output` associated types and never
declared by hand, so a published contract cannot disagree with the type the tool
body deserializes. Any type appearing in an `Input` or `Output` therefore needs
`JsonSchema` — including `ArtifactRef`, whose whole purpose is to be returned
inside an output. They are compiled at registration: a schema that cannot be
compiled is a refusal to declare the tool, not a surprise on the first call.
`schemars` closes an object schema only for a type carrying
`#[serde(deny_unknown_fields)]`, so every tool `Input` type must carry it —
otherwise an agent's misspelled field is discarded silently instead of reported.

Validation runs in both directions and the order carries the guarantees. Input is
validated before the body runs, so a rejected input means nothing executed and a
corrected retry is safe; it is also validated before policy evaluation, so policy
classifies what will actually execute rather than an unparsed blob. Output is
validated before delivery, so a consumer that trusted the published schema never
receives a shape it cannot handle. Both gates locate findings with RFC 6901 JSON
Pointers.

Declared risk and capabilities are frozen in the descriptor. A tool cannot lower
its declared risk for one call; whether a specific invocation is more dangerous
than its level suggests is decided when that invocation is evaluated. `RiskLevel`
has one definition and one total order — `observe < workspace_write < execute <
network < remote_write < destructive` — because a policy comparison that means
different things in different modules is not a policy.

The tool body is the only foreign code in the pipeline and runs under
`catch_unwind`; a panic becomes a structured `tool_panicked` error and leaves the
registry and calling thread usable, so one buggy tool cannot orphan a run record.
This relies on the workspace unwinding rather than aborting on panic; do not set
`panic = "abort"` without replacing the containment. A contained panic ends that
call rather than resuming it, and an abort is not a panic and is not contained.

`ToolError`, `RegistryError`, and the other stable namespaces each own a `KINDS`
table in declaration order with a round-trip test. Adding a variant requires
adding its kind; the two namespaces must not collide, because `harkness contract`
publishes their concatenation.

The resolved `(id, version)` accompanies an invocation on both its success and its
failure path, so a caller that asked for a tool without naming a version never has
to resolve twice to learn what ran — and two lookups can disagree where one
cannot. `InvocationError` therefore has no `From<ToolError>` conversion: building
a tool failure requires naming the tool, so a `?` cannot produce one that forgot.

## Agent Interface & Mock Scenario Invariants

`agent` is a plain-data decision seam. An `Agent` receives one redacted
`Observation` and returns one `AgentAction`; it never receives a registry,
policy evaluator, approval gate, store, scheduler, execution context, or tool
body. `CallTool.input` deliberately remains a `serde_json::Value`: the agent may
request work, but only the coordinator may validate, authorize, persist,
schedule, and execute it. In particular, the `invalid_tool_input` scenario must
emit its bad value verbatim and let the real registry return `invalid_input`
before any tool body runs. Adding a convenience method to `MockAgent` that
performs or pre-validates an action would be a privileged path a future model
agent does not have.

Agent-facing tool results and failures are constructed only through projections
that require the coordinator's `Redactor`; their fields stay private so raw
executor output cannot be wrapped directly. Result projection rewrites every
JSON string value recursively without rewriting object keys, while failure
projection rewrites caller-controlled detail and preserves the Harkness-defined
error-kind discriminant. Artifact references already came from the redacting
artifact store and are carried unchanged. These live result, failure, and
observation types do not implement public deserialization; persisted
observations decode only through the crate-private versioned record path.

`MockAgent` advances only through `Agent::next_action`. A scenario transition is
one structural observation pattern and one action; patterns omit incidental
record ids and may select only the stable fields their case is about, such as an
error kind, approval direction, or artifact media type. A mismatch returns a
typed `scenario_divergence` naming the expected and actual observation kinds
and does not advance the cursor. The ten built-ins are Rust data mirrored
byte-for-byte by versioned JSON fixtures. The registry order is stable, every
script is bounded, and exactly its final action is terminal.

Process scenarios name fixture executables rather than host utilities: their
argv re-executes the hermetic integration-test binary through an exact ignored
child test. The fixture harness installs platform-native links under those
names and scopes a prepended `PATH` to the coordinator invocation, so the real
cleared-environment process runner resolves the same bare name the frozen action
contains. A built-in must never depend on a POSIX-only utility or on a program a
test did not create explicitly.

Scenario fixtures probe `v` before their strict `deny_unknown_fields` body, so a
future fixture asks for an upgrade while a same-version unknown field is a
malformed current fixture. The fixture files are frozen wire evidence: changing
an action, pattern, field, or spelling means publishing a new version beside v1,
not editing what v1 meant. They do no I/O, read no environment, sleep on no
clock, and reach no network or model.

`AgentSessionState` is independently schema-versioned and strict. Its session id
names one conversation; its fixture version, definition digest, and cursor name
the exact next transition; its chained, domain-separated SHA-256 digest commits
to the observations already consumed without retaining workspace content. The
definition digest commits to the caller-supplied scenario id too, so the raw id
is not made durable or hidden from redaction. Recovery resolves that exact
retained definition and refuses a same-id script whose bytes differ. A resumed
mock continues the chain from the history digest. Session ids are not
determinism evidence — two replays may have different ids — while identical
observation histories must yield identical actions and digests.

Standalone actions and observations are persisted only through
`AgentActionRecord` and `ObservationRecord`, whose schema version is probed
before their strict body. The raw enums remain serializable because they are
embedded in the independently versioned scenario fixture. Their generic event
payloads encode the already-redacted versioned record as numeric bytes, so the
store's mandatory string redaction cannot rewrite enum tags, semantic versions,
or UUID spellings; observation decoding is crate-private. Checkpoints use the
same rule through `AgentSessionState::to_event_payload`: every machine identity
and digest is encoded as numeric bytes, because the store correctly redacts
every JSON string value. Decoding goes through `from_event_payload`; a raw
checkpoint or record JSON object is not an event payload.

## Tool Execution Invariants

A recorded tool call always reaches a terminal state. That is the executor's one
promise and everything else here serves it: a tool that panics, hangs, ignores its
token, floods a stream, or contradicts its own output schema becomes one recorded
failure of one call, never a crashed run or a row stuck in `running` with nothing
written about why.

The body runs on its own thread and the executor waits on a channel, because a
synchronous Rust function cannot be interrupted from outside — a timeout enforced
on the calling thread is not a timeout. When the grace period expires the worker is
*abandoned*, not killed, since Rust cannot kill a thread; it owns its whole
`ExecutionContext`, so nothing dangles. A tool that never polls its token therefore
leaks a thread per call, which is why cancellation is a contract rather than a
courtesy. A child *process* has no such caveat and is killed with its supervised
process tree.

A tool receives a token belonging to its own call, never the caller's.
`Cancellation` latches and has no reset, so enforcing a deadline by cancelling the
caller's would leave it cancelled for good — one slow step silently cancelling
every later call of a run that shares it, each recorded `cancelled` with nobody
having cancelled anything. The executor reads the caller's token and cancels the
call's, which costs one poll interval of propagation and is why cancellation
latency is ~20ms rather than ~0. A cancel that arrives before dispatch is seeded
onto the call's token directly, so the pipeline's gate still refuses to start a
body that was cancelled before it began.

Stopping means cancelling the token, so a body stopped by its deadline reports
`cancelled` — that is the echo of the executor's own decision, not evidence. A
`cancelled` or `timed_out` arriving after the executor decided to stop yields to
the executor's verdict; every other outcome, including a success completed as the
stop arrived, is the tool's to report and is recorded as it stands. Do not
"simplify" this into always taking one side: recording `cancelled` over a completed
side effect makes the history lie about what is on disk, and recording `cancelled`
for a timeout tells a user they stopped work they did not.

A timeout is persisted as `failed` with the `timed_out` kind, not as a lifecycle
state; the domain has none and adding one would be a migration for something the
kind already says. A result the store refuses for exceeding the inline bound is
likewise converted into a recorded `payload_too_large` failure rather than returned
as an error — returning one would strand the call in `running`, the exact outcome
the bound exists to prevent.

Timeouts are declared per tool and default from `RiskLevel`, following
`GitAccess::default_timeout`: local work is bounded by wall clock, anything
reaching a remote by cancellation alone. A caller may replace a declared limit
with any *finite* one, longer or shorter — only the author knows the usual case
and only the caller knows this one — but may never remove the bound. The
invariant is not "the tool's number wins" but "the call has a way to end", and
`ToolTimeout::OnlyByCancellation` is the author's claim that the body polls its
token, which nothing can verify and a caller therefore cannot assert on the
author's behalf.

There are two dispatches because a decision resumes the work it decided.
Approval-gated work never passes through `pending`: the domain moves a held
record from `awaiting_approval` straight to `running` *by* approving it, so there
is no moment at which an approved call waits to be dispatched separately.
`ToolExecutor::execute` therefore admits `pending` and `execute_approved` admits
`awaiting_approval`; each admits exactly one state. `ToolCall::dispatch_approved`
records the decision, pins the resolved version, and transitions in one step,
because an approval is a decision about `(id, version)` and a window in which the
call is approved and running while the row still names the version that was
*asked for* would make the audit describe work nobody authorized.

Neither entry point accepts a `running` call, and neither should be widened to.
That refusal is what stops a call being executed twice and its side effects
applied twice; telling a call abandoned by a dead process from one still
executing is a question about run ownership — the `owner_pid` column exists for
it — and is not the executor's to answer.

An executor may abandon an ordinary synchronous tool after the termination
grace because Rust cannot kill its worker thread. A built-in tool with a bounded
irreversible commit phase crosses that boundary through
`ExecutionContext::begin_irreversible` immediately before its first commit and
only after all validation is complete. The transition is one-way and races the
executor's stop request under one mutex: either the stop wins and the commit is
refused, or the commit wins and the executor waits for the body's real outcome
instead of persisting cancellation while workspace bytes can still change.
Third-party tools cannot enter this phase; making the hook public would let an
untrusted body disable the executor's abandonment guarantee indefinitely.

A supervised process unit is the unit of execution, so it is the unit that ends:
a Unix process group or a Windows Job Object. A Unix descendant may deliberately
detach into another session and outlive the portable process-group boundary; the
supervisor makes its pipe readers stoppable so that descendant cannot hold the
call open. A Windows child is assigned to its Job Object before it runs, so that
platform retains a whole-descendant boundary. When the direct child exits, the
supervisor is terminated before its output is collected: a pipe
reaches end of file only when every write end closes, and a child that started a
background helper leaves one open, so waiting for end of file would mean waiting
however long the helper runs. Signalling after the child has been reaped is sound
while any Unix group member is alive — the group keeps the identifier reserved —
and is a harmless `ESRCH` once none is. Closing a Windows Job Object configured
with `KILL_ON_JOB_CLOSE` provides the corresponding descendant guarantee. A
captured stream is *finished* on the stop paths rather than dropped: an unfinished
artifact deletes the bytes it staged, and a build log matters most when the build
was killed.

`tool_calls.tool_version` is the one column of a recorded request that is ever
rewritten, and only by `ToolCall::dispatch`, which pins the resolved version in the
same transition that starts the call. `update_tool_call` still names none of the
request columns; the dispatch runs one extra narrowly-scoped statement instead.
Resolving a second time later can disagree with the first, and an approval bound to
the recorded version would then no longer describe the work it authorized.

Progress travels over a bounded `sync_channel`, so a tool outrunning its consumer
blocks rather than queueing: progress describes work in flight, and an unbounded
queue reports the past while consuming memory in the present. A dropped receiver is
not an error — a consumer that gave up must not strand a running tool. Bursts are
appended in one transaction (`Store::append_events`), because a transaction per
event makes commit latency, not the work, decide whether a call finishes inside its
timeout; sequence numbers are still allocated one at a time inside that
transaction, so a batch is indistinguishable from separate appends.

Every buffer a child controls the size of is bounded: the retained stream tail, the
progress channel, the stderr segment channel, and the accumulating segment itself —
a program printing a megabyte with no newline must not be able to grow the reader's
buffer to match. Full streams go to artifacts through `ArtifactStream`, which is
why `ArtifactWriter::open` is the required method and `write` is provided in terms
of it: buffered and streamed content must not be able to take different routes to
redaction, hashing, and naming.

A tool's child starts with *no* inherited environment. `harkness-cli` runs from
hooks and from inside other processes, so the environment is not a place a decision
may come from; only the fixed baseline and exact names published by the tool
descriptor are copied into the empty environment.

A result the output schema refuses is never delivered, and the value is not thrown
away either: it is written as the `rejected-output.json` artifact of that call,
because only the value says what the tool actually produced. Preserving it is best
effort — a context with no artifact store must not change the failure the caller is
told about.

## Built-In Mutation & Process Tool Invariants

`fs.apply_patch`, `process.exec`, and `test.run` are registered at `1.0.0` through
`tools::register_mutating_tools`. Their descriptor risks are the scheduling and
policy contract: patching is `WorkspaceWrite`; both process tools are `Execute`
and declare that they spawn processes. No caller or registry helper special-cases
their identifiers to recover metadata the descriptors omitted.

A patch names every target twice: once in the unified diff and once in its base
precondition. The two path sets must be identical. An existing file carries the
lowercase SHA-256 of its exact bytes and a new file carries `null`; a mismatch is
`stale_patch`. Containment, every hash, and every hunk are checked against
in-memory base images before the first file is written, so a `patch_conflict`
never leaves a prefix of the call applied. Deletes, renames, copies, binary
patches, and missing parent directories are refused rather than interpreted as a
more destructive operation than this tool declares. Old and new patch headers
must name the same path. Every `.git` component and every symlink component is
refused, even when a symlink's target remains inside the workspace, so the audited
path is always the file that is replaced. Patch parsing and repository diff
production stay behind `harkness-git`, the owner of production Git behavior.

Each prepared file is replaced through a temporary file in the target directory:
write, preserve or apply the requested regular/executable mode, sync, rename,
then sync the directory. Immediately before each replacement, the lexical target
and `ContainedPath` are revalidated and its exact bytes or absence are compared
with the already-approved base image. An external edit therefore becomes
`stale_patch` rather than being overwritten. Cancellation has one final gate
before this bounded commit phase and is not reported between replacements, so a
partially applied call is never recorded as cancelled. The result artifact is
produced from libgit2's actual index-to-worktree diff over exactly the touched path
set, not by echoing the caller's patch.

`process.exec` has no shell form. `argv[0]` is the program and every later value
is one argument even when it contains shell metacharacters. The cwd crosses the
execution context's boundary, stdin is null, and the environment starts empty.
An input override may replace only a fixed baseline name or an exact name the
descriptor published; it can never enlarge that set. Both streams always go to
artifacts and only a bounded tail enters the inline result.

The child timeout defaults to 120 seconds and is clamped to 1 through 600. It is
inside the enclosing call deadline: reaching it kills the supervised process
tree and returns a typed result with `timed_out`, the enforced limit, duration,
signal, tails, and both artifact references. Reaching the enclosing deadline
remains a tool-call `timed_out` failure. `test.run` calls the same in-process supervisor —
never nested tool dispatch — and adds only `passed = exit_code == 0 &&
!timed_out`; a failing test is a valid test result, not a failed tool invocation.

## Built-In Read-Only Tool Invariants

`workspace.inspect`, `fs.read`, `workspace.search`, `git.status`, and `git.diff`
are registered at `1.0.0` through `tools::register_read_only_tools`. All five are
`RiskLevel::Observe`, declare no capabilities, and declare that they spawn no
processes — which is what lets the scheduler run them concurrently and takes them
off the global process limit. No read takes the repository lock, and none spawns
Git: status, head, ignore checks and diffs all go through `GitService`'s
in-process libgit2 verbs, and search is a walk in this process.

**Containment decides before anything opens a path.** Every path input crosses
`ExecutionContext::resolve` *first*. A check that opens or stats a path — the
symlink-component walk `workspace.search` runs on its root — comes after, because
running one on a caller's raw string answers "does this exist, is it a link, is
it a directory, is it readable" for any path on the host, and the distinct
refusals are a filesystem oracle whether or not a byte is read. That walk stays
lexical rather than moving to the resolved path, because resolution
canonicalizes: a root reached through a link inside the workspace would already
have been rewritten to its target and the link would be invisible. It folds `.`
and `..` first, since dropping a `..` would inspect a different path than the one
that was resolved.

**A path component that is not a filesystem entry is not probed.** A prefix and a
root cannot be links, and on Windows a canonical path begins `\\?\C:`, which names
the volume *device*; asking for its attributes fails with
`ERROR_INVALID_FUNCTION` rather than describing a file, so every search refuses
itself before reading an entry. `PathBoundary::escaping_symlink` skips them for
the same reason and the two walks must not drift.

**Every budget is charged, and nothing is discarded to pay for something else.**
Matches and omissions have separate byte allowances, because charging them to one
budget lets a workspace full of unsearchable files spend the whole response on
omissions and report "nothing found" for a query that matched. A per-file cap
ends that file rather than that line — a cap that ends only the innermost loop
emits one record per later matching line, unbounded. Exhaustion trims from the
tail and is always a named omission; clearing the answer is not a way to fit a
budget. A file that reads short for any reason other than the scan budget is
named too, never silently abandoning the rest of the walk.

**A result is bounded as it will be stored, not as it was read.** `max_bytes`
counts decoded bytes and JSON escaping is not a fixed factor over them — a
control byte becomes six characters — so `fs.read` measures its serialized result
against the store's inline bound and re-truncates until it fits. A tool that can
exceed that bound at its own defaults fails on ordinary input.

**A projection matches the producer it claims to match.** `git.status` reports a
rename under its *destination*: libgit2's status entry path is the delta's old
file, so taking it verbatim names the source twice and drops the new name.
`status.renames=copies` means Git's `-C`, never `--find-copies-harder`, so a copy
of an unmodified file stays an `added` path exactly as `git status` reports it.
Rename configuration is read the way Git reads it — `copy` beside `copies`, any
non-zero integer and a valueless key as true — and a spelling this build does not
recognize degrades to Git's default rather than failing the read, because the
value comes from untrusted repository content and from the user's global config.

**A `*_base64` sidecar is not redacted and must not be published beside a value
that was.** It carries the path's exact bytes, which is what makes a name no
lossy conversion can round-trip recoverable. `project_path` therefore stays
redaction-free: `git.diff` feeds one projection to two destinations that redact
differently — the inline result through `redact_inline_file`, the spilled artifact
through the store's `redact_payload` — and each route must redact exactly once.

## Runtime Scheduling Invariants

Mutation serialization is keyed by `WorkspaceKey` — `ProjectId` *and* canonical root,
canonicalized once when the key is built. Neither half alone is an identity: a path reused by
another catalog entry is a different workspace, and one project's linked worktrees are separate
checkouts that may legitimately be mutated at once. A key is never built from a lexical path, for
the same reason a trust decision is never stored against one.

Only the *front* of a workspace's queue is ever considered for dispatch. Nothing is scanned past
it, and that is the fairness story *within* a workspace: a queued mutation stops later reads from
being admitted, so a continuous stream of reads cannot starve it. Adding a "run the next admissible
call instead" optimization reintroduces starvation and must not be done.

Between workspaces the only contended resource is the global process limit, and it has its own
answer: a freed slot is offered to each workspace first in turn, from a rotating cursor, and the
workspace that released it takes part on the same terms as the rest. A fixed sweep order — or
letting the releaser re-admit before the others are asked — gives the lowest-ordered key a
permanent advantage, and two workspaces with a steady supply of process-backed calls would leave
one of them never starting. Nothing else here orders one workspace against another.

A parked submitter is counted on its workspace (`WorkspaceState::waiting`) and keeps that workspace
out of the idle sweep. `Condvar::wait_timeout` releases the workspace mutex for the duration of the
wait, so a producer blocked on a full queue holds *neither* lock; without the count it is invisible
and its workspace can be collected beneath it. The producer then wakes and pushes into an orphan,
the next submission for the same key builds a second `Workspace` with an empty running set, and one
worktree ends up with two mutation slots.

A worker releases its slots from a `Drop` guard, not from statements at the end of its closure. The
executor's panic boundary covers the tool body and nothing else, so a panic in the pipeline around
it unwinds the scheduler's own worker; without the guard the mutation slot is held forever, a
process slot is lost from a pool that never grows back, and shutdown can never reach idle again.

One recorded call has at most one claim on a scheduler at a time; a second submission is refused
with `already_scheduled`. The executor refuses a duplicate too, but only *after* dispatch, by which
point the loser has taken a workspace slot — and a second claim also makes
`ToolExecutor::cancel_undispatched` racy, since it reads a call's state and writes its terminal
state in two steps: a queued claim being swept could read `pending`, be overtaken by the other
claim's dispatch, and record `cancelled` over a body that had just started. The claim is released
when the call reaches a terminal state, so a genuine retry is never blocked. A workspace's running
set is additionally keyed by a per-scheduler dispatch sequence rather than by `ToolCallId`, which
keeps that collision unrepresentable rather than merely unreached.

A worker is counted into `Workers` by `admit`, under the workspace lock, not beside the
`thread::spawn` that follows. `stop` reads the running set under that same lock, so a call it can
see and cancel is one `wait_until_idle` is already obliged to wait for; counting later leaves a
window in which `shutdown` trips a token, observes no live workers, and reports a clean stop while
a worker is still about to start. The resulting order — workspace lock → `Workers` — cannot invert,
because nothing under `Workers` takes a workspace lock.

At most one call above `RiskLevel::Observe` runs per workspace at a time, and it runs alone —
reads do not overlap a mutation of the same worktree. This is a safety property, not a throughput
choice: concurrent mutations interleave index writes, and a read taken across one describes a state
that was never on disk. `RepositoryLock` remains the backstop beneath it and the two are not
redundant; the repository lock is keyed by Git's common directory and so covers every linked
worktree at once, while the scheduler's key covers one checkout.

Lock order inside the scheduler is **workspace map → one workspace → process limit**, never
reversed, and no two workspaces are locked at once. No scheduler lock is held across an executor
call, a store write, or a child wait: admission decides under the lock and dispatch spawns outside
it. The wider order is unchanged — **scheduler workspace slot → repository lock → catalog lock** —
and holds by construction, because the scheduler calls no catalog or Git code at all.

The process limit is global, `min(MAX_PROCESS_CONCURRENCY, available_parallelism)`, and its slot
is acquired *last* in an admission decision, after every workspace check has passed, so a slot is
never taken for a call that then fails one. Acquisition is a try and never a wait: a call that
cannot have a slot stays queued rather than occupying a thread hoping for one, which is what stops
the limit becoming a second way to deadlock. Whether a call needs a slot comes from the tool's own
`spawns_processes` declaration — risk is not a proxy for it in either direction — and a call that
spawns nothing takes none.

Every queue has a named capacity constant and a full one blocks its producer. Nothing is discarded
to make room: a dropped call is a run whose history omits work somebody asked for. A producer
parked on a full queue is woken by shutdown rather than left waiting for room that will never be
made.

A queued call that is cancelled is recorded `cancelled` *without being dispatched*. Dispatching one
in order to stop it would start a body, take a process slot, and write a `running` state for work
that never began. The terminal state is still written by `ToolExecutor::finish`, so every terminal
recording in the runtime is paired with its event in one transaction regardless of which layer
decided it. `cancel_run` trips the caller's token for running calls — that is what a user asking to
stop *is* — and the executor's own rule about never writing a caller's token is unchanged, because
the executor is not the one asking.

Shutdown cancels rather than abandons and then waits for its workers, so no child
process tree outlives the application. Dropping a `Scheduler` shuts it down; a
caller wanting a different deadline calls `shutdown` first.

## Interruption, Lease & Retry Invariants

A run reaches `interrupted` because a *recovery sweep* proved its owning process
gone, and for no other reason. Nothing inside a live process may write that
state: a coordinator that marked its own run `interrupted` because one call
ended without a verdict would be recording that the owning process stopped while
demonstrably still running. A call whose worker thread died is a fact about the
call — it becomes a `ToolFailed` observation carrying the `interrupted` kind, and
what to do about it is the agent's decision, exactly as every other tool failure
is.

**A lock file is the death signal; a timestamp is not.** A crashed process
writes nothing, so no column can be trusted to say its runs are abandoned. Every
coordinator holds one advisory lock file under `locks/` that the kernel releases
however the process ends, and a lock that can be taken is the proof. `renewed_at`
may only ever *widen* the window in which a claim is treated as alive: a held
lock outranks any timestamp, because a wedged-but-live process still holds the
workspace its runs are mutating, and only a claim that cannot be probed at all
falls back to the expiry grace. `LEASE_RENEW_INTERVAL` and `LEASE_EXPIRY_GRACE`
are the two published constants; a rule that let a stale timestamp end a live
process's runs would be the opposite of what this whole mechanism is for.

The lock is taken *before* the row exists and the row is written with the first
run that claims it. Reversing either would open a window: a row whose lock is not
yet held reads as dead to a concurrent sweep, and a row written at construction
would accumulate one per start of every read-only front end. A lease identity is
never reused, so removing a proved-dead lock file cannot make two coordinators
lock two inodes under one name.

**A sweep claims only what it can prove.** It runs once, at coordinator
construction, before any new work is accepted, under a short-lived exclusive
recovery lock; a live sibling process — the command line beside a running
application — is untouched. One transaction per run, so a poisoned record cannot
block the recovery of the other ninety-nine; a run that fails to recover is
reported in `RecoveryReport` rather than retried or silently skipped. The
candidate query reads state spellings and lease identities, never timelines, so
recovery is O(non-terminal runs) rather than O(events). A claim is read and
probed once however many runs point at it — marking the first writes the claim
off, and re-reading it would describe one death a hundred different ways.

Recovery only appends. No event is deleted and none is rewritten: the timeline up
to the moment the process stopped is exactly what that process wrote, and the
markings are new states with new events after it. A pending approval becomes
`Superseded` — the spelling the approval lifecycle already defines for "the run
will not resume, so the question no longer has a subject" — which is terminal, so
a prompt left open in a restarted front end authorizes nothing.

**A retry is a new run, never a rewind.** `retry_run` creates a fresh run for the
same task carrying `retry_of`, and appends one `run_retried` line to the
original, whose own state and history are otherwise untouched. It is permitted
from any terminal state but `succeeded`, and refused for a run whose record is
not terminal — which is also how a run another process is driving stays refused.
The decision is made on the persisted state alone and never on whether a worker
in this process is still winding down, because a retry offered the instant a run
shows `failed` must not succeed or fail by timing.

Retry carries no authorization forward. Grants are matched on the run they were
given for, so a fresh run id is what makes every protected call ask again;
nothing needs to expire an inherited grant because none exists.
`workspace_may_be_modified` is the honest half: true whenever the earlier attempt
started a tool call that could write, computed from persisted lifecycle —
`started_at` is set by the transition into `running` and by nothing else — and
never from whether the tool "probably" finished. A tool this build no longer
registers counts as one that could write, because the flag exists to warn rather
than to reassure. Only a retry may carry it; a run that follows nothing has no
earlier attempt to attribute a change to, and the wire form refuses the claim.

## Approval Invariants

An approval is persisted and committed *before* any surface is notified, and the
row and its `approval_requested` event share one transaction. There is no
event-free way to record one: a question nobody is told about is a run that
stopped for no visible reason, and a timeline entry with no row behind it is a
question nobody can answer. Restarting lists the pending requests with every
binding field intact, which is what makes answering one after a restart safe
rather than a guess about what was being asked.

No transaction spans the wait. The ticket is taken *before* the request is
persisted, the request is committed, the calling thread parks on a condition
variable keyed by `ApprovalId`, and the decision is a second short write.
Registering first is what closes the window in which a fast decision lands
between persisting and parking. An answer for an approval with no live ticket is
discarded rather than kept: a restart superseding an interrupted run's requests,
and a cancellation resolving approvals whose callers have exited, both resolve
approvals nobody is waiting for, and a gate keyed on answers rather than on
waiters would grow without bound in exactly those cases.

`grant_applies` is the security core and admits no partial application. The run
and the workspace identity bind every scope — together they stop a grant
replaying into another attempt of the same task or another checkout of the same
project. Each scope then adds the axes that give it meaning:

- `ExactCall` binds the recorded `tool_call_id`, the tool identity including its
  version, and the canonical input hash. Binding the call is what makes it *one*
  call, which is the whole point of reducing every remote-write and destructive
  request to this scope: authorizing one force push must not authorize a second,
  byte-identical one later in the same run. The input hash stays beside it rather
  than being made redundant by it, so an authorization cannot survive the input
  being re-derived differently.
- `ToolForRun` binds the tool identity, version included, and ignores the call
  and the input.
- `CapabilityForRun` compares **no** tool identity, only that the candidate
  declares at least one capability and that every capability it declares is
  covered. Comparing a version here would match one tool's version string
  against an unrelated tool's, so whether a grant covered a call would turn on
  two tools happening to share a number. Subset, not overlap: a tool needing
  `{network, fs.write}` must not run under a grant for `network` alone. A
  candidate declaring nothing matches no capability grant, or that scope would
  silently be the broadest in the system.

The matcher reads no clock and opens no transaction. A grant's lifetime is its
run; a request's `expires_at` is a deadline for a *human to answer*, and the only
thing it does is stop a lapsed request from being granted — enforced by
`ApprovalRequest::decide`, because a deadline nothing checks is advice rather
than a deadline. A lapsed request stays `pending` until something expires it, so
a caller that sets a deadline owes it a sweeper. Carrying the deadline into the
grant instead would make a `ToolForRun` approval given "for the remainder of the
run" stop applying part-way through one.

One approval has one waiter, and `ApprovalGate::ticket` refuses a second rather
than issuing it. Two tickets would share one registration, so whichever dropped
first would release it and leave the survivor parked on a key `resolve` no longer
finds — permanently, since a wait has no timeout. A refused ticket is a mistake a
caller can see.

An `ApprovalGrant` is projected from a granted request and has no constructor and
no lifecycle field. `granted` is terminal, so a request that reached it cannot
leave and every other state yields no grant at all — "a dead approval authorizes
nothing" is a shape the types hold rather than a check to remember. It is also
the only production source of a `policy::RunGrant`; policy cannot mint one, so an
`Ask` becomes an `Allow` only because the matcher accepted a grant.

Scope ceilings are enforced when a request is *created*, not when a grant is
matched, so a `RemoteWrite` or `Destructive` request that asked for a run-wide
scope is stored as an exact call and keeps both spellings. A record claiming a
breadth the matcher would never honor is a lie in the audit trail, not defence in
depth. A decision may narrow to the single call in front of a human and may never
widen, which is re-checked against the stored request rather than trusted from
the surface.

Absence of an answer is never consent, and never a resolution either. Closing a
window, dismissing a dialog, and losing a surface all leave the request pending.
Only an explicit decision, an expiry, a run cancellation, or a recovery sweep
resolves one, and the last three record `Expired`, `Cancelled` or `Superseded`
with **no** decision attached — the waiter still observes a denial, and the
record still says no human answered. Synthesizing a decision there would make the
audit claim one that was never made.

`canonical_input_hash` is frozen and versioned by its domain constant. Object
keys sort by UTF-8 bytes, arrays keep their order, integers and doubles have
disjoint spellings, strings escape only what JSON requires, and there is no
insignificant whitespace. A non-finite number is refused rather than encoded,
because it serializes as `null` and would fold two different inputs onto one
hash; no such `Value` is constructible today, but `serde_json`'s
`arbitrary_precision` is a feature Cargo would unify workspace-wide, so the guard
is load-bearing. Changing the encoding means a new domain constant and a new
committed fixture, never an edit to the existing one — every stored `input_hash`
was derived under it.

The `approvals` table is the request *queue* and is deliberately not the
per-record `approvals_json` audit history. That column is a bounded, ordered
trail of decisions already made about one record; this table is a question with
an identity, a lifecycle, an expiry, and the binding fields a grant is matched
on, which has to be listed across runs and answered from either front end. Its
update statement names none of the binding columns, so a resolution cannot
re-target the approval a human answered; the only one it may move is
`effective_scope`, and only downwards.

## External-Integration Policy & Approval Invariants

The eight v0.5 operations are typed by `ExternalCapability` and projected into the existing open
`Capability` vocabulary with these stable spellings: `launch_external_agent`,
`connect_mcp_server`, `invoke_mcp_tool`, `read_forge_resource`, `push_remote_branch`,
`create_pull_request`, `modify_forge_resource`, and `execute_workflow_recipe`. Do not add a loose
boolean beside them or duplicate their spellings in an adapter. An adapter declares the projected
capability; runtime policy owns its meaning.

Risk mappings are floors, never suggestions. Agent launch, MCP connection, and imported MCP tool
invocation are at least `Execute`; forge reads are at least `Network`; branch pushes, pull-request
creation, and forge mutation are at least `RemoteWrite`; a recipe is no lower than the maximum risk
of its compiled steps. A classifier may raise a floor and may never lower it. The three remote-write
operations remain exact-call approvals through the existing `RemoteWrite` scope ceiling.

Policy context binds an agent or MCP-server launch to its executable SHA-256, an MCP tool invocation
to its schema fingerprint, and a recipe execution to its content hash. A required hash that is
absent is a typed denial, not an unbound prompt. `IntegrationIdentity` is also copied into the
approval request and compared as one whole value for **every** scope; a changed hash, present versus
absent, or absent versus present defeats the grant. This comparison stays outside the scope match so
neither `ToolForRun` nor `CapabilityForRun` can bypass identity drift.

ACP permission options and MCP annotations are advisory audit context only. They are persisted in
the external policy context and never read to choose risk, verdict, scope, or grant applicability.
Repository policy may tighten an external capability to `Ask` or `Deny` and may never grant
`Allow`; an attempted repository grant invalidates the whole file and fails closed.

Every noninteractive external `Ask` becomes `Deny` with its capability-specific stable kind in the
CLI exit-code-3 family. The eight noninteractive kinds and the missing-identity kinds are published
by `harkness contract`; adding one means extending the shared table and its completeness tests, not
adding a front-end-only spelling.

`ExternalPolicyContext` is an explicitly versioned strict JSON value embedded in the existing
policy decision. Optional fields are omitted, same-version unknown fields are refused, and the
frozen request and decision fixtures pin its wire form. Approval identity columns were added by
runtime database migration 6 and are nullable so v5 rows retain their exact meaning; never edit the
released migration or v6 fixture.

## Protocol Transport Invariants

`harkness-transport` is the only place in the workspace that launches a protocol peer. An adapter
speaks to a `JsonRpcTransport`; it never calls `Command::spawn`, never holds a `ChildStdin`, and
never sends a signal. That is what makes a future remote transport a new implementation of one trait
rather than an edit to protocol logic, and it is why `shutdown` takes `self: Box<Self>` — teardown
has to consume the connection, and a by-value `self` would make the trait non-object-safe.

A peer's environment is **allowlisted, not scrubbed**. `SpawnSpec` starts from `env_clear` and admits
exactly the pairs a caller named; nothing is inherited and no wildcard exists. This is stricter than
`GitCommand`'s denylist on purpose and the two must not be reconciled: Git is one known program whose
credential helpers are the reason its runner keeps a denylist, while an agent is a program somebody
else wrote running on a user's workspace. The program and the working directory are absolute by
refusal rather than by convention, spawn is argv-only, and the child leads its own process group so
teardown reaches its helpers.

Everything a peer controls the size of is bounded, and the bound is enforced *before* the bytes are
held. The inbound line is refused the moment it crosses `max_message_bytes`, so the most this process
holds for a peer that never writes a newline is the limit plus one read chunk; a limit checked after
a line is assembled is not a limit. The inbound queue, the outbound queue, the peer-message queue,
and the retained stderr tail are each capped, and a full queue blocks its producer rather than
growing. Nothing is discarded to make room.

A message count is not a bound on memory, and the peer-message queue therefore carries a byte budget
beside its count. A peer picks both how many messages it sends and how large each one is, so it picks
their product: the count bound alone permits thousands of near-maximum messages and tens of gigabytes
with every per-message limit respected. Whichever bound is reached first stops the pump. Any new
queue holding peer-supplied content owes the same pair.

A connection that faults is **quarantined and never resynchronized**. A non-JSON line, JSON that is
not a JSON-RPC 2.0 message, a response to an id nobody sent, a second response to one already
answered, and an oversized line all end the conversation. The thread that observed the fault gets the
fault; every later caller gets `quarantined` naming it. Guessing where the next message starts is how
one bad line becomes a wrong answer, so there is no recovery path and none may be added. Nothing a
peer can put on standard output may panic a reader thread.

Standard error is captured and is **never** an error signal. The MCP specification reserves it for
free-form logging a client must not read as errors, so no byte written there fails a request,
quarantines a connection, or changes a `ShutdownOutcome`. `StderrSink` is a trait because the
destination is the artifact store, which lives above this crate; a sink method returns no `Result`,
because a destination that stopped accepting bytes is a reason to stop capturing and never a reason
to disturb a working conversation.

Teardown is `close stdin → wait → SIGTERM → wait → SIGKILL` against the process *group*, it runs on
`Drop` as well as on `shutdown`, and it is idempotent. Standard input is closed unconditionally and
before the child's exit is *waited on* — the writer thread parks on its queue until every sender is
gone, so a peer that had already exited would otherwise strand a thread teardown then waits on.
Whether the peer was *already* gone is sampled non-blockingly before that close, and has to be:
a well-behaved peer's read loop ends the instant its input pipe does, so asking afterwards cannot
tell a disconnect nobody noticed from the healthiest shutdown there is, and `AlreadyExited` would
be reported for both. A `try_wait` waits on nothing and joins nothing, which is what makes the two
orderings compatible rather than a contradiction. The
group is then killed on **every** rung, not only when the escalation is reached: the direct child
being gone is not the group being gone, and a peer that backgrounded a helper and exited politely
leaves that helper on the workspace *and* holding the standard-output pipe, so the reader never sees
end of file either. While any member is alive the group keeps its identifier, so the signal is exact
in the case it exists for; where the reaped child was the last member the group is already gone and
the call is a harmless `ESRCH`, with a residual race — a window of microseconds in which that pid was
recycled as another group's leader — that is stated rather than argued away. `harkness-runtime` takes
the same trade signalling a reaped tool child's group. `ShutdownOutcome` records the rung reached, because "this agent had to be
killed" is a bug report rather than an implementation detail. Windows has no `SIGTERM` and reports
`killed` rather than claiming an escalation that did not happen.

Correlation lives above the transport and disconnect kinds are finished there. The transport reports
a clean exit as `idle` because it does not know what anybody asked for; `Connection` refines that to
`exit_before_response` when the caller had a request outstanding. Outbound ids are allocated and
never reused within a connection, which makes a duplicate outbound id unrepresentable rather than
guarded against.

A request that gives up **retires** its id rather than forgetting it, and the two must not be
confused. A peer answering after this side's deadline passed is a peer behaving correctly — Harkness
chose the deadline — so that late answer is discarded quietly. Forgetting the id instead would read
it as a response to a request nobody sent, quarantine the connection, and make `request_timed_out`
terminal in practice while `is_terminal` says it is not; an adapter trusting that and retrying would
kill a live agent session over one slow call. A retired id records whether its one permitted answer
has arrived, so a genuine *second* answer is still `duplicate_response_id`. Retained ids are bounded
and evicted oldest-first; an answer arriving after eviction reads as an unknown id, which is the same
conclusion reached later.

The peer-message queue is bounded by the pump **refusing to read**, never by discarding: a dropped
peer request is one an adapter never answers, and a dropped notification is history that silently did
not happen. The bound therefore stalls the whole stream, and a caller waiting for a response behind
unread messages cannot wait that out — one ordered stream, a bounded application queue, and an answer
behind unconsumed messages leaves grow, discard, and fail as the whole of the choice. It fails, by
name, as `peer_queue_full`. Do not "fix" that by silently dropping the overflow or by removing the
bound; an adapter whose peer streams must consume what it sends.

There is deliberately **no dispatcher thread**. Whichever caller is already blocked holds the pump,
reads in poll-interval slices, routes what it finds, and re-offers it; a caller that is not leading
waits on a condition variable for the same slice. Lock order is pump → table, and the pump is never
taken while the pending table or the peer queue is held. Three OS threads per connection — stdout
reader, writer, stderr reader — is the whole cost, and a fourth must not be added to "simplify"
reading: a dispatcher blocked pushing into a full queue stalls every response behind it, which is the
same stall with another thread paid for.

A send that waits without reading is half of a deadlock, so `Connection` pumps between attempts
rather than blocking inside the transport. The other half is a peer that floods its own output and
stops reading its input until somebody drains it: this side's reader fills the inbound queue and
parks, the peer's pipe fills, and the two wait for each other. `try_send` therefore hands the message
*back* instead of taking it — returning it rather than requiring a clone is what keeps retrying
affordable for a large one — and `send` is the blocking convenience built on top. A blocking send
that does not drain must not be reintroduced.

A caller's deadline bounds its *send* as well as its wait. A peer that stops reading its standard
input fills its pipe and then the queue behind it, and an enqueue bounded only by the transport's own
backstop overruns a one-second caller by thirty. Which bound expired decides which failure it is, and
the distinction is load-bearing: the backstop expiring is a broken peer and is terminal, while the
caller's own deadline expiring is `send_timed_out`, which is not — a short deadline is not evidence
about the peer, and collapsing the two would let an impatient call end a working session.

Cancellation is polled every 20 ms in every blocking phase, well inside the workspace's 250 ms
visibility target, and an already-cancelled token launches nothing at all. `harkness_git::Cancellation`
is re-exported rather than wrapped, so an adapter passes down the token it already holds instead of
translating between two cancellation mechanisms.

## External-Integration Boundary Invariants

`harkness-acp`, `harkness-mcp`, `harkness-forge`, and `harkness-recipe` sit strictly below
`harkness-runtime`. None of them may name `harkness-runtime`, `harkness-cli`, or `harkness-gui` in
its manifest, and none may depend on another adapter — shared machinery goes below all four, not
sideways between two of them, which is where `harkness-transport` is. Each crate carries a test that
reads its own `Cargo.toml` and fails on any of those six names, so the rule breaks the build rather
than a review; `harkness-acp` adds two more against the same file, for a draft protocol feature and
for the async ACP SDK whose crate name is a prefix of the permitted schema crate's, and
`harkness-transport` carries the layering test against a longer list, since everything is above it.
The sideways rule
needs that test most: no dependency cycle exists to catch an adapter-to-adapter edge while
`harkness-runtime` does not yet name the adapters. ADR-0009 records why.

Protocol wire types are private to their adapter. No type defined by ACP, MCP, or the GitHub REST
API — and no type generated from their schemas — may appear in an adapter's public API, in a
`harkness-runtime` domain record, or in anything persisted: `runtime.db` columns, event payloads,
artifact metadata, `projects.json`, or CLI JSON output. Conversion to Harkness-owned types happens
at the adapter's public surface, which is the only place an upstream protocol revision may break.
A raw transcript captured as an artifact is opaque content, not a typed dependence, and is allowed.

External identity and trust are `harkness-runtime` types, not adapter types, because they compose
with workspace trust and policy. An adapter reports what it observed — a path, a hash, a version, a
schema fingerprint — as plain data; the runtime builds the record. Trust is per subject and bound to
that identity, never a boolean, and it is a precondition rather than an authorization: a trusted
agent still passes policy and approval on every action. An external permission system supplements
Harkness policy and never replaces it. ADR-0016 records the shape.

## Integration Identity & Trust Invariants

`harkness-runtime/src/integration` owns that model. A `TrustRecord` names a subject kind, the
`IdentityBasis` it was granted against, a scope, and a `granted_at`.

There is deliberately **no structural key for "the same grant"**, and none may be added. `check`
does not compare bases for equality — it ignores the display name and the executable path, and it
accepts a semver-compatible upgrade — so a key over those fields would be a key over a compatibility
relation, which is not a key. A revoked record and a later grant about the same subject would also
collide on such a key, and a store upserting on it would overwrite the revocation the state machine
exists to preserve. The matching relation is `check` itself: find the records for a subject, then
ask each one about the observation. A store addresses a row by its own row identity, never by a
projection of these fields, which `regrant` deliberately moves.

Every basis field is optional, because no subject has all of them — and that has one sharp edge a
constructor must close. A basis carrying *none* of the evidence its kind is recognized by reduces
`check` to comparing fields both sides leave empty, so it answers `Valid` for every observation ever
made. `require_evidence` therefore refuses such a grant at construction and on load: an agent needs
an executable, an MCP server an executable or endpoint, a tool schema a fingerprint, a recipe a
content hash, and a workspace a workspace-scoped grant. Both forge subjects need an endpoint naming
the *resource* and not merely the host — an account's login and a repository's path both live there,
and `check` never compares a display name, so a grant naming only `github.com` would answer `Valid`
for every other account or repository on it. `regrant` re-checks, so a re-grant is not how a record
loses the field that made it checkable.

`TrustRecord::check` is pure — no clock, no filesystem, no hashing — and is the only place trust is
decided. Two basis fields are deliberately not compared, and neither exclusion may be quietly
widened: a `display_name`, because ADR-0016 fixes that trust never binds to a name, and an
executable's *path*, because an identical binary reached through another path is the same program
while a different binary at the same path is not. Every other field is compared, and a field the
observation *lacks* invalidates rather than passing by absence — a deleted executable, a server that
stopped reporting its protocol revision, and an unreadable recipe are all drift, never validity.

`InvalidationReason::PRECEDENCE` is the fixed order a multi-trigger observation reports, and the
comparisons are a table in that order rather than a sequence of `if`s so the documented order and
the applied order cannot drift. The order is: the grant's reach, then evidence a subject cannot
misreport (executable digest, endpoint host, endpoint resource, schema fingerprint, content hash),
then what it may now do and who now configures it, then the version it reports for itself.

`Untrusted` is the initial state of the machine and what a lookup answers when no record matches; a
wire record spelling it is refused, because absence is what untrusted means. `Revoked` is terminal:
re-granting after a user said no is a new record, never a rewrite of the state they chose.
`Invalidated` is not, since nobody decided it — `regrant` rebases the basis and moves `granted_at`
on the same record. `Invalidated -> Revoked` exists because a re-prompt can be *declined*, and
without that edge a refusal after drift would leave the record saying only what it already said
before anybody was asked — collapsing the very distinction the state machine draws. An invalidation
reason is required by `Invalidated` and permitted nowhere else, so revoking clears it.

A workspace scope must name a rooted root, checked on the record rather than on
`TrustScope::workspace`, because the variant's field is public and a constructor can be routed
around. A relative root fails closed *silently*: the user is re-prompted forever with
`WorkspacePathChanged` and never learns why.

Neither that check nor `ExecutableIdentity`'s may use `Path::is_absolute` or `Path::has_root`.
Both answer for the platform doing the asking, and these are durable records that outlive the
machine that wrote them: on Unix `C:\agents\agent.exe` is neither absolute nor rooted, and on
Windows `/usr/local/bin/agent` is rooted but not absolute. Either predicate alone refuses a valid
record — the committed fixtures first, on the `windows-latest` leg of the `core` matrix job — and
refuses it with a reason that describes the reader rather than the record. `is_rooted_anywhere`
recognizes both conventions, which costs nothing because nothing in this module resolves a path.

Identity records carry no secrets — no tokens, no credential material, no `CredentialSource` — and a
test asserts every serialized record shape is free of fields named like one. Every text field is
bounded and refuses surrounding whitespace rather than trimming it, because trimming would make two
spellings compare equal in the record and unequal everywhere the value came from. An endpoint host
*is* lowercased, because DNS names are case-insensitive and two spellings of one host must not
compare as two hosts.

`INTEGRATION_RECORD_SCHEMA_VERSION` is independent of `RUNTIME_RECORD_SCHEMA_VERSION` so trust
records and run records evolve without dragging each other along; the scope and its workspace are
two flat wire fields rather than a tagged enum precisely so the strict body keeps
`deny_unknown_fields`, which serde's `flatten` would silently disable.

Pinned external versions: ACP protocol version 1 (ADR-0014), MCP revisions 2026-07-28 primary and
2025-11-25 fallback (ADR-0013), `X-GitHub-Api-Version: 2026-03-10` on every GitHub request
(ADR-0018), and `agent-client-protocol-schema` at a schema/v1 release with every `unstable_*`
feature off (ADR-0010). Targeting a newer protocol revision requires a superseding ADR, not a
version bump. No `tokio`, `async-std`, `smol`, or `futures` enters the workspace, and no `async fn`
appears in any crate (ADR-0003, ADR-0011).

Every persisted activity row and every user-facing presentation carries exactly one activity class:
`HarknessObserved`, `HarknessMediated`, `AcpReported`, `SnapshotInferred`, or `Unobserved`. An agent
claim is never rendered as a verified Harkness observation, and `Unobserved` is a class rather than
an omission — v0.5 has no OS-level sandbox and says so. ADR-0017 records why.

## ACP Handshake Invariants

`harkness-acp/src/wire.rs` is the only module in the workspace that names
`agent-client-protocol-schema`, and no type it defines appears anywhere else — not in this crate's
public API, not above it, not persisted. "Does a wire type escape" is answered by reading one `use`
list rather than by trusting a convention. Method names and the two JSON-RPC codes the adapter
branches on come from upstream too, checked against it by test, so a renumbering upstream is a
failing assertion rather than a request no agent answers.

Harkness offers `OFFERED_PROTOCOL_VERSION` — the latest it supports — and proceeds on any version in
`SUPPORTED_PROTOCOL_VERSIONS`. The two are separate constants because negotiation is "is the answer
one of ours", and that question does not change shape when the answer grows; a test holds the offered
version equal to the schema crate's own `LATEST`, so an upstream release moving it cannot make
Harkness silently offer a version it does not implement. Anything else closes the connection with
`unsupported_protocol_version` naming both sides, sends no further request, and never retries: a
mismatch is permanent until software changes. **The version is decided before any capability is
read**, so an agent speaking a version this build does not know is refused for the version rather
than for a capability shape that version was free to change.

**Capability advertisement is input, never policy.** The adapter sends exactly the three flags it was
handed and turns none of them on itself; each is a promise to mediate a request an agent may then
make, and #153 is the single authority for those. `AdvertisedClientCapabilities::default()`
advertises nothing.

**An omitted capability is an unsupported capability**, held structurally: every field of
`AcpAgentCapabilities` is a `bool` that is `false` unless the agent said otherwise, with no third
state for silence. An `Option<bool>` would let a caller ask whether the agent was *silent* about
`loadSession`, and the only honest answer to that is the one ACP already fixes. A capability whose
value has the wrong type decodes the same way and that is correct rather than lenient: a capability
object nobody can read is an agent with fewer features, not an agent that failed to answer.
`protocolVersion` is the one field with no default, so a response missing it is
`malformed_response` and not an ACP response at all.

**`authenticate` is gated on the agent's own advertisement, before anything is written.** An empty
`authMethods` means the agent wants no authentication, so sending one anyway is a request Harkness
should not have made rather than a question for the peer. No credential material passes through the
crate; v1's one method shape has the agent authenticate itself and Harkness only names which offered
way to use. A rejection is `authentication_failed` and stays distinct from a transport failure,
because #150 chooses between re-prompting a person and relaunching a program on exactly that
difference.

**Nothing may arrive from the agent during the handshake.** ACP gives it nothing to send before
`initialize` returns — no session to update, no file to read, no terminal to create — so a request or
notification in that window is `protocol_violation` and closes the connection. The check is exact
rather than heuristic: the transport delivers one ordered stream through one pump, so a peer queue
that is empty when the response arrives is proof the agent sent nothing.

`AcpError::kind()` follows the `GitError` convention, and the published namespace is
`AcpError::kinds()` — this crate's table followed by the transport's. A transport failure is carried
whole and keeps the discriminant #147 gave it rather than being re-spelled, which makes the namespace
a union exactly as `InvocationError` is; the two tables must not collide, and a test holds them
disjoint. `is_terminal` answers whether the connection survived and every variant answers it
deliberately.

An agent's method name reaching a Harkness diagnostic is clamped, because a peer must not choose how
long a Harkness message is. Its JSON-RPC `message` and `data` are not: they are prose whose whole
value is being complete, they live in an `AgentRefusal` field rather than inside a sentence Harkness
wrote, and the transport's `max_message_bytes` already bounds them. A caller making either durable
owes it the store's inline bound.

**The adapter launches nothing.** `AcpConnection::new` takes a connection that already exists,
because which executable may run is a trust decision bound to a digest and that decision is #150's
under ADR-0016.

## Canonical JSON Key Order

`serde_json::Map` is a `BTreeMap` — and therefore sorted — only until some crate anywhere in the
workspace enables `preserve_order`, at which point it becomes an insertion-ordered `IndexMap` and
Cargo unifies that choice onto every member. `agent-client-protocol-schema` requires that feature and
ADR-0010 requires that crate, so the workspace has already lost the free version of this property
once. Nothing may rely on it again.

Unification is per build graph rather than per repository, and that is the sharp edge: `cargo test
-p harkness-runtime` does not build `harkness-acp`, so it resolves `serde_json` *without*
`preserve_order` and sees a `BTreeMap`, while `cargo test --workspace` sees an `IndexMap`. A
byte-level assertion over an untyped `Value` can therefore pass under one command and fail under the
other. Anything that freezes such bytes must sort them, and anything that merely reads them must not
assume an order at all.

`harkness_runtime::canonical_json` is the one definition: every object key sorted by its exact UTF-8
bytes at every depth, arrays untouched, idempotent. Three places take it because their bytes are a
contract rather than a value — a delivered tool result that a recorded hash is taken over, a built-in
agent scenario mirrored byte-for-byte by a frozen fixture, and the CLI's published `--json` envelope.
A fourth place that freezes the bytes of an untyped `Value` owes the same call.

`approval::canonical` stays a separate encoder and must not be folded into it: that one writes a
canonical byte string for hashing, refuses what it cannot encode, and is frozen by a published domain
constant, while this one hands back a `Value` a caller goes on to serialize however it likes. Both
sort by exact key bytes rather than by any locale or character-wise ordering, so the order is a
property of the value and not of the platform that encoded it.

## Commit & Pull Request Guidelines

Write short, imperative commit subjects, matching history such as `Prevent concurrent imports from orphaning managed checkouts`. Keep each commit focused; append the PR number only when added by the merge workflow. Pull requests should explain the behavior change, testing performed, and relevant issue. Include screenshots for visible QML changes and call out platform or Qt/KDE dependency assumptions.

For commit-and-push-only requests, a failed `gh auth status` is not by itself a
blocker. Inspect the configured Git remote and retry the networked Git command
with the required elevated sandbox permission; prefer the repository's existing
SSH remote, and use HTTPS only when working credentials are available. Require
GitHub CLI authentication only for operations that actually use the GitHub API,
such as creating or editing a pull request.
