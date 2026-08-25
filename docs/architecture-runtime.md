# The run runtime

`harkness-runtime` is where a request to do something becomes a record of
something having been done. Every front end drives it, no front end contains any
of it, and everything it decides it also writes down.

This document is the map. The invariants a contributor can violate silently live
in `AGENTS.md`; the item-level API is rustdoc. What is here is the shape that
only becomes visible after reading several files at once.

- [Where the crate sits](#where-the-crate-sits)
- [Inside the crate](#inside-the-crate)
- [The containment hierarchy](#the-containment-hierarchy)
- [The two state machines](#the-two-state-machines)
- [What one tool call actually goes through](#what-one-tool-call-actually-goes-through)
- [Threads, cancellation, and the four locks](#threads-cancellation-and-the-four-locks)
- [The front-end boundary](#the-front-end-boundary)
- [The extension seams](#the-extension-seams)
- [Where to read next](#where-to-read-next)

## Where the crate sits

Dependencies flow strictly downward. Nothing lower reaches back up, and the four
external-integration adapters are depended *on* by the runtime rather than the
other way round (ADR-0009).

```text
harkness-cli ──┐                              ┌─> harkness-git
harkness-gui ──┴─> harkness-core ─────────────┤
                   harkness-runtime ──────────┤
                   harkness-context ──────────┤
                   harkness-provider ─────────┘

harkness-acp  ──┬─> harkness-transport ──> harkness-git
harkness-mcp  ──┘

harkness-acp  ──┐
harkness-mcp  ──┤
harkness-forge──┼─> harkness-runtime
harkness-recipe─┘
```

Three edges are worth knowing the reason for.

**`harkness-runtime` depends on `harkness-git` for one type.** `Cancellation` is
what `tool`'s `ExecutionContext` carries, so a tool that shells out to Git passes
the same token down instead of translating between two cancellation mechanisms.

**`harkness-runtime` depends on `harkness-context`, never the reverse.** A
workspace snapshot can be captured with no database of runs in the process. The
runtime is what makes one *durable*, because evidence about a run belongs beside
the run and not in a cache the user may delete (ADR-0004).

**`harkness-runtime` names `harkness-acp`.** The runtime is the composition
point: the agent registry needs the run store *and* the ACP handshake, and trust
has to compose with `trust` and `policy`, which an adapter cannot see. Pointing
the edge downward is what ADR-0009 leaves available.

## Inside the crate

| Module | Owns | Depends on |
| --- | --- | --- |
| `domain` | The records and their lifecycle state machines. No I/O, no clock of its own. | — |
| `store` | `runtime.db`: the migrated SQLite schema, the append-only event log, the artifact store, redaction before persistence. | `domain` |
| `tool` | The typed tool contract: descriptors, generated schemas, the registry, the execution context, the executor. | `domain` |
| `tools/` | The nine production tools this build registers. | `tool`, `trust` |
| `trust` | Workspace trust, the filesystem boundary, the allowlisted child environment, and request classification. | `tool` |
| `policy` | Layered rule loading and one pure `Allow`/`Ask`/`Deny` evaluation. | `trust`, `tool` |
| `approval` | The durable request, its lifecycle, the frozen input hash, the grant matcher, and the gate a parked call waits on. | `policy`, `store` |
| `schedule` | When a call runs: per-workspace mutation serialization, a read cap, a global process cap, and the cancellation chain. | `tool` |
| `agent` | The plain-data decision seam, and `MockAgent`'s ten deterministic scripts. | `tool` |
| `coordinator` | The orchestration loop those pieces meet in, plus the lease, the recovery sweep, and retry. | everything above |
| `context` | One `Arc`-shared `ContextEngine` per open project. | `harkness-context` |
| `integration` | Identifiers, identity records, and trust records for the external subjects v0.5 talks to. | `trust`, `policy` |
| `agent_registry` | `agents.json`, the discovery probe, the executable-digest grant, and the health check. | `integration`, `store`, `harkness-acp` |
| `observe` | Span vocabulary, the bounded JSON-lines log, and `StandardRedactor`. | — |

Two pairs are deliberately split and are the ones most often confused.

**`domain::ToolCall` records *that* a tool ran; `tool` is what defines and
executes one.** `store` and `tool` both build on `domain` and not on each other,
so persistence and execution are reasoned about — and tested — separately.

**`trust::TrustState` is about running one workspace's code;
`integration::TrustState` is about an external subject whose identity can change
under a decision the user already made.** They are different questions with the
same word attached.

## The containment hierarchy

```text
Task              one piece of work in one workspace
 └── Run          one attempt at that task
      └── Step    one ordered stage of the attempt
           └── ToolCall    one invocation, with its input, output, and decision

Run ──> RunEvent   the append-only per-run log: how the run got where it is
Run ──> Artifact   content too large for a row, on disk beside the database
Run ──> Approval   the questions the run asked a human, and how they were answered
```

Relationships are stored as typed IDs, and containment is enforced by the
*database* rather than re-checked in Rust: a tool call may only name a step of
the run it claims, and an event, an artifact, and an approval may only name a
step or a call of their own run. See
[Run lifecycle and storage](run-lifecycle-and-storage.md#the-schema).

Every durable record carries a schema version that is probed before its strict
body is parsed, so a row written by a newer build reads as an upgrade request
rather than as corruption. Every row is rebuilt into its wire record and
re-validated by `domain` on load, which is what makes an impossible record
unable to enter the process at all.

## The two state machines

Runs and steps share one table. Tool calls have their own, because a call has
two states a run does not need — `awaiting_approval`, which is a question
outstanding, and `denied`, which is a refusal rather than a failure.

### Runs and steps

`ExecutionState`, spelled in `runs.state` and `steps.state`.

| From \ To | `running` | `waiting_for_approval` | `succeeded` | `failed` | `cancelled` | `interrupted` |
| --- | --- | --- | --- | --- | --- | --- |
| `queued` | ✅ | ❌ | ❌ | ✅ | ✅ | ✅ |
| `running` | ❌ | ✅ | ✅ | ✅ | ✅ | ✅ |
| `waiting_for_approval` | ✅ | ❌ | ❌ | ✅ | ✅ | ✅ |
| `succeeded` | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| `failed` | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| `cancelled` | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| `interrupted` | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |

Two absences carry the meaning. `queued -> succeeded` is missing because work
that never started did not succeed, and `queued -> waiting_for_approval` is
missing because nothing has been asked yet. `waiting_for_approval -> succeeded`
is missing for the same reason: a granted approval returns the record to
`running`, and finishing is a second transition.

### Tool calls

`ToolCallState`, spelled in `tool_calls.state`.

| From \ To | `awaiting_approval` | `running` | `succeeded` | `failed` | `denied` | `cancelled` | `interrupted` |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `pending` | ✅ | ✅ | ❌ | ✅ | ✅ | ✅ | ✅ |
| `awaiting_approval` | ❌ | ✅ | ❌ | ❌ | ✅ | ✅ | ✅ |
| `running` | ❌ | ❌ | ✅ | ✅ | ❌ | ✅ | ✅ |
| `succeeded` | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| `failed` | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| `denied` | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| `cancelled` | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| `interrupted` | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |

`awaiting_approval -> failed` is absent on purpose: a call parked on a question
that is never answered ends `denied`, `cancelled`, or `interrupted`, each of
which says *why* nobody ran it. A call that reached `pending` and then failed
schema validation is `failed`, and `pending -> succeeded` is absent because
nothing can succeed without having run.

`interrupted` is written by the recovery sweep and by nothing else, which is what
makes it mean exactly one thing: the owning process stopped.

### What holds the tables to these shapes

Both tables are constants — `domain::EXECUTION_TRANSITIONS` and
`domain::TOOL_CALL_TRANSITIONS` — and the tests below drive *every* ordered pair
of states, asserting that a declared edge succeeds and that every other pair is
refused with the record left untouched. Editing a table without editing this
document is therefore catchable in review, and the reverse is caught by
`.github/scripts/verify-doc-references.sh`.

| Table | Package | Test |
| --- | --- | --- |
| `EXECUTION_TRANSITIONS` | `harkness-runtime` | `domain::record::tests::every_declared_execution_transition_succeeds_and_every_other_pair_is_invalid` |
| `TOOL_CALL_TRANSITIONS` | `harkness-runtime` | `domain::record::tests::every_declared_tool_call_transition_succeeds_and_every_other_pair_is_invalid` |
| both | `harkness-runtime` | `domain::state::tests::no_transition_leaves_a_terminal_state` |
| persistence | `harkness-runtime` | `store::tests::an_invalid_run_transition_leaves_the_row_untouched` |

The approval record has a third, much smaller table; it lives in
[Approvals](approvals.md#the-lifecycle).

## What one tool call actually goes through

An agent may *request* a tool. Everything between the request and the tool body
belongs to a different module, and the order is fixed:

```text
agent asks for (tool_id, version, input)
  │
  ├─ tool::ToolRegistry      resolve (id, version)          → unknown_tool / unknown_tool_version
  ├─ tool::ErasedTool        validate input vs its schema   → invalid_input, before any body runs
  ├─ Tool::request_effects   derive paths and flags         → forbidden_path
  ├─ trust::classify_request floor at the declared risk     → a RequestClassification
  ├─ policy::evaluate        built-in ∪ user ∪ repository   → allow | ask | deny
  ├─ approval                persist the question, park     → granted | denied | expired | cancelled
  ├─ schedule::submit        workspace slot, read cap, PIDs → queued, then dispatched
  ├─ tool::ToolExecutor      run on its own thread          → succeeded | failed | cancelled | timed_out
  ├─ tool::ErasedTool        validate output vs its schema  → invalid_output, result discarded
  └─ store                   terminal state + its event, in one transaction
```

Four properties come from that order rather than from any one step.

**Validation precedes policy**, so policy classifies what will actually execute
rather than an unparsed blob. **Validation precedes execution**, so a rejected
input means the body provably never ran — `ToolError::happened_before_execution`
states it in the type. **The approval is persisted before any surface is
notified**, so a pause that only exists in one process's memory is not a state
the store can be found in. And **the terminal state and its event commit
together**, so a timeline that says a run moved and a store that does not are not
states that can disagree.

## Threads, cancellation, and the four locks

There is no async runtime anywhere in the workspace. Concurrency is
`std::thread`, and stopping work is `harkness_git::Cancellation` — one token
type, passed down rather than re-invented per layer.

- A run owns **one worker thread** and one agent.
- A tool body runs on **its own thread**, so a hang becomes a `TimedOut` outcome
  rather than a wait with no end. Rust cannot kill a thread, so an unstoppable
  body is abandoned; a *child process* is not, and `ToolProcess` kills its whole
  process group.
- Progress travels over a **bounded channel**, so a tool reporting faster than
  the log can record waits instead of growing a queue.
- Cancelling a run reaches the operating system:

```text
cancel_run → queued calls swept and recorded `cancelled`, undispatched
           → running calls' caller tokens tripped
             → executor cancels each call's own token
               → cooperative body returns / ToolProcess kills the process group
```

Four independent locking mechanisms exist, and confusing them is the main source
of deadlock risk. **The ordering is: scheduler workspace slot, then repository
lock, then catalog lock.** The store takes none of them, and no caller may hold a
store transaction while acquiring any.

| Mechanism | Scope | Held across |
| --- | --- | --- |
| Scheduler workspace slot (`schedule`) | in-process, keyed by `(ProjectId, canonical root)` | never an executor call, a store write, or a child wait |
| Repository lock (`harkness-git`) | advisory file lock keyed by Git's *common* directory | network operations |
| Catalog lock (`harkness-core`) | global across all projects | never a long Git operation |
| Run store (`store`) | one mutex-guarded writer plus pooled readers | never a user wait |

The coordinator's lease and recovery locks are not a fifth mechanism: a lease is
taken once at construction and held for the coordinator's life, the recovery lock
is taken and released inside one startup sweep, and neither is held while any of
the four is acquired. The agent registry's `agents.lock` is not one either; its
order against the store is fixed at `agents.lock` → run store and never the
reverse. `AGENTS.md` is the normative statement of all of this.

**No transaction is ever held across a user wait.** Work that needs a human
decision persists the request, commits, and only then parks on a condition
variable. The store's single writer stays free for every other run for as long as
the user takes to answer.

## The front-end boundary

The command line and the window are two readers and two drivers of one runtime.
Neither contains a rule the other does not.

- **Both build the same `ToolRegistry`** and drive the same `RunCoordinator`, so
  what the window runs and what an agent runs is the same typed operation under
  the same gates. `harkness tool invoke` is the proof that a tool needs no model:
  it resolves, validates, evaluates policy, records the call, and executes,
  through the ordinary pipeline. It is not a bypass, and the call it records is
  readable afterwards with `harkness run show`.

  <!-- verified -->
  ```sh
  harkness --json tool invoke workspace.inspect --input '{}' --project ws
  harkness --json contract
  ```
- **QML holds no domain logic.** Every run surface is a projection over a Rust
  bridge object; the vocabulary a surface renders — state words, event kinds, the
  palette — is shared QML, and the decisions behind them are the runtime's.
- **A read never creates a run store, and never builds a coordinator.** Building
  one takes the lease and runs the recovery sweep, which are writes — and a user
  who opened a panel to look at something has not asked for a write.
- **One coordinator per process.** A second would take a second lease and leave
  every run the first started uncancellable.
- **The scopes a surface may offer are the record's own.** `ApprovalRequest`
  publishes its `grantable_scopes`, so no front end can express a breadth the
  runtime would refuse.

`front_end_equivalence::the_window_and_the_command_line_report_the_same_runs_states_and_events`
is where that claim is checked: one store, two readers.

## The extension seams

v0.3 is deliberately closed — the only agent is `MockAgent` and the only tools
are the nine compiled in. The seams the later milestones plug into already exist
and are named here so the map is complete.

`harkness-provider` is the model-endpoint boundary: a provider-neutral message
and event vocabulary, a streaming turn assembler, and a scripted provider backed
by frozen JSON, with every concrete adapter's wire types private to its module.
`harkness-acp` and `harkness-mcp` speak the Agent Client Protocol and the Model
Context Protocol over the shared subprocess JSON-RPC engine in
`harkness-transport`; `harkness-forge` is forge-neutral contracts, and
`harkness-recipe` is the workflow-recipe format. All four are *below* the
runtime, none of them may depend on a front end or on each other, and none of
them decides whether a call may proceed: an external subject that is registered,
enabled, trusted and unchanged still passes `policy` and `approval` on every
action it takes. ADR-0009 through ADR-0018, under `docs/adr/`, decide the layering, the
protocol revisions, the transport seam, the trust shape, and the activity
classes. `docs/acp.md` and `docs/agents.md` are the ACP and agent-registry
references.

## Where to read next

| Question | Document |
| --- | --- |
| How do I write a tool? | [Tool authoring](tool-authoring.md) |
| What decides whether a call may run? | [Policy](policy.md) |
| What does approving something actually authorize? | [Approvals](approvals.md) |
| What is written down, and where? | [Run lifecycle and storage](run-lifecycle-and-storage.md) |
| What can I run to see all of this work? | [Mock-agent scenarios](mock-agent-scenarios.md) |
| How do I debug a run that failed at 2 a.m.? | [Diagnostics and redaction](observability.md) |
| What proves any of this? | [The verification suite](verification-suite.md) |
