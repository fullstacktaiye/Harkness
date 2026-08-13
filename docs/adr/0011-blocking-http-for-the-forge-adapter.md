# ADR-0011: Blocking `ureq` on worker threads for the forge adapter

- **Status**: Accepted
- **Date**: 2026-08-13
- **Deciders**: Taiye Babatope
- **Implemented by**: [#164](https://github.com/fullstacktaiye/harkness/issues/164), [#166](https://github.com/fullstacktaiye/harkness/issues/166), [#167](https://github.com/fullstacktaiye/harkness/issues/167), [#168](https://github.com/fullstacktaiye/harkness/issues/168)
- **Builds on**: ADR-0003 (blocking HTTP with SSE on the calling worker thread), ADR-0009, ADR-0018

## Context

ADR-0003 decided the workspace's HTTP question for v0.4: blocking `ureq` on the
calling worker thread, no `tokio`, no `async fn`, cancellation through the same
`harkness_git::Cancellation` token everything else already polls. It scoped the
dependency deliberately — "the HTTP client is `ureq`, scoped to
`harkness-provider` alone. No other crate gains an HTTP dependency" — because at
the time there was exactly one caller and no reason to speculate about a second.

v0.5 produces the second caller. `harkness-forge` talks to the GitHub REST API:
repository reads, issue listing and search, pull-request creation and duplicate
detection, and branch reads
([#164](https://github.com/fullstacktaiye/harkness/issues/164)). That surface
looks nothing like a streaming chat completion. It is a pinned, small set of
request/response endpoints returning JSON in kilobytes, with `Link`-header
pagination, `x-ratelimit-*` headers, and 403/429 secondary limits carrying
`retry-after`.

So the question is not "which HTTP client" a second time. It is whether the
reasoning in ADR-0003 was about the streaming case specifically, or about the
workspace, and whether a second HTTP consumer changes the answer.

## Decision

**`harkness-forge` uses blocking `ureq` on the caller's worker thread.** ADR-0003
is extended, not re-decided: its scoping sentence is amended to permit exactly
one further crate, and the no-async rule is restated as binding for v0.5.

**No `tokio`, `async-std`, `smol`, or `futures` enters the workspace in v0.5, and
no `async fn` appears in any Harkness crate.** ADR-0010's prohibition on the ACP
SDK crates is a consequence of this same rule.

Concretely, for every forge request:

- The request executes on the worker thread the caller is already on. There is no
  internal thread, no channel, and no pool inside the adapter; concurrency is the
  scheduler's ([#93](https://github.com/fullstacktaiye/harkness/issues/93),
  planned), which makes the ceiling configured rather than emergent.
- Cancellation is `harkness_git::Cancellation`, polled at the established 20 ms
  cadence between the phases of a request and between pages of a paginated read.
  Read timeouts are set so a blocked `read()` wakes often enough that the
  workspace-wide 250 ms cancellation-visibility target holds.
- **Backoff never sleeps uncancellably.** A `rate_limited { reset }` or an
  `abuse_limited` with `retry-after` waits in bounded slices that poll the token,
  so a user cancelling a run does not wait out a GitHub rate-limit window.
- Connect, first-byte, and read deadlines are explicit and separate, as ADR-0003
  requires. Unlike the streaming case there *is* a total-request deadline here: a
  REST call that has not completed is not a long generation, it is a stuck call.
- Redirects are disabled, so a configured base URL cannot replay the
  `Authorization` header to another host. Combined with ADR-0018's rule that
  credentials are constructed per request from a `CredentialSource` and dropped
  with it, a token never travels somewhere the configuration did not name.
- Idempotent `GET`s may be retried under bounded, cancellable backoff. **Mutations
  are never blindly retried**; an unknown completion is resolved by reading the
  remote state back ([#168](https://github.com/fullstacktaiye/harkness/issues/168)),
  because a retried `POST /pulls` is a second pull request.

## Consequences

- One concurrency model and one cancellation token still describe the whole
  workspace, now including remote forge I/O. A cancelled run cancels its Git
  work, its tool work, its model request, its agent subprocess, and its in-flight
  GitHub call through the same `Arc<AtomicBool>`.
- A blocked thread per in-flight forge request. For a desktop application issuing
  a handful of REST calls this is free; for a hypothetical bulk import of ten
  thousand issues it is not, and that is a real limitation rather than an
  oversight. The bound is the scheduler's queue, and it is visible.
- No new dependency tree. `ureq` and its `rustls` transitive set enter the lock
  file with [#125](https://github.com/fullstacktaiye/harkness/issues/125)
  (planned, v0.4); the forge adapter is the second consumer of a cost already
  accepted, so its marginal dependency footprint is zero. If v0.4 ships without
  [#125](https://github.com/fullstacktaiye/harkness/issues/125), this ADR is what
  pays for the tree instead, and the accounting in ADR-0003 applies unchanged.
- Two crates now hold HTTP knowledge, and rate-limit handling, pagination, and
  retry semantics are forge-specific enough that they are not shared with the
  provider adapter. Someone will eventually propose a common HTTP wrapper crate;
  that proposal needs a third consumer and a superseding ADR, not a refactor.
- Tests need no runtime. The adapter's own tests run against the in-process
  loopback `TcpListener` fixture pattern ADR-0003 established, and the fake forge
  ([#169](https://github.com/fullstacktaiye/harkness/issues/169)) is a
  `std::net::TcpListener` speaking the pinned API surface.
- Paginating a large issue list holds a worker thread for the duration of every
  page. The page-size and total-page caps in
  [#164](https://github.com/fullstacktaiye/harkness/issues/164) exist because of
  this, not in addition to it.
- If v0.6 wants many concurrent remote agents or a genuinely parallel forge sync,
  this decision and ADR-0003 both need revisiting together, by a superseding ADR.
  The point of restating the rule here is that the second HTTP consumer did not
  quietly become the precedent for the third.

## Alternatives considered

**`reqwest` + `tokio`, confined to `harkness-forge`.** The GitHub ecosystem
assumes it, and `octocrab` would come nearly free. Rejected for the reasons
ADR-0003 already established and which a REST surface does not weaken: `block_on`
at the boundary makes the executor every caller's problem, and a nested
`block_on` inside a tool body invoked from a coordinator thread is a deadlock
waiting for the right stack. A second cancellation type to bridge at every
boundary is the other half of the cost.

**`octocrab` or another GitHub SDK.** Endpoint coverage, pagination, and
rate-limit handling for free. Rejected: it is async, so it carries the previous
alternative's problems, and it would also pull GitHub wire types into a typed
public surface that ADR-0009 requires to stay private and forge-neutral. The v0.5
endpoint list is roughly six endpoints — small enough that an SDK's coverage is
not the constraint.

**Shell out to the `gh` CLI**, mirroring the hermetic `git` invocation policy.
Genuinely tempting: it solves authentication, pagination, and API versioning at
once, and the repository already has a hardened subprocess runner. Rejected: it
makes a working `gh` installation a runtime requirement of the desktop
application, it hands credential handling to a tool whose auth state Harkness
does not control (defeating ADR-0018's `CredentialSource` discipline), and its
output format is a compatibility surface with no version pin comparable to
`X-GitHub-Api-Version`. Reusing an existing `gh` login is recorded in ADR-0018 as
a deferred *authentication* seam, which is a much narrower idea than routing all
API traffic through it.

**A shared `harkness-http` crate wrapping `ureq` for both consumers.** Rejected as
premature by the same reasoning ADR-0001 used to refuse a `harkness-cancel`
crate: two consumers whose only common code is "call `ureq`" do not justify a
crate, and the interesting parts — SSE assembly on one side, rate limits and
pagination on the other — are not shared at all.
