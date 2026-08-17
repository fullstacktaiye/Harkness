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
//! # The handshake
//!
//! ```no_run
//! use harkness_acp::{
//!     AcpConnection, AdvertisedClientCapabilities, ClientIdentity,
//! };
//! use harkness_transport::{Cancellation, Connection, SpawnSpec};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // #150 decides which executable may run and builds the connection; this
//! // crate never launches a program on its own initiative.
//! let connection = Connection::spawn(
//!     SpawnSpec::new("/usr/local/bin/some-agent", "/home/user/project").arg("--acp"),
//!     Cancellation::default(),
//! )?;
//! let mut agent = AcpConnection::new(connection);
//!
//! let outcome = agent.initialize(
//!     &ClientIdentity::new("harkness", env!("CARGO_PKG_VERSION")),
//!     // #153 decides these three; the adapter advertises exactly what it is
//!     // handed and turns none of them on by itself.
//!     &AdvertisedClientCapabilities::default(),
//! )?;
//!
//! if let Some(method) = outcome.capabilities.auth_methods.first() {
//!     agent.authenticate(&method.id)?;
//! }
//! # let _ = agent.shutdown();
//! # Ok(())
//! # }
//! ```
//!
//! # Reading a failure
//!
//! [`AcpError::kind`] is the stable discriminant, and the namespace it draws
//! from is the union of this crate's table and the transport's — a broken pipe
//! during `initialize` stays `write_failed` rather than being re-spelled on the
//! way up. [`AcpError::is_terminal`] answers the question a caller actually has:
//! whether the agent is still there to talk to.
//!
//! # What is not here yet
//!
//! Sessions, prompt turns, and streaming updates ([#151]); permission requests
//! into policy and approvals ([#152]); filesystem and terminal mediation
//! ([#153]); workspace isolation and activity classification ([#154]); the
//! reference agent ([#155]) and the conformance suite ([#156]). Registration,
//! executable identity, trust, and health checks are [#150]'s, and this crate's
//! job there is to report what it observed rather than to decide anything about
//! it.
//!
//! [ADR-0009]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0009-v05-adapter-crate-boundaries.md
//! [ADR-0010]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0010-official-acp-schema-crate.md
//! [ADR-0012]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0012-stdio-only-protocol-transports.md
//! [ADR-0014]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0014-acp-protocol-version-one.md
//! [ADR-0016]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0016-per-subject-trust-records.md
//! [ADR-0017]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0017-honest-observability-activity-classes.md
//! [#147]: https://github.com/fullstacktaiye/harkness/issues/147
//! [#150]: https://github.com/fullstacktaiye/harkness/issues/150
//! [#151]: https://github.com/fullstacktaiye/harkness/issues/151
//! [#152]: https://github.com/fullstacktaiye/harkness/issues/152
//! [#153]: https://github.com/fullstacktaiye/harkness/issues/153
//! [#154]: https://github.com/fullstacktaiye/harkness/issues/154
//! [#155]: https://github.com/fullstacktaiye/harkness/issues/155
//! [#156]: https://github.com/fullstacktaiye/harkness/issues/156

#![warn(missing_docs)]

mod capabilities;
mod connection;
mod error;
#[cfg(test)]
mod testing;
mod wire;

pub use capabilities::{
    AcpAgentCapabilities, AdvertisedClientCapabilities, AgentDescription, AuthCapabilities,
    AuthMethod, AuthMethodId, ClientIdentity, McpCapabilities, PromptCapabilities,
    SessionCapabilities,
};
pub use connection::{AcpConnection, AcpTimeouts, AuthenticateOutcome, InitializeOutcome};
pub use error::{AcpError, AgentRefusal};

/// The ACP protocol version Harkness offers in `initialize`.
///
/// The latest version it supports, which ADR-0014 phrases deliberately: when
/// Harkness does adopt a later version, this number moves and the negotiation
/// code does not.
pub const OFFERED_PROTOCOL_VERSION: u16 = 1;

