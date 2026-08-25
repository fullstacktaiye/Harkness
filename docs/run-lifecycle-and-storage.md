# Run lifecycle and storage

A run record says what is true now. The event log says how the run got there.
The artifact store holds what was too large for either. Together they are the
whole durable answer to "what did Harkness do", and this document is what they
promise.

- [The lifecycle](#the-lifecycle)
- [The event log](#the-event-log)
- [Event kinds](#event-kinds)
- [The schema](#the-schema)
- [The migration ladder](#the-migration-ladder)
- [Connection discipline](#connection-discipline)
- [The inline payload threshold](#the-inline-payload-threshold)
- [Artifacts](#artifacts)
- [Redaction](#redaction)
- [Interrupted runs](#interrupted-runs)
- [Retrying](#retrying)
- [Reading a run back](#reading-a-run-back)
- [Backups](#backups)
- [Running the benchmarks](#running-the-benchmarks)
- [What proves this](#what-proves-this)

## The lifecycle

A task identifies work in one workspace. Each attempt is a run, a run is divided
into ordered steps, and a step contains tool calls.

Fresh constructors produce only `queued` runs and steps and `pending` calls.
Table-checked transition methods are the only public mutators, and
outcome-specific methods attach failure detail, tool output, or approval audit
records *atomically* with their transition — so a record cannot be found having
moved without the evidence for why.

### Runs and steps

| From \ To | `running` | `waiting_for_approval` | `succeeded` | `failed` | `cancelled` | `interrupted` |
| --- | --- | --- | --- | --- | --- | --- |
| `queued` | ✅ | ❌ | ❌ | ✅ | ✅ | ✅ |
| `running` | ❌ | ✅ | ✅ | ✅ | ✅ | ✅ |
| `waiting_for_approval` | ✅ | ❌ | ❌ | ✅ | ✅ | ✅ |
| terminal (`succeeded`, `failed`, `cancelled`, `interrupted`) | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |

### Tool calls

| From \ To | `awaiting_approval` | `running` | `succeeded` | `failed` | `denied` | `cancelled` | `interrupted` |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `pending` | ✅ | ✅ | ❌ | ✅ | ✅ | ✅ | ✅ |
| `awaiting_approval` | ❌ | ✅ | ❌ | ❌ | ✅ | ✅ | ✅ |
| `running` | ❌ | ❌ | ✅ | ✅ | ❌ | ✅ | ✅ |
| terminal (`succeeded`, `failed`, `denied`, `cancelled`, `interrupted`) | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |

A **timeout has no state of its own**: it is persisted as `failed` carrying the
`timed_out` failure kind, so a diagnostic line and the `tool_calls.state` column
it describes agree. `denied` is a refusal rather than a failure, which is why it
is reachable from `pending` and `awaiting_approval` but never from `running`:
nothing that started is denied afterwards.

Two extra columns carry the timestamps the states require. `started_at` is
*required* by `running`, `waiting_for_approval` and `succeeded`, and *forbidden*
on `queued`; for a call it is required by `running` and `succeeded` and forbidden
on `pending`, `awaiting_approval` and `denied`. A `revision` counter gives every
record optimistic concurrency, and a transition timestamp never moves backwards.

The full tables and the tests that drive every ordered pair of states are in
[The run runtime](architecture-runtime.md#the-two-state-machines).

## The event log

`run_events` is append-only in the strict sense: the module that owns it contains
no `UPDATE` and no `DELETE`, because every consumer's trust in the log would
otherwise be unearned. It is what the window's timeline renders, what
`harkness run show` prints, and what an approval audit is read out of.

**Sequence numbers.** Each event carries a per-run `seq` that starts at `1`. The
number is allocated as `1 + MAX(seq)` *inside the same transaction that inserts
the row*, and every write goes through the store's single writer connection under
`BEGIN IMMEDIATE`, so two appenders cannot read the same maximum. `(run_id, seq)`
is the primary key, so even a caller reaching the table another way cannot
produce a duplicate.

**Gaps are permitted; monotonicity is not.** A gap costs a reader nothing —
pagination is `seq > last`, not `seq = last + 1` — while a repeated or reordered
number would silently change what a timeline says happened.

**Atomicity with the state it describes.** A lifecycle change and the event
describing it commit in one transaction. Either both are visible or neither is;
there is no window in which a run has moved and its history does not say so.

Reading a page: `EventPage::oldest(limit)` walks forwards, `EventPage::newest(limit)`
opens at the tip and walks backwards, the default page is 200 events and the
maximum is 1,000. That bound is a *row* bound rather than a byte bound — the
arithmetic against `MAX_INLINE_PAYLOAD_BYTES` is the caller's to do.

## Event kinds

The `kind` column is free text rather than a checked enumeration, so a kind added
by a later build needs no migration and a kind this build does not know renders
as an opaque timeline entry instead of failing the read.

| Kind | Says |
| --- | --- |
| `run_state_changed` | A run entered a new lifecycle state. |
| `run_interrupted` | A recovery sweep found the run's owning process gone, and which claim was dead. |
| `run_retried` | A later run was created to re-attempt this one. Recorded on the *original*. |
| `step_started` | A step began executing. |
| `step_finished` | A step reached a terminal state. |
| `tool_call_state_changed` | A tool call entered a new lifecycle state. |
| `policy_decision` | A validated request received its binding policy decision. |
| `tool_progress` | A running tool reported progress. |
| `approval_requested` | Work paused for a human decision. |
| `approval_decided` | A human decision was recorded. |
| `approval_identity_drift` | An otherwise applicable approval no longer matched external identity. |
| `artifact_created` | Content was stored outside the log. |
| `agent_observation` | A redacted observation was delivered to the run's agent. |
| `agent_action` | A redacted action was returned by the run's agent. |
| `agent_checkpoint` | The agent's resumable session checkpoint was recorded. |
| `snapshot_captured` | A workspace snapshot was made durable evidence for this run. |
| `context_cache_recreated` | The disposable context index cache was thrown away and rebuilt. |
| `external_agent_health_checked` | A registered external agent was spawned, negotiated with, and torn down. |
| `external_agent_trust_invalidated` | A registered agent's executable stopped matching its grant. |
| `diagnostic` | Anything a run wants to say that no other kind covers. |

`run_interrupted` is distinct from the `run_state_changed` entry that follows it
on purpose: that one says the run reached `interrupted`, this one says why it was
detected and which claim was found dead.

`snapshot_captured` is recorded by the persistence path and by nothing else.
Capturing a snapshot is a read of the workspace that writes nothing, so an engine
that emitted this would be claiming an audit trail it never stored.

The `agent_observation`, `agent_action` and `agent_checkpoint` payloads encode
their already-redacted versioned record as **numeric bytes**, because the store
correctly redacts every JSON string value and would otherwise rewrite enum tags,
semantic versions, and UUID spellings.

A real timeline, from the flagship scenario's first tool call:

```json
{"seq":6,"kind":"step_started","payload":{"state":"running"}}
{"seq":7,"kind":"policy_decision","payload":{"risk":"observe","decision":{
   "verdict":"allow","source":"built_in",
   "reason":"observe is allowed by the trusted-workspace default"}}}
{"seq":8,"kind":"tool_call_state_changed","payload":{
   "state":"running","tool_id":"workspace.inspect","tool_version":"1.0.0"}}
{"seq":9,"kind":"tool_call_state_changed","payload":{"state":"succeeded","detail":null}}
{"seq":10,"kind":"step_finished","payload":{"state":"succeeded"}}
```

## The schema

`runtime.db` lives under the Harkness data directory. Every table is `STRICT`, so
a wrong-typed binding fails at write time instead of silently storing an
affinity-converted value. Timestamps are RFC 3339 UTC text at fixed nanosecond
precision, so lexicographic and chronological comparison agree and the recency
index can serve keyset pages.

| Table | Holds | Notable |
| --- | --- | --- |
| `tasks` | one piece of work in one workspace | `project_id` is nullable: a run may target a workspace the catalog does not know |
| `runs` | one attempt | `lease_id`, `retry_of`, `workspace_may_be_modified` |
| `steps` | ordered stages | `UNIQUE (run_id, ordinal)` |
| `tool_calls` | invocations | `input_json`, `output_json`, `policy_decision_json`, the pinned `tool_version` |
| `run_events` | the append-only log | `PRIMARY KEY (run_id, seq)`, `WITHOUT ROWID` |
| `artifacts` | metadata for content on disk | `sha256`, `byte_size`, `storage_path`, `availability` |
| `approvals` | the question queue | binding fields, `requested_scope` *and* `effective_scope` |
| `workspace_trust` | one decision per project | bound to project identity **and** canonical root |
| `runtime_leases` | which process claims which runs | `pid` is audit only |
| `workspace_snapshots` | durable context evidence | envelope columns denormalized out of `payload_json` and checked against it |
| `integration_trust_records` | per-subject trust for external agents, servers, recipes and forges | see `docs/agents.md` |
| `agent_runtime_state` | one registered agent's health and observation record | see `docs/agents.md` |

**Containment is enforced by the database, not re-checked in Rust.** A tool call
may only name a step that already belongs to the run it claims; an event, an
artifact and an approval may only name a step or a call of their own run. Those
are composite foreign keys against redundant `UNIQUE (id, run_id)` indexes, which
is why those indexes exist. A timeline that could name another run's step would
be a worse failure than a refused write: nothing downstream re-checks it, and the
wrong step would simply be rendered.

`run_events` is `WITHOUT ROWID` so `(run_id, seq)` *is* the storage order, making
a timeline read a sequential scan rather than an index lookup per row.

Two indexes exist for questions rather than for tables. `runs_by_recency (created_at
DESC, id DESC)` is the run listing's cursor key in the order the listing scans it,
and `runs_by_state (state, created_at, id)` makes the startup sweep O(non-terminal
runs) rather than O(runs) — which is what keeps recovery off the critical path of
a store that has recorded a year of history.

## The migration ladder

`PRAGMA user_version` records how far the ladder has been climbed. A build
applies every migration above that number in ascending order, each inside its own
transaction that *also* advances the recorded version, so an interrupted upgrade
either leaves the previous version intact or lands the next one whole.

| Version | Migration | Added |
| --- | --- | --- |
| 1 | `001_initial_schema.sql` | `tasks`, `runs`, `steps`, `tool_calls` |
| 2 | `002_events_and_artifacts.sql` | `run_events`, `artifacts` |
| 3 | `003_workspace_trust.sql` | `workspace_trust` |
| 4 | `004_policy_decisions.sql` | `tool_calls.policy_decision_json` |
| 5 | `005_approvals.sql` | `approvals` |
| 6 | `006_approval_integration_identity.sql` | the three external identity columns on `approvals` |
| 7 | `007_run_leases_and_retry.sql` | `runtime_leases`, `runs.lease_id`, `runs.retry_of`, `runs.workspace_may_be_modified` |
| 8 | `008_workspace_snapshots.sql` | `workspace_snapshots` |
| 9 | `009_agent_registry.sql` | `integration_trust_records`, `agent_runtime_state` |

**A released migration is never edited.** A new persisted field, state spelling,
or table means a version bump plus a *new* frozen fixture beside the existing
ones. `crates/harkness-runtime/src/store/fixtures/runtime-v{1..9}.db` are those
fixtures: each is opened by a test that migrates it to current and reads back the
records that version introduced.

Two processes climbing the same ladder is a case the code handles rather than
one it assumes away. Reading `user_version` outside a write transaction only says
what was true at the moment of the read, so each step takes the write lock with
`BEGIN IMMEDIATE` and re-reads the version underneath it, treating a version that
moved as another process's work rather than as a step still owed.

A database whose recorded version is **above** what this build understands is
refused as an upgrade request rather than treated as corruption — and refused
*before* the connection requests WAL, so its bytes are left exactly as they were
found.

## Connection discipline

Every connection applies the same pragmas before it is used:

```text
journal_mode = WAL        readers work while a writer commits
foreign_keys = ON         containment is enforced rather than declared
busy_timeout = 5000       a contended write waits rather than failing
synchronous = NORMAL      process-crash consistency, not power-loss durability
```

`synchronous=NORMAL` is a deliberate trade for commit latency. The guarantee this
store makes is **process-crash consistency**, not power-loss durability, which is
the right trade for a record of work a user can re-run.

**Single writer, short transactions.** Every write goes through one
mutex-guarded connection, and every read-modify-write runs in one `BEGIN
IMMEDIATE` transaction. Reads use separate pooled connections and are never
blocked by a writer.

**No transaction is ever held across a user wait.** Work needing a human decision
persists the request, commits, and only then waits; resuming is a second, equally
short transaction.

**The store takes no lock of its own beyond that.** It takes neither the
repository lock nor the catalog lock, and no caller may hold a store transaction
while acquiring either.

## The inline payload threshold

No column holds more than **64 KiB** (`MAX_INLINE_PAYLOAD_BYTES`) of caller data,
and the bound covers every caller-controlled column rather than tool payloads
alone: titles, workspace paths, tool identifiers, failure detail, and the
approval history are each held to it. A limit with exceptions is not a limit
anyone can rely on.

Oversized data is refused with `StoreError::PayloadTooLarge` naming the
threshold. The refusal is symmetric: a row that arrived from outside Harkness
holding more than the threshold **fails to load**, because reading it back would
import the very cost the bound exists to prevent.

One consequence is worth stating plainly. A caller recording a failure whose
message exceeds the threshold is refused, and the record keeps its previous
state; the caller must summarize and retry. Truncating silently would store
something the caller never wrote.

**An event payload is the one caller value that is not refused for being too
large.** An event describes something that *already happened*, and refusing to
record it loses history rather than protecting it. It is written to an artifact
instead and the stored event carries a reference to it under the
`payload_artifact` key, so the full bytes stay recoverable while the row stays
small. The threshold is measured *after* redaction and the artifact holds the
redacted encoding — exactly the bytes the row would have held had they fit.

## Artifacts

Content lives at `<data_dir>/artifacts/<run_id>/<artifact_id>`, with one row in
`artifacts` recording what it is.

**File first, row second.** Finalizing an artifact is *write, sync, rename, then
insert*. Every other ordering can produce a metadata row pointing at bytes that
were never made durable, and a reader has no way to tell that row from a good
one. This ordering can only produce two harmless outcomes:

| Crash point | What is left | What a reader sees |
| --- | --- | --- |
| before the rename | a `.tmp-` file | nothing; readers only ever name `<artifact_id>` |
| between rename and insert | an orphan file | nothing; no row refers to it |
| after the insert | a row and its file | the artifact |

An orphan file costs disk and nothing else. An ordinary refusal is not a crash,
and every such path removes what it wrote, so a caller retrying does not leave a
file per attempt behind.

**The stored path is checked, not trusted.** `storage_path` is derivable from
`(run_id, id)`. It is stored anyway so the layout is legible from the database
alone, and every read compares it against the derived form: a row edited to name
`../../.ssh/id_rsa` is refused with `StoreError::ForbiddenArtifactPath` rather
than opened. The path Harkness actually uses is always rebuilt from the two
identifiers, never joined from the stored text.

**A missing file degrades a read, never fails one.** Deleting the bytes from
outside Harkness is something a user may simply do, so `Store::artifact` stats
the file and reports `missing` or `size_mismatch`. Loading the run and reading
its event log are untouched, because neither opens an artifact; only asking for
the *content* fails, and it fails naming the artifact.

**Streaming is the whole write surface.** `ArtifactSink` is an `io::Write` and
there is no method taking a whole artifact, so no caller can be tempted into
holding one in memory. Bytes pass through the redactor, then a hasher and a
counter, then the file, so the recorded size and SHA-256 describe exactly the
bytes on disk.

From a real flagship run:

```text
artifact  66851cc2-…  applied.patch     text/x-diff   177  available
artifact  05bed6ab-…  test-stdout.log   text/plain    194  available
artifact  ee103cba-…  test-stderr.log   text/plain      0  available
```

## Redaction

Every caller value that becomes durable passes through a `Redactor` first, and
`Store::open` installs `StandardRedactor` rather than `PassThrough` — so the
rules arrive by opening a store instead of by remembering to ask for them. Event
payload values, an artifact's label and media type, an approval's summary and
decision reason, a task's title, a tool's result, and every failure message go
through it; artifact content goes through the stream wrapper.

**Two durable caller documents deliberately do not**, and both are load-bearing
bytes rather than prose:

- `tool_calls.input_json` is what the executor reads back and *runs*, and what an
  approval's hash was taken over. Rewriting it would run a different command than
  the one that was approved.
- `workspace_snapshots.payload_json` is bound by a digest `harkness-context`
  re-derives on load.

Anything new that persists caller content comes through the redactor.
[Diagnostics and redaction](observability.md) is the reference, including the
coverage table and its two exemptions.

## Interrupted runs

A run left behind by a process that died is not a gap in the record. It is an
outcome, detected by evidence rather than inferred from a timestamp.

Every coordinator holds one **lease**: an advisory lock file under
`<data_dir>/locks/runtime-lease-<id>.lock` that the *kernel* releases however the
process ends, plus a `runtime_leases` row that every run it starts points at. The
row is the durable record and the file is the liveness oracle; neither alone
would do, because a row cannot notice a `SIGKILL` and a lock file with no row
names nothing.

`runs.owner_pid` has existed since migration 1 and is deliberately not what
decides anything: process identifiers are reused, so a row naming a live pid is
not evidence that the process holding it wrote the row (ADR-0020).

Construction sweeps **before** accepting any work. Every run whose claim is
provably dead is marked `interrupted` — the run, its unfinished steps, its
in-flight tool calls, and every approval nobody can answer any more — each with
its own appended event, and with the timeline before that moment left exactly as
the dead process wrote it. A live sibling's runs are never disturbed, because the
proof is a lock the kernel released rather than a clock that stopped moving.

Killing a process parked on an approval and reading the run back afterwards:

```text
run_state: interrupted
call: workspace.inspect  succeeded
call: fs.read            succeeded
call: fs.apply_patch     interrupted
approval: superseded (workspace_write, no decision recorded)

event 25  run_state_changed  {"state":"waiting_for_approval"}
event 26  run_interrupted    {"lease_id":"276ebb2a-…","reason":"lease_lock_released"}
event 27  tool_call_state_changed {"state":"interrupted","reason":"lease_lock_released"}
event 28  step_finished      {"state":"interrupted","reason":"lease_lock_released"}
event 29  approval_decided   {"approval_id":"a672d2d0-…","state":"superseded"}
event 30  run_state_changed  {"state":"interrupted"}
```

The `approval_decided` entry carries a state and **no verdict**, because nobody
answered it.

## Retrying

Nothing is resumed. `retry_run` creates a *new* run for the same task, and the
relationship is a column on the new row rather than a rewrite of the old one — a
terminal run's timeline is evidence.

- `retry_of` names the attempt this run follows. The self-reference is a real
  containment claim and is enforced: a retry naming a run that does not exist
  would make its own provenance unreadable.
- `workspace_may_be_modified` is `true` when the earlier attempt started any tool
  call that could write. v0.3 never rolls back or re-applies a partial mutation,
  so this flag is the only warning a front end has — and Harkness never undoes a
  partial edit on your behalf.
- **No approval carries over.** A grant's lifetime is its run.
- The original records a `run_retried` event and is never otherwise touched
  again.

Only a `failed`, `cancelled` or `interrupted` run can be retried; anything else
is refused by name:

```json
{"v":1,"type":"error","ok":false,"error":{
  "kind":"run_not_retryable",
  "message":"run 1ff4e03d-… is succeeded; only a failed, cancelled or interrupted run can be retried",
  "details":{"run_id":"1ff4e03d-…","state":"succeeded"}}}
```

Retrying the interrupted run above produces:

```text
kind: run_retry
new run: fbbea40e-dd4f-4ce6-9aec-a18874caca70  succeeded
retry_of: ece24ed8-c4a1-4c9c-8462-6b18995ed432
workspace_may_be_modified: false
```

`false` there is honest rather than optimistic: the killed attempt was still
waiting to be *allowed* to write, so nothing had been written.

## Reading a run back

<!-- verified -->
```sh
harkness --json run list --limit 5
```

Runs page newest-first with an opaque `next_cursor`, exactly as `git log` does.
A run's timeline pages separately, by the sequence number `run show` returns as
*its* `next_cursor`, in either direction:

<!-- verified -->
```sh
harkness --json agent run --scenario read_only_success --project ws
harkness --json run show $RUN --limit 200 --order oldest
```

`run show` reports the run, its steps, its tool calls with the policy decision
each was admitted under, its approvals with how they were answered, its artifacts
with media type, size and availability, and a page of the timeline.

**A read never creates `runtime.db` and never builds a coordinator.** Building one
takes the lease and runs the recovery sweep, which are writes — and a user who ran
`harkness run list` to look at something has not asked for a write. On a data
directory nothing has run in, the listing is empty and the directory is left
exactly as it was found; `run_list_is_empty_and_creates_no_run_store_before_anything_has_run`
is where that is checked.

## Backups

A WAL database is three files: `runtime.db`, `runtime.db-wal`, and
`runtime.db-shm`. Copying only `runtime.db` from a running Harkness loses every
commit still in the log. Either copy all three, or call `Store::checkpoint` first
and copy `runtime.db` alone — and check that it returned `Ok`, because a reader
on another connection can leave frames behind, which is reported as
`StoreError::IncompleteCheckpoint` rather than by failing the statement.

The rest of the data directory is described in `CLAUDE.md`. `context/` is the one
disposable subtree: deleting it costs warm-up time and no evidence (ADR-0004).
`logs/` is disposable in a weaker sense — it is evidence about the *process*
rather than about a run.

## Running the benchmarks

The store's latency targets are `#[ignore]`d, because a debug build measures
nothing meaningful. Every one of them reports through
`harkness_test_fixtures::latency::record`, which prints the machine beside the
number and binds the budget only where `debug_assertions` is off — so a debug run
records a measurement instead of failing on one.

```sh
sh .github/scripts/run-ignored-exact-test.sh \
    harkness-runtime store::tests::loading_a_thousand_event_run_meets_the_latency_target --release
```

```text
harkness-latency target=store::load_thousand_event_run measured_ns=2323528 budget_ns=500000000 profile=release enforced=true os=linux arch=x86_64 cpus=8
```

| Budget | Test |
| --- | --- |
| 10 ms per state-change batch | `store::tests::persisting_a_state_change_batch_meets_the_latency_target` |
| 10 ms per state-change batch with its events | `store::tests::persisting_a_state_change_batch_with_its_events_meets_the_latency_target` |
| 100 ms to list 100 runs | `store::tests::listing_one_hundred_runs_meets_the_latency_target` |
| 500 ms to load a 1,000-event run | `store::tests::loading_a_thousand_event_run_meets_the_latency_target` |

[The verification suite](verification-suite.md#latency-targets) is the complete
list, including the four budgets outside this crate.

## What proves this

| Claim | Package | Test |
| --- | --- | --- |
| Every record type round-trips through the store | `harkness-runtime` | `store::tests::every_record_type_round_trips_through_the_store` |
| A state change and its event commit atomically or not at all | `harkness-runtime` | `store::tests::a_state_change_and_its_event_commit_atomically_or_not_at_all` |
| Sequence numbers stay monotonic under concurrent appends | `harkness-runtime` | `store::tests::event_sequences_are_monotonic_per_run_under_concurrent_appends` |
| An oversized payload becomes an artifact with a reference | `harkness-runtime` | `store::tests::oversized_event_payloads_become_artifacts_with_a_reference` |
| A row holding more than a column may hold fails to load | `harkness-runtime` | `store::tests::a_row_holding_more_than_a_column_may_hold_fails_to_load` |
| An unknown event kind loads as opaque | `harkness-runtime` | `store::tests::an_unknown_event_kind_loads_as_opaque` |
| Artifact finalize is write, sync, rename, then row | `harkness-runtime` | `store::tests::artifact_finalize_is_write_sync_rename_then_row` |
| A missing artifact file degrades to `missing` | `harkness-runtime` | `store::tests::a_missing_artifact_file_degrades_to_availability_missing` |
| A storage path outside the artifacts directory is refused | `harkness-runtime` | `store::tests::a_storage_path_outside_the_artifacts_directory_is_refused_on_read` |
| An opened store scrubs without being asked to | `harkness-runtime` | `store::tests::an_opened_store_scrubs_without_being_asked_to` |
| A tool call input is stored exactly as the caller wrote it | `harkness-runtime` | `store::tests::a_tool_call_input_is_stored_exactly_as_the_caller_wrote_it` |
| A newer schema is refused and the file is left untouched | `harkness-runtime` | `store::tests::a_newer_schema_is_refused_as_upgrade_and_leaves_the_file_untouched` |
| Independent processes migrate a new database exactly once | `harkness-runtime` | `store::tests::independent_processes_migrate_a_new_database_exactly_once` |
| A v1 database migrates to current and still reads its runs | `harkness-runtime` | `store::tests::a_v1_database_migrates_to_current_and_still_reads_its_existing_runs` |
| Every migration in this build is recorded in order | `harkness-runtime` | `store::tests::every_migration_in_this_build_is_recorded_in_order` |
| Killing a process makes the next start mark what it left | `harkness-runtime` | `coordinator::tests::recovery::killing_a_process_mid_run_makes_the_next_start_mark_everything_it_left` |
| A run owned by a live second process is left alone | `harkness-runtime` | `coordinator::tests::recovery::a_run_owned_by_a_live_second_process_is_left_alone` |
| Retrying an interrupted run creates a fresh attempt with provenance | `harkness-runtime` | `coordinator::tests::recovery::retrying_an_interrupted_run_creates_a_fresh_attempt_with_provenance` |
| A read creates no run store before anything has run | `harkness-cli` | `run_list_is_empty_and_creates_no_run_store_before_anything_has_run` |
| Retrying names the run it follows | `harkness-cli` | `retrying_records_a_new_attempt_that_names_the_run_it_follows` |
