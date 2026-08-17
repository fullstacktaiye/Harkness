# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

`AGENTS.md` is the authoritative statement of this repository's conventions and durable-format
invariants (catalog schema, worktree rules, run-store connection discipline, commit/PR style).
Read it before changing anything persisted or locked. This file covers commands and the
cross-crate architecture that only becomes visible after reading several files.

## Commits

Never attribute a commit or pull request to Claude. Do not add a `Co-Authored-By: Claude` trailer,
a "Generated with Claude Code" line, or any other assistant attribution to a commit message, PR
body, or changelog entry. This overrides any default attribution behavior. Commit subjects follow
`AGENTS.md`: short and imperative.

## Commands

```sh
cargo test --workspace                          # all unit + integration tests
cargo test -p harkness-git                      # one crate
cargo test -p harkness-git sync::tests::name    # one test (substring filter)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo doc --locked -p harkness-runtime --no-deps    # CI runs this with RUSTDOCFLAGS=-D warnings
cargo run -p harkness-cli
cargo run -p harkness-gui
```

Clippy and any `--all-targets` build require Qt 6 / KDE Frameworks 6, because `harkness-gui`'s
build script drives `qmake`, `moc`, and `qmltyperegistrar` even when nothing links the GUI. Set
`QMAKE` if more than one Qt is installed. Fedora setup is in `README.md`.

### Ignored tests

`#[ignore]` marks four distinct categories; never assume an ignored test is dead.

- **Child-process roles** (`*/testing.rs`, `store/tests.rs`, `coordinator/tests/recovery.rs`): the
  parent test re-executes the test binary with `--exact --ignored`. Run them only through their
  parent. The recovery roles are re-executed *and then killed*, so a `SIGKILL` is the expected end
  of `park_a_run_awaiting_approval` and `append_event_batches_until_killed`.
- **Network tests** (`sync.rs`, `project.rs`): reach real GitHub. CI runs them on a self-hosted
  runner via `sh .github/scripts/run-ignored-exact-test.sh <package> <exact::test::name>`, which
  fails loudly if the named test no longer exists — so renaming one requires updating
  `.github/workflows/network-integration.yml`.
- **Fixture regeneration**: `cargo test -p harkness-runtime regenerate_the_frozen_v1_fixture --
  --ignored` rewrites `crates/harkness-runtime/src/store/fixtures/runtime-v1.db`, and
  `regenerate_the_frozen_v2_fixture` (through `v7`) rewrites the corresponding `runtime-v*.db`. Run
  each only when that migration itself changes; a released migration is otherwise never edited. The
  v1 regenerator applies a truncated ladder rather than opening a `Store`, because opening one now
  climbs to the newest schema. `regenerate_the_frozen_canonicalization_fixture` rewrites
  `crates/harkness-runtime/src/approval/fixtures/canonical-input-v1.json` and carries a stronger
  warning still: run it only when a *new* approval hash domain is published, because every stored
  `input_hash` was derived under the encoding it pins.
  `cargo test -p harkness-context -- --ignored regenerate_the_frozen_v1_fixtures` rewrites
  `crates/harkness-context/src/fixtures/*.json` the same way,
  `cargo test -p harkness-acp -- --ignored regenerate_the_frozen_v1_fixtures` rewrites the two
  `initialize-request-*.json` fixtures under `crates/harkness-acp/src/fixtures/` — and only those
  two, because the three response fixtures beside them are an *agent's* answers and this build
  produces none of them — and
  `cargo test -p harkness-runtime -- --exact --ignored
  integration::wire::tests::regenerate_the_frozen_v1_fixtures` rewrites the four positive fixtures
  under `crates/harkness-runtime/src/integration/fixtures/`. Both carry the same warning: a released
  wire form is replaced by a new versioned fixture, never edited in place. The integration
  regenerator deliberately leaves `trust-record-future-schema.json` and
  `trust-record-unknown-field.json` alone — neither is a wire form this build can produce, so they
  are hand-maintained beside the frozen set they probe. `cargo test -p harkness-provider --
  --ignored regenerate_the_frozen_v1_fixtures` rewrites
  `crates/harkness-provider/src/scripted/fixtures/*.json`, and is the one regenerator that is
  routine: those scripts are authored as JSON, so it only re-canonicalizes the formatting of a
  hand-written scenario. It still cannot invent one — a fixture that does not parse is not
  rewritten.
