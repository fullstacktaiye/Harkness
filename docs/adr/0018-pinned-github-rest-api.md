# ADR-0018: Pin `X-GitHub-Api-Version: 2026-03-10`, authenticate with a fine-grained PAT through `CredentialSource`

- **Status**: Accepted
- **Date**: 2026-08-13
- **Deciders**: Taiye Babatope
- **Implemented by**: [#164](https://github.com/fullstacktaiye/harkness/issues/164), [#165](https://github.com/fullstacktaiye/harkness/issues/165)
- **Builds on**: [#124](https://github.com/fullstacktaiye/harkness/issues/124) (`CredentialSource`), ADR-0011, ADR-0009, ADR-0016

## Context

The GitHub REST API is versioned by date, selected per request with an
`X-GitHub-Api-Version` header. A request that omits the header does not get "the
latest" — it gets **2022-11-28**, the default for unversioned requests, which has
an end-of-life scheduled for **March 10, 2028**. An unpinned client is therefore
not merely unspecified; it is silently pinned to a version with a known
expiration, and it will keep working until it abruptly does not.

The current version is **2026-03-10**. The v0.5 surface is small and known:
repository identity, issue listing and search, draft pull-request creation,
duplicate detection by head branch, and branch reads
([#164](https://github.com/fullstacktaiye/harkness/issues/164)).

Authentication is the other half. GitHub offers classic PATs, fine-grained PATs,
GitHub App installation tokens, and device-flow OAuth. Fine-grained tokens are
the only option that lets a user grant repository-scoped, permission-scoped
access to a specific set of repositories — the difference between "Harkness may
open pull requests on this one repository" and "Harkness may act as me
everywhere".

The workspace already has a credential discipline planned:
`CredentialSource` ([#124](https://github.com/fullstacktaiye/harkness/issues/124))
holds a *reference* — an environment variable name or a file path — and never a
value, and the value is never written to `projects.json`, `runtime.db`, events,
artifacts, logs, or prompts. `harkness-git`'s runner already disables terminal
prompts so a credential request can never block on a hidden TTY.

## Decision

**Every GitHub request pins the API version**, alongside the standard accept
header:

```text
X-GitHub-Api-Version: 2026-03-10
Accept: application/vnd.github+json
```

No request may be issued without the version header. This is a property of the
one place requests are constructed in `harkness-forge`, not a rule applied at
each call site, on the same reasoning that makes
`crates/harkness-git/src/runner.rs` a choke point rather than a convenience: an
option meaning "this request is pinned" is only true because nothing outside can
widen the invocation carrying it.

**A version mismatch is a typed error.** A `410` or an explicit version error maps
to `api_version_mismatch`, which tells the user that Harkness's pinned version is
no longer served — an actionable statement — rather than surfacing a decoding
failure.

**Moving the pin requires a superseding ADR.** Not a constant edit. The pinned
date is a compatibility claim tested against a live API, and changing it changes
what every response is expected to contain.

**Authentication is a fine-grained personal access token, resolved through
`CredentialSource`.** Concretely:

- Configuration stores a **reference** — an environment variable name or a file
  path — never a token value. Nothing persists the secret: not the catalog, not
  `runtime.db`, not an event payload, not an artifact, not a log line, not a
  prompt, not a compiled recipe plan (ADR-0015).
- The `Authorization` header is **constructed per request and dropped with it**.
  There is no long-lived client object holding a token in a field.
- Redirects are disabled (ADR-0011), so the header cannot be replayed to a host
  the configuration did not name.
- Token permissions are introspected and surfaced as account status, so a user
  learns that a token lacks pull-request write *before* a run reaches the step
  that needs it.
- Revocation is implemented as removing the reference and clearing account
  status. Harkness cannot revoke a token it never held.

**A forge account and a forge host are trust subjects** under ADR-0016. A remote
repointed to a different host does not inherit a grant made against the original.

**Multiple accounts and hosts are modelled; only `github.com` is exercised in
v0.5.** The enterprise base-URL seam exists in the types and is not implemented,
so adding GitHub Enterprise later is configuration rather than surgery.

**Two authentication paths are deferred seams, documented and not built.** Device
-flow OAuth is the better long-term user experience — no token to paste, scoped
consent, refreshable — and it needs a client registration and a callback story
that v0.5 does not have. Reusing an existing `gh` CLI login would be convenient
and is refused on principle rather than effort: it takes credential handling out
of `CredentialSource` and puts Harkness's access at the mercy of another tool's
auth state. Both are recorded here so the next person to want them finds a
decision rather than an omission.

## Consequences

- Harkness's responses have a known shape, and a breaking API change surfaces as
  a typed mismatch on a date GitHub publishes in advance rather than as a
  deserialization bug in the field.
- The pin will go stale. Someone has to notice, verify, and move it deliberately,
  and doing so is an ADR-sized event. That friction is the point — a pin nobody
  has to think about is a pin nobody notices breaking.
- A user has to create a fine-grained PAT and put it somewhere Harkness can
  reference. That is more setup than clicking "sign in with GitHub", and it is
  the accepted cost of not building an OAuth flow in v0.5.
- Users get least-privilege access as the default posture, scoped to the
  repositories they choose, and a token they can revoke on GitHub without
  touching Harkness at all.
- Harkness cannot refresh, rotate, or repair a credential, because it does not
  hold one. An expired token produces a clear authentication failure and a
  pointer at the configured reference.
- Tolerant deserialization is required, not optional: unknown response fields are
  ignored and serde defaults fill absent optional ones, so an additive GitHub
  change within the pinned version does not break a read. A *load-bearing* field
  that is missing is a typed `partial_response` rather than a silent default —
  the distinction between "GitHub added something" and "GitHub did not send what
  was needed".
- Pinning does not remove the need to handle rate limits, secondary limits, and
  pagination; those are version-independent and are ADR-0011's cancellable-backoff
  territory.
- Only `github.com` is tested. The enterprise seam is untested code shape, and
  the first enterprise user will find something. Modelling it now is cheaper than
  retrofitting a base URL through a service surface later, and the honest claim is
  "designed for, not verified against".

## Alternatives considered

**Send no version header** and take whatever GitHub serves. Less code and no pin
to maintain. Rejected: it is not "latest", it is 2022-11-28 with an EOL in March
2028, so it is a pin with none of the benefits — chosen by default, undocumented,
and expiring.

**Pin to 2022-11-28 deliberately**, the most widely deployed version. Rejected:
it starts a new milestone on a version with a published end of life, and it
guarantees a migration inside the support window of the release being built.

**Follow the latest version automatically**, reading GitHub's version discovery
and using whatever is current. Superficially the most maintenance-free option.
Rejected: it makes the response contract change without a code change, which is
the exact failure the header exists to prevent. It also makes a bug reproducible
only on the day it happened.

**Classic PATs.** Simpler for users who already have one, and broader endpoint
coverage. Rejected: the scoping is account-wide, so the smallest grant that lets
Harkness open a pull request on one repository also lets it act across every
repository the user can reach. Fine-grained tokens exist for this.

**GitHub App installation tokens.** The right answer for a hosted service:
short-lived, per-installation, revocable centrally. Rejected for v0.5: it
requires registering and operating a GitHub App, and a desktop application acting
as the user does not fit the installation model without a server component
Harkness does not have.

**Device-flow OAuth in v0.5.** The best user experience of the options and the
likely eventual destination. Rejected on scope: client registration, token
storage, and refresh handling are a milestone's worth of work orthogonal to the
forge features v0.5 is shipping. Recorded above as a deferred seam.

**Store the token in the OS keyring** rather than behind a `CredentialSource`
reference. Genuinely better for the "where do I put this" problem. Rejected as
out of scope here: it is a change to
[#124](https://github.com/fullstacktaiye/harkness/issues/124)'s discipline
workspace-wide, not a GitHub decision, and a keyring entry is a legitimate future
`CredentialSource` variant rather than a replacement for the model.
