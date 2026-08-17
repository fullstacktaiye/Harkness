//! The ACP error namespace.
//!
//! Every way an `initialize` or an `authenticate` can fail lands in
//! [`AcpError`], and each variant answers a question a caller has to be able to
//! ask without reading a message: is this agent too new, did it say something
//! that is not an ACP response, did it refuse the request, or did the connection
//! beneath it end? The discriminants are stable snake_case strings following the
//! `GitError` convention, so #150's health records and the CLI envelope can name
//! an ACP failure without matching on prose.
//!
//! # Two tables, one namespace
//!
//! A transport failure is *not* re-spelled here. [`AcpError::Transport`] carries
//! the [`TransportError`] whole and delegates [`kind`](AcpError::kind) to it, so
//! a broken pipe during `initialize` is `write_failed` at every layer that
//! reports it rather than `write_failed` beneath and `acp_transport_failed`
//! above. That makes this namespace the union of two tables — the one below and
//! [`TransportError::KINDS`] — exactly as `InvocationError` is the union of the
//! registry's and the tool's, and for the same reason: a caller publishing an
//! exit code per kind publishes their concatenation, so the two must not
//! collide. [`AcpError::kinds`] is that concatenation and a test holds it
//! disjoint.
//!
//! # What a peer's own words cost
//!
//! An agent's JSON-RPC error object arrives whole in an [`AgentRefusal`],
//! preserved verbatim because a truncated diagnosis is worse than a large one in
//! memory. It is boxed for the reason `InvocationError` boxes a `ToolError`:
//! every `Result<_, AcpError>` would otherwise be as wide as the rarest failure
//! in it.

use std::fmt::{self, Write as _};

use harkness_transport::TransportError;
use serde_json::Value;
use thiserror::Error;

use crate::{SUPPORTED_PROTOCOL_VERSIONS, capabilities::AuthMethodId};

/// One JSON-RPC error object, exactly as the agent wrote it.
///
/// The three fields travel together because they mean nothing apart: a code with
/// no message is a number, and a message with no code is prose. Harkness adds no
/// interpretation — it has no vocabulary for what an agent's own error numbers
/// mean, and inventing one would be guessing on a caller's behalf.
///
/// Both `message` and `data` are the agent's, and the only bound on them is the
/// transport's `max_message_bytes` — 16 MiB by default. A caller making either
/// durable owes it the run store's inline bound, the same way
/// `ToolError::as_failure` clamps what it is handed rather than trusting its
/// source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentRefusal {
    /// The JSON-RPC error code.
    pub code: i64,
    /// The agent's own short description.
    pub message: String,
    /// The agent's own structured detail, when it sent any.
    pub data: Option<Value>,
}

impl fmt::Display for AgentRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} (code {})", self.message, self.code)
    }
}