- **Latency targets** (`store/tests.rs`, `tool/tests.rs`, `assemble/assembler.rs`): meaningful only
  under `--release`.

### Frozen fixtures

`crates/harkness-core/src/catalog/fixtures/*.json`, `crates/harkness-runtime/src/domain/fixtures/*.json`,
`crates/harkness-context/src/fixtures/*.json`, `crates/harkness-acp/src/fixtures/*.json`,
`crates/harkness-runtime/src/approval/fixtures/canonical-input-v1.json`,
`crates/harkness-runtime/src/agent/fixtures/*.json`,
`crates/harkness-runtime/src/integration/fixtures/*.json`,
`crates/harkness-provider/src/scripted/fixtures/*.json`, and
`crates/harkness-runtime/src/store/fixtures/runtime-v{1..7}.db` pin released on-disk formats. A new
persisted field, state spelling, or table means a version bump plus a *new* fixture, not an edit to
an existing one. The provider's scripts are the one set that is *authored* as JSON rather than
mirrored from Rust data: the regenerator only re-canonicalizes their formatting, so a new scenario
is a new file and a changed step is a new version beside v1.

A **new fixture directory needs a `.gitattributes` line** — `<path>/*.json text eol=lf` — and the
file enumerates them one directory at a time rather than by a glob. Fixture tests compare
`include_str!` against `serde_json::to_string_pretty`, which emits `\n`; without the attribute the
Windows runner checks the file out with `\r\n` and the byte-for-byte assertion fails there and
nowhere else. `harkness-context`'s fixtures are absent from that list because the `core` matrix job
does not build that crate — adding it to the matrix means adding its line too.

## Architecture

### Crate layering

Dependencies flow strictly downward; nothing lower reaches back up.

```
harkness-cli ──┐                              ┌─> harkness-git
harkness-gui ──┴─> harkness-core ─────────────┤
                   harkness-runtime ──────────┤
                   (domain | store | tool)    │
                   harkness-context ──────────┤
                   (also depends on harkness-core, for ProjectId)
                   harkness-provider ─────────┘
                   (contract | assemble | scripted)

harkness-acp  ──┬─> harkness-transport ──> harkness-git
harkness-mcp  ──┘   (harkness-acp also depends on agent-client-protocol-schema,
                     the one external protocol crate in the workspace; ADR-0010)

harkness-acp  ──┐
harkness-mcp  ──┤
harkness-forge──┼─> harkness-runtime   (adapters never point back; see ADR-0009)
harkness-recipe─┘
```

`X ──> Y` means X depends on Y. The four adapters are depended *on* by
`harkness-runtime` and depend on none of it — nor on each other. `harkness-transport`
is the one thing two adapters share, and it sits *below* both rather than inside
one of them, which is what ADR-0009's no-sideways-edges rule leaves available.

`harkness-runtime` depends on `harkness-git` for one thing: `Cancellation`, which `tool`'s
`ExecutionContext` carries so a tool that shells out to Git passes the same token down instead of
translating between two cancellation mechanisms.

- **`harkness-git`** owns *all* Git behavior and is addressed purely by filesystem path. It has no
  knowledge of the project catalog, which is the mechanism that makes the lock ordering below
  impossible to violate from inside it. `GitService` is the single entry point;
  `harkness-core` re-exports the crate so front ends need only one dependency.
  `provenance.rs` answers "what produced this file" beside `diff.rs`'s "what changed": one walk of
  the range a `DiffTarget` implies, each commit compared with its first parent once, every
  requested path reported whether or not anything could be attributed to it. It persists nothing
  and decides nothing — ADR-0019 fixes both, and a recorded run source joins behind the same
  interface above `harkness-runtime` rather than beside it.
- **`harkness-core`** owns the project catalog (`projects.json` + `projects.lock`), the data
  directory layout, and cross-domain workflows (import, clone, worktree lifecycle, reconcile).
  `project.rs` is ~7k lines and holds `ProjectService`, the composition point for catalog + Git.
