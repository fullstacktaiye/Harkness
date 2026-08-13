//! The transport error namespace.
//!
//! Every failure a protocol peer can cause lands in [`TransportError`], and each
//! variant answers a question an adapter has to be able to ask without reading a
//! message: did the program fail to start, did it stop talking, did it say
//! something that is not a message, or did Harkness give up on it? The
//! discriminants are stable snake_case strings in a [`KINDS`](TransportError::KINDS)
//! table, following `GitError`, so the CLI envelope and the GUI job model can
//! name a transport failure without matching on text.

use std::{io, path::PathBuf, time::Duration};

use thiserror::Error;

use crate::message::RequestId;

/// Why a connection stopped being a connection.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TransportError {
    /// The spawn description could not produce a hermetic invocation.
    ///
    /// Raised before anything is launched. A relative program or working
    /// directory, a zero message bound, or an environment name that cannot be
    /// passed to `execve` are refusals rather than best-effort spawns, because
    /// every one of them makes the invocation depend on state this crate does
    /// not control.
    #[error("the connection cannot be spawned: {detail}")]
    InvalidSpawnSpec {
        /// What about the description was refused.
        detail: String,
    },

    /// The peer program could not be launched.
    #[error("failed to start '{}': {source}", program.display())]
    SpawnFailed {
        /// The program that was to be launched.
        program: PathBuf,
        /// The operating system's reason.
        #[source]
        source: io::Error,
    },

    /// The peer did not finish its handshake within the startup deadline.
    ///
    /// The deadline covers spawn through the adapter's own handshake window,
    /// which is why the adapter has to declare the window closed by calling
    /// [`handshake_complete`](crate::JsonRpcTransport::handshake_complete). A
    /// transport that never hears from the adapter treats every later blocking
    /// call as still being inside startup, which is the safe reading: a peer
    /// that never completed `initialize` is not one to keep waiting on.
    #[error("the peer did not complete its handshake within {} ms", deadline.as_millis())]
    StartupDeadlineExceeded {
        /// The window the peer was given.
        deadline: Duration,
    },

    /// A message could not be put on the wire.
    ///
    /// Newline-delimited framing means a message whose encoding contains a
    /// newline is two messages to the peer. Such a message is refused rather
    /// than escaped, because escaping it would deliver something the caller did
    /// not write.
    #[error("the message cannot be framed: {detail}")]
    UnencodableMessage {
        /// What about the encoding was refused.
        detail: String,
    },

    /// An inbound line exceeded the configured maximum message size.
    ///
    /// `bytes` is what had been read when the bound was breached, not the true
    /// length of the line: the reader stops at the limit rather than measuring
    /// something it has already refused to hold.
    #[error("the peer sent at least {bytes} bytes on one line, over the {limit} byte limit")]
    MessageTooLarge {
        /// Bytes of the offending line read before the reader stopped.
        bytes: usize,
        /// The configured bound.
        limit: usize,
    },

    /// The peer sent something this connection cannot interpret as its next
    /// message, so the message stream no longer has a known position.
    #[error("the connection lost synchronization: {detail}")]
    Desynchronized {
        /// Which way synchronization was lost.
        detail: DesyncDetail,
    },

    /// The peer process ended.
    #[error("the peer disconnected: {kind}")]
    Disconnected {
        /// What the peer was doing when it ended.
        kind: DisconnectKind,
    },

    /// The operation was cancelled through its [`Cancellation`](crate::Cancellation).
    #[error("the transport operation was cancelled")]
    Cancelled,

    /// A message could not be handed to the peer.
    #[error("failed to write to the peer: {detail}")]
    WriteFailed {
        /// What went wrong on the way to the peer's standard input.
        detail: String,
    },

    /// A request's own deadline passed with no response.
    ///
    /// Distinct from [`Disconnected`](Self::Disconnected): the peer is still
    /// running and may yet answer, which is exactly why the correlation entry is
    /// dropped — a late answer to a request nobody is waiting for is an unknown
    /// response id, and that is a desynchronization the adapter should hear
    /// about rather than a stale value delivered to whoever asks next.
    #[error("the peer did not answer request {id} before its deadline")]
    RequestTimedOut {
        /// The request that went unanswered.
        id: RequestId,
    },

    /// The connection was quarantined by an earlier fault and does no further
    /// I/O.
    ///
    /// The thread that observed the fault receives the fault itself; every
    /// later caller receives this, naming it. A quarantined connection is never
    /// resynchronized — the peer's stream has an unknown position and guessing
    /// at where the next message starts is how one bad line becomes a wrong
    /// answer.
    #[error("the connection was quarantined by an earlier {fault_kind} fault: {detail}")]
    Quarantined {
        /// [`kind`](TransportError::kind) of the fault that quarantined it.
        fault_kind: &'static str,
        /// That fault's own message.
        detail: String,
    },
}

