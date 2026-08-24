# Approvals

[Policy](policy.md) can answer `Ask`. This document is what happens next: the
question becomes a durable record before anybody is shown it, an answer becomes a
grant bound to the exact call it was given for, and the call that was waiting
learns what happened through a structured observation rather than a hang.

Three sentences carry most of it.

- **An answer authorizes exactly what was asked.** A grant is matched on the run,
  the workspace identity, the tool id, the tool version, and — at the narrowest
  scope — a hash of the validated input. One mismatch on any axis is not a weaker
  match; it is no match.
- **Absence of an answer is never consent.** Closing a window, dismissing a
  surface, losing a process, and letting a deadline pass all leave the request
  unanswered, and each is recorded as what it was rather than as a refusal
  somebody made.
- **Nothing is held while a human thinks.** The waiting call holds no database
  transaction and no scheduler slot.

- [Two things called an approval](#two-things-called-an-approval)
- [The scopes](#the-scopes)
- [What a grant is bound to](#what-a-grant-is-bound-to)
- [The exact-binding hash](#the-exact-binding-hash)
- [One-call-only work](#one-call-only-work)
- [The lifecycle](#the-lifecycle)
- [Expiry](#expiry)
- [Restarts and cancellation](#restarts-and-cancellation)
- [Answering one](#answering-one)
- [What is recorded](#what-is-recorded)
- [What proves this](#what-proves-this)

## Two things called an approval

`domain::Approval` is the audit entry appended to a run, step, or tool-call
record. It says *that* a record was approved, and it travels with the record.

`approval::ApprovalRequest` is the question itself: an identity, a lifecycle,
an expiry, and the fields a grant is matched on. It is a row in its own table,
because it has to be listed across runs after a restart and answered from either
front end.

The first is a line in a record's history. The second is what this document is
about.

## The scopes

There are three, and they are the durable vocabulary a request is stored with
and a surface renders.

| Scope | Authorizes | Additional axes matched |
| --- | --- | --- |
| `exact_call` | This tool, this version, this exact input, this run. | the recorded call, the tool identity, the input hash |
| `tool_for_run` | This tool and version for the rest of the run, whatever the input. | the tool identity |
| `capability_for_run` | The declared capabilities of this request, for the rest of the run. | every capability the candidate requires must be in the grant's set |

`capability_for_run` is a subset test, not a set-equality test: a candidate
requiring `{fs.write}` is covered by a grant holding `{fs.write, process.spawn}`,
and a candidate requiring `{fs.write, process.spawn}` is not covered by a grant
holding `{fs.write}` alone. A candidate declaring *no* capabilities is covered by
nothing at this scope, because "the empty set is a subset of everything" would
make a capability grant a grant for anything.

A request may only be created at `capability_for_run` if it actually declares a
capability; otherwise the scope would describe nothing.

`ApprovalRequest::grantable_scopes` is what a surface offers. It is the record's
own answer, not a second derivation: it returns `[exact_call]` when the request
was reduced to one call, and `[exact_call, <effective>]` otherwise — narrowest
first. A front end therefore cannot show a breadth `decide` would refuse.

`policy::RunGrantScope` is the in-memory projection of the same three. Policy is
told only that the matcher accepted a grant and at what breadth, never the fields
the matcher used to decide.

## What a grant is bound to

`grant_applies` is the security core. Every scope binds the run and the workspace
identity; each scope then adds the axes that give it meaning.

```text
                           exact_call   tool_for_run   capability_for_run
run id                          ✅            ✅                ✅
workspace binding               ✅            ✅                ✅
recorded tool call              ✅            —                 —
tool id + version               ✅            ✅                —
canonical input hash            ✅            —                 —
declared capabilities           —             —                 ✅ (subset)
external identity evidence      ✅            ✅                ✅
```

Two of those rows are easy to under-read.

**The workspace binding is both halves.** A catalog identity alone survives the
checkout being moved; a path alone is reused by whatever project occupies it
next. A grant that matched on either half by itself would replay across
checkouts. A workspace with no catalog identity is bound by its canonical root
alone, and absent matches only absent.

**The tool *version* is matched, not just the id.** A new version is code the
approver never saw.

There is no partial application and no "close enough" — `matching_grants` returns
only the grants that cover a candidate, and `ApprovalGrant::matching` is the only
production route to a `policy::RunGrant`, which policy itself cannot construct.
That is what keeps "an approval exists for this call" a claim one module makes
rather than one any caller can assert.

A grant's lifetime is its **run**. The request's `expires_at` is deliberately not
carried across: it is a deadline for a *human to answer*, so the only thing it
can do is stop a request from ever becoming a grant. Reusing it as the grant's
lifetime would make a `tool_for_run` approval given "for the remainder of the
run" quietly stop applying part-way through it.

## The exact-binding hash

An `exact_call` grant is only worth anything if changing the input it authorized
produces a different identity. The encoding is therefore a security boundary
rather than a formatting preference, and it is frozen.

**What is hashed.** The *validated* tool input — every field the tool will
actually deserialize — canonically encoded, then absorbed as:

```text
SHA-256( len("harkness.approval.canonical-input.v1") ‖ "harkness.approval.canonical-input.v1"
       ‖ len(canonical text)                        ‖ canonical text )
```

Length framing makes the concatenation injective, and the domain constant carries
its own version, so a future encoding is a new constant and a new hash rather
than a silent change in what an old recorded hash means. The result is stored in
`approvals.input_hash` as 64 lowercase hexadecimal characters; uppercase is
refused on parse rather than folded, because the column is compared as text.

**The canonical encoding**, one spelling per value:

| Value | Spelling |
| --- | --- |
| `null`, `true`, `false` | themselves |
| Integers | decimal, no plus sign, no leading zeros |
| Other numbers | the shortest decimal that round-trips through `f64`, always carrying `.` or `e` |
| Strings | quoted, escaping only `"`, `\`, and the control characters below `U+0020`; every other character, non-ASCII included, is its own UTF-8 bytes |
| Arrays | their own order — array order is part of the value |
| Objects | keys sorted by **UTF-8 byte order**, no repeated key |

No insignificant whitespace appears anywhere, so the encoding is a function of
the value alone and never of how the value was parsed or printed. A non-finite
number has no JSON spelling and is *refused* rather than encoded: `serde_json`
renders both infinities and NaN as `null`, so encoding one would fold two visibly
different inputs into one hash — the exact collision this module exists to
prevent.

What follows from that: `{"a":1,"b":2}` and `{ "b" : 2 , "a" : 1 }` hash equal,
`[1,2]` and `[2,1]` do not, and changing any byte of any field yields a different
hash and therefore defeats an `exact_call` grant.

**The value itself is never in the approval row.** `approvals.input_summary` is a
short human-readable digest; the input stays in `tool_calls.input_json`, where a
surface expands it on demand. That column is also the one document the store
deliberately does *not* redact, because it is what the executor reads back and
runs, and what this hash was taken over — rewriting it would run a different
command than the one that was approved.

`canonical-input-v1.json` under `crates/harkness-runtime/src/approval/fixtures/`
is frozen wire evidence of the encoding. Its regenerator carries the strongest
warning in the repository: run it only when a *new* hash domain is published,
because every stored `input_hash` was derived under the encoding it pins.

## One-call-only work

A `remote_write` or `destructive` request is a one-call approval whatever was
asked for.

The ceiling is applied **when the request is created**, not when a grant is
matched. The stored row therefore shows the downgrade — `requested_scope:
tool_for_run`, `effective_scope: exact_call` — instead of recording a breadth
that was never honored. A record claiming a breadth the matcher would never
apply would be a lie in the audit trail rather than a defence in depth.

Two consequences follow. A surface rendering such a request offers no scope
choice at all, because `grantable_scopes` is a single entry. And policy applies
the same rule from its own side: when the effective risk is `remote_write` or
`destructive`, only an `ExactCall` grant is even considered, so a broad grant
obtained earlier in the run cannot answer one.

## The lifecycle

Only `pending` has outgoing edges. Every other state is final, which is what
makes "approval is granted before execution, never retroactively" checkable: a
resolved request can never become a grant.

| State | Reached by | A grant? |
| --- | --- | --- |
| `pending` | the request being recorded | no — the only non-final state |
| `granted` | a human allowing it, at the scope the decision names | yes |
| `denied` | a human refusing it | no |
| `expired` | outliving `expires_at` without an answer | no |
| `cancelled` | the run being cancelled while it waited | no |
| `superseded` | the run being abandoned, so the question has no subject | no |

```text
pending ─┬─> granted
         ├─> denied
         ├─> expired
         ├─> cancelled
         └─> superseded
```

`ApprovalGrant::of` projects a request into a grant only from `granted`, and
carries the *effective* scope — what was allowed, not what was asked. Every other
state yields no grant at all, so "a dead approval authorizes nothing" is a shape
the type system holds rather than a check somebody has to remember.

The three unanswered endings record **no decision**. An `expired`, `cancelled`,
or `superseded` row has a `resolved_at` and nothing in its decision columns,
because nobody answered it and writing a refusal there would make the audit claim
one. The waiter still observes a denial — it has to, or it would hang — and the
observation carries the terminal state beside the verdict, so a caller can tell
"a human said no" from "the run was cancelled underneath you".

## Expiry

An expiry is a deadline for a human, and it is enforced where the answer arrives
rather than by a timer nobody checks.

`ApprovalRequest::decide` reads the deadline against the instant supplied with
the decision, and refuses a late answer with `Expired`. **Refusing keeps the
record `pending`** — closing it is a separate `expire` transition, so the waiter
observes an expiry rather than a late grant, and the row never passes through a
state nobody put it in. A lapsed request therefore still reads as `pending` until
a sweeper closes it, which is why the window withdraws its Approve button while
the stored state still says `pending`: a deadline is closed by a sweeper and not
by the clock.

v0.3 sets no expiry on the requests it creates (`expires_at` is `null`), so in
practice a question waits until it is answered, the run is cancelled, or the
process stops.

## Restarts and cancellation

The request is written and committed *before* any surface is notified, because a
pause that only exists in one process's memory is not a pause a user can be asked
to survive. Restarting therefore lists the pending requests with every binding
field intact.

What happens to them depends on what happened to their run:

- **The process was killed.** The next coordinator's recovery sweep marks the run
  `interrupted`, its unfinished steps and in-flight calls with it, and every
  approval nobody can answer any more as `superseded` — each with its own
  appended event. Nothing is resumed.
- **The run was cancelled.** Approvals it was waiting on resolve as `cancelled`.
- **A retry is started.** It is a *new* run for the same task. No approval
  carries over, because a grant's lifetime is its run.

An answer that arrives for an approval with no live waiter is discarded rather
than kept. That is the common case and not a lost wake-up: a restart has already
superseded the requests an interrupted run left behind, and a cancellation has
already resolved the ones whose callers exited. An answer kept for a waiter that
will never arrive is a leak rather than a safety net.

## Answering one

### On the command line

The question goes to standard error — as a progress envelope under `--json`, so
standard output stays exactly one result object — and one line comes back on
standard input.

```text
approval 21ad694d-… requested: fs.apply_patch 1.0.0 (workspace_write risk, at most scope tool_for_run) — request to run fs.apply_patch@1.0.0
answer approve (this call only), approve-tool, approve-capability, deny, or show-input
```

| Answer | Grants |
| --- | --- |
| `approve`, `approve-call` | `exact_call` — this call and nothing else |
| `approve-tool` | `tool_for_run` |
| `approve-capability` | `capability_for_run` |
| `deny` | nothing; the reason is recorded |
| `show-input` | nothing; prints the recorded input and asks again |

**The bare answer is the narrowest one.** Widening is a separate word a person
has to type, so nobody grants a tool for the rest of a run by answering the
question in front of them. Every answer is still narrowed against what the stored
request permits, so a remote-write or destructive request — already reduced to a
single call when it was created — cannot be widened at all.

Closing standard input is a denial. Ctrl-C at a prompt cancels the run and exits
130, recording no decision. Without `--interactive` there is nobody to ask, so
the request is denied with kind `approval_required_noninteractive` at exit 3.

`harkness approvals approve <id> --scope call|tool-this-run|capability-this-run`
and `harkness approvals deny <id>` are the same three breadths as a command. They
reach a *live* waiter — a thread parked in the process that started the run — so
a second process reports `approval_not_active` rather than persisting a decision
nothing would ever act on.

### In the window

The review surface is a **page**, deliberately not a dialog. A dialog has an
implicit accept: escape, the close button and a click outside all resolve one,
and the affirmative button is conventionally the default. None of that may be
true of an approval, so the page has Back and nothing else, no default-focused
button, and no code path from navigation, destruction or window close that grants
anything. Leaving leaves the request open.

The page names the tool, its version, the risk, the workspace the answer is bound
to, and the summary the tool published. The exact input the hash binds is a
separate, explicit expansion, rendered as inert monospace text. The breadths
offered are the record's own `grantable_scopes`.

## What is recorded

Every question and every answer is in the run's timeline. From a real run of the
flagship scenario:

```json
{"kind":"approval_requested","payload":{
  "approval_id":"21ad694d-c717-42fb-a63d-468584a691cb",
  "tool":"fs.apply_patch@1.0.0","risk":"workspace_write",
  "requested_scope":"tool_for_run","effective_scope":"tool_for_run",
  "expires_at":null,"summary":"request to run fs.apply_patch@1.0.0"}}
```

```json
{"kind":"approval_decided","payload":{
  "approval_id":"21ad694d-c717-42fb-a63d-468584a691cb",
  "state":"granted","verdict":"granted","scope":"exact_call",
  "decided_via":"cli","reason":"approved on the Harkness command line"}}
```

The `approval_requested` payload carries the summary and never the input.
`decided_via` is recorded because an audit of who authorized what is not answered
by the verdict alone, and because either front end can answer any pending
request. The pair above is the whole story of one decision: asked at
`tool_for_run`, answered at `exact_call`, by the command line.

`harkness --json run show <run-id>` reads the requests back with every binding
field, including `was_downgraded`, which says whether the risk ceiling reduced
the scope before anyone saw it. The queue itself is listed across every run, and
`--all` adds the answered ones, paged by run:

<!-- verified -->
```sh
harkness --json approvals list
harkness --json approvals list --all --limit 10
```

The pending listing takes no page, and that is not an oversight: a request exists
only while a call is parked waiting for it, and the scheduler caps how many calls
can be in flight, so the set is bounded by construction. History is not bounded
that way, which is why `--all` pages by *run* rather than by request.

## What proves this

| Claim | Package | Test |
| --- | --- | --- |
| A grant agreeing on every axis applies at every scope | `harkness-runtime` | `approval::matcher::tests::a_grant_that_agrees_on_every_axis_applies_at_every_scope` |
| One mismatched run defeats every scope | `harkness-runtime` | `approval::matcher::tests::one_mismatched_run_defeats_every_scope` |
| One mismatched workspace defeats every scope | `harkness-runtime` | `approval::matcher::tests::one_mismatched_workspace_defeats_every_scope` |
| A mismatched version defeats the scopes naming a tool | `harkness-runtime` | `approval::matcher::tests::one_mismatched_tool_version_defeats_the_scopes_that_name_a_tool` |
| A mismatched input hash defeats only `exact_call` | `harkness-runtime` | `approval::matcher::tests::a_mismatched_input_hash_defeats_only_the_exact_call_scope` |
| A capability grant covers a subset and refuses anything extra | `harkness-runtime` | `approval::matcher::tests::a_capability_grant_covers_a_subset_and_refuses_anything_extra` |
| A dead lifecycle yields no grant at all | `harkness-runtime` | `approval::matcher::tests::a_dead_lifecycle_yields_no_grant_at_all` |
| An answer deadline never becomes the grant's lifetime | `harkness-runtime` | `approval::matcher::tests::an_answer_deadline_never_becomes_the_lifetime_of_the_grant_it_produced` |
| The canonical encoding and hash are frozen | `harkness-runtime` | `approval::canonical::tests::the_frozen_fixture_pins_the_encoding_and_the_hash` |
| Key order and whitespace do not change the hash | `harkness-runtime` | `approval::canonical::tests::key_order_and_whitespace_do_not_change_the_hash` |
| One changed byte changes the hash | `harkness-runtime` | `approval::canonical::tests::changing_one_byte_of_one_field_changes_the_hash` |
| A non-finite number has no canonical spelling | `harkness-runtime` | `approval::canonical::tests::a_non_finite_double_has_no_canonical_spelling` |
| Only `pending` has outgoing edges, and it reaches every other state | `harkness-runtime` | `approval::record::tests::only_pending_has_outgoing_edges_and_reaches_every_other_state` |
| Remote-write and destructive requests are stored as one call | `harkness-runtime` | `approval::record::tests::remote_write_and_destructive_requests_are_stored_as_one_call_approvals` |
| The grantable scopes are exactly the ones a decision is accepted at | `harkness-runtime` | `approval::record::tests::the_grantable_scopes_are_exactly_the_ones_a_decision_is_accepted_at` |
| A late answer cannot grant the request it lapsed on | `harkness-runtime` | `approval::record::tests::an_answer_after_the_deadline_cannot_grant_the_request_it_lapsed_on` |
| No transaction spans the period a request is pending | `harkness-runtime` | `store::tests::no_transaction_spans_the_period_a_request_is_pending` |
| A pending request survives a crash with every binding field intact | `harkness-runtime` | `store::tests::a_pending_request_survives_a_crash_with_every_binding_field_intact` |
| An unanswered resolution records no verdict in the timeline | `harkness-runtime` | `store::tests::an_unanswered_resolution_records_no_verdict_in_the_timeline` |
| A hand-edited row fails to load instead of becoming a grant | `harkness-runtime` | `store::tests::a_hand_edited_approval_row_fails_to_load_instead_of_becoming_a_grant` |
| Input tampered while parked is refused at dispatch | `harkness-runtime` | `coordinator::tests::approved_dispatch_rejects_input_tampered_while_parked` |
| A wider grant has to be asked for by name | `harkness-cli` | `a_wider_grant_has_to_be_asked_for_by_name` |
| Closing standard input denies | `harkness-cli` | `interactive_mode_denies_when_standard_input_closes_without_an_answer` |
| Ctrl-C at an open prompt records no decision | `harkness-cli` | `ctrl_c_at_an_open_approval_prompt_cancels_without_recording_a_decision` |