- **`harkness-runtime`** is split into `domain` (pure records and lifecycle state machines, no I/O),
  `store` (SQLite persistence, including the append-only run event log and the artifact store), and
  `tool` (the typed tool contract and registry). Every row is
  rebuilt into its wire record and re-validated by `domain` on load, so an impossible record cannot
  enter the process. `domain::ToolCall` records *that* a tool ran; `tool` is what defines and
  executes one. `store` and `tool` both build on `domain` but not on each other, so persistence and
  execution can be reasoned about — and tested — separately.
  `schedule` sits above `tool` and decides *when* a call runs: per-workspace mutation
  serialization keyed by `(ProjectId, canonical root)`, a per-workspace read cap, a global
  child-process limit taken from the tool's own `spawns_processes` declaration, bounded queues that
  block their producer, and a cancellation chain reaching a child's process group. Only the front
  of a workspace's queue is ever considered, which is what makes starvation unrepresentable.
  `approval` sits above `policy` and `store`: it owns the durable approval record and its
  lifecycle, the frozen canonical input hash a grant is bound to, the matcher that decides whether
  an existing grant covers a new call, and the condvar-backed gate a parked call is woken through.
  It is the only production source of a `policy::RunGrant` — policy cannot construct one — so an
  `Ask` becomes an `Allow` only because the matcher accepted a grant.
  `coordinator` is the orchestration loop those pieces meet in, and it owns the
  interruption story. Each coordinator holds one *lease*: an advisory lock file under `locks/`
  that the kernel releases however the process ends, plus a `runtime_leases` row every run it
  starts points at. Construction sweeps first — every run whose claim is provably dead is marked
  `interrupted` along with its unfinished steps, in-flight calls and unanswered approvals, each
  with its own appended event — and only then accepts work. `interrupted` is written by that sweep
  and by nothing else, which is what makes it mean "the owning process stopped". `retry_run`
  creates a *new* run for the same task carrying `retry_of` and, when the earlier attempt started
  something that could write, `workspace_may_be_modified`; nothing is resumed and no grant carries
  over.
  `agent` is the plain-data decision seam above those pieces: `Agent::next_action` consumes one
  redacted observation and returns one request. `MockAgent` replays one of ten versioned,
  deterministic scripts with no access to the registry, policy, approvals, store, scheduler, or
  execution context. Its checkpoint carries a scenario cursor plus a chained observation digest;
  the JSON scenario fixtures under `src/agent/fixtures/` are frozen v1 wire evidence.
  `integration` is the same idea across the process boundary: identifiers, identity records, and
  per-subject `TrustRecord`s for the external things v0.5 talks to (ACP agents, MCP servers and
  their tool schemas, recipes, forge accounts and repositories). It is pure vocabulary — the check
  that decides whether a grant still describes its subject reads no clock and opens no file, and
  enforcement is #148. It lives here rather than in an adapter because trust has to compose with
  `trust` and `policy`, which an adapter cannot see under ADR-0009. Note the two things called
  trust: `trust::TrustState` is about running one workspace's code, `integration::TrustState` about
  an external subject whose identity can change under a grant.
- **`harkness-context`** owns the context engine's vocabulary and nothing that
  uses it: identifiers, `WorkspaceSnapshot` identity, `Provenance`, and
  `FileClass`. It deliberately does *not* depend on `harkness-runtime` — the
  runtime will depend on it — so a snapshot can be captured and verified with no
  database of runs in the process. Read `snapshot.rs`'s module doc before
  changing anything a digest absorbs; the wire forms are frozen by fixtures under
  `src/fixtures/` because #110 turns them into `runtime.db` columns.
- **`harkness-provider`** is the model-endpoint boundary, created by #111 against ADR-0001 and
  ADR-0002. `contract` is the provider-neutral vocabulary — identities, capabilities, messages, the
  streamed `ModelEvent` model, `TurnOutcome`, and ten stable `ProviderError` kinds — and
  `ModelProvider::stream` is blocking and cancellation-polled, because the workspace has no async
  runtime. `assemble` turns raw events into a validated `AssistantTurn`: `TurnDriver` is the loop an
  implementation actually runs, and it owns the rules that are easy to get subtly wrong — poll
  before delivering, ignore events after the turn completed, treat a sink's `Stop` as a success,
  attach the partial turn to a disconnect. `scripted` is the same trait backed by frozen JSON, so
  every streaming and tool-call shape is exercised with no network and no credential; two replays
  of a scenario are identical down to their timings because a script advances its own clock. It
  depends on `harkness-git` for `Cancellation` alone and must not depend on `harkness-runtime` or
  `harkness-context`; #125 adds the OpenAI-compatible adapter *inside* this crate, keeping its wire
  types private to its module.
