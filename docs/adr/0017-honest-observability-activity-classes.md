# ADR-0017: Every activity carries an evidence class, and a claim is never shown as a fact

- **Status**: Accepted
- **Date**: 2026-08-13
- **Deciders**: Taiye Babatope
- **Implemented by**: [#154](https://github.com/fullstacktaiye/harkness/issues/154), [#151](https://github.com/fullstacktaiye/harkness/issues/151), [#153](https://github.com/fullstacktaiye/harkness/issues/153), [#177](https://github.com/fullstacktaiye/harkness/issues/177)
- **Builds on**: ADR-0006 (repository content is untrusted), ADR-0008 (workspace snapshot identity), [#88](https://github.com/fullstacktaiye/harkness/issues/88) (event log), ADR-0016

## Context

Every run record in Harkness today describes something Harkness did. A
`ToolCall` row exists because the runtime invoked a tool, validated its input,
executed it, and validated its output. The record and the act are the same event,
so the question of how much to believe it never comes up.

An external ACP agent breaks that. The agent runs as its own process and reports
what it is doing through a `session/update` stream: plan entries, message chunks,
and `tool_call` and `tool_call_update` notifications carrying titles, statuses,
content, and locations. Those are the agent's account of its own work. Harkness
did not execute them, did not validate them, and cannot verify most of them. An
agent that streams a `tool_call` saying "ran the test suite — 48 passed" has
asserted something; nothing about receiving that message makes it true.

The protocol offers no way to observe what an agent does outside the `fs/*` and
`terminal/*` methods it chooses to route through the client. An agent can write
files directly. v0.5 has no OS-level sandbox — no namespaces, no seccomp, no
container — so an agent process runs with the user's privileges and can touch the
filesystem outside its worktree without Harkness seeing it.

Harkness does have two other sources of knowledge. Anything routed through
mediated `fs/*` and `terminal/*` calls
([#153](https://github.com/fullstacktaiye/harkness/issues/153)) passes through
Harkness's own tools, with policy applied. And workspace snapshots (ADR-0008)
taken before and after a session, diffed, show what actually changed on disk —
without saying who changed it or why.

Four kinds of knowledge with genuinely different strength, plus one gap. The
failure mode is that they render identically: a UI stream in which an agent's
claim about a passing test suite looks exactly like a test result Harkness
produced, and a user merges on the strength of it.

## Decision

**Every persisted activity row and every user-facing presentation carries exactly
one `ActivityClass`.** It is a domain enum, persisted on the event, not a UI
heuristic applied at render time.

| Class | Means | Example |
| --- | --- | --- |
| `HarknessObserved` | Harkness executed it and holds the result | a typed tool call in the registry; a Git command through the runner |
| `HarknessMediated` | An external party asked, Harkness performed it under policy | an agent's `fs/write_text_file` served by [#153](https://github.com/fullstacktaiye/harkness/issues/153) |
| `AcpReported` | The agent said so; Harkness has no evidence | every `session/update`: plans, message chunks, `tool_call` reports |
| `SnapshotInferred` | A before/after diff shows it happened; attribution is inference | a file the agent wrote directly, seen by diffing snapshots |
| `Unobserved` | Harkness knows it cannot know | anything the agent did outside the worktree |

**Exactly one.** A claim that also shows up in a snapshot diff does not become
observed — two records exist, one `AcpReported` and one `SnapshotInferred`, and
the reconciliation between them is itself evidence. Collapsing them would destroy
the interesting case, which is when they disagree.

**An agent claim is never presented as a verified fact.** Not in the UI, not in a
run report, not in a summary, and not in the final diff handoff. `AcpReported` is
rendered visually and semantically distinctly from `HarknessObserved`
([#177](https://github.com/fullstacktaiye/harkness/issues/177)), and where a
class is `AcpReported` the presentation says whose claim it is.

**"The tests passed" is a claim until Harkness runs them.** An agent reporting a
successful verification produces an `AcpReported` record. If Harkness wants to
state it, Harkness runs the tests through its own tooling and produces a
`HarknessObserved` record beside it. This is the point at which the classification
stops being bookkeeping and starts being the difference between a truthful run
report and a plausible one.

**`Unobserved` is a class, not an omission.** The absence of OS sandboxing in
v0.5 is recorded as a limitation the model can express, and the user
documentation ([#184](https://github.com/fullstacktaiye/harkness/issues/184))
states it plainly: Harkness does not confine an external agent process to its
worktree, it observes what it can and says what it cannot. Naming the gap is what
stops a later contributor from building policy on containment that was never
enforced.

**Detection within the observable boundary still applies.** Changes to *other*
Harkness-managed worktrees of the same repository are detected through their
snapshots and produce a security event and a failed run, rather than a quiet
surprise in a different checkout.

## Consequences

- A run report can be read as evidence, because each line says how it is known.
  This is the deliverable; everything else here is the mechanism.
- Users see more nuance than they asked for. A stream with five visually distinct
  classes is busier than one undifferentiated log, and the UI work to make that
  legible rather than noisy is real
  ([#177](https://github.com/fullstacktaiye/harkness/issues/177)).
- Snapshots have a cost. Pre-session, post-session, and bounded per-turn captures
  are filesystem work proportional to the worktree, and per-turn capture is
  bounded rather than continuous for exactly that reason. Without them,
  everything an agent did directly would be `Unobserved`.
- Attribution from a diff is inference and is labelled as such. A user editing
  files in their editor during an agent session will have those edits appear as
  `SnapshotInferred`, and reconciliation against mediated writes
  ([#153](https://github.com/fullstacktaiye/harkness/issues/153)) narrows this
  but does not eliminate it.
- Every new activity-producing code path has to choose a class, and the honest
  choice is sometimes the unflattering one. A contributor who wants a nicer
  timeline will be tempted to classify a claim as observed; that is a
  review-blocking defect, and the adversarial suite
  ([#182](https://github.com/fullstacktaiye/harkness/issues/182)) asserts against
  it.
- Harkness will sometimes look less capable than a tool that renders agent claims
  as results. A competitor's timeline saying "✅ tests passed" reads better than
  one saying "agent reported: tests passed". Being right is the product.
- The classification is persisted, so historical runs remain classifiable and the
  model can be audited after the fact. It is also a new enum in a durable format,
  with the schema-version and frozen-fixture discipline that implies.
- If a future milestone adds OS-level sandboxing, the classification model does
  not change — `Unobserved` simply gets smaller. That is the seam this ADR leaves
  open.

## Alternatives considered

**One undifferentiated activity stream.** Simplest to build, simplest to read,
and it is what most agent UIs ship. Rejected: it launders claims into facts,
which is the failure this ADR exists to prevent. The user cannot recover the
distinction from a stream that never recorded it.

**Two classes: verified and unverified.** Most of the benefit, a fraction of the
complexity. Rejected: it collapses distinctions the user needs to act on.
`HarknessMediated` (Harkness performed it under policy) and `SnapshotInferred`
(the bytes changed, attribution inferred) are both "unverified" and mean very
different things, and `Unobserved` — the honest admission — has no home at all in
a binary.

**Classify at render time from the record's source.** No persisted enum, no
schema change, and the UI can be improved later. Rejected: it makes historical
runs unclassifiable, it puts a security-relevant distinction in presentation code
where it can be lost in a refactor, and it means two front ends can disagree
about what the same record means.

**Verify agent claims automatically** — re-run every reported command and promote
the claim to `HarknessObserved` when it checks out. Rejected as a *substitute*:
most claims are not mechanically checkable ("I refactored the parser"), re-running
side-effecting commands is unsafe, and the cost is unbounded. Harkness does run
verification deliberately
([#131](https://github.com/fullstacktaiye/harkness/issues/131),
[#132](https://github.com/fullstacktaiye/harkness/issues/132)); the result is a
separate `HarknessObserved` record, which is this decision working, not an
alternative to it.

**Trust reports from agents whose executable the user trusted** (ADR-0016).
Rejected on the same distinction ADR-0006 draws for trusted repositories: trust
means "the user accepts running this program", not "this program's self-reports
are evidence". A trusted agent can still be wrong, and a buggy agent reporting a
passing test suite is the ordinary case, not the adversarial one.

**Ship OS-level sandboxing in v0.5 so `Unobserved` is empty.** The genuinely
better answer, eventually. Rejected as scope: namespaces, seccomp, and container
integration are platform-specific and large, and Harkness targets Linux desktops
today with macOS and Windows in CI. Deferring it is defensible; implying it
exists is not, which is why `Unobserved` is a named class rather than silence.
