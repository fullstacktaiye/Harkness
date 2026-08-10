# Repository Guidelines

## Project Structure & Module Organization

Harkness is a Rust 2024 workspace split into six crates under `crates/`:

- `harkness-core`: project catalog, storage layout, cross-domain project workflows, and directory-listing logic shared by front ends.
- `harkness-git`: all production Git behavior: inspection, diffs and history, file context and hunk staging, branch and worktree mutation, commits, clone and synchronization, hermetic process execution, and repository locking.
- `harkness-test-fixtures`: hermetic repository, filesystem, and process fixtures shared only by crate tests.
- `harkness-runtime`: typed task, run, step, and tool-call records, the typed tool contract and registry every executable operation implements, the execution contracts shared by front ends, and the SQLite run store that makes those records durable.
- `harkness-cli`: the `harkness` command and its integration tests in `tests/`.
- `harkness-gui`: the Qt 6/KDE Kirigami application. Rust/CXX-Qt bindings live in `src/` and `cxx/`; UI components live in `qml/`.

Desktop integration assets are in `data/`. The root `CMakeLists.txt` provides release build and local installation support; Cargo remains the primary development interface.

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

Schemas are generated from the `Input` and `Output` associated types and never
declared by hand, so a published contract cannot disagree with the type the tool
body deserializes. They are compiled at registration: a schema that cannot be
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

`jsonschema` is built with `default-features = false`, which drops
`resolve-http` and `resolve-file`. A `$ref` to a URL or a local file therefore
cannot be followed: schema compilation reaches nothing outside the process.

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

## Commit & Pull Request Guidelines

Write short, imperative commit subjects, matching history such as `Prevent concurrent imports from orphaning managed checkouts`. Keep each commit focused; append the PR number only when added by the merge workflow. Pull requests should explain the behavior change, testing performed, and relevant issue. Include screenshots for visible QML changes and call out platform or Qt/KDE dependency assumptions.

For commit-and-push-only requests, a failed `gh auth status` is not by itself a
blocker. Inspect the configured Git remote and retry the networked Git command
with the required elevated sandbox permission; prefer the repository's existing
SSH remote, and use HTTPS only when working credentials are available. Require
GitHub CLI authentication only for operations that actually use the GitHub API,
such as creating or editing a pull request.