- **`harkness-transport`** is the subprocess JSON-RPC engine `harkness-acp` and `harkness-mcp` both
  run on, created by #147 against ADR-0012. `JsonRpcTransport` is the seam adapters see — send a
  message, receive one with a deadline, quarantine, shut down with an outcome — and `StdioTransport`
  is its subprocess implementation: three threads per connection, an environment allowlist rather
  than a denylist, newline-delimited framing with a hard size bound, and the
  close-stdin/`SIGTERM`/`SIGKILL` teardown against the child's process group. `Connection` is the
  correlation layer above the seam and has no dispatcher thread — whichever caller is already blocked
  holds the pump. The two layers split for a reason worth remembering: the transport reports a clean
  peer exit as `idle` because it does not know what anybody asked for, and `Connection` is what
  refines that into `exit_before_response`. Nothing here knows a method name; every protocol semantic
  is #149's and #157's.
- **`harkness-acp`** is the ACP client. `wire.rs` is the only module in the workspace that names
  `agent-client-protocol-schema`, and nothing it defines leaves it — that one `use` list is the whole
  of ADR-0009's wire-privacy rule. `capabilities.rs` holds the Harkness-owned vocabulary the crate's
  public API is written in, `connection.rs` the handshake, and `error.rs` a `kind()` namespace that
  is the *union* of its own table and `harkness-transport`'s: a transport failure is carried whole
  and keeps the discriminant #147 gave it, exactly as `InvocationError` delegates to `ToolError`.
  Negotiation is total over the selected version and decided before any capability is read; an
  unsupported one closes the connection and asks nothing more. `docs/acp.md` is the reference.
- **`harkness-mcp`, `harkness-forge`, `harkness-recipe`** are the remaining v0.5
  external-integration adapters, currently compile-clean skeletons whose only code is the test each
  one runs against its own `Cargo.toml`. They may depend on `harkness-git` and `harkness-core`,
  never on `harkness-runtime`, a front end, or one another; protocol wire types stay private to the
  adapter that speaks them. `docs/adr/0009` through `docs/adr/0018` decide the layering, the
  protocol revisions, the transport seam, the trust shape, and the activity classes these crates
  are built against — read them before adding code here.
- **`harkness-test-fixtures`** is dev-only: hermetic temp repos, process fixtures, and the
  child-re-execution helpers. `COMMIT_EPOCH_SECONDS` is fixed so fixture repos hash identically.

### Four independent concurrency mechanisms

Getting these confused is the main source of deadlock risk:

0. **Scheduler workspace slot** (`harkness-runtime/src/schedule`) — in-process only, keyed by
   `(ProjectId, canonical root)`. At most one mutating tool call per workspace, reads capped, child
   processes capped globally. Nests *outside* the repository lock; its own internal order is
   workspace map → one workspace → process limit, and none of the three is ever held across an
   executor call, a store write, or a child wait.
1. **Repository lock** (`harkness-git/src/lock.rs`) — advisory file lock keyed by Git's *common
   directory*, so every linked worktree of one repo shares it. Taken for every mutation, never for
   a read. Held across network operations. Lives under `locks/` in the data directory, never inside
   the user's `.git`.
2. **Catalog lock** (`harkness-core/src/catalog/lock.rs`) — global across all projects, guarded by
   the stable `projects.lock` inode. Never held during a long Git operation.
3. **Run store** (`harkness-runtime/src/store`) — one mutex-guarded writer connection plus pooled
   readers; `BEGIN IMMEDIATE` per read-modify-write.

**Ordering: scheduler workspace slot, then repository lock, then catalog lock.** The store takes
none of them, and no caller may hold a store transaction while acquiring any. The scheduler cannot
violate the two beneath it because it calls no catalog or Git code at all.