/// Why an ACP exchange did not produce an answer.
///
/// Boxed where an agent's own words are carried, following `InvocationError`:
/// every `Result<_, AcpError>` in this crate is otherwise as wide as its rarest
/// failure, and a JSON-RPC error object is the widest thing here by far.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AcpError {
    /// The agent selected a protocol version Harkness does not speak.
    ///
    /// ADR-0014 makes this a first-class outcome rather than an error path: the
    /// connection is closed cleanly and both versions are named, because the
    /// alternative — proceeding optimistically — turns a sentence a user can act
    /// on into an arbitrary failure deep inside a session that looks like a
    /// Harkness bug.
    #[error(
        "the agent selected ACP protocol version {agent_selected}, and Harkness speaks {}",
        supported_versions()
    )]
    UnsupportedProtocolVersion {
        /// The version the agent chose, recorded verbatim for diagnostics.
        agent_selected: u16,
    },

    /// The agent's answer is not the response this method defines.
    ///
    /// Reported for a missing or wrong-typed *required* field only. An omitted
    /// or malformed capability is not this: ACP fixes omission as unsupported
    /// and upstream decodes a wrong-typed capability to the same value, so a
    /// capability object nobody can read is an agent with fewer features rather
    /// than an agent that failed to answer.
    #[error("the agent's {method} response is not one this protocol defines ({detail})")]
    MalformedResponse {
        /// The method whose response could not be read.
        method: &'static str,
        /// The field that was wrong, and how.
        detail: String,
    },

    /// The agent spoke out of turn during the handshake.
    ///
    /// ACP has no method an agent may call before `initialize` returns: there is
    /// no session to update, no file to read, and no terminal to create. A
    /// request or notification arriving inside the handshake window is therefore
    /// a peer that is not following the protocol, and the connection is closed
    /// rather than resynchronized — the same posture the transport takes for a
    /// stream whose position it no longer knows.
    #[error(
        "the agent sent {detail} during {during}, which ACP does not allow before the handshake completes"
    )]
    ProtocolViolation {
        /// The handshake method that was in flight.
        during: &'static str,
        /// What arrived, named by its JSON-RPC method.
        detail: String,
    },

    /// The agent answered the request with a JSON-RPC error.
    ///
    /// The connection is fine and the agent is running; it declined this call.
    #[error("the agent refused {method}: {refusal}")]
    AgentRejectedRequest {
        /// The method that was refused.
        method: &'static str,
        /// What the agent said, verbatim.
        refusal: Box<AgentRefusal>,
    },

    /// The agent does not implement a method ACP requires of it.
    ///
    /// Split from [`AgentRejectedRequest`](Self::AgentRejectedRequest) because
    /// the two lead somewhere different: a refusal is about this call, while
    /// `-32601` for `initialize` means the program on the other end is not an
    /// ACP agent at all and no later call will do better.
    #[error("the agent does not implement {method}")]
    MethodNotSupported {
        /// The method the agent did not recognize.
        method: &'static str,
    },

    /// The agent requires authentication before it will serve the request.
    ///
    /// The `-32000` code ACP reserves for it. The caller decides which of the
    /// advertised methods to use and whether to ask a human first, which is why
    /// this is reported rather than acted on here.
    #[error("the agent requires authentication before {method}")]
    AuthenticationRequired {
        /// The method that was refused for want of authentication.
        method: &'static str,
    },

    /// The agent rejected an authentication attempt.
    ///
    /// Distinct from a transport failure on purpose: "your credentials were
    /// refused" and "the agent died mid-call" are the same outcome to a caller
    /// that only checks for `Err`, and #150 has to tell them apart to decide
    /// between re-prompting a user and relaunching a program.
    #[error("the agent rejected authentication method '{method_id}': {refusal}")]
    AuthenticationFailed {
        /// The method that was attempted.
        method_id: AuthMethodId,
        /// What the agent said, verbatim.
        refusal: Box<AgentRefusal>,
    },

    /// Authentication was attempted with a method the agent never offered.
    ///
    /// Raised before anything is written. An agent that advertised no method at
    /// all reaches this too, which is the point: `authMethods` being empty means
    /// the agent wants no authentication, and sending one anyway is a request
    /// Harkness should not have made rather than a question for the peer.
    #[error(
        "the agent did not advertise authentication method '{requested}'; it offers {}",
        advertised_methods(advertised)
    )]
    AuthMethodNotAdvertised {
        /// The method the caller asked for.
        requested: AuthMethodId,
        /// Every method the agent did advertise, in the order it listed them.
        advertised: Vec<AuthMethodId>,
    },

    /// A method was called before `initialize` negotiated the connection.
    #[error("{method} was called before the connection was initialized")]
    NotInitialized {
        /// The method that was called too early.
        method: &'static str,
    },

    /// `initialize` was called on a connection that had already negotiated.
    ///
    /// The handshake is once per connection: a second one would re-negotiate a
    /// version and a capability set that sessions on this connection were
    /// already created against.
    #[error("the connection has already completed its handshake")]
    AlreadyInitialized,

    /// The connection was closed by an earlier failure and does no further I/O.
    ///
    /// The caller that observed the failure receives the failure itself; every
    /// later caller receives this, naming it.
    #[error("the connection was closed by an earlier {because} failure")]
    ConnectionClosed {
        /// [`kind`](AcpError::kind) of the failure that closed it.
        because: &'static str,
    },

    /// A request could not be encoded for the wire.
    ///
    /// A guard rather than an expected outcome. Every field of every request
    /// this crate builds is a string, a boolean, or an integer, none of which
    /// `serde_json` can refuse — but the request types are governed upstream, so
    /// a future release adding a field this reasoning does not cover becomes a
    /// typed refusal here instead of a panic in a user's session.
    #[error("the {method} request could not be encoded: {detail}")]
    UnencodableRequest {
        /// The method whose request could not be built.
        method: &'static str,
        /// What `serde_json` refused.
        detail: String,
    },

    /// The connection beneath the protocol failed.
    ///
    /// Carried whole rather than re-spelled, so a caller sees one vocabulary:
    /// `disconnected`, `cancelled`, `request_timed_out`, and the rest keep the
    /// discriminants #147 publishes for them.
    #[error("{source}")]
    Transport {
        /// What the connection reported.
        #[source]
        source: TransportError,
    },
}

