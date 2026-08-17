# ADR-0010: Adopt the official ACP schema crate rather than hand-rolled wire types

- **Status**: Accepted
- **Date**: 2026-08-13
- **Deciders**: Taiye Babatope
- **Implemented by**: [#149](https://github.com/fullstacktaiye/harkness/issues/149)
- **Builds on**: ADR-0009 (wire types are private to the adapter), ADR-0014 (protocol version 1), ADR-0003 (no async runtime)

## Context

The workspace is dependency-light on purpose. Its direct dependencies are the
ones that would be reckless to reimplement — `serde`, `git2`, `rusqlite`,
`schemars`, `uuid`, `time`, `clap`, `cxx-qt` — and the convention has held
through four milestones. Adding an externally governed dependency is therefore a
decision worth writing down rather than a line in a manifest.

The Agent Client Protocol is JSON-RPC 2.0 with a large payload vocabulary:
`initialize`, session creation and loading, prompt turns, and a `session/update`
stream carrying plan entries, message chunks, tool-call reports with content
blocks and locations, and usage updates, plus the client-served `fs/*` and
`terminal/*` methods. It is specified as JSON Schema artifacts under `schema/v1`
in the official repository, and those artifacts are published to crates.io as
**`agent-client-protocol-schema`** (currently 1.6.0, Apache-2.0, MSRV 1.88).

Two things about that crate decide this ADR. First, it is types only: its
dependencies are `serde`, `serde_json`, `serde_with`, `schemars`, `derive_more`,
`strum`, `anyhow`, and optionally `tracing` and `diffy`. There is no async
anywhere in it, which matters because ADR-0003 forbids `futures` entering the
workspace. Second, it is *not* the SDK. The sibling crate
`agent-client-protocol` (2.0.0) layers traits and a connection model on top and
depends on `futures` and `futures-concurrency`; `agent-client-protocol-tokio`
goes further. Those are the crates that would violate ADR-0003, and they are not
the ones under consideration.

Its feature flags are the other relevant fact: every draft or in-flight protocol
feature is behind an `unstable_*` gate — `unstable_protocol_v2`,
`unstable_elicitation`, `unstable_session_fork`, `unstable_mcp_over_acp`, and
others — and none is on by default.

## Decision

**`harkness-acp` uses `agent-client-protocol-schema` for ACP wire types.** The
pin lives in the workspace manifest:

```toml
agent-client-protocol-schema = { version = "1.6", default-features = false }
```

Three constraints hold it in place.

**Pinned to schema/v1.** The crate's major version tracks the schema major
version, so the `1.x` requirement *is* the schema/v1 pin. Moving to a `2.x`
release is adopting a different protocol version and requires a superseding ADR,
not a `cargo update`.

**No `unstable_*` feature may be enabled.** `unstable_protocol_v2` above all:
ADR-0014 fixes Harkness at protocol version 1, and a feature flag is not the
mechanism by which that changes. `default-features = false` is written
explicitly at the pin so that a future default-feature addition upstream cannot
enable anything here silently.

**Only the SDK's schema crate, never the SDK.** `agent-client-protocol` and
`agent-client-protocol-tokio` are forbidden dependencies for the same reason
ADR-0003 forbids `tokio`: they bring an async model the workspace does not have.
Harkness supplies its own JSON-RPC plumbing (ADR-0012,
[#147](https://github.com/fullstacktaiye/harkness/issues/147)) and uses the
upstream crate for exactly one thing — agreeing with the specification about what
the bytes mean.

The types stay **private to `harkness-acp`** under ADR-0009. That is what makes
this reversible: the adoption is a decision about one crate's internals, not
about Harkness's vocabulary.

## Outcome

Shipped in [#149](https://github.com/fullstacktaiye/harkness/issues/149) at
`agent-client-protocol-schema` 1.6.0, with no `unstable_*` feature enabled. The
types are reachable from exactly one module, `harkness-acp/src/wire.rs`, and a
manifest test in the crate fails the build on an `unstable_` string, on the async
SDK — whose name is a prefix of the permitted crate's, so the check counts
occurrences rather than looking for absence — and on the six crates ADR-0009 puts
above or beside this one. Upstream also supplies the two JSON-RPC error codes the
adapter branches on and the `initialize`/`authenticate` method names, each
asserted against the crate rather than typed locally.

One consequence was not anticipated in the list below and is worth recording,
because it landed two crates away from anything ACP-shaped. The schema crate
requires `serde_json/preserve_order`, and Cargo unifies features across every
workspace member: adding this dependency turned `serde_json::Map` from a sorted
`BTreeMap` into an insertion-ordered `IndexMap` for `harkness-runtime`,
`harkness-cli`, and everything else. Three places had frozen the bytes of an
untyped `Value` and were inheriting their key order from that map type — a
delivered tool result a recorded hash is taken over, a built-in agent scenario
mirrored byte-for-byte by a fixture, and the CLI's published `--json` envelope.
All three now sort explicitly through `harkness_runtime::canonical_json`, so the
bytes are a property of the value rather than of a transitive feature, and the
released output is unchanged. The same swap also made `serde_json::Value` large
enough to trip `clippy::result_large_err`. Neither cost is an argument against
the decision; both are the shape "an externally governed dependency" takes in
practice, and the containment is that a workspace-wide property is now written
down rather than inherited.

## Consequences

- Harkness deserializes what the specification says, not what a reading of the
  specification said on the day someone typed it. Errata, added optional fields,
  and clarified enum spellings arrive as a version bump instead of as a bug
  report from a user whose agent does not work.
- The dependency-light convention takes a real dent, and the lock file gains
  `derive_more`, `strum`, `serde_with`, and `anyhow` transitively. `schemars` and
  `serde_json` are already workspace dependencies, so the marginal tree is
  smaller than the crate's dependency list suggests, but it is not nothing.
- `anyhow` enters the tree through a dependency while remaining forbidden in
  Harkness's own code, which uses typed `thiserror` errors. That asymmetry is
  deliberate and worth stating so nobody reads the lock file as precedent.
- An upstream MSRV bump becomes a Harkness MSRV bump. 1.88 is comfortably below
  what edition 2024 already requires, and the risk is real but bounded and
  visible.
- Upstream governs the type names, so the conversion layer at the adapter
  boundary is written against names Harkness does not choose. ADR-0009 already
  requires that layer, so the cost is a naming inconvenience inside one file
  rather than a structural one.
- The `unstable_*` prohibition means Harkness cannot experiment with a draft
  feature by flipping a flag. Trying one is a code change plus an ADR, which is
  the intended friction.
- If the upstream types turn out to disagree with real agents during
  [#149](https://github.com/fullstacktaiye/harkness/issues/149), the fallback is
  the rejected alternative below, and switching to it touches only
  `harkness-acp`'s internals — no domain record, no persisted format, no caller.

## Alternatives considered

**Hand-rolled `serde` types for the subset of ACP that v0.5 uses.** No new
dependency, complete control of names and shapes, and only the ~15 methods the
milestone actually exercises need modelling. Rejected as the primary choice for
one reason that outweighs the rest: silent divergence. A hand-rolled type that
is subtly wrong — an optional field modelled as required, a camel-case spelling
missed, an enum variant absent — fails at runtime against a conformant agent, in
the field, and it fails as "that agent is broken" rather than as a compile error.
The official artifacts are the specification's own definition of correct, and
tracking errata by hand is unpaid work with a deadline set by someone else. This
remains the documented fallback if the pin proves unusable.

**The full `agent-client-protocol` SDK (2.0.0)**, taking its connection model as
well as its types. It would delete
[#147](https://github.com/fullstacktaiye/harkness/issues/147) outright. Rejected:
it depends on `futures` and `futures-concurrency`, so adopting it contradicts
ADR-0003 and drags an async model into a synchronous workspace through a side
door. Harkness also wants its transport to be its own — process-group
termination, env allowlisting, `Cancellation` polling, stderr to artifacts,
bounded message sizes — and those are properties of Harkness's process discipline
rather than of ACP.

**Generate types from the `schema/v1` JSON Schema artifacts at build time** with
`typify` or similar. No runtime dependency on someone else's Rust, and the
artifacts stay the source of truth. Rejected: it adds a build-time code generator
and a vendored schema copy to keep in sync, and it produces types nobody reviewed
for a protocol nobody on the project wrote. It is the same dependency, taken
with extra steps and less scrutiny.

**Vendor a copy of the schema crate's source into the repository.** Immune to
yanks and upstream churn. Rejected: it converts a version bump into a merge, and
the whole value of the decision is that upstream errata arrive cheaply.

**Wait for ACP to stabilize further before depending on any of it.** Rejected:
protocol version 1 *is* the stable version, and it is what agents in the field
speak today. Waiting means shipping nothing while the hand-rolled alternative
accrues the divergence this ADR exists to avoid.
