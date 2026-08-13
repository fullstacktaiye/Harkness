# ADR-0012: stdio-only protocol transports behind a transport seam

- **Status**: Accepted
- **Date**: 2026-08-13
- **Deciders**: Taiye Babatope
- **Implemented by**: [#147](https://github.com/fullstacktaiye/harkness/issues/147), [#149](https://github.com/fullstacktaiye/harkness/issues/149), [#157](https://github.com/fullstacktaiye/harkness/issues/157)
- **Builds on**: ADR-0009 (adapter layering), ADR-0003 (no async runtime), [#90](https://github.com/fullstacktaiye/harkness/issues/90) (command-execution safety)

## Context

Both protocol adapters talk to a program Harkness launches. An ACP agent is a
subprocess speaking JSON-RPC 2.0 over stdin and stdout; an MCP stdio server is a
subprocess speaking newline-delimited JSON-RPC, one message per line, with stderr
reserved for free-form logging the client must not interpret as errors. The two
specifications describe, in different words, the same transport shape — including
the same teardown sequence: close stdin, wait, `SIGTERM`, wait, `SIGKILL`.

Both specifications also describe remote transports, and neither is somewhere
Harkness can go in v0.5. ACP's remote transports are work in progress upstream.
MCP's Streamable HTTP is stable but arrives with an OAuth story — and the
specification has deprecated HTTP+SSE and OAuth Dynamic Client Registration,
which is exactly the churn a milestone should not build on.

The workspace has one piece of subprocess discipline already:
`crates/harkness-git/src/runner.rs`, which is argv-only, scrubs redirecting
environment variables, pins configuration as arguments, runs children in their
own process group so cancellation kills the whole tree, drains both streams
concurrently, and polls `Cancellation` every 20 ms. That runner is Git-shaped: it
runs a command to completion and collects its output. A JSON-RPC conversation is
long-lived and bidirectional, with request/response correlation, peer-initiated
messages, and streaming notifications. The discipline generalizes; the runner
does not.

[#147](https://github.com/fullstacktaiye/harkness/issues/147) weighed three homes
for that generalized engine — inside `harkness-git`, duplicated per adapter, or a
shared home below both — chose the third, and explicitly deferred its final form
and name to this ADR.

## Decision

**v0.5 supports stdio transports only.** ACP agents and MCP servers are local
subprocesses. No HTTP, WebSocket, or Streamable HTTP transport ships in v0.5, and
no adapter may open a socket to a protocol peer.

**Adapters never touch a child process.** An adapter speaks to a transport. It
does not call `Command::spawn`, does not hold a `ChildStdin`, does not read a
file descriptor, and does not send a signal. The seam is a trait:

```rust
/// Decided seam, implemented by #147: adapters speak to a transport,
/// never to a child process or socket directly.
pub trait JsonRpcTransport {
    fn send(&self, message: OutboundMessage) -> Result<(), TransportError>;
    fn recv_deadline(&self, deadline: Instant) -> Result<InboundMessage, TransportError>;
    fn shutdown(self, grace: Duration) -> ShutdownOutcome;
}
```

The sketch fixes the shape, not the signatures: blocking calls with explicit
deadlines, no futures, and teardown that reports an outcome rather than returning
`()` and hoping.
[#147](https://github.com/fullstacktaiye/harkness/issues/147) finalizes the exact
types, including whatever object-safety accommodation `shutdown` needs.

**The shared engine lives in a new crate, `harkness-transport`, created by
[#147](https://github.com/fullstacktaiye/harkness/issues/147).** It sits beside
`harkness-git` at the bottom of the graph, below both protocol adapters and above
nothing. It may depend on `harkness-git` for `Cancellation`, on the precedent
ADR-0001 established. It must not depend on `harkness-acp`, `harkness-mcp`, or
anything above them, and ADR-0009's no-sideways-edges rule is what makes a
separate crate the only available answer: the engine cannot live in one adapter
and be used by the other.

The engine **carries no protocol semantics**. Framing, spawn hermeticity,
correlation, bounds, and lifecycle are its whole job. `initialize`,
`server/discover`, version negotiation, sessions, and tool calls are the
adapters', and the engine cannot tell one JSON-RPC method from another.

**The transport owns the hermeticity contract**, generalizing the Git runner
without modifying it: argv-only with no shell, an explicit environment allowlist
rather than inherit-and-scrub (nothing is inherited, so no credential can leak
into an agent's environment by default), a pinned working directory,
`process_group(0)` so termination reaches the whole tree, newline-delimited
framing that refuses a message whose encoding would contain an embedded newline,
a configurable maximum message size whose breach quarantines the connection, and
stderr captured to artifacts and never read as an error signal.

**A remote transport is a new implementation of the trait and nothing else.**
That is the test this decision has to pass, and it is why the seam is drawn at
"messages in, messages out" rather than at "here is a `Child`": when ACP's remote
transports stabilize or Streamable HTTP becomes worth supporting, no adapter
protocol logic changes. It is also why the trait carries a deadline on `recv`
rather than a timeout on the process — a socket has no `SIGKILL`.

## Consequences

- One implementation of process hermeticity for protocol peers, tested once,
  fixed once. The alternative was three copies of the Git runner's discipline —
  Git's, ACP's, and MCP's — diverging at the pace they were each debugged.
- The workspace gains a twelfth crate whose entire public surface is a trait and
  one implementation of it. That is a lot of manifest for one seam, and ADR-0009's
  no-sideways-edges rule is what makes it non-optional.
- Adapters become testable without processes. A test transport that replays a
  scripted message sequence exercises negotiation, session lifecycle, and error
  taxonomy with no `fork`, which is what makes the conformance suites
  ([#156](https://github.com/fullstacktaiye/harkness/issues/156),
  [#162](https://github.com/fullstacktaiye/harkness/issues/162)) deterministic.
- Users cannot connect to a hosted agent or a remote MCP server in v0.5. This is
  a real capability gap and it is stated as one in the user documentation
  ([#184](https://github.com/fullstacktaiye/harkness/issues/184)) rather than
  left to be discovered.
- Everything runs as a local child process with the user's own privileges. The
  environment allowlist and the worktree scoping are what constrain it; there is
  no OS-level sandbox in v0.5, and ADR-0017 requires that limitation be stated
  honestly rather than implied away.
- Blocking reads with deadlines mean cancellation latency is bounded by the
  deadline slice, not instantaneous. The workspace-wide 250 ms visibility target
  is what
  [#147](https://github.com/fullstacktaiye/harkness/issues/147) sizes those slices
  against.
- A future remote transport will want connection pooling, reconnection, and
  authentication — concepts this trait has nowhere to put. When that lands, the
  trait probably grows or gains a sibling. That is a smaller change than the
  alternative, and it is the change this seam is designed to localize.

## Alternatives considered

**Extend `harkness-git`'s runner to serve protocol connections.** The discipline
is already there and already tested. Rejected: it couples protocol transport to
the Git crate, widens a deliberately crate-private surface into a public one, and
makes `harkness-git` — which exists to know only about filesystem paths and Git —
a stakeholder in JSON-RPC framing.

**Duplicate the transport inside each adapter.** No new crate, and each adapter
tunes its own I/O. Rejected outright: it forks hermeticity into copies that get
fixed one at a time. The specific failure is easy to picture — the environment
allowlist gets a fix in the MCP copy after a credential leak, and the ACP copy
keeps the bug for a milestone.

**No trait: adapters use the concrete stdio engine directly**, and introduce the
abstraction when a second transport actually exists. The usual and often correct
YAGNI position. Rejected here on cost asymmetry: the trait is small and costs
almost nothing now, while retrofitting it later means editing adapter protocol
logic that by then handles negotiation, sessions, permissions, and tool calls —
exactly the code this ADR promises a remote transport will not touch. The seam
also pays for itself immediately in tests.

**Support MCP Streamable HTTP in v0.5** alongside stdio, since the specification
has it and hosted MCP servers exist. Rejected: it drags in OAuth, and the
specification has just deprecated Dynamic Client Registration — building an
authorization flow against a surface mid-deprecation is how a milestone acquires
a rewrite. The trait is the commitment that this is a scheduling decision rather
than an architectural one.

**Put the shared engine in `harkness-core`.** Both adapters may already depend on
it. Rejected: `harkness-core` owns the project catalog and the data-directory
layout, and a JSON-RPC engine there has no relationship to either. A crate whose
contents are "things more than one other crate needed" stops being a boundary.
