//! The shared subprocess JSON-RPC engine both protocol adapters run on.
//!
//! An ACP agent is a program Harkness launches and speaks JSON-RPC 2.0 to over
//! its standard streams. An MCP stdio server is a program Harkness launches and
//! speaks newline-delimited JSON-RPC to over its standard streams. The two
//! specifications describe, in different words, one transport — down to the same
//! teardown sequence — so it is built once, here, below both adapters
//! ([ADR-0012]).
//!
//! # What this crate is for
//!
//! Every external agent and server Harkness launches gets the process discipline
//! users already rely on for Git: no shell interpretation, no inherited
//! environment, hard bounds on everything a peer controls, prompt cancellation,
//! and a teardown that never leaves a process group running on a workspace. When
//! a peer misbehaves, what comes back is a typed, specific failure — *it exited
//! before responding*, *it died mid-message*, *it sent something that is not a
//! message* — beside whatever it wrote to standard error, rather than a hung UI.
//!
//! # What this crate is not for
//!
//! It carries **no protocol semantics** and cannot tell one JSON-RPC method from
//! another. `initialize`, version negotiation, `session/new`, `server/discover`,
//! `tools/list`, and every cancellation notification are the adapters' — [#149]
//! for ACP, [#157] for MCP — as is the decision to relaunch a peer that died
//! ([#158]). This engine reports and stays down.
//!
//! # The pieces
//!
//! - [`SpawnSpec`] describes a peer: an absolute program, argv, an exhaustive
//!   environment allowlist, a pinned working directory, and the bounds the
//!   connection will hold it to.
//! - [`JsonRpcTransport`] is the seam ADR-0012 draws — messages in, messages
//!   out, and a teardown that reports an outcome. [`StdioTransport`] is its
//!   subprocess implementation, and a remote transport would be another.
//! - [`Connection`] correlates requests to responses over any transport, which
//!   is what an adapter actually writes against.
//! - [`StderrSink`] is where a peer's logging goes. `harkness-runtime` implements
//!   it over the artifact store; this crate ships a discarding sink and a bounded
//!   [`StderrTail`].
//!
//! ```no_run
//! use std::time::{Duration, Instant};
//!
//! use harkness_transport::{Cancellation, Connection, SpawnSpec, StderrTail};
//!
//! let logging = StderrTail::new(64 * 1024);
//! let connection = Connection::spawn(
//!     SpawnSpec::new("/usr/local/bin/some-agent", "/home/user/project")
//!         .arg("--stdio")
//!         .env("PATH", "/usr/bin:/bin")
//!         .stderr_sink(logging.clone()),
//!     Cancellation::default(),
//! )?;
//!
//! let answer = connection.request(
//!     "initialize",
//!     Some(serde_json::json!({ "protocolVersion": 1 })),
//!     Instant::now() + Duration::from_secs(10),
//! )?;
//! connection.handshake_complete();
//!
//! let outcome = connection.shutdown(Duration::from_secs(5));
//! # let _ = (answer, outcome, logging.text());
//! # Ok::<(), harkness_transport::TransportError>(())
//! ```
//!
//! # Mapping these failures into a protocol
//!
//! An adapter turns a [`TransportError`] into its own vocabulary, and the useful
//! rule is that only four of them are about one *call* —
//! [`RequestTimedOut`](TransportError::RequestTimedOut),
//! [`SendTimedOut`](TransportError::SendTimedOut),
//! [`UnencodableMessage`](TransportError::UnencodableMessage), and
//! [`PeerQueueFull`](TransportError::PeerQueueFull) — which
//! [`is_terminal`](TransportError::is_terminal) answers for. Everything else has
//! ended the conversation, and an adapter that retries over a quarantined
//! connection is retrying into a stream whose position is unknown. Retry policy
//! is the adapter's either way: this engine never retries anything, and blindly
//! retrying a mutating call is forbidden by the milestone's idempotency rules.
//!
//! # Consume what the peer sends you
//!
//! An adapter whose peer streams — ACP session updates, MCP progress — has to
//! take those messages off the connection. Peer-initiated messages queue behind
//! a bound, and the engine will not discard one to make room or grow to hold
//! it, so a response arriving after `PEER_CAPACITY` unread updates cannot be
//! reached until they are consumed. That is reported as
//! [`PeerQueueFull`](TransportError::PeerQueueFull) rather than left to look
//! like a slow peer. Either run a thread on
//! [`next_peer_message`](Connection::next_peer_message) — the ACP shape, and the
//! one that composes with concurrent [`request`](Connection::request) calls — or
//! interleave the two within a turn.
//!
//! [ADR-0012]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0012-stdio-only-protocol-transports.md
//! [#149]: https://github.com/fullstacktaiye/harkness/issues/149
//! [#157]: https://github.com/fullstacktaiye/harkness/issues/157
//! [#158]: https://github.com/fullstacktaiye/harkness/issues/158

