# Changelog

This file starts at `0.3.0`, the first version the workspace manifest names.
The v0.1 and v0.2 milestones shipped before the workspace tracked a version at
all — their history is the merge log, and inventing numbers for them after the
fact would be a release record nobody could check.

Each entry says what a user can now do and what the system now refuses. The
refusals are the point: most of v0.3 is machinery whose whole job is to be in
the way of something.

## 0.3.0 — Typed tool runtime, policies, approvals, persistent runs, and a mock agent

The execution spine Harkness needs before any language model is connected to it.
A deterministic agent plans, calls typed tools against a repository, stops for a
durable human decision before anything protected runs, survives being killed, and
leaves a timeline that reads the same from the command line and from the window
because both are thin clients of one engine and one store.

Nothing in this release requires a language model, an API key, a network service,
or a GitHub account — at runtime or in the default test suite.

`docs/release-readiness-v0.3.md` is the release gate's audit record: every
criterion the milestone was held to, named beside the test that proves it, and
the two coverage gaps that were filed rather than waived.

### The runtime

- **A new crate, `harkness-runtime`**, holding the run domain model, the SQLite
  run store, the typed tool contract, policy, approvals, scheduling, the agent
  seam, and the coordinator that composes them. `harkness-cli` and `harkness-gui`
  both sit on top of it and neither owns any of it.
- **Typed tools with generated schemas.** A tool declares metadata; its
  input and output JSON Schemas are derived from its associated types rather than
  written by hand, so a descriptor cannot publish a contract that disagrees with
  the type its body deserializes. A schema that will not compile fails
  registration instead of the first call, and the registry refuses a second tool
  claiming an identity it already holds.
- **A fixed execution pipeline**: validate input, deserialize, execute under
  `catch_unwind`, serialize, validate output. Each step's position is a
  guarantee — only a refused input promises the body never ran, and a panicking
  tool becomes a structured error rather than taking the process with it.
- **Nine registered tools** covering the vertical slice: `workspace.inspect`,
  `workspace.search`, `fs.read`, `git.status`, `git.diff`, `fs.apply_patch`,
  `process.exec`, `test.run`, and `check.run`. Five of them are `observe`; one
  writes inside the workspace; three run a program. None is `network`,
  `remote_write`, or `destructive` — v0.3 registers no tool at those levels.
- **Progress, timeouts, and cancellation** reaching a child's whole process
  group, on the same `Cancellation` token every Git operation already carried.

### Safety

- **Workspace trust** as an explicit, recorded decision. Trust authorizes
  nothing on its own; it moves the question from "may Harkness look at this at
  all" to "may Harkness do this particular thing".
- **Six risk levels** — `observe`, `workspace_write`, `execute`, `network`,
  `remote_write`, `destructive` — declared per tool and refined by input.
- **Policy on every protected call**, evaluated to `Allow`, `Ask`, or `Deny`
  with a reason that is persisted with the decision. Repository configuration
  layers over user policy and may only tighten it: no repository input can lower
  any verdict.
- **Path containment and symlink refusal** for every filesystem-touching tool.
  A path resolving outside the workspace is refused by name, before anything is
  written.
- **Argv-only process spawning** with an environment allowlist rather than a
  denylist. Shell metacharacters survive as single arguments because no shell
  ever sees them.
- **Redaction before persistence.** Every durable channel — event payloads,
  error details, artifacts, and the diagnostic log — passes through the
  redactor first, and a test byte-scans the whole data directory after a run
  that deliberately leaks sentinels through every channel it has.

### Approvals

- **Durable approval requests** that survive a restart, are answerable from
  either front end, and are never granted by a window closing.
- **A grant is bound to the exact request**: tool id, tool version, canonical
  input hash, workspace, run, scope, and lifecycle status. A call approved and
  then mutated while parked is refused at dispatch with the mismatch recorded,
  not executed under a grant that no longer describes it.
