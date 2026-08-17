# Agent Client Protocol

Harkness speaks the Agent Client Protocol as a **client**: it launches an agent
someone else wrote as a child process and talks JSON-RPC 2.0 to it over that
process's standard input and output. Everything Harkness says to an agent and
everything it makes of the answers lives in `harkness-acp`, which sits strictly
below `harkness-runtime` and may not name it (ADR-0009).

This document covers what the handshake establishes. Sessions, prompt turns,
streaming updates, permission requests, and filesystem and terminal mediation are
later issues in the same crate and will be documented beside this.

## Protocol version

Harkness offers and accepts **protocol version 1**, and only version 1
(ADR-0014). `OFFERED_PROTOCOL_VERSION` is the version sent in `initialize` — the
latest Harkness supports — and `SUPPORTED_PROTOCOL_VERSIONS` is the set it will
proceed on. They are separate constants because negotiation is "is the agent's
answer one of ours", and that question does not change shape when the answer
grows.

The v2 draft is a negotiation boundary rather than a feature to reach for. An
agent that selects any version outside the supported set gets a clean close and
the caller gets `AcpError::UnsupportedProtocolVersion`, carrying the version the
agent selected and naming both sides:

```
the agent selected ACP protocol version 2, and Harkness speaks 1
```

No further request is sent on that connection. A version mismatch is permanent
until software changes, so retrying is a way to launch a program repeatedly for
no reason. Adopting ACP v2 requires a superseding ADR — not a cargo feature, not
a dependency bump.

The wire types come from the official `agent-client-protocol-schema` crate
pinned to a schema/v1 release (ADR-0010). Every `unstable_*` feature stays off,
`unstable_protocol_v2` above all, and a manifest test in the crate fails the
build if one appears.

## What Harkness advertises

`initialize` sends three client capabilities, and `harkness-acp` advertises
**exactly** what its caller passes:

| Capability | Wire field | What it promises |
| --- | --- | --- |
| `fs_read_text_file` | `clientCapabilities.fs.readTextFile` | Harkness serves `fs/read_text_file` |
| `fs_write_text_file` | `clientCapabilities.fs.writeTextFile` | Harkness serves `fs/write_text_file` |
| `terminal` | `clientCapabilities.terminal` | Harkness serves every `terminal/*` method |

The adapter never turns one on by itself. Each is a promise to mediate a request
an agent may then make, and mediation is #153's — one authority, one place to
read it. `AdvertisedClientCapabilities::default()` advertises nothing, which is
the safe advertisement: an agent told Harkness serves no client method will not
ask it to.

`clientInfo` carries a product name, an optional display title, and the Harkness
version. Nothing else — no username, no workspace path, no project identifier.
`initialize` happens before any trust decision has been made about the program on
the other end.

## What the agent advertises

An omitted capability **is** an unsupported capability. That is ACP's rule and
`AcpAgentCapabilities` holds it structurally: every capability is a `bool` that
is `false` unless the agent said otherwise, with no third state for "the agent
was silent". Making silence representable is how a client ends up calling
`session/load` against an agent that never claimed to implement it.

A capability whose value has the wrong type decodes the same way, because the
schema crate defaults every optional field on error. That is the right answer
rather than a leniency to work around: a capability object nobody can read is an
agent with fewer features, not an agent that failed to answer, and refusing the
whole handshake over one would take a working agent away from a user.

`protocolVersion` is the one field with no default. A response missing it, or
carrying something that is not an integer, is `AcpError::MalformedResponse` and
closes the connection — it is not an ACP response at all.

The snapshot returned to the caller covers `loadSession`, each
`promptCapabilities` field, each `mcpCapabilities` field, each
`sessionCapabilities` field, the agent's auth capabilities, its advertised
`authMethods`, and its optional `agentInfo`. #150 persists it: an agent that
starts advertising a different set has changed in a way its trust grant was not
given for.

## Authentication

`authenticate` is gated on the agent's own advertisement and the gate is checked
**before anything is written**. An agent that advertised no method wants no
authentication, and one that advertised a different method was not asked about
this one; either way, sending the request is a mistake Harkness would be making,
not a question for the peer. Both cases are
`AcpError::AuthMethodNotAdvertised`, which names what the agent did offer.

No credential material passes through this crate. ACP v1 has one method shape —
the agent handles authentication itself — and Harkness only names which of the
offered ways to use.

An agent that advertised a method and then answers `-32601` to `authenticate`
is not refusing a credential — it is not serving the call at all, and that is
`AcpError::MethodNotSupported`. Telling a caller its credentials were rejected
would send it to re-prompt a person over a conformance bug no answer of theirs
can fix.

