# ADR-0002: Model provider, native agent, and external coding agent are three contracts

- **Status**: Accepted
- **Date**: 2026-08-10
- **Deciders**: Taiye Babatope
- **Implemented by**: [#111](https://github.com/fullstacktaiye/harkness/issues/111), [#124](https://github.com/fullstacktaiye/harkness/issues/124), [#125](https://github.com/fullstacktaiye/harkness/issues/125), [#126](https://github.com/fullstacktaiye/harkness/issues/126)
- **Builds on**: [#96](https://github.com/fullstacktaiye/harkness/issues/96) (the `Agent` seam), [#97](https://github.com/fullstacktaiye/harkness/issues/97) (the coordinator), ADR-0001

## Context

"Agent" is used in the surrounding ecosystem for at least three different
things: an HTTP endpoint that completes messages, an orchestration loop that
decides what to do next, and a separately shipped program that owns its own loop
and edits files on its own. They have different lifetimes, different failure
modes, and different trust properties, and the only thing they have in common is
the word.

Harkness already has one of the three. `Agent`
([#96](https://github.com/fullstacktaiye/harkness/issues/96), planned) is
`next_action(Observation) -> AgentAction` — a decision-maker driven by the
`RunCoordinator` ([#97](https://github.com/fullstacktaiye/harkness/issues/97),
planned), which executes every resulting tool call through the registry
([#87](https://github.com/fullstacktaiye/harkness/issues/87)), policy
([#91](https://github.com/fullstacktaiye/harkness/issues/91)), approvals
([#92](https://github.com/fullstacktaiye/harkness/issues/92)), and persistence
([#86](https://github.com/fullstacktaiye/harkness/issues/86),
[#88](https://github.com/fullstacktaiye/harkness/issues/88)). v0.4 adds the
second — a model endpoint — and v0.5 adds the third
([#149](https://github.com/fullstacktaiye/harkness/issues/149)–[#156](https://github.com/fullstacktaiye/harkness/issues/156),
ACP).

The tempting move is one `AgentProvider` abstraction that covers a model
endpoint and an external coding agent, since both "produce actions from a
prompt". Taking it would mean the union of two capability sets, the union of two
failure taxonomies, and a permission model that is meaningful for one and vacuous
for the other — and it would have to be unpicked before the ACP milestone could
ship anything honest.

## Decision

Fix three terms. They are distinct contracts, and no type unifies them.

**A model provider** accepts messages and tool definitions and streams back text
and tool-call *requests*. It has no filesystem, Git, process, or credential
access; it cannot execute anything. Its contract is `ModelProvider::stream`
([#111](https://github.com/fullstacktaiye/harkness/issues/111)). Its wire types
are private to its adapter module. Its failures are transport and protocol
failures — `endpoint_unreachable`, `rate_limited`, `context_overflow`,
`disconnected` — none of which can leave a workspace modified.

**The native agent** is Harkness. It owns planning, context selection, prompt
construction, tool execution, policy evaluation, approval gating, verification,
persistence, retry, cancellation, and completion. It implements the `Agent`
trait ([#96](https://github.com/fullstacktaiye/harkness/issues/96)) and is
driven by the unchanged coordinator; the `ModelAgent`
([#126](https://github.com/fullstacktaiye/harkness/issues/126)) is the
implementation that consults a model provider between decisions. The model never
touches a tool; it asks, and the runtime decides.

**An external coding agent** is a separate program that owns its own loop and
would ask Harkness for permission to read, write, and execute. It is not a model
provider and it is not an `Agent` implementation in the v0.4 sense: its
capabilities, permission requests, session lifecycle, and trust story are its
own. **Hosting one is explicitly deferred** to the ACP milestone
([#149](https://github.com/fullstacktaiye/harkness/issues/149)–[#156](https://github.com/fullstacktaiye/harkness/issues/156)).
v0.4 ships no external-agent integration and names the seam only.

No public item in the workspace is named `AgentProvider`, and no trait is
implemented by both a model endpoint and an external coding agent
([#111](https://github.com/fullstacktaiye/harkness/issues/111) acceptance
criterion).

The three meet in one place and in one direction: the native agent consumes a
model provider. An external coding agent, when it arrives, will sit beside the
native agent under the coordinator — consuming Harkness tools rather than being
consumed by them — and it will bring its own contract, not widen this one.

## Consequences

- The `Agent` trait needs no new variants for v0.4. Context arrives as an
  ordinary `Observation::ToolResult` from a context tool, verification as a
  `ToolResult` from a test tool, denials as `PolicyDenied`/`ApprovalOutcome`.
  Streaming reaches the UI through an event sink injected at construction, not
  through a trait change — so the mock agent and its scenarios keep compiling.
- Capability negotiation stays honest. `ProviderCapabilities` describes what an
  endpoint supports and nothing else; unknown means unknown and callers degrade
  conservatively. There is no field that means "may write files", because a
  provider cannot.
- Permission and approval prompts remain about Harkness tools, so the user is
  always approving something Harkness will execute and can describe exactly.
  When ACP arrives, its permission requests map *into* that model
  ([#152](https://github.com/fullstacktaiye/harkness/issues/152)) rather than
  running beside it.
- The vocabulary is enforced in rustdoc and in naming, which is weaker than a
  type check. A reviewer has to notice a proposal that reintroduces the merged
  abstraction under a new name.
- Some duplication is accepted: streaming assembly, cancellation, and session
  persistence will each exist in a model-provider form now and an ACP form
  later. Two honest implementations beat one that lies about half its inputs.
- Deferring external agents means v0.4 cannot claim to run Claude Code, Codex
  CLI, or Gemini CLI. That is a real scope limit and is stated as one.

## Alternatives considered

**One `AgentProvider` trait covering both.** Fewer concepts, one configuration
surface, one UI. Rejected: it forces a union capability set that is
unimplementable for half its implementors, gives permission requests two
incompatible meanings, and makes every later ACP feature negotiate with a
contract shaped by an HTTP endpoint. The abstraction would be paid for twice —
once to build it and once to remove it.

**Model providers as a special case of external agents** ("an endpoint is just
an agent with no tools"). Rejected: it inverts the trust story. An external agent
is a program to be sandboxed and mediated; a model endpoint is a network service
that returns text. Treating the smaller thing as a degenerate case of the larger
one imports the larger one's whole security surface for no gain.

**External agents as a special case of model providers** ("an agent is just an
endpoint that streams tool calls"). Rejected for the mirror reason: it hides
that the external agent already ran the tool, already made the decision, and is
reporting rather than requesting. Harkness would record requests it never
granted.

**Deciding later, after the first adapter ships.** Rejected: the first adapter's
shape becomes the de-facto contract, which is precisely the lock-in ADR-0007
exists to avoid. The vocabulary costs nothing now and is expensive to retrofit
once records name it.