The coordinator's **lease and recovery locks** are not a fifth mechanism either. A lease lock is
taken once at construction and held for the coordinator's life; the recovery lock is taken and
released inside one startup sweep. Neither is held while any of the four above is acquired, and
neither is taken while a store transaction is open — the sweep's per-run transaction is opened and
committed underneath the recovery lock, never the other way round.

A `harkness-transport` connection is not a fifth mechanism and must not become one: its locks are
per connection, its own order is pump → correlation table, and it takes none of the four above —
it cannot, since it depends on `harkness-git` for `Cancellation` and on nothing else in the
workspace. A caller holding one of the four while blocked on a peer is holding it across a network
of somebody else's making, so treat a transport call the same way as a Git network verb.

### The hermetic Git invocation policy

`harkness-git/src/runner.rs` is a single choke point, not a convenience. Because `harkness-cli` is
an agent tool invoked from hooks and from inside other `git` processes, every invocation scrubs
`GIT_DIR`-family redirection and `GIT_CONFIG_*` injection, pins configuration as arguments, disables
terminal prompts, runs in its own process group so cancellation kills the whole tree, and drains
both streams concurrently. A typed option meaning "publish this one branch" is only true because
nothing outside can widen the invocation carrying it. Add new Git calls through `GitCommand`;
`git2`/libgit2 is used for inspection only.

### CLI wire contract

`harkness-cli/src/main.rs` emits exactly one envelope on stdout: `{"v": 1, "type":
success|error|progress, ...}`. Progress for clone/fetch/pull/push goes to stderr, one JSON object
per line, so stdout stays parseable. Help and version stay plain text even under `--json`. Exit
codes are fixed (0/1/2/3/4/5/130) and `harkness --json contract` reports `exit_code_by_kind` for
every error kind — a new error kind must be added to that namespace so callers never hardcode a
mapping. JSON output is a deliberate hand-written projection, not the catalog's storage serializer;
non-UTF-8 paths get lossy strings plus `path_is_lossy: true` and, where exactness matters, a
`*_base64` sibling field.

### GUI structure

`harkness-gui` is cxx-qt: `src/backend.rs` (`HarknessBackend`) is the Git and catalog QObject, and
`src/file_tree_model.rs`, `src/changes_model.rs`, `src/run_list_model.rs`,
`src/run_timeline_model.rs`, `src/approval_model.rs`, and `src/runs_backend.rs` are the rest;
`qml/` holds the Kirigami UI, registered as the static QML module
`io.github.fullstacktaiye.harkness` by `build.rs` and force-linked in `main.rs`. New QML files —
and every new bridge file — must be added to `build.rs`'s lists.

- **The run bridge is deliberately outside `backend.rs`.** `runs_backend.rs` carries the mutations
  (`cancelRun`, `retryRun`, `approve`, `deny`, `loadApprovalInput`) and owns the process's one
  `RunCoordinator` per data directory — `backend.rs`'s `check_coordinator_for` delegates to it,
  because a second coordinator would take a second lease and leave every run the checks panel
  started uncancellable. The three models drive their own reads: cxx-qt gives one bridge object no
  handle to another, so a model created in QML is not reachable from `RunsBackend`'s Rust.
  A read never *creates* `runtime.db` — `read_coordinator` probes `Store::open_existing` first.

- **cxx-qt does not camel-case names.** A `snake_case` member reaches QML spelled exactly as
  written, and a camel-case call site silently resolves to `undefined`. Every multi-word invokable
  carries an explicit `#[cxx_name = "..."]`; properties are kept to a single word.
- **Every long operation is `std::thread::spawn` + `qt_thread().queue(...)`** to return to the Qt
  thread. Results are gated on monotonically increasing request counters (e.g.
  `next_review_request`) and on the still-open project, so a stale reply is dropped rather than
  applied. Follow that pattern for new async work.
