# ADR-0007: One OpenAI-compatible adapter as the tracer bullet

- **Status**: Accepted
- **Date**: 2026-08-10
- **Deciders**: Taiye Babatope
- **Implemented by**: [#125](https://github.com/fullstacktaiye/harkness/issues/125), [#111](https://github.com/fullstacktaiye/harkness/issues/111), [#124](https://github.com/fullstacktaiye/harkness/issues/124)
- **Builds on**: ADR-0002, ADR-0003, [#87](https://github.com/fullstacktaiye/harkness/issues/87) (the descriptors whose schemas become tool definitions), [#96](https://github.com/fullstacktaiye/harkness/issues/96) (the scenario-fixture precedent the scripted provider mirrors), [#104](https://github.com/fullstacktaiye/harkness/issues/104) (the opt-in `#[ignore]`d test convention)

## Context

v0.4 has to prove the whole native workflow end to end against a real model. It
does not have to support every provider, and trying to would be actively harmful:
three half-finished adapters teach less than one complete one, and the second
adapter is worth writing only once the first has found the places the contract
is wrong.

The risk in shipping one adapter is that its wire format silently becomes the
engine's vocabulary. That is the lock-in ADR-0002 draws the boundary against;
this ADR chooses which adapter goes first and states what CI runs against.

The workspace's existing testing discipline points at the answer for the CI
question. Network tests are `#[ignore]`d and run on a self-hosted runner through
`.github/scripts/run-ignored-exact-test.sh <package> <exact::test::name>`, which
fails loudly if the named test disappears — so the default suite is hermetic and
the networked ones are explicit, named, and maintained.

## Decision

**The one production adapter for v0.4 speaks the OpenAI-compatible
chat-completions wire format** (`POST {base_url}/chat/completions`, SSE
streaming, `tools` with `function` entries, `usage` in the final chunk):
`harkness-provider::openai_compat`
([#125](https://github.com/fullstacktaiye/harkness/issues/125)).

One format covers, at minimum:

- **Ollama** (`/v1` compatibility surface)
- **llama.cpp** server
- **vLLM**
- **LM Studio**
- **hosted OpenAI-compatible endpoints**, including gateways and proxies that
  expose the same route

That set is why this format wins the first slot:

- **No paid API is needed to develop or demonstrate Harkness.** A local endpoint
  needs no credential and no account, so the flagship workflow is reproducible by
  anyone with a laptop.
- **It supports the two capabilities the native loop actually requires** — SSE
  streaming and tool calls. A format missing either would not exercise the loop.
- **It is one adapter, not a family.** Ollama's native `/api/chat`, Anthropic's
  messages API, and Gemini's API are explicitly out of scope for v0.4.

**All wire types stay private to the module.** No chat-completions struct,
field name, or error shape appears in `harkness-provider::contract`,
`harkness-runtime`, `runtime.db`, the CLI envelope, or QML. The adapter
translates in both directions: `ModelRequest` in, `ModelEvent` out,
`ProviderError` for every failure. Harkness's dotted tool identifiers are mapped
to the endpoint's `^[a-zA-Z0-9_-]{1,64}$` name grammar (`fs.read` → `fs__read`)
with collision detection and exact reverse mapping, and a mapping failure is
raised before any network I/O.

**CI runs against fakes; real endpoints are opt-in.**

- The **scripted provider**
  ([#111](https://github.com/fullstacktaiye/harkness/issues/111)) implements the
  same `ModelProvider` contract from versioned JSON fixtures and is what every
  loop, prompt, UI, and workflow test uses. It replays byte-identically and can
  inject every `ProviderError` kind plus the malformed-stream matrix — split
  arguments, duplicate and missing tool-call IDs, mid-stream disconnects, missing
  terminal events, empty responses.
- The **adapter's own tests** run against an in-process loopback HTTP fixture
  built on `std::net::TcpListener` plus recorded transcripts. No new test
  dependency, no network, no credentials.
- **Real-endpoint smoke tests are `#[ignore]`d** and run only through the
  existing self-hosted path, `sh .github/scripts/run-ignored-exact-test.sh
  harkness-provider <exact::test::name>`. Renaming one requires updating the
  workflow, exactly as for the existing network tests.

Capabilities are never guessed. `supports_streaming` and `supports_tool_calls`
default true for this adapter family and are overridable per profile;
`context_window` is `None` unless explicitly configured, and callers estimate
conservatively against an unknown window rather than assuming one.

Adding a second adapter is a new module implementing `ModelProvider`, with no
change to `contract`. If a second adapter cannot be added without changing the
contract, the contract is wrong and that is the signal to fix it.

## Consequences

- v0.4 is demonstrable with no account, no key, and no spend, on hardware the
  contributor already has.
- CI is deterministic, offline, and free. Every failure mode the loop must handle
  is reachable in a unit test, including the ones a live endpoint produces only
  intermittently.
- The contract gets validated by two implementations from day one — a fake and a
  real adapter — which catches "the contract is really just adapter #1" earlier
  than a second real adapter would.
- Users on Anthropic, Gemini, or Ollama's native API are unsupported in v0.4
  unless they front their model with a compatible gateway. That is a real limit
  and is stated as one.
- Endpoints in the compatible family differ in what they actually implement:
  some omit `usage`, some ignore `tools`, some return non-SSE error pages with a
  200. The adapter degrades conservatively for each — absent usage leaves
  `TurnOutcome.usage = None`, and a non-SSE body is `malformed_response` with a
  bounded excerpt — rather than assuming uniformity behind a shared format name.
- Recorded transcripts drift from live endpoints over time. The opt-in smoke
  tests are what detect the drift, so they have to actually be run before a
  release; that obligation belongs to the release gate
  ([#141](https://github.com/fullstacktaiye/harkness/issues/141)).
- Following redirects is disabled, so a base URL must be exact. A user pointing
  at a redirecting host gets a typed error instead of credentials replayed to a
  host they did not configure.

## Alternatives considered

**Ship two or three adapters in v0.4** (OpenAI-compatible plus Anthropic plus
Ollama native). Broader support at launch. Rejected: it triples the surface
before the contract has been proven once, and the second adapter's real value —
finding where the contract leaks — is available more cheaply from the scripted
provider plus one real one. Second adapters are cheap *after* the first.

**Ollama's native `/api/chat` as the first adapter.** The most likely local
setup for the target user. Rejected: it covers exactly one server, whereas
Ollama's own `/v1` surface is already in the compatible family — choosing the
native API would mean supporting one endpoint instead of five for the same work.

**Anthropic's messages API first.** Excellent tool-call semantics and streaming.
Rejected for v0.4: it requires a paid key to develop or demo, which makes the
flagship workflow unreproducible for a contributor without an account, and
covers one vendor.

**Mock only the HTTP layer instead of the provider.** Tests would exercise real
SSE parsing everywhere. Rejected as the *primary* strategy: it couples every
loop test to the wire format, which is what the boundary exists to prevent. It
is retained where it belongs — the adapter's own tests use exactly this, against
the loopback fixture.

**Require a live endpoint in CI** (a runner with Ollama). Rejected: nondeterministic
output, model-version-dependent assertions, and a CI failure mode that is a model
mood. The scripted provider gives determinism; the opt-in smoke tests give reality.