impl TransportError {
    /// Every stable discriminant this error namespace can emit.
    pub const KINDS: &'static [&'static str] = &[
        "invalid_spawn_spec",
        "spawn_failed",
        "startup_deadline_exceeded",
        "unencodable_message",
        "message_too_large",
        "desynchronized",
        "disconnected",
        "cancelled",
        "write_failed",
        "request_timed_out",
        "quarantined",
    ];

    /// Stable machine-readable discriminant for agent-facing error handling.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::InvalidSpawnSpec { .. } => "invalid_spawn_spec",
            Self::SpawnFailed { .. } => "spawn_failed",
            Self::StartupDeadlineExceeded { .. } => "startup_deadline_exceeded",
            Self::UnencodableMessage { .. } => "unencodable_message",
            Self::MessageTooLarge { .. } => "message_too_large",
            Self::Desynchronized { .. } => "desynchronized",
            Self::Disconnected { .. } => "disconnected",
            Self::Cancelled => "cancelled",
            Self::WriteFailed { .. } => "write_failed",
            Self::RequestTimedOut { .. } => "request_timed_out",
            Self::Quarantined { .. } => "quarantined",
        }
    }

    /// Whether this failure is terminal for the connection that raised it.
    ///
    /// Every kind here is terminal except the two that describe one *call*:
    /// a request that ran out of time and a message that could not be framed
    /// both leave a healthy connection behind, and an adapter that tore one down
    /// for a slow answer would restart an agent mid-session over a timeout it
    /// chose.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        !matches!(
            self,
            Self::RequestTimedOut { .. } | Self::UnencodableMessage { .. }
        )
    }
}

/// Which way a connection lost its place in the peer's message stream.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DesyncDetail {
    /// A line on standard output was not one complete JSON value.
    ///
    /// Covers the MCP rule that a server must not write non-protocol data to
    /// standard output: a startup banner and a truncated message are the same
    /// event to a reader that has to know where the next message begins.
    NonJsonLine {
        /// The parser's account of what it found.
        detail: String,
    },

    /// A line parsed as JSON but is not a JSON-RPC 2.0 message.
    NotJsonRpc {
        /// Which part of the message shape was wrong.
        detail: String,
    },

    /// A response named a request id this connection never issued.
    UnknownResponseId {
        /// The id the peer answered.
        id: RequestId,
    },

    /// A response named a request id that had already been answered.
    DuplicateResponseId {
        /// The id the peer answered twice.
        id: RequestId,
    },
}

impl DesyncDetail {
    /// Every stable discriminant a desynchronization can carry.
    pub const DETAILS: &'static [&'static str] = &[
        "non_json_line",
        "not_json_rpc",
        "unknown_response_id",
        "duplicate_response_id",
    ];

    /// Stable machine-readable discriminant for this detail.
    #[must_use]
    pub fn detail(&self) -> &'static str {
        match self {
            Self::NonJsonLine { .. } => "non_json_line",
            Self::NotJsonRpc { .. } => "not_json_rpc",
            Self::UnknownResponseId { .. } => "unknown_response_id",
            Self::DuplicateResponseId { .. } => "duplicate_response_id",
        }
    }
}

impl std::fmt::Display for DesyncDetail {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonJsonLine { detail } => {
                write!(
                    formatter,
                    "the peer wrote a line that is not one JSON message ({detail})"
                )
            }
            Self::NotJsonRpc { detail } => {
                write!(
                    formatter,
                    "the peer wrote JSON that is not a JSON-RPC 2.0 message ({detail})"
                )
            }
            Self::UnknownResponseId { id } => {
                write!(
                    formatter,
                    "the peer answered request {id}, which was never sent"
                )
            }
            Self::DuplicateResponseId { id } => {
                write!(formatter, "the peer answered request {id} more than once")
            }
        }
    }
}

/// What the peer was doing when its process ended.
///
/// The three are told apart from what the connection could observe, and nothing
/// else: whether a request was outstanding, and whether the last thing on
/// standard output was a complete line. An adapter deciding whether to relaunch
/// a server needs that distinction — a peer that dies mid-response is a
/// different bug report from one that exits while idle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisconnectKind {
    /// The peer exited with requests outstanding and answered none of them.
    ExitBeforeResponse,

    /// The peer exited part-way through a line, so a message was cut in half.
    MidResponse,

    /// The peer exited with nothing outstanding.
    Idle,
}

impl DisconnectKind {
    /// Every stable discriminant a disconnect can carry.
    pub const KINDS: &'static [&'static str] = &["exit_before_response", "mid_response", "idle"];

    /// Stable machine-readable discriminant for this kind.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExitBeforeResponse => "exit_before_response",
            Self::MidResponse => "mid_response",
            Self::Idle => "idle",
        }
    }
}