- `tests/qml_smoke.rs` includes `src/main.rs` directly and loads `Main.qml` to catch QML errors.
- **QML hot reload** (`cxx/qmlhotreload.h`, `src/hotreload.rs`) makes a working-copy build read
  `qml/` from disk rather than from the compiled resource, and rebuild the window on save. It is an
  URL interceptor rather than an extra import path because the module's `qmldir` maps every type to
  a `qrc:` URL; only files under the module's `qml/` prefix are redirected, so the type registry
  stays the one the build produced. It installs itself only when that directory exists beside the
  binary, which an installed build's does not. Reload loads the replacement before dropping the old
  root, so a QML parse error leaves the running window up instead of closing the last window and
  ending the process. `Main.qml`'s `restoreProjectId` is the reload's one piece of carried state.

### Runtime domain

`domain/mod.rs` documents the containment hierarchy (task → run → step → tool call) and the two
transition tables (`EXECUTION_TRANSITIONS`, `TOOL_CALL_TRANSITIONS`). Constructors only ever produce
`queued`/`pending`; table-checked transition methods are the only public mutators, and
outcome-specific methods attach failure detail, tool output, or approval audit atomically with the
transition. Serialization uses borrowing `*WireRef` types that must stay byte-compatible with their
owned `*Wire` counterparts.

### Runtime tool contract

`tool/mod.rs` documents the whole layer; read it before adding a tool. The shape that is easy to get
wrong: `Tool` declares *metadata*, never schemas. Schemas are generated from the `Input`/`Output`
associated types by `schemars` in `erase()` and compiled into validators there, so a descriptor
cannot publish a contract that disagrees with the type the body deserializes — and a broken schema
fails registration rather than the first call. Give every `Input` type
`#[serde(deny_unknown_fields)]`; that attribute is the only thing that closes the published schema.

`ErasedTool::execute_json` is the fixed pipeline — validate input, deserialize, execute under
`catch_unwind`, serialize, validate output — and each step's position is a guarantee, not a
preference. `invoke()` is the entire entry point: registry + id + JSON + `ExecutionContext`, with no
agent, policy engine, or database involved. `ExecutionContext` reuses `harkness_git::Cancellation`
rather than introducing a second cancellation mechanism, and `ProgressEvent` is the typed
generalization of the `impl FnMut(String)` callback Git verbs take.

Three error namespaces meet here: `RegistryError` (declaring and resolving), `ToolError` (invoking),
and `InvocationError` as their union — `InvocationError::kinds()` is what #99 publishes, so the two
`KINDS` tables must not collide. `InvocationError::Tool` carries the resolved `ToolIdentity` beside
the error, so a caller that named no version can still record `tool_calls.tool_version` for a failed
row without re-resolving. That is also why there is no `From<ToolError> for InvocationError`: a `?`
would silently drop the identity, so constructing the variant requires naming the tool.

### `serde_json` map ordering is a workspace-wide feature

`serde_json::Map` is a sorted `BTreeMap` only while nothing enables `preserve_order`; that feature
swaps it for an insertion-ordered `IndexMap`, and Cargo unifies features across every workspace
member, so one crate's dependency decides it for all of them.
`agent-client-protocol-schema` requires it, which means the workspace is on `IndexMap` today and any
code that froze the bytes of a `Value` had to stop inheriting its key order.
`harkness_runtime::canonical_json` is the one sorter; the three callers are `tool::erased`'s output
gate, `agent::scenario`'s `call`, and `harkness-cli`'s envelope. The same feature also grew `Value`
past `clippy::result_large_err`'s threshold, which is why `SendRejection` carries an `allow` and
`AcpError` boxes its `AgentRefusal`.

## Data directory

`HARKNESS_DATA_DIR` replaces the platform data directory outright; the CLI's `--data-dir` takes
precedence over it. Tests and isolated front ends rely on this — use it rather than touching real
user data. The directory holds `projects.json`, `projects.lock`, `runtime.db` (+ `-wal`/`-shm`),
`artifacts/`, `locks/`, `repositories/`, and `worktrees/`. Artifact content lives at
`artifacts/<run_id>/<artifact_id>`; the `artifacts` table records the metadata and re-derives that
path rather than trusting the one it stored. `locks/` holds three unrelated families: the
repository locks `harkness-git` keys by common directory, `managed-import-<project>.lock`, and the
coordinator's `runtime-lease-<id>.lock` plus the single `runtime-recovery.lock`.
