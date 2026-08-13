# ADR-0014: ACP protocol version 1 only; v2 is a negotiation boundary

- **Status**: Accepted
- **Date**: 2026-08-13
- **Deciders**: Taiye Babatope
- **Implemented by**: [#149](https://github.com/fullstacktaiye/harkness/issues/149), [#150](https://github.com/fullstacktaiye/harkness/issues/150)
- **Builds on**: ADR-0010 (the pinned schema crate), ADR-0009, ADR-0012

## Context

The Agent Client Protocol carries an integer `protocolVersion`, exchanged during
`initialize`: the client offers the latest version it supports, and the agent
replies with the version it selected. Version **1** is the current stable
version, and it is what agents in the field speak.

A **v2 draft** was published on 2026-07-20. It is a draft, its methods may still
move, and the official schema crate reflects that status: v2 support sits behind
the `unstable_protocol_v2` cargo feature, off by default, and enabling it pulls
an extra dependency. Upstream is signalling, in the strongest way a Rust crate
can, that this is not yet something to build against.

The failure this decision exists to prevent is specific and has a shape. An
adapter that adopts draft features early works beautifully against the one agent
tracking the draft and fails against every v1-only agent — which is nearly all of
them. The mirror-image failure is an adapter that treats an unexpected selected
version as a parse error, so when an agent eventually selects 2, the user gets a
deserialization failure somewhere deep in a session instead of a sentence
explaining that the agent is too new.

## Decision

**Harkness speaks ACP protocol version 1.** The client offers 1 in `initialize`
and implements the v1 method set.

**No `unstable_*` feature of `agent-client-protocol-schema` may be enabled**, and
`unstable_protocol_v2` above all. This is a rule, not a mechanism, and the
distinction matters: cargo features are additive from members, so a crate
inheriting the workspace pin can still write `features = ["unstable_protocol_v2"]`
and the workspace entry cannot veto it. ADR-0010's `default-features = false`
guards a different hole — a future upstream *default* feature enabling something
here silently — and 1.6.0 declares no `default` feature today, so that pin is
insurance rather than enforcement. Enforcement is this ADR plus the manifest
test [#149](https://github.com/fullstacktaiye/harkness/issues/149) adds for the
`unstable_` prefix, alongside the ADR-0009 layering check the adapter crates
already carry.

**Version disagreement is a first-class outcome, not an error path.** The
negotiation rule is:

- The client sends the **latest version it supports** in `initialize`. Today
  that is 1; the phrasing is deliberate, because when Harkness does adopt a later
  version the negotiation code should not change.
- If the agent selects a version Harkness supports, the session proceeds and the
  selected version is recorded.
- If the agent selects any other version, Harkness **closes the connection
  cleanly and tells the user what happened** in a sentence naming both versions:
  which version the agent selected and which versions Harkness speaks. It is not
  a deserialization failure, not a panic, and not a retry loop.

**The selected protocol version is part of an agent's identity** under ADR-0016.
An agent that begins selecting a different version has changed in a
security-relevant way, and its trust grant is invalidated with that reason rather
than carried over.

**Adopting ACP v2 requires a superseding ADR.** Not a feature flag, not a
dependency bump, not a "while I was in there". The superseding ADR is where the
compatibility question gets answered — whether Harkness offers 2 and falls back,
or supports both, or moves — and none of those answers should be reached by
someone fixing something else.

## Consequences

- Harkness works against the agents that exist. v1 is what Gemini CLI and the
  other reference implementations speak, and the reference integration
  ([#155](https://github.com/fullstacktaiye/harkness/issues/155)) is exercised
  against a real one.
- A user with a bleeding-edge agent that has moved to v2 cannot use it, and
  learns so in one clear sentence at connect time. That is the intended
  behavior. The alternative — a session that starts and then fails oddly — is
  worse in every way that matters to someone debugging.
- Harkness cannot use v2-only capabilities, whatever they turn out to be. The
  cost is unknown because the draft is a draft, which is precisely the argument.
- The negotiation code is written once, for a rule that outlives version 1. When
  v2 stabilizes, the work is deciding the compatibility posture and adding a
  version to the supported set — not building version handling.
- Recording the selected version in agent identity means an agent that upgrades
  itself re-prompts the user for trust. This will happen to somebody whose agent
  auto-updates and it will feel like friction; it is a correct reading of "the
  thing you trusted now behaves differently".
- The v2 draft could stabilize during v0.5 and leave Harkness visibly behind for
  a milestone. Accepted: the containment is that the negotiation boundary already
  exists, so the gap is a scheduling question with a bounded fix rather than a
  rewrite.
- Tracking a moving draft means a session-level failure surface that is hard to
  test, because the peer's behavior changes between releases. Pinning to the
  stable version is what makes the conformance suite
  ([#156](https://github.com/fullstacktaiye/harkness/issues/156)) meaningful:
  there is a fixed thing to conform to.

## Alternatives considered

**Support v1 and v2 simultaneously**, negotiating per agent. Maximum
compatibility, and the schema crate technically has the types behind a flag.
Rejected: the v2 types are gated `unstable` because they may still change, so
"support" would mean tracking a moving target across a milestone, and every
adapter code path would double while one half of it was untestable against any
stable peer. Compatibility with a draft is not compatibility.

**Adopt v2 now** and let v1-only agents fall away. Rejected: v1-only agents are
the overwhelming majority of what users have installed, and a milestone whose
external-agent feature works with almost nothing has not shipped the feature.

**Send `protocolVersion: 1` and ignore whatever the agent selects**, proceeding
optimistically. Simple, and works whenever the versions are compatible in
practice. Rejected: it converts a clean, explainable refusal into an arbitrary
failure later in a session, and it makes the failure look like a Harkness bug.
The negotiation exists in the protocol precisely so this does not have to be
guessed.

**Treat an unsupported selected version as a transport error and retry.**
Rejected: nothing about retrying changes the answer, and a retry loop against a
subprocess Harkness launches is a way to spawn an agent repeatedly for no reason.
A version mismatch is a permanent condition until software changes.

**Gate v2 behind a Harkness cargo feature or a user-visible experimental
setting.** Tempting, and it looks cautious. Rejected: an off-by-default feature
still means the code exists, is compiled in CI configurations, and accrues
maintenance against a draft — and the first bug report from someone who enabled
it is a support obligation nobody agreed to. The ADR is the flag.