Every other rejection is `AcpError::AuthenticationFailed`, deliberately distinct
from a transport failure: "your credentials were refused" and "the agent died
mid-call" are the same outcome to a caller that only checks for `Err`, and #150
has to tell them apart to choose between re-prompting a person and relaunching a
program.

## Error kinds

`AcpError::kind()` is a stable snake_case discriminant, following `GitError`.
The published namespace is `AcpError::kinds()`, which is this crate's table
followed by the transport's — a transport failure keeps the discriminant #147
gave it rather than being re-spelled, so a broken pipe during `initialize` is
`write_failed` at every layer that reports it. The two tables are held disjoint
by a test, because a caller publishing an exit code per kind publishes their
concatenation.

| Kind | Meaning | Connection survives |
| --- | --- | --- |
| `unsupported_protocol_version` | The agent selected a version Harkness does not speak | no |
| `malformed_response` | The answer is not the response this method defines | no |
| `protocol_violation` | The agent called a method before the handshake finished | no |
| `agent_rejected_request` | A JSON-RPC error, carried whole | yes |
| `method_not_supported` | `-32601` for a method ACP requires, or for `authenticate` after the agent advertised one | yes |
| `authentication_required` | `-32000` from a call that authentication would unblock | yes |
| `authentication_failed` | The agent rejected the attempt | yes |
| `auth_method_not_advertised` | Nothing was written; the agent never offered it | yes |
| `not_initialized` | A method was called before `initialize` | yes |
| `already_initialized` | A second handshake on one connection | yes |
| `connection_closed` | An earlier failure closed it | no |
| `unencodable_request` | A request could not be built for the wire | yes |

`AcpError::is_terminal()` answers the "connection survives" column, and
`AcpError::transport()` hands back the underlying `TransportError` when there is
one.

`authentication_required` is reachable from `initialize` only from an agent that
is out of spec: `-32000` is the code for a call authentication would unblock, and
nothing has been advertised to authenticate *with* until `initialize` answers. It
is reported accurately rather than translated, but there is no retry that helps —
the method set an agent will accept is in the response it declined to send. The
kind exists for the session methods #151 adds.

## Nothing arrives during the handshake

ACP gives an agent nothing to send before `initialize` returns: there is no
session to update, no file to read, and no terminal to create. A request or
notification arriving inside that window is `AcpError::ProtocolViolation` and
closes the connection.

The check is exact rather than a heuristic. The transport delivers one ordered
stream through one pump, so everything the agent wrote before its response was
routed before its response was — a peer queue that is empty when the response
arrives is proof the agent sent nothing.

The agent-chosen method name is repeated back in that diagnostic, clamped to 128
bytes, so a peer cannot choose how long a Harkness message is. An agent's own
`message` and `data` on a JSON-RPC error are *not* clamped: they are prose whose
whole value is being complete, they live in a field of their own rather than
inside a sentence Harkness wrote, and the transport's `max_message_bytes` already
bounds them. A caller making either durable owes it the run store's inline bound.

## Deadlines and cancellation

Every wait is bounded. `AcpTimeouts` carries three:

- `initialize` defaults to the transport's own startup deadline, because the two
  bound the same window from different sides — two different numbers would mean
  one of them never fires.
- `authenticate` is much longer, because an agent that authenticates by opening a
  browser is waiting for a person.
- `shutdown_grace` is how long each teardown rung waits before escalating.

The startup window closes when `initialize` succeeds and not before: #147 cannot
recognize a handshake, since `initialize` is a method name it has no opinion
about, so the adapter is what declares the peer has proven it speaks ACP.

Cancellation travels on the `harkness_git::Cancellation` the connection was built
with and is observed within the workspace's 250 ms visibility target.

## Launching an agent

`harkness-acp` never launches an executable. `AcpConnection::new` takes a
connection that already exists, because deciding which program may run is a trust
decision bound to an executable digest and that decision is #150's under
ADR-0016. A crate that could launch a program would be a second route around it.

## Frozen fixtures

`crates/harkness-acp/src/fixtures/` pins the `initialize` request Harkness sends
and the responses it decodes. The two request fixtures are regenerated from the
crate itself:

```sh
cargo test -p harkness-acp -- --ignored regenerate_the_frozen_v1_fixtures
```

The three response fixtures are hand-maintained, because none is a wire form this
build produces: one is an agent's full answer, one omits every field
serialization always writes, and one selects the version Harkness refuses.