- **`remote_write` and `destructive` are always one call**, never a run-wide or
  session-wide breadth.
- **Noninteractive execution denies an unanswered `Ask`** rather than waiting
  forever or proceeding.

### Persistence

- **`runtime.db`**, a SQLite store in WAL mode with foreign keys, a busy
  timeout, and a versioned migration ladder whose every released step is frozen
  as a committed fixture. A schema newer than this build is refused as an
  upgrade with the file's bytes left alone.
- **An append-only event log** with per-run monotonic sequence numbers. Every
  row is rebuilt into its wire record and re-validated on load, so an impossible
  record cannot enter the process.
- **Artifacts on disk** at `artifacts/<run-id>/<artifact-id>`, with metadata in
  the database. Large output is streamed to an artifact rather than accumulated;
  a missing artifact file degrades that artifact to unavailable instead of
  corrupting the run.
- **Interruption is detected, not guessed.** Each coordinator holds an advisory
  lease; construction sweeps every run whose claim is provably dead, marks it
  and its unfinished work `interrupted`, and appends an event for each. That
  sweep is the only writer of `interrupted`, which is what makes the state mean
  "the owning process stopped".
- **Retry creates a new run** carrying `retry_of` and, where the earlier attempt
  started something that could write, `workspace_may_be_modified`. Nothing is
  resumed and no grant carries over.

### The mock agent

- **Ten deterministic scenarios** replayed through the same `Agent` trait a real
  agent will implement, with no access to the registry, policy, approvals,
  store, scheduler, or execution context.
- **A checkpoint carrying a chained observation digest**, so a replay that
  diverges from its script says which observation it expected and which it got.
- Scenario wire forms are frozen as JSON fixtures beside the Rust definitions.

### Command line

- `harkness run list | show | cancel | retry`, `harkness approvals list |
  approve | deny`, `harkness tool list | describe | invoke`, and `harkness agent
  scenarios | run`, all inside envelope v1 with the existing exit-code contract.
  `harkness check run` joins them, putting a configured project check through
  the same durable runtime.
- `harkness --json contract` reports `exit_code_by_kind` for every error kind,
  so a caller never hardcodes a mapping.
- `--verbose` mirrors the diagnostic log to stderr in the log's own rendering,
  so what a verbose run shows is exactly what was recorded.

### Window

- A **runs list** paging through the whole store, a **run detail timeline** that
  folds consecutive progress ticks of one call into a single counted row, and an
  **approval review page** that is a page rather than a dialog, because a dialog
  has an implicit accept and absence of an answer is never consent.
- Approving offers only the breadths the runtime would actually accept, read off
  the record rather than derived a second time. A lapsed deadline withdraws
  Approve while the stored request still reads as pending.
- Every string a tool, an agent, or the repository wrote is rendered as plain
  text.
- A front-end read takes the store and never the coordinator: building one takes
  this process's lease and runs the recovery sweep, which writes.

### Diagnostics

- A bounded JSON-lines log under `logs/` — `harkness.log` plus at most four
  rotated generations of 4 MiB — created by the first line written rather than
  by initialization, so a command that records nothing leaves a data directory
  it only read exactly as it found it.
- Every runtime span names `run_id` as a field of its own, because work crosses
  threads and an inherited field would be lost.

### Documentation and verification

- Seven documents under `docs/` covering the runtime, tool authoring, policy,
  approvals, run lifecycle and storage, the mock-agent scenarios, and
  diagnostics — plus `docs/verification-suite.md`, which names the test behind
  every release-blocking scenario.
- **The documentation is checked, not trusted.** Documented commands are
  executed as written, the worked example is compared byte for byte against the
  file it mirrors, every cited repository path and cross-document link is
  resolved, and every test a document names is re-derived from the test
  binaries.
- Twelve latency budgets, each measured in every profile and enforced only where
  the number means something — the threshold binds when `debug_assertions` is
  off, so a debug run records a measurement instead of failing on one.
