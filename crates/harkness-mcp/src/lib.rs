//! The Model Context Protocol boundary: how Harkness discovers and calls tools
//! that live in someone else's process.
//!
//! An MCP server is a program that publishes tools with JSON Schemas and runs
//! them on request. This crate owns the client half of that conversation — the
//! wire types, protocol-revision selection, tool discovery and pagination, and
//! tool invocation with its progress and cancellation — and owns none of what
//! Harkness then does with a discovered tool. Namespacing a discovered tool into
//! the registry, classifying its risk, evaluating policy, and requiring an
//! approval are all `harkness-runtime`'s, which is above this crate and which
//! this crate may not name ([ADR-0009]).
//!
//! # The boundary
//!
//! MCP wire types are **private to this crate**. Nothing above may name one, and
//! no MCP type is ever persisted; conversion into Harkness domain types happens
//! at this crate's public surface. A server's published JSON Schema is untrusted
//! input from an external process, normalized here before anything upstream sees
//! it ([#160]).
//!
//! # Protocol revisions
//!
//! Primary is the **stateless 2026-07-28** revision, selected by the specification's
//! `server/discover` probe. A server that does not answer the probe — any error,
//! any timeout, not one specific error code — gets the legacy **2025-11-25**
//! `initialize` handshake instead ([ADR-0013]). Features the specification has
//! deprecated (Roots, Sampling, Logging, HTTP+SSE, OAuth Dynamic Client
//! Registration) keep working upstream for a deprecation window and are not
//! adopted here.
//!
//! Transport is stdio only, behind the transport trait seam, so Streamable HTTP
//! and its OAuth story can be added later without touching protocol logic
//! ([ADR-0012]). The subprocess, its framing, its timeouts, and its cancellation
//! are [#147]'s.
//!
//! A server, its executable, and each tool schema it publishes are **separate
//! trust subjects**: trusting a server is not trusting a schema it changed
//! afterwards ([ADR-0016]).
//!
//! # What is not here yet
//!
//! All of it. This crate is a compile-clean skeleton so that [#157] (the client
//! adapter, discovery probe, and legacy fallback), [#158] (registration,
//! lifecycle, health), [#159] (tool discovery, caching, schema fingerprints),
//! [#160] (schema normalization, namespacing, risk, policy), [#161] (execution,
//! progress, output bounds, artifacts), and [#162] (the conformance suite) each
//! land against a decided contract instead of deciding one.
//!
//! [ADR-0009]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0009-v05-adapter-crate-boundaries.md
//! [ADR-0012]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0012-stdio-only-protocol-transports.md
//! [ADR-0013]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0013-mcp-stateless-with-legacy-fallback.md
//! [ADR-0016]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0016-per-subject-trust-records.md
//! [#147]: https://github.com/fullstacktaiye/harkness/issues/147
//! [#157]: https://github.com/fullstacktaiye/harkness/issues/157
//! [#158]: https://github.com/fullstacktaiye/harkness/issues/158
//! [#159]: https://github.com/fullstacktaiye/harkness/issues/159
//! [#160]: https://github.com/fullstacktaiye/harkness/issues/160
//! [#161]: https://github.com/fullstacktaiye/harkness/issues/161
//! [#162]: https://github.com/fullstacktaiye/harkness/issues/162

#![warn(missing_docs)]

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
            "harkness-acp",
            "harkness-forge",
            "harkness-recipe",
        ] {
            assert!(
                !manifest.contains(forbidden),
                "{forbidden} appears in crates/harkness-mcp/Cargo.toml; ADR-0009 forbids an \
                 adapter crate from depending on anything above it or beside it",
            );
        }
    }
}