/// Every ACP protocol version Harkness will proceed on.
///
/// A set rather than an equality test, for the same reason: accepting a selected
/// version is "is it one of ours", and the shape of that question does not change
/// when the answer grows. Adopting ACP v2 requires a superseding ADR, not an
/// entry here.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[u16] = &[OFFERED_PROTOCOL_VERSION];

#[cfg(test)]
mod tests {
    /// ADR-0009 draws two edges this crate may not have. It sits strictly below
    /// `harkness-runtime`, so it may not depend on the runtime or on a front end;
    /// and adapters do not depend on each other, so shared machinery goes below
    /// all four rather than sideways between two of them. A manifest is the only
    /// place either rule can be broken, so the manifest is what this reads — the
    /// sideways rule especially, since nothing else would catch it: no dependency
    /// cycle exists to trip on while the runtime does not yet name the adapters.
    /// The check is a plain substring search rather than a parse, which also
    /// catches a name in a `[dev-dependencies]` entry or in a comment claiming
    /// the rule no longer holds.
    #[test]
    fn the_manifest_names_no_crate_above_or_beside_this_one() {
        let manifest = include_str!("../Cargo.toml");
        for forbidden in [
            "harkness-runtime",
            "harkness-cli",
            "harkness-gui",
            "harkness-mcp",
            "harkness-forge",
            "harkness-recipe",
        ] {
            assert!(
                !manifest.contains(forbidden),
                "{forbidden} appears in crates/harkness-acp/Cargo.toml; ADR-0009 forbids an \
                 adapter crate from depending on anything above it or beside it",
            );
        }
    }

    /// ADR-0014 fixes Harkness at protocol version 1 and names this test as the
    /// enforcement. Cargo features are additive from members, so the workspace
    /// pin's `default-features = false` cannot veto a member that asks for one:
    /// `unstable_protocol_v2` would compile in a draft protocol, and every other
    /// `unstable_*` gate is a feature upstream says may still change shape.
    /// Adopting any of them is an ADR, and this is what makes that friction
    /// real rather than advisory.
    #[test]
    fn the_manifest_enables_no_unstable_protocol_feature() {
        let manifest = include_str!("../Cargo.toml");
        assert!(
            !manifest.contains("unstable_"),
            "an unstable_ feature appears in crates/harkness-acp/Cargo.toml; ADR-0014 fixes \
             Harkness at ACP protocol version 1 and adopting a draft feature requires a \
             superseding ADR",
        );
    }

    /// ADR-0003 keeps the workspace synchronous, and ADR-0010 adopts the schema
    /// crate rather than the SDK for exactly that reason: `agent-client-protocol`
    /// and `agent-client-protocol-tokio` layer an async connection model on the
    /// same types. Depending on either would drag a runtime in through a side
    /// door, and the names differ from the permitted one by a suffix.
    #[test]
    fn the_manifest_names_no_async_runtime_and_no_acp_sdk() {
        let manifest = include_str!("../Cargo.toml");
        for forbidden in [
            "tokio",
            "async-std",
            "smol",
            "futures",
            "agent-client-protocol-tokio",
        ] {
            assert!(
                !manifest.contains(forbidden),
                "{forbidden} appears in crates/harkness-acp/Cargo.toml; ADR-0003 keeps the \
                 workspace synchronous and ADR-0010 adopts the schema crate rather than the SDK",
            );
        }

        // The SDK's own name is a prefix of the schema crate's, so it is checked
        // by counting rather than by absence: the manifest may name
        // `agent-client-protocol-schema` and nothing else that starts that way.
        assert_eq!(
            manifest.matches("agent-client-protocol").count(),
            manifest.matches("agent-client-protocol-schema").count(),
            "crates/harkness-acp/Cargo.toml names an agent-client-protocol crate other than the \
             schema crate; ADR-0010 permits only the schema artifacts",
        );
    }
}
