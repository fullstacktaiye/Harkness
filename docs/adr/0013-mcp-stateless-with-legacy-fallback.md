# ADR-0013: MCP 2026-07-28 stateless is primary; 2025-11-25 is a probe-selected fallback

- **Status**: Accepted
- **Date**: 2026-08-13
- **Deciders**: Taiye Babatope
- **Implemented by**: [#157](https://github.com/fullstacktaiye/harkness/issues/157), [#158](https://github.com/fullstacktaiye/harkness/issues/158), [#159](https://github.com/fullstacktaiye/harkness/issues/159)
- **Builds on**: ADR-0009, ADR-0012, ADR-0016

## Context

The Model Context Protocol identifies revisions by date. The current stable
revision, **2026-07-28**, is stateless: there is no `initialize` handshake, no
`notifications/initialized`, and no `Mcp-Session-Id`. Every request carries its
own context in `_meta` — `io.modelcontextprotocol/protocolVersion`,
`io.modelcontextprotocol/clientCapabilities`, and
`io.modelcontextprotocol/clientInfo` — and servers identify themselves through
`io.modelcontextprotocol/serverInfo` in result `_meta`. Servers **must** implement
`server/discover`, which advertises supported protocol versions, capabilities,
and identity. The specification reserves error codes `-32020`..`-32099` for
itself, including `UnsupportedProtocolVersion` (-32022).

The previous revision, **2025-11-25**, is handshake-era: `initialize` followed by
`notifications/initialized`, with version and capabilities agreed once per
connection. Servers speaking it are deployed, numerous, and will remain so for a
long time.

A client therefore has to answer one question before it can do anything: which
era is this server? Getting it wrong is not a graceful degradation. Sending
stateless `_meta` requests to a handshake-era server produces failures that look
like a broken server, and sending `initialize` to a stateless server produces
failures that look like a broken client.

The specification also deprecated a set of features that keep working for a
stated window: Roots, Sampling, Logging, the HTTP+SSE transport, and OAuth
Dynamic Client Registration.

## Decision

**2026-07-28 stateless is the primary protocol.** `harkness-mcp` constructs every
request in stateless form, injecting the `_meta` protocol version, client
capabilities, and client info, and reads `serverInfo` from result `_meta`.

**Era selection is the specification's `server/discover` probe, and nothing
else.** On connect, the client calls `server/discover` first and interprets the
outcome:

| Probe outcome | Era | Version selection |
| --- | --- | --- |
| A `DiscoverResult` | modern | choose a mutually supported version from the advertised list |
| A recognized modern error, such as `UnsupportedProtocolVersion` (-32022) | modern | choose from the advertised supported list; **never** fall back |
| Any other error, or a bounded timeout | legacy | `initialize` / `notifications/initialized` at 2025-11-25 |

**The fallback triggers on the absence of a modern answer, never on one specific
error code.** A handshake-era server may respond to an unknown method with
`-32601`, with a transport-level failure, with a malformed reply, or with
silence, and all four mean the same thing. Keying the fallback on a particular
code would make Harkness work against the servers that happen to be polite.

The converse is equally load-bearing: a *recognized modern error* proves a modern
server and **forbids** the fallback. A stateless server that rejects the client's
proposed version has answered the era question; retrying with `initialize` would
turn a clear "no mutually supported version" into a confusing handshake failure
and hide a real incompatibility.

**The negotiated era and version are recorded per connection**, surfaced to the
user, and carried into the server's identity for trust purposes — an era change
between sessions is a security-relevant identity change under ADR-0016, not a
detail.

**Deprecated features are never adopted.** Roots, Sampling, Logging, HTTP+SSE,
and OAuth DCR do not appear in `harkness-mcp`, in either era. A deprecation window
is time to leave, not time to arrive.

**Two spec behaviors are refused explicitly rather than half-implemented.** A
`resultType` of `input_required` (the multi-round tool-request flow) is refused
with a typed `mcp_input_required_unsupported` error and a recorded event; a
missing `resultType` from a legacy-era server is treated as `complete`. A
2026-07-28 server that writes a JSON-RPC *request* to stdout is a protocol
violation and quarantines the connection.

Per ADR-0009, all of this is invisible above `harkness-mcp`: the era is an
adapter-internal fact, and the runtime sees Harkness tool descriptors either way.

## Consequences

- Harkness works against both generations of MCP server with one code path
  choosing between them, and the choice is the specification's own recommended
  probe rather than a heuristic Harkness invented.
- Every connection pays one extra round trip before its first useful request. On
  a local subprocess that is microseconds; against a legacy server it is one
  round trip plus the probe timeout before the handshake. The timeout is bounded
  and configurable, and it is the price of not guessing.
- Two eras means two code paths through request construction and error mapping,
  and the conformance suite
  ([#162](https://github.com/fullstacktaiye/harkness/issues/162)) has to exercise
  both, plus the probe's four outcomes. That is roughly double the protocol test
  surface and it is not optional.
- Legacy-era support has a shelf life. When 2025-11-25 servers become rare, the
  fallback becomes dead weight, and removing it will need a superseding ADR
  because some user's server will still speak it.
- Refusing `input_required` means a server whose tools need mid-call input simply
  does not work with Harkness in v0.5. The refusal is typed and recorded, so the
  user learns why instead of watching a call hang. Supporting it is a future
  issue, not a bug report.
- Declining the deprecated features costs real capability — Sampling in
  particular is how some servers expect to reach a model. Harkness's answer is
  that a server does not get to borrow the user's model; if that changes, it
  changes through the provider layer with policy on it, not through MCP.
- Recording the era in server identity means a server that silently upgrades
  invalidates its trust grant and asks the user again. That will occasionally
  annoy someone whose server auto-updated, and it is the correct behavior: the
  thing they trusted now speaks a different protocol.

## Alternatives considered

**Handshake-era 2025-11-25 only.** Every deployed server speaks it today, and
it is one code path. Rejected: it targets a superseded revision at the start of a
milestone, so v0.5 would ship already needing the migration, and it forfeits
`server/discover` — the mechanism that makes version selection a protocol
feature instead of a guess.

**Stateless 2026-07-28 only, with no fallback.** Clean, current, half the test
surface. Rejected: it fails against most of the servers users actually have
installed, and it fails at connect time with an error the user cannot act on.
"Upgrade your server" is not a thing Harkness can ask on behalf of an ecosystem.

**Try stateless first and fall back when a request fails.** No probe round trip.
Rejected: the failure arrives *after* a real request, so the fallback happens
mid-operation with a partially executed call to reason about, and "a request
failed" is indistinguishable from a server that is merely broken. The probe makes
era selection a discrete step with a discrete answer.

**Configure the era per server, entered by the user.** No probe, no ambiguity,
and an escape hatch for a nonconforming server. Rejected as the primary
mechanism: it asks users to know a protocol revision date to add a server, and it
goes stale silently when a server upgrades. Worth revisiting as an *override* if
a real server needs one; not worth having as the default path.

**Key the fallback on `-32601` (method not found).** The most likely legacy
response, and precise. Rejected: it is one of several ways a legacy server can
reject an unknown method, and a timeout — a plausible response from a server that
never answers — is not an error code at all. Precision here buys nothing and
excludes servers.
