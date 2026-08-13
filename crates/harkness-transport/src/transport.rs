//! The seam ADR-0012 draws: adapters speak to a transport, never to a process.
//!
//! Everything an adapter can do to a peer is here — put a message on the wire,
//! take one off it with a deadline, declare the peer's stream unusable, and tear
//! the connection down with an outcome that says how far the teardown had to go.
//! There is no `Child`, no file descriptor, and no signal in this trait, which is
//! the property that lets a remote transport be added later without a protocol
//! adapter changing a line.
//!
//! Two signatures differ from the ADR's sketch, which fixes the shape rather
//! than the signatures.
//!
//! - `shutdown` takes `self: Box<Self>`. Teardown has to consume the connection,
//!   and a by-value `self` would make the trait non-object-safe — the exact
//!   accommodation ADR-0012 left to this issue, and the same one
//!   `ArtifactStream::finish` makes in `harkness-runtime` for the same reason.
//! - One [`Message`] type serves both directions rather than an `OutboundMessage`
//!   and an `InboundMessage`. JSON-RPC is symmetric and both peers may open a
//!   request, so two structurally identical types would buy a conversion at every
//!   adapter boundary and no property at all.

use std::time::{Duration, Instant};

use crate::{error::TransportError, message::Message};

/// A JSON-RPC conversation with one peer.
///
/// Implementations are shared across threads: a [`Connection`](crate::Connection)
/// may have one thread awaiting a response while another consumes peer-initiated
/// messages, so `send` and `recv_deadline` take `&self` and must be safe to call
/// concurrently.
pub trait JsonRpcTransport: Send + Sync {
    /// Hands one message to the peer, giving up at `deadline`.
    ///
    /// Returns once the message is queued for the peer, not once the peer has
    /// read it — nothing but a response tells a caller that.
    ///
    /// The deadline is not ceremony. A peer that has stopped reading its own
    /// standard input fills its pipe, then the queue behind it, and an enqueue
    /// with no bound would sit there; one with a bound of the transport's own
    /// choosing would overrun a caller that asked for an answer within a second
    /// by however much the two numbers differ. So the caller's deadline travels
    /// with the message, which is what ADR-0012 means by blocking calls with
    /// explicit deadlines.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError`] when the message cannot be framed, the peer is
    /// gone, the connection is quarantined, or the message could not be handed
    /// over before `deadline`.
    fn send(&self, message: Message, deadline: Instant) -> Result<(), TransportError>;

    /// Hands over one message if there is room for it right now.
    ///
    /// The non-blocking half of [`send`](Self::send), and the reason it exists
    /// is a deadlock rather than a preference. A caller blocked inside `send`
    /// is not reading, and a peer that floods its own output stops reading its
    /// input until somebody drains it — so a `send` that waits without reading
    /// is two sides waiting for each other. Returning the message instead lets
    /// the caller pump between attempts, and returning it rather than requiring
    /// a clone is what keeps that affordable for a large one.
    ///
    /// # Errors
    ///
    /// Returns [`SendRejection::NoRoom`] with the message back when the peer's
    /// queue is full, and [`SendRejection::Failed`] when the connection cannot
    /// carry the message at all.
    fn try_send(&self, message: Message) -> Result<(), SendRejection>;

    /// Takes the next message from the peer, waiting until `deadline`.
    ///
    /// Blocking with an explicit deadline rather than returning a future is the
    /// workspace's concurrency model ([ADR-0003]), and the deadline is on the
    /// *call* rather than on the process precisely so a remote transport can
    /// implement it: a socket has no `SIGKILL`.
    ///
    /// Cancellation is observed while waiting, so a caller that passes a distant
    /// deadline still returns promptly when its token is tripped.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::RequestTimedOut`]'s sibling conditions — a
    /// disconnect, a desynchronization, an oversized message, cancellation, or a
    /// quarantine — and reports the deadline passing by returning
    /// [`TransportError::Disconnected`] only when the peer has actually gone. A
    /// deadline that passes with the peer still alive is not an error the
    /// transport raises; the caller owns that decision, because only it knows
    /// what it was waiting for.
    ///
    /// [ADR-0003]: https://github.com/fullstacktaiye/harkness/blob/main/docs/adr/0003-blocking-http-and-sse.md
    fn recv_deadline(&self, deadline: Instant) -> Result<Option<Message>, TransportError>;

    /// Declares the peer's message stream unusable and stops all further I/O.
    ///
    /// Called by the layer that detected a fault the transport itself could not
    /// see — a response to a request nobody sent is a correlation failure, and
    /// correlation lives above this trait. The connection is never resynchronized
    /// afterwards: guessing where the next message starts is how one bad line
    /// becomes a wrong answer.
    fn quarantine(&self, fault: &TransportError);