impl AcpError {
    /// Every stable discriminant this crate defines for itself.
    ///
    /// Not the whole namespace: [`kinds`](Self::kinds) is, because a
    /// [`Transport`](Self::Transport) failure keeps the discriminant #147 gave
    /// it rather than being re-spelled here.
    pub const KINDS: &'static [&'static str] = &[
        "unsupported_protocol_version",
        "malformed_response",
        "protocol_violation",
        "agent_rejected_request",
        "method_not_supported",
        "authentication_required",
        "authentication_failed",
        "auth_method_not_advertised",
        "not_initialized",
        "already_initialized",
        "connection_closed",
        "unencodable_request",
    ];

    /// Stable machine-readable discriminant, delegated for a transport failure
    /// to the namespace that owns it.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::UnsupportedProtocolVersion { .. } => "unsupported_protocol_version",
            Self::MalformedResponse { .. } => "malformed_response",
            Self::ProtocolViolation { .. } => "protocol_violation",
            Self::AgentRejectedRequest { .. } => "agent_rejected_request",
            Self::MethodNotSupported { .. } => "method_not_supported",
            Self::AuthenticationRequired { .. } => "authentication_required",
            Self::AuthenticationFailed { .. } => "authentication_failed",
            Self::AuthMethodNotAdvertised { .. } => "auth_method_not_advertised",
            Self::NotInitialized { .. } => "not_initialized",
            Self::AlreadyInitialized => "already_initialized",
            Self::ConnectionClosed { .. } => "connection_closed",
            Self::UnencodableRequest { .. } => "unencodable_request",
            Self::Transport { source } => source.kind(),
        }
    }

    /// Every kind an ACP call can report: this crate's table followed by the
    /// transport's.
    ///
    /// Returned owned rather than as a `const` because it is the concatenation
    /// of two independently maintained tables, and copying the transport's
    /// entries into this file is exactly the drift the tables exist to prevent.
    #[must_use]
    pub fn kinds() -> Vec<&'static str> {
        Self::KINDS
            .iter()
            .chain(TransportError::KINDS)
            .copied()
            .collect()
    }

    /// Whether this failure ended the connection that raised it.
    ///
    /// A caller holding a non-terminal failure still has a working agent and may
    /// make another call; one holding a terminal failure has a closed connection
    /// and must relaunch the program to continue. The distinction is #150's, and
    /// this is what it reads.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        match self {
            // Harkness closed the connection itself on each of these: an agent
            // speaking another version, answering with something that is not an
            // ACP response, or calling a method before the handshake finished is
            // not a peer to keep talking to.
            Self::UnsupportedProtocolVersion { .. }
            | Self::MalformedResponse { .. }
            | Self::ProtocolViolation { .. }
            | Self::ConnectionClosed { .. } => true,
            // The agent declined a call, or Harkness declined to make one. Both
            // leave a connection that works.
            Self::AgentRejectedRequest { .. }
            | Self::MethodNotSupported { .. }
            | Self::AuthenticationRequired { .. }
            | Self::AuthenticationFailed { .. }
            | Self::AuthMethodNotAdvertised { .. }
            | Self::NotInitialized { .. }
            | Self::AlreadyInitialized
            | Self::UnencodableRequest { .. } => false,
            Self::Transport { source } => source.is_terminal(),
        }
    }

    /// The transport failure beneath this one, when there is one.
    #[must_use]
    pub fn transport(&self) -> Option<&TransportError> {
        match self {
            Self::Transport { source } => Some(source),
            _ => None,
        }
    }
}

impl From<TransportError> for AcpError {
    fn from(source: TransportError) -> Self {
        Self::Transport { source }
    }
}

/// The versions Harkness speaks, for a message that names both sides.
fn supported_versions() -> String {
    let mut rendered = String::new();
    for (position, version) in SUPPORTED_PROTOCOL_VERSIONS.iter().enumerate() {
        if position > 0 {
            rendered.push_str(", ");
        }
        let _ = write!(rendered, "{version}");
    }
    rendered
}

