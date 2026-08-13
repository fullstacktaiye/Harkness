# ADR-0003: Blocking HTTP with SSE on the calling worker thread

- **Status**: Accepted
- **Extended by**: ADR-0011 — the decision below is unchanged, but its scoping
  sentence ("scoped to `harkness-provider` alone … No other crate gains an HTTP
  dependency") admits a second crate, `harkness-forge`, from v0.5 onward. Read
  ADR-0011 before concluding that a forge HTTP dependency is forbidden.
- **Date**: 2026-08-10
- **Deciders**: Taiye Babatope
- **Implemented by**: [#125](https://github.com/fullstacktaiye/harkness/issues/125), [#111](https://github.com/fullstacktaiye/harkness/issues/111), [#124](https://github.com/fullstacktaiye/harkness/issues/124)
- **Builds on**: [#89](https://github.com/fullstacktaiye/harkness/issues/89) (tool execution semantics), [#93](https://github.com/fullstacktaiye/harkness/issues/93) (scheduling), ADR-0001

## Context

The workspace has no HTTP client, no async runtime, and no futures anywhere.
Every long operation is a `std::thread` doing blocking work while polling a
cancellation token: `harkness-git` runs the system `git` binary in its own
process group and polls `Cancellation` — an `Arc<AtomicBool>` — every 20 ms
(`POLL_INTERVAL`, `crates/harkness-git/src/runner.rs:126`). Progress is an
`impl FnMut(String)` callback, generalized in the tool layer to a typed
`ProgressEvent`. The GUI returns results to the Qt thread with
`qt_thread().queue(...)`; the Qt main thread never blocks.

v0.4 introduces the workspace's first direct network I/O: a streaming
chat-completions request whose response is Server-Sent Events read over seconds
to minutes, cancellable at any point, with partial output surfacing live in the
UI.

An async runtime would be a genuine architectural change rather than a
dependency addition. `tokio` in one crate is not contained: every caller of that
crate needs a runtime handle or a `block_on`, so the executor question reaches
`harkness-runtime`'s tool bodies, the coordinator, and eventually
`HarknessBackend`. Two concurrency worlds also means two cancellation
mechanisms, and every boundary between them is a place a cancel gets dropped.

## Decision

v0.4 uses blocking HTTP. **No `tokio`, `async-std`, `smol`, or `futures` enters
the workspace in v0.4**, and no `async fn` appears in any crate.

- The HTTP client is **`ureq`** (blocking, `rustls` by default), scoped to
  `harkness-provider` alone
  ([#125](https://github.com/fullstacktaiye/harkness/issues/125)). No other
  crate gains an HTTP dependency.
- `ModelProvider::stream` executes entirely on the **caller's worker thread**.
  It writes the request, then reads the response body as a stream and parses SSE
  from that reader in place. There is no internal thread, no channel, and no
  queue: back-pressure is the caller's blocking sink.
- **Cancellation is `harkness_git::Cancellation`**, the same token Git verbs and
  tool execution contexts already carry, polled between reads and between parsed
  events at the established **20 ms cadence**. Read timeouts are set so a
  `read()` wakes at least every `min(read_timeout, 1s)`, which bounds how long a
  silent endpoint can delay a cancel. Observing cancellation drops the
  connection and returns `ProviderError` kind `cancelled`; no sink event is
  delivered after the poll that observed it.
- Timeouts are explicit and separate: a connect deadline, a
  first-byte deadline, and an inter-event read timeout, all from the provider
  profile ([#124](https://github.com/fullstacktaiye/harkness/issues/124)).
  There is no total-request deadline in the transport — a long generation is not
  a failure; the loop's wall-clock budget
  ([#126](https://github.com/fullstacktaiye/harkness/issues/126)) is what bounds
  it.
- Disconnect is detected by the reader, not by a timer: a stream that ends
  without a terminal event is `disconnected`, with the partial assembled turn
  attached for diagnostics
  ([#111](https://github.com/fullstacktaiye/harkness/issues/111)).

Redirects are disabled. A redirect is a `malformed_response`, so a configured
base URL cannot silently replay credentials to another host.

## Consequences

- **No async runtime exists anywhere in the workspace in v0.4.** One concurrency
  model workspace-wide, and one cancellation token: a cancelled run cancels its
  Git work, its tool work, and its in-flight model request through the same
  object. Nothing downstream has to answer the executor question.
- A blocked thread per in-flight model request. Concurrent runs are bounded by
  the scheduler ([#93](https://github.com/fullstacktaiye/harkness/issues/93)),
  not by an executor, so the cap is explicit and configured rather than emergent.
  This is a poor fit for hundreds of simultaneous requests and an excellent fit
  for the handful a desktop application actually issues.
- Cancellation latency is bounded by the read-timeout slice rather than
  instantaneous. The target is the workspace-wide one: visible within 250 ms.
- Tests need no runtime and no `#[tokio::test]`. The scripted provider
  ([#111](https://github.com/fullstacktaiye/harkness/issues/111)) replays
  deterministically, and the adapter's own tests run against an in-process
  loopback `std::net::TcpListener` fixture — no new test dependency.
- `ureq` brings `rustls` and its transitive tree into the lock file. That is the
  cost of TLS and is accepted; the alternative is either no hosted endpoints or a
  system-OpenSSL build dependency on every platform.
- If a future milestone needs genuine async — many concurrent remote agents, for
  example — this decision has to be revisited by a superseding ADR rather than
  by one crate quietly adding a runtime. Adding an async dependency is a
  decision, not an implementation detail.
- A pathological endpoint that dribbles one byte per read-timeout slice keeps a
  thread alive until a higher-level budget stops it. That is a documented
  limitation, not an oversight: the transport does not judge slow generation.

## Alternatives considered

**`tokio` + `reqwest` with `block_on` at the boundary.** The mainstream choice,
and the one with the best SSE ecosystem. Rejected: `block_on` at a boundary is
exactly how the executor requirement leaks — one crate's `Runtime::new` becomes
every caller's problem, and nested `block_on` inside a tool body called from a
coordinator thread is a deadlock waiting for the right stack. It also introduces
a second cancellation mechanism (`CancellationToken`) that must be bridged to
`Cancellation` at every boundary.

**Async only inside `harkness-provider`, with a private runtime.** Contained on
paper. Rejected: the containment is a promise, not a property, and the
provider's public API would still hand callers a value produced under a runtime
whose lifetime it manages. The complexity is real and the isolation is not.

**Shell out to `curl` for SSE**, mirroring the hermetic `git` invocation policy.
Rejected: that policy exists because Git *is* an external program with
process-level semantics worth controlling. HTTP is not, and adding a runtime
dependency on `curl` to avoid a Rust dependency on `ureq` trades a lock-file
entry for a deployment requirement.

**Hand-rolled HTTP over `std::net::TcpStream` plus `rustls`.** No client
dependency. Rejected: chunked transfer encoding, keep-alive, proxies, and TLS
verification are exactly the things worth not reimplementing on the path that
carries user credentials.