    /// Declares the peer's handshake finished, ending the startup deadline.
    ///
    /// The startup deadline covers spawn *through* the adapter's handshake,
    /// which the transport cannot recognize — `initialize` is a method name it
    /// has no opinion about. So the adapter says when the window closes. A
    /// transport with no startup phase does nothing here.
    fn handshake_complete(&self) {}

    /// What this connection has moved and is holding.
    #[must_use]
    fn counters(&self) -> Counters {
        Counters::default()
    }

    /// Tears the connection down and reports how far it had to go.
    ///
    /// Never fails: teardown that could fail leaves a caller with a live child
    /// and nothing to do about it. What it reports instead is the rung reached,
    /// which is a diagnostic about the peer rather than about the teardown.
    fn shutdown(self: Box<Self>, grace: Duration) -> ShutdownOutcome;
}

/// Why [`JsonRpcTransport::try_send`] did not hand a message over.
#[derive(Debug)]
pub enum SendRejection {
    /// There is no room at the moment. The message comes back so a caller can
    /// drain what the peer has sent and try again without rebuilding it.
    NoRoom(Message),
    /// The connection cannot carry the message at all.
    Failed(TransportError),
}

/// How far a teardown had to escalate.
///
/// The rungs are the MCP specification's shutdown sequence, and they are
/// reported rather than discarded because each one says something different
/// about the peer: a server that needed `SIGKILL` is a bug report, and one that
/// had already exited before shutdown began is a disconnect nobody noticed.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ShutdownRung {
    /// The peer had already exited when teardown began.
    AlreadyExited,
    /// The peer exited after its standard input was closed.
    ClosedStdin,
    /// The peer exited after its process group was sent `SIGTERM`.
    Signalled,
    /// The peer's process group had to be killed.
    Killed,
}

impl ShutdownRung {
    /// Every stable discriminant a shutdown can report.
    pub const RUNGS: &'static [&'static str] =
        &["already_exited", "closed_stdin", "signalled", "killed"];

    /// Stable machine-readable discriminant for this rung.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AlreadyExited => "already_exited",
            Self::ClosedStdin => "closed_stdin",
            Self::Signalled => "signalled",
            Self::Killed => "killed",
        }
    }
}

/// What one teardown did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShutdownOutcome {
    /// The furthest rung the sequence had to reach.
    pub rung: ShutdownRung,
    /// The peer's exit code, absent when a signal ended it.
    pub exit_code: Option<i32>,
    /// Bytes the peer wrote to standard error over the connection's life.
    pub stderr_bytes: u64,
}

/// What a connection has moved and is holding.
///
/// Exposed so the stress benchmarks in [#183] can assert that a peer streaming
/// notifications into a slow consumer applies backpressure instead of growing
/// this process, and so a front end can show a stuck connection's queue depth
/// rather than a spinner.
///
/// [#183]: https://github.com/fullstacktaiye/harkness/issues/183
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Counters {
    /// Bytes read from the peer's standard output.
    pub bytes_read: u64,
    /// Bytes written to the peer's standard input.
    pub bytes_written: u64,
    /// Messages queued for the peer and not yet written.
    pub outbound_depth: usize,
    /// Messages read from the peer and not yet taken by a caller.
    pub inbound_depth: usize,
    /// Requests sent and not yet answered.
    pub outstanding_requests: usize,
    /// Peer-initiated messages waiting for an adapter to consume them.
    pub peer_depth: usize,
}

#[cfg(test)]
mod tests {
    use super::ShutdownRung;

    #[test]
    fn every_shutdown_rung_is_declared_in_order() {
        let cases = [
            (ShutdownRung::AlreadyExited, "already_exited"),
            (ShutdownRung::ClosedStdin, "closed_stdin"),
            (ShutdownRung::Signalled, "signalled"),
            (ShutdownRung::Killed, "killed"),
        ];

        let rungs = cases.iter().map(|(_, rung)| *rung).collect::<Vec<_>>();
        assert_eq!(rungs, ShutdownRung::RUNGS);
        for (rung, expected) in cases {
            assert_eq!(rung.as_str(), expected);
        }
    }

    /// The rungs are ordered by escalation, so "did this peer need more than
    /// closing stdin" is a comparison rather than a match.
    #[test]
    fn rungs_order_by_escalation() {
        assert!(ShutdownRung::AlreadyExited < ShutdownRung::ClosedStdin);
        assert!(ShutdownRung::ClosedStdin < ShutdownRung::Signalled);
        assert!(ShutdownRung::Signalled < ShutdownRung::Killed);
    }
}