impl std::fmt::Display for DisconnectKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let described = match self {
            Self::ExitBeforeResponse => "it exited before responding",
            Self::MidResponse => "it exited part-way through a message",
            Self::Idle => "it exited with nothing outstanding",
        };
        formatter.write_str(described)
    }
}

#[cfg(test)]
mod tests {
    use std::{io, path::PathBuf, time::Duration};

    use super::{DesyncDetail, DisconnectKind, TransportError};
    use crate::message::RequestId;

    #[test]
    fn every_error_kind_is_declared_in_order() {
        let cases = [
            (
                TransportError::InvalidSpawnSpec {
                    detail: "fixture".to_owned(),
                },
                "invalid_spawn_spec",
            ),
            (
                TransportError::SpawnFailed {
                    program: PathBuf::from("/nowhere/agent"),
                    source: io::Error::from(io::ErrorKind::NotFound),
                },
                "spawn_failed",
            ),
            (
                TransportError::StartupDeadlineExceeded {
                    deadline: Duration::from_secs(1),
                },
                "startup_deadline_exceeded",
            ),
            (
                TransportError::UnencodableMessage {
                    detail: "fixture".to_owned(),
                },
                "unencodable_message",
            ),
            (
                TransportError::MessageTooLarge { bytes: 2, limit: 1 },
                "message_too_large",
            ),
            (
                TransportError::Desynchronized {
                    detail: DesyncDetail::NonJsonLine {
                        detail: "fixture".to_owned(),
                    },
                },
                "desynchronized",
            ),
            (
                TransportError::Disconnected {
                    kind: DisconnectKind::Idle,
                },
                "disconnected",
            ),
            (TransportError::Cancelled, "cancelled"),
            (
                TransportError::WriteFailed {
                    detail: "fixture".to_owned(),
                },
                "write_failed",
            ),
            (
                TransportError::RequestTimedOut {
                    id: RequestId::Number(1),
                },
                "request_timed_out",
            ),
            (
                TransportError::Quarantined {
                    fault_kind: "desynchronized",
                    detail: "fixture".to_owned(),
                },
                "quarantined",
            ),
        ];

        let kinds = cases.iter().map(|(_, kind)| *kind).collect::<Vec<_>>();
        assert_eq!(kinds, TransportError::KINDS);
        for (error, expected) in cases {
            assert_eq!(error.kind(), expected, "unexpected kind for {error:?}");
        }
    }

    #[test]
    fn every_desynchronization_detail_is_declared_in_order() {
        let cases = [
            (
                DesyncDetail::NonJsonLine {
                    detail: "fixture".to_owned(),
                },
                "non_json_line",
            ),
            (
                DesyncDetail::NotJsonRpc {
                    detail: "fixture".to_owned(),
                },
                "not_json_rpc",
            ),
            (
                DesyncDetail::UnknownResponseId {
                    id: RequestId::Number(7),
                },
                "unknown_response_id",
            ),
            (
                DesyncDetail::DuplicateResponseId {
                    id: RequestId::from("seven"),
                },
                "duplicate_response_id",
            ),
        ];

        let details = cases.iter().map(|(_, detail)| *detail).collect::<Vec<_>>();
        assert_eq!(details, DesyncDetail::DETAILS);
        for (detail, expected) in cases {
            assert_eq!(detail.detail(), expected);
        }
    }

    #[test]
    fn every_disconnect_kind_is_declared_in_order() {
        let cases = [
            (DisconnectKind::ExitBeforeResponse, "exit_before_response"),
            (DisconnectKind::MidResponse, "mid_response"),
            (DisconnectKind::Idle, "idle"),
        ];

        let kinds = cases.iter().map(|(_, kind)| *kind).collect::<Vec<_>>();
        assert_eq!(kinds, DisconnectKind::KINDS);
        for (kind, expected) in cases {
            assert_eq!(kind.as_str(), expected);
        }
    }

    /// A slow answer and an unframable message are the caller's problem with one
    /// call. Everything else means the peer's stream has an unknown position or
    /// the peer is gone, and an adapter that kept using the connection would be
    /// guessing.
    #[test]
    fn only_per_call_failures_leave_the_connection_usable() {
        assert!(
            !TransportError::RequestTimedOut {
                id: RequestId::Number(1)
            }
            .is_terminal()
        );
        assert!(
            !TransportError::UnencodableMessage {
                detail: "fixture".to_owned()
            }
            .is_terminal()
        );
        assert!(TransportError::Cancelled.is_terminal());
        assert!(
            TransportError::Disconnected {
                kind: DisconnectKind::Idle
            }
            .is_terminal()
        );
    }
}
