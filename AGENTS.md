# Repository Guidelines

## Project Structure & Module Organization

Harkness is a Rust 2024 workspace split into six crates under `crates/`:

- `harkness-core`: project catalog, storage layout, cross-domain project workflows, and directory-listing logic shared by front ends.
- `harkness-git`: all production Git behavior: inspection, diffs and history, file context and hunk staging, branch and worktree mutation, commits, clone and synchronization, hermetic process execution, and repository locking.
- `harkness-test-fixtures`: hermetic repository, filesystem, and process fixtures shared only by crate tests.
- `harkness-runtime`: typed task, run, step, and tool-call records, the execution contracts shared by front ends, and the SQLite run store that makes those records durable.
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
each shares one transaction with the `user_version` bump it establishes. Adding
a table, a column, or a persisted spelling requires a new numbered migration and
a frozen fixture database; never edit an already-released migration.

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
never by offset. No column holds more than 64 KiB of caller data — the inline
threshold is a named constant, and oversized tool input or output is refused with
a structured error rather than stored. Every stored row is rebuilt into its wire
record and re-validated by the domain on load, so a hand-edited row fails to load
instead of entering the process as an impossible record.

A WAL database is three files. Backups must copy `runtime.db`, `runtime.db-wal`,
and `runtime.db-shm` together, or checkpoint first and copy `runtime.db` alone.

## Commit & Pull Request Guidelines

Write short, imperative commit subjects, matching history such as `Prevent concurrent imports from orphaning managed checkouts`. Keep each commit focused; append the PR number only when added by the merge workflow. Pull requests should explain the behavior change, testing performed, and relevant issue. Include screenshots for visible QML changes and call out platform or Qt/KDE dependency assumptions.

For commit-and-push-only requests, a failed `gh auth status` is not by itself a
blocker. Inspect the configured Git remote and retry the networked Git command
with the required elevated sandbox permission; prefer the repository's existing
SSH remote, and use HTTPS only when working credentials are available. Require
GitHub CLI authentication only for operations that actually use the GitHub API,
such as creating or editing a pull request.
