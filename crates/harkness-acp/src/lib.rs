//! The Agent Client Protocol boundary: everything Harkness says to an external
//! coding agent, and nothing about what it does with the answer.
//!
//! An ACP agent is a program someone else wrote, launched as a child process and
//! spoken to in JSON-RPC 2.0 over its stdin and stdout. This crate owns that
//! conversation end to end — the wire types, the `initialize` exchange, protocol
//! version and capability negotiation, session lifecycle, and the streaming
//! updates a prompt turn produces. It owns nothing about runs, steps, policy, or
//! approvals, and it cannot: it sits strictly below `harkness-runtime` and may
//! not name it ([ADR-0009]).
//!
//! # The boundary
//!
//! ACP wire types are **private to this crate**. Nothing in `harkness-runtime`
//! or above may name one, and no ACP type is ever persisted. Conversion into
//! Harkness domain types happens at this crate's public surface, which is the
//! seam that lets an upstream protocol revision land here without becoming a
//! `runtime.db` migration.
//!
//! The wire types themselves come from the official `agent-client-protocol-schema`
//! crate, pinned to a schema/v1 release, rather than from hand-rolled serde
//! ([ADR-0010]). Its `unstable_*` features stay off, `unstable_protocol_v2`
//! above all: Harkness speaks **protocol version 1**, and the v2 draft is a
//! negotiation boundary — a version this client refuses politely, not a feature
//! set it reaches for ([ADR-0014]).
//!
//! Transport is stdio only, behind the transport trait seam so a remote
//! transport can be added later without touching protocol logic ([ADR-0012]).
//! The subprocess, its framing, its timeouts, and its cancellation are
//! [#147]'s, not this crate's.
//!
//! Everything an agent reports about what it did is a *claim*. It is recorded as
//! `AcpReported` and never presented as a Harkness observation ([ADR-0017]), and
//! the agent's executable identity is a trust subject in its own right
//! ([ADR-0016]).
//!
//! # What is not here yet
//!
//! All of it. This crate is a compile-clean skeleton: the boundary exists before
//! the code so that [#149] (wire types, initialization, negotiation), [#150]
//! (registration, executable identity, health), [#151] (sessions, streaming,
//! cancellation, resume), [#152] (permission requests into policy and
//! approvals), [#153] (filesystem and terminal mediation), [#154] (workspace
//! isolation and activity classification), [#155] (a reference agent), and
//! [#156] (the conformance suite) each land against a decided contract instead
//! of deciding one.
//!
//! [ADR-0009]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0009-v05-adapter-crate-boundaries.md
//! [ADR-0010]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0010-official-acp-schema-crate.md
//! [ADR-0012]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0012-stdio-only-protocol-transports.md
//! [ADR-0014]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0014-acp-protocol-version-one.md
//! [ADR-0016]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0016-per-subject-trust-records.md
//! [ADR-0017]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0017-honest-observability-activity-classes.md
//! [#147]: https://github.com/fullstacktaiye/harkness/issues/147
//! [#149]: https://github.com/fullstacktaiye/harkness/issues/149
//! [#150]: https://github.com/fullstacktaiye/harkness/issues/150
//! [#151]: https://github.com/fullstacktaiye/harkness/issues/151
//! [#152]: https://github.com/fullstacktaiye/harkness/issues/152
//! [#153]: https://github.com/fullstacktaiye/harkness/issues/153
//! [#154]: https://github.com/fullstacktaiye/harkness/issues/154
//! [#155]: https://github.com/fullstacktaiye/harkness/issues/155
//! [#156]: https://github.com/fullstacktaiye/harkness/issues/156

#![warn(missing_docs)]

#[cfg(test)]
mod tests {
    /// ADR-0009 places this crate strictly below `harkness-runtime`: the runtime
    /// depends on the adapters and no adapter depends on the runtime or on a
    /// front end. A manifest is the only place that rule can be broken, so the
    /// manifest is what this reads. The check is a plain substring search rather
    /// than a parse, which also catches the name in a `[dev-dependencies]` entry
    /// or a comment claiming the rule no longer holds.
    #[test]
    fn the_manifest_names_no_crate_above_this_one() {
        let manifest = include_str!("../Cargo.toml");
        for forbidden in ["harkness-runtime", "harkness-cli", "harkness-gui"] {
            assert!(
                !manifest.contains(forbidden),
                "{forbidden} appears in crates/harkness-acp/Cargo.toml; ADR-0009 forbids an \
                 adapter crate from depending on anything above it in the graph",
            );
        }
    }
}
