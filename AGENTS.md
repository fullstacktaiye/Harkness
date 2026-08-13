# Repository Guidelines

## Project Structure & Module Organization

Harkness is a Rust 2024 workspace split into seven crates under `crates/`:

- `harkness-core`: project catalog, storage layout, cross-domain project workflows, and directory-listing logic shared by front ends.
- `harkness-git`: all production Git behavior: inspection, diffs and history, file context and hunk staging, branch and worktree mutation, commits, clone and synchronization, hermetic process execution, and repository locking.
- `harkness-context`: the context engine's typed vocabulary — workspace snapshot identity, stable identifiers, provenance, and file classification.
- `harkness-test-fixtures`: hermetic repository, filesystem, and process fixtures shared only by crate tests.
- `harkness-runtime`: typed task, run, step, and tool-call records, the typed tool contract and registry every executable operation implements, the execution contracts shared by front ends, and the SQLite run store that makes those records durable.
- `harkness-cli`: the `harkness` command and its integration tests in `tests/`.
- `harkness-gui`: the Qt 6/KDE Kirigami application. Rust/CXX-Qt bindings live in `src/` and `cxx/`; UI components live in `qml/`.

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
managed projects remain v1-compatible, while the first worktree requires v2.
Read-only operations must never rewrite the file.

Additive optional fields must deserialize missing values to a safe default and
must be omitted when absent. Any new `ProjectSource` variant or other data an
older build cannot preserve requires a catalog version bump and a frozen JSON
fixture. Same-version unknown fields are rejected instead of being silently
dropped on the next write.

New durable JSON formats use explicit schema versions and RFC 3339 UTC
timestamps. The project catalog's human-readable `time` encoding is a legacy
exception that remains frozen until a future catalog v3 migration; do not copy
it into new formats. JSON-backed path fields currently require UTF-8, so
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

## Run Store Schema & Connection Invariants

Run history lives in `runtime.db` beside `projects.json`, never inside it. The
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

Tool output is re-serialized through `serde_json::Value`, whose object map is a
`BTreeMap`, so a delivered result has canonical key order whatever order the tool
declares its fields in. Approval and provenance hashing depends on that stability.

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
courtesy. A child *process* has no such caveat and is killed by process group.

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

A process group is the unit of execution, so it is the unit that ends. When the
direct child exits, the group is signalled before its output is collected: a pipe
reaches end of file only when every write end closes, and a child that started a
background helper leaves one open, so waiting for end of file would mean waiting
however long the helper runs. Signalling after the child has been reaped is sound
while any member is alive — the group keeps the identifier reserved — and is a
harmless `ESRCH` once none is. A captured stream is *finished* on the stop paths
rather than dropped: an unfinished artifact deletes the bytes it staged, and a
build log matters most when the build was killed.

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
Only an explicit decision, an expiry, or a run cancellation resolves one, and the
last two record `Expired` or `Cancelled` with **no** decision attached — the
waiter still observes a denial, and the record still says no human answered.
Synthesizing a decision there would make the audit claim one that was never made.

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

## Commit & Pull Request Guidelines

Write short, imperative commit subjects, matching history such as `Prevent concurrent imports from orphaning managed checkouts`. Keep each commit focused; append the PR number only when added by the merge workflow. Pull requests should explain the behavior change, testing performed, and relevant issue. Include screenshots for visible QML changes and call out platform or Qt/KDE dependency assumptions.

For commit-and-push-only requests, a failed `gh auth status` is not by itself a
blocker. Inspect the configured Git remote and retry the networked Git command
with the required elevated sandbox permission; prefer the repository's existing
SSH remote, and use HTTPS only when working credentials are available. Require
GitHub CLI authentication only for operations that actually use the GitHub API,
such as creating or editing a pull request.