#![warn(missing_docs)]

mod connection;
mod error;
mod frame;
mod message;
mod spawn;
mod stderr;
mod stdio;
mod transport;

pub use connection::{Connection, PeerMessage};
pub use error::{DesyncDetail, DisconnectKind, TransportError};
pub use message::{Message, Notification, PeerError, Request, RequestId, Response};
pub use spawn::{DEFAULT_MAX_MESSAGE_BYTES, DEFAULT_STARTUP_DEADLINE, SpawnSpec};
pub use stderr::{DiscardedStderr, StderrSink, StderrTail};
pub use stdio::StdioTransport;
pub use transport::{Counters, JsonRpcTransport, ShutdownOutcome, ShutdownRung};

/// The workspace's cancellation token, re-exported.
///
/// Re-exported rather than wrapped so an adapter that already holds one — from a
/// GUI job, a scheduler slot, or a Git operation — passes the same token down
/// instead of translating between two cancellation mechanisms. That is the same
/// reason `harkness-runtime`'s `ExecutionContext` carries this type.
pub use harkness_git::Cancellation;

#[cfg(test)]
mod tests {
    /// ADR-0012 puts this crate at the bottom of the graph, beneath both
    /// protocol adapters, and the only dependency it is allowed above
    /// `harkness-git` is none. A manifest is the only place that can be broken,
    /// so the manifest is what this reads — and it has to be read directly,
    /// because no dependency cycle exists to trip on: the adapters do not yet
    /// name this crate, so nothing else would catch an edge added here.
    ///
    /// The check is a plain substring search rather than a parse, which also
    /// catches a name in a `[dev-dependencies]` entry or in a comment claiming
    /// the rule no longer holds.
    #[test]
    fn the_manifest_names_no_crate_above_this_one() {
        let manifest = include_str!("../Cargo.toml");
        for forbidden in [
            "harkness-runtime",
            "harkness-cli",
            "harkness-gui",
            "harkness-acp",
            "harkness-mcp",
            "harkness-forge",
            "harkness-recipe",
            "harkness-core",
        ] {
            assert!(
                !manifest.contains(forbidden),
                "{forbidden} appears in crates/harkness-transport/Cargo.toml; ADR-0012 puts the \
                 shared transport below every crate that uses it",
            );
        }
    }

    /// ADR-0003 keeps the workspace synchronous, and ADR-0012 sizes this crate's
    /// blocking waits against that decision rather than reaching for a runtime
    /// the rest of the workspace does not have.
    #[test]
    fn the_manifest_names_no_async_runtime() {
        let manifest = include_str!("../Cargo.toml");
        for forbidden in ["tokio", "async-std", "smol", "futures"] {
            assert!(
                !manifest.contains(forbidden),
                "{forbidden} appears in crates/harkness-transport/Cargo.toml; ADR-0003 keeps the \
                 workspace synchronous",
            );
        }
    }
}
