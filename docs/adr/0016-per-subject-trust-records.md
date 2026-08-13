# ADR-0016: Trust is a per-subject record bound to an identity, never a boolean

- **Status**: Accepted
- **Date**: 2026-08-13
- **Deciders**: Taiye Babatope
- **Implemented by**: [#146](https://github.com/fullstacktaiye/harkness/issues/146), [#148](https://github.com/fullstacktaiye/harkness/issues/148), [#150](https://github.com/fullstacktaiye/harkness/issues/150), [#158](https://github.com/fullstacktaiye/harkness/issues/158), [#171](https://github.com/fullstacktaiye/harkness/issues/171)
- **Builds on**: [#90](https://github.com/fullstacktaiye/harkness/issues/90) (workspace trust), [#91](https://github.com/fullstacktaiye/harkness/issues/91) (policy), [#92](https://github.com/fullstacktaiye/harkness/issues/92) (approvals), ADR-0006, ADR-0009

## Context

v0.5 introduces six new kinds of thing a user can decide to trust, all of them
controlled by someone other than Harkness: an ACP agent's executable, an MCP
server, each tool schema that server publishes, a workflow recipe, a forge
account, and a forge repository. A forge *host* is deliberately not a seventh:
it is part of the identity basis of the account and repository that reach it, so
a remote repointed at a different host invalidates those grants rather than
needing a subject of its own. Workspace trust
([#90](https://github.com/fullstacktaiye/harkness/issues/90), planned) already
exists but answers a different question — whether the user accepts running this
workspace's code — and it is keyed to a path.

Every one of the new subjects can change out from under a grant, and the ways
they change are not exotic:

- The binary at `~/.local/bin/some-agent` is replaced. Same path, different
  program.
- An MCP server updates and a tool's input schema gains a parameter, or widens
  one.
- A repository recipe is edited after the user trusted it.
- A Git remote is repointed at a different host.
- A server that spoke one protocol revision now speaks another (ADR-0013), or an
  agent selects a different protocol version (ADR-0014).

A boolean cannot represent any of that. `trusted: true` keyed to a path or a name
survives every one of these changes, which means the grant outlives the thing it
was made about. The user answered a question about a specific program and is
being held to that answer about a different one.

The repository already has the shape of the answer in three places. Approvals
bind to an exact request rather than to an operation kind
([#92](https://github.com/fullstacktaiye/harkness/issues/92)). Workspace identity
is a composite digest rather than a mutable pointer (ADR-0008). `RepositoryLock`
derives a UUID-v5 from the canonical Git common directory rather than from a path
string. In each case identity is derived from content, and a decision is bound to
the identity rather than to the name.

## Decision

**Trust is a `TrustRecord` per subject, and every record names what it is a grant
about.** A record carries:

- the **subject kind** — `AgentExecutable`, `McpServer`, `McpToolSchema`,
  `Recipe`, `ForgeAccount`, `ForgeRepository`, or `Workspace`;
- the **identity basis**: the exact hashes and versions that were trusted —
  executable path plus SHA-256 for local subprocess subjects, endpoint identity
  for remote ones, schema fingerprints for MCP tools, content hash for recipes,
  canonical remote identity for forge repositories, plus protocol version where
  one applies;
- the **scope** — global, or one workspace;
- `granted_at`, RFC 3339 UTC;
- and a **state**: `Untrusted → Trusted → (Revoked | Invalidated { reason })`.

**Trust is checked against a currently observed identity, not looked up by name.**
The evaluation is a pure function of a record plus an observation: it either
remains valid, or it names the invalidation reason. That function is what makes
the whole model testable without any of the subjects being present.

**Invalidation reasons are a typed vocabulary, not free text**: executable hash
change, incompatible version change, endpoint host change, tool schema
fingerprint change, recipe content hash change, capability expansion, workspace
path change, repository remote change. A user who is asked to re-grant trust is
told which of these happened.

**Subjects are separate even when they arrive together.** An MCP server and each
tool schema it publishes are distinct subjects, because a server that changes a
tool's schema after being trusted has changed something the user did not agree
to. Trusting the server is not trusting every future shape of everything it
serves.

**Trust lives in `harkness-runtime`, in `integration/`, not in the adapters.**
Trust has to compose with workspace trust
([#90](https://github.com/fullstacktaiye/harkness/issues/90)) and the policy
evaluator ([#91](https://github.com/fullstacktaiye/harkness/issues/91)), neither
of which an adapter can see under ADR-0009. Adapters report what they observed —
a path, a hash, a version, a fingerprint — as plain data, and the runtime builds
the identity record. This is the concrete case ADR-0009 means by "integration glue
lives in the runtime".

**Trust is a precondition, never an authorization.** A trusted agent still passes
policy on every tool call, and still requires approval for anything the policy
lattice says needs one. Trust answers "may Harkness talk to this thing at all";
policy and approvals answer "may this specific action happen". An external
permission system — an agent's own allowlist, a server's own consent prompt —
**supplements** Harkness policy and never replaces it, on the same principle
ADR-0006 applies to repository content: something outside Harkness's control does
not get to widen what Harkness will do.

This ADR fixes the shape. [#146](https://github.com/fullstacktaiye/harkness/issues/146)
defines the types, wire formats, and frozen fixtures;
[#148](https://github.com/fullstacktaiye/harkness/issues/148) enforces them.

## Consequences

- A swapped executable, a mutated tool schema, an edited recipe, or a repointed
  remote invalidates the grant and asks the user again, with the reason stated.
  This is the behavior the whole model exists for.
- Users get re-prompted more than they would like. An agent that auto-updates
  invalidates its grant on every release, and there is no way to distinguish "the
  vendor shipped a patch" from "someone replaced the binary" by hashing. This is
  the accepted cost, and the mitigation is a good re-grant experience
  ([#176](https://github.com/fullstacktaiye/harkness/issues/176)) rather than a
  weaker check.
- Every subject needs an identity that can actually be computed. Hashing an agent
  binary at registration is cheap; fingerprinting every tool schema on every
  discovery is not free, and
  [#159](https://github.com/fullstacktaiye/harkness/issues/159) caches it. Where
  an identity is genuinely unavailable, the record says so rather than
  substituting a name.
- One trust model across every subject kind means one persistence path, one
  invalidation function, one audit view, and one GUI
  ([#176](https://github.com/fullstacktaiye/harkness/issues/176)). Five ad-hoc
  flags would have made that hub impossible to build, which is the failure this
  prevents most concretely.
- Trust records are a new persisted format with a schema version and frozen
  fixtures, and adding a subject kind later is an additive schema change with all
  the discipline `AGENTS.md` requires of one.
- Hashing identifies, it does not vet. A trusted-and-unchanged agent binary can
  still be malicious; the model guarantees only that it is the *same* binary the
  user decided about. Containment for what it does is policy, approvals, worktree
  scoping, and the honest accounting of ADR-0017 — and v0.5 has no OS-level
  sandbox to add to that list.
- Recording the protocol version in identity couples ADR-0013 and ADR-0014 to
  this model: an era or version change is an identity change. That is deliberate,
  and it means a protocol upgrade is a user-visible event.

## Alternatives considered

**One boolean per subject** — `trusted: true`, keyed by name or path. Simple,
obvious, and what most tools do. Rejected: it is precisely the vulnerability. A
grant keyed to a path is inherited by whatever occupies that path next, and
nothing about a boolean can notice.

**Trust the workspace, and let everything configured in it inherit that trust.**
It reuses [#90](https://github.com/fullstacktaiye/harkness/issues/90) and asks the
user one question instead of five. Rejected on the distinction ADR-0006 already
drew: "the user accepts running this workspace's code" is a different claim from
"every external program this workspace names may act on the user's behalf". A
repository can add an MCP server to its configuration; workspace trust must not
be what authorizes it.

**Trust with an expiry — grant for a session, or for thirty days.** Bounds the
blast radius without hashing anything. Rejected: time is uncorrelated with the
risk. A binary swapped one minute after a grant is inside the window, and an
unchanged binary is re-prompted for at day thirty for no reason. Expiry is worth
having *in addition* to identity binding; it is not a substitute, and v0.5 does
not need it.

**Delegate to each ecosystem's own permission system** — the agent's allowlist,
the MCP server's consent flow. Less to build, and it matches what users may
already have configured. Rejected: those systems are controlled by the party
being trusted, they differ in semantics, and several are advisory. They
supplement Harkness policy; they cannot be it.

**A single trust record for a server covering all its tools.** Fewer records,
fewer prompts, and a user's mental model is usually "I trust this server".
Rejected: schema drift is the specific attack — a trusted server that widens a
tool's parameters after the grant. Separate subjects are what make that visible.
The prompt volume is a UI problem, and grouping tool grants under their server in
the trust hub solves it without weakening the check.

**Store trust in an adapter crate**, next to the code that observes the identity.
Rejected: it inverts ADR-0009's layering, and it puts trust somewhere the policy
evaluator cannot reach — which would make "trust is a precondition, policy still
applies" unenforceable in the type system.