/// The methods an agent offered, for a message that names what was available.
fn advertised_methods(advertised: &[AuthMethodId]) -> String {
    if advertised.is_empty() {
        return "none".to_owned();
    }
    advertised
        .iter()
        .map(|method| format!("'{method}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use harkness_transport::{DisconnectKind, TransportError};
    use serde_json::json;

    use super::{AcpError, AgentRefusal, AuthMethodId};

    /// Every variant, in declaration order, so a new one that forgets its kind
    /// fails here rather than reaching a caller as an unnamed failure.
    fn every_variant() -> Vec<(AcpError, &'static str)> {
        vec![
            (
                AcpError::UnsupportedProtocolVersion { agent_selected: 2 },
                "unsupported_protocol_version",
            ),
            (
                AcpError::MalformedResponse {
                    method: "initialize",
                    detail: "fixture".to_owned(),
                },
                "malformed_response",
            ),
            (
                AcpError::ProtocolViolation {
                    during: "initialize",
                    detail: "fixture".to_owned(),
                },
                "protocol_violation",
            ),
            (
                AcpError::AgentRejectedRequest {
                    method: "initialize",
                    refusal: Box::new(AgentRefusal {
                        code: -32603,
                        message: "fixture".to_owned(),
                        data: Some(json!({"detail": "fixture"})),
                    }),
                },
                "agent_rejected_request",
            ),
            (
                AcpError::MethodNotSupported {
                    method: "initialize",
                },
                "method_not_supported",
            ),
            (
                AcpError::AuthenticationRequired {
                    method: "initialize",
                },
                "authentication_required",
            ),
            (
                AcpError::AuthenticationFailed {
                    method_id: AuthMethodId::new("oauth"),
                    refusal: Box::new(AgentRefusal {
                        code: -32000,
                        message: "fixture".to_owned(),
                        data: None,
                    }),
                },
                "authentication_failed",
            ),
            (
                AcpError::AuthMethodNotAdvertised {
                    requested: AuthMethodId::new("oauth"),
                    advertised: vec![AuthMethodId::new("api-key")],
                },
                "auth_method_not_advertised",
            ),
            (
                AcpError::NotInitialized {
                    method: "authenticate",
                },
                "not_initialized",
            ),
            (AcpError::AlreadyInitialized, "already_initialized"),
            (
                AcpError::ConnectionClosed {
                    because: "unsupported_protocol_version",
                },
                "connection_closed",
            ),
            (
                AcpError::UnencodableRequest {
                    method: "initialize",
                    detail: "fixture".to_owned(),
                },
                "unencodable_request",
            ),
        ]
    }

    #[test]
    fn every_error_kind_is_declared_in_order() {
        let cases = every_variant();
        let kinds = cases.iter().map(|(_, kind)| *kind).collect::<Vec<_>>();
        assert_eq!(kinds, AcpError::KINDS);
        for (error, expected) in cases {
            assert_eq!(error.kind(), expected, "unexpected kind for {error:?}");
        }
    }

    /// The published namespace is the concatenation of two tables, so a caller
    /// mapping a kind to an exit code needs them disjoint: one spelling meaning
    /// two things is a mapping that cannot be written.
    #[test]
    fn the_two_kind_tables_do_not_collide() {
        let published = AcpError::kinds();
        assert_eq!(
            published.len(),
            AcpError::KINDS.len() + TransportError::KINDS.len()
        );
        assert_eq!(&published[..AcpError::KINDS.len()], AcpError::KINDS);
        assert_eq!(&published[AcpError::KINDS.len()..], TransportError::KINDS);

        let unique = published.iter().collect::<HashSet<_>>();
        assert_eq!(
            unique.len(),
            published.len(),
            "kinds collide: {published:?}"
        );
    }

    /// A transport failure keeps the discriminant #147 gave it, all the way up.
    #[test]
    fn a_transport_failure_keeps_its_own_kind() {
        for source in [
            TransportError::Cancelled,
            TransportError::SendTimedOut,
            TransportError::Disconnected {
                kind: DisconnectKind::ExitBeforeResponse,
            },
        ] {
            let expected = source.kind();
            let terminal = source.is_terminal();
            let error = AcpError::from(source);
            assert_eq!(error.kind(), expected);
            assert_eq!(error.is_terminal(), terminal);
            assert!(AcpError::kinds().contains(&expected));
            assert!(error.transport().is_some());
        }
    }

    /// A refusal names both versions, because a user reading it has to know
    /// whether to upgrade Harkness or downgrade the agent.
    #[test]
    fn a_version_mismatch_names_both_sides() {
        let message = AcpError::UnsupportedProtocolVersion { agent_selected: 2 }.to_string();
        assert!(message.contains('2'), "{message}");
        assert!(message.contains('1'), "{message}");
    }

    /// An agent offering nothing is the ordinary case for the refusal, so the
    /// message has to read as a sentence rather than trail off into an empty
    /// list.
    #[test]
    fn an_unadvertised_method_names_what_was_available() {
        let none = AcpError::AuthMethodNotAdvertised {
            requested: AuthMethodId::new("oauth"),
            advertised: Vec::new(),
        }
        .to_string();
        assert!(none.contains("it offers none"), "{none}");

        let some = AcpError::AuthMethodNotAdvertised {
            requested: AuthMethodId::new("oauth"),
            advertised: vec![AuthMethodId::new("api-key"), AuthMethodId::new("device")],
        }
        .to_string();
        assert!(some.contains("'api-key', 'device'"), "{some}");
    }

    /// Whether the connection survived is the question #150 asks to decide
    /// between re-prompting a user and relaunching a program, so every variant
    /// has to answer it deliberately.
    #[test]
    fn only_failures_that_closed_the_connection_are_terminal() {
        for (error, kind) in every_variant() {
            let expected = matches!(
                kind,
                "unsupported_protocol_version"
                    | "malformed_response"
                    | "protocol_violation"
                    | "connection_closed"
            );
            assert_eq!(
                error.is_terminal(),
                expected,
                "unexpected verdict for {kind}"
            );
        }
    }
}
