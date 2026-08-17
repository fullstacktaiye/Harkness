//! The negotiated conversation: `initialize`, version negotiation, and
//! `authenticate`.
//!
//! An [`AcpConnection`] wraps one [`Connection`] and adds the only two things
//! ACP puts in front of every later feature — agreeing on a protocol version and
//! learning what the agent can do. Sessions, prompt turns, streaming updates,
//! permission requests, and filesystem mediation are #151, #152, and #153, and
//! each of them is a method on this type rather than a second connection type.
//!
//! # Why this crate does not launch anything
//!
//! [`AcpConnection::new`] takes a connection that already exists. The adapter
//! never spawns an agent on its own initiative, because deciding *which*
//! executable may run is a trust decision bound to an executable digest, and
//! that decision belongs to #150 under ADR-0016. A crate that could launch a
//! program would be a second route around it.

use std::{
    mem,
    time::{Duration, Instant},
};

use harkness_transport::{
    Connection, DEFAULT_STARTUP_DEADLINE, PeerError, PeerMessage, ShutdownOutcome, ShutdownRung,
    TransportError,
};

use crate::{
    SUPPORTED_PROTOCOL_VERSIONS,
    capabilities::{
        AcpAgentCapabilities, AdvertisedClientCapabilities, AgentDescription, AuthMethodId,
        ClientIdentity, agent_capabilities, agent_description,
    },
    error::{AcpError, AgentRefusal, quoted},
    wire,
};

/// How long a caller may wait for one agent answer, by exchange.
///
/// Every wait is bounded and none is unbounded, which is the whole of the
/// policy: an agent that never answers `initialize` must not become an
/// application that never starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AcpTimeouts {
    /// How long the agent has to answer `initialize`.
    ///
    /// Defaults to the transport's own startup deadline, because the two bound
    /// the same window from different sides — #147 gives up on a peer that has
    /// not finished its handshake, and this gives up on the request that *is*
    /// the handshake. Two different numbers would mean one of them never fires.
    pub initialize: Duration,
    /// How long the agent has to answer `authenticate`.
    ///
    /// Much longer than the handshake on purpose: an agent that authenticates
    /// by opening a browser is waiting for a human, and a deadline sized for a
    /// process answering a question would end every one of those.
    pub authenticate: Duration,
    /// How long teardown waits at each rung before escalating.
    pub shutdown_grace: Duration,
}

impl Default for AcpTimeouts {
    fn default() -> Self {
        Self {
            initialize: DEFAULT_STARTUP_DEADLINE,
            authenticate: Duration::from_secs(300),
            shutdown_grace: Duration::from_secs(5),
        }
    }
}

/// What one `initialize` established.
///
/// Returned rather than kept as adapter state alone: #150 persists the
/// negotiated version and the capability snapshot as part of the agent's
/// identity, so an agent that starts selecting a different version or
/// advertising a different capability set is drift a trust grant was not given
/// for rather than a change nobody noticed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitializeOutcome {
    /// The version both sides agreed on. Always `1` today, by ADR-0014.
    pub protocol_version: u16,
    /// Everything the agent said it can do.
    pub capabilities: AcpAgentCapabilities,
    /// What the agent calls itself, when it said.
    pub agent_info: Option<AgentDescription>,
    /// How long the exchange took, for the health record #150 writes.
    pub elapsed: Duration,
}

/// What one `authenticate` established.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthenticateOutcome {
    /// The method that was accepted.
    pub method: AuthMethodId,
    /// How long the exchange took, for the health record #150 writes.
    pub elapsed: Duration,
}

/// An ACP conversation, before and after it has been negotiated.
pub struct AcpConnection {
    link: Link,
    /// The capability snapshot, present exactly once `initialize` has succeeded.
    /// One field rather than a flag beside a snapshot, so "negotiated" and "what
    /// was negotiated" cannot disagree.
    negotiated: Option<AcpAgentCapabilities>,
    timeouts: AcpTimeouts,
}

/// A conversation, or the teardown that ended it.
enum Link {
    /// Usable.
    Open(Connection),
    /// Torn down, and by what.
    Closed {
        /// [`AcpError::kind`] of the failure that closed it.
        because: &'static str,
        /// What that teardown reported.
        outcome: ShutdownOutcome,
    },
}

/// What a closing [`Link`] carries for the instant between the connection being
/// moved out of the adapter and its teardown returning.
///
/// Never observed. `close` holds `&mut self` across both steps and nothing it
/// calls can reach back into the adapter, so no caller can see this value; it
/// exists because [`Connection::shutdown`] consumes the connection and the
/// connection has to leave `self` before it can be consumed.
const PENDING_TEARDOWN: ShutdownOutcome = ShutdownOutcome {
    rung: ShutdownRung::AlreadyExited,
    exit_code: None,
    stderr_bytes: 0,
};

/// How long a wait becomes when a caller's timeout cannot be added to a clock.
///
/// `Instant + Duration` panics on overflow, and every field of [`AcpTimeouts`]
/// is public, so `Duration::MAX` is a spelling a caller reaching for "wait
/// indefinitely" will actually write. A year is not indefinite and is not meant
/// to be: it is far past any deadline anybody means, and it keeps the invariant
/// that every wait is bounded.
const EFFECTIVELY_UNBOUNDED: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// Turns one caller-supplied timeout into a deadline that cannot panic.
fn deadline_from(started: Instant, timeout: Duration) -> Instant {
    started
        .checked_add(timeout)
        .unwrap_or_else(|| started + EFFECTIVELY_UNBOUNDED)
}

impl AcpConnection {
    /// Speaks ACP over an existing connection, with the default timeouts.
    #[must_use]
    pub fn new(connection: Connection) -> Self {
        Self::with_timeouts(connection, AcpTimeouts::default())
    }

    /// Speaks ACP over an existing connection, with timeouts of the caller's
    /// choosing.
    #[must_use]
    pub fn with_timeouts(connection: Connection, timeouts: AcpTimeouts) -> Self {
        Self {
            link: Link::Open(connection),
            negotiated: None,
            timeouts,
        }
    }

    /// Negotiates the protocol version and learns what the agent can do.
    ///
    /// Sends the latest version Harkness supports, the three client capabilities
    /// exactly as `capabilities` sets them, and `clientInfo`. On any answer other
    /// than a version Harkness speaks, the connection is closed and no further
    /// request is sent — ADR-0014 makes that a reported outcome rather than a
    /// retry, because a version mismatch is permanent until software changes.
    ///
    /// # Errors
    ///
    /// [`AcpError::UnsupportedProtocolVersion`] when the agent selected a
    /// version Harkness does not speak, [`AcpError::MalformedResponse`] when the
    /// answer is not an `initialize` response, [`AcpError::ProtocolViolation`]
    /// when the agent called a method before the handshake finished,
    /// [`AcpError::AlreadyInitialized`] on a second call, and the connection's
    /// own failure — a disconnect, a deadline, a cancellation — through
    /// [`AcpError::Transport`].
    pub fn initialize(
        &mut self,
        client: &ClientIdentity,
        capabilities: &AdvertisedClientCapabilities,
    ) -> Result<InitializeOutcome, AcpError> {
        let started = Instant::now();
        self.ensure_open()?;
        if self.negotiated.is_some() {
            return Err(AcpError::AlreadyInitialized);
        }

        let request = wire::initialize_request(client, capabilities);
        let params = wire::encode(wire::INITIALIZE, &request)?;
        let deadline = deadline_from(started, self.timeouts.initialize);

        let answered = {
            let connection = self.open()?;
            connection.request(wire::INITIALIZE, Some(params), deadline)
        };
        let body = match self.collect(wire::INITIALIZE, answered, peer_error) {
            Ok(body) => body,
            Err(error) => return Err(self.refuse_handshake_stall(wire::INITIALIZE, error)),
        };

        let response = match wire::decode::<wire::InitializeResponse>(wire::INITIALIZE, body) {
            Ok(response) => response,
            Err(error) => {
                self.close(error.kind());
                return Err(error);
            }
        };

        // The version question is answered before anything else is read, so a
        // refusal is decided by the one field the negotiation is about and never
        // by a capability shape a version Harkness does not speak was free to
        // change.
        let selected = response.protocol_version.as_u16();
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&selected) {
            let error = AcpError::UnsupportedProtocolVersion {
                agent_selected: selected,
            };
            self.close(error.kind());
            return Err(error);
        }

        self.refuse_early_peer_traffic(wire::INITIALIZE)?;

        let capabilities = agent_capabilities(&response);
        let agent_info = agent_description(&response);

        // The startup window closes here and not before. #147 cannot recognize
        // a handshake — `initialize` is a method name it has no opinion about —
        // so the adapter is what says the peer has proven it speaks ACP. An
        // authentication that follows is a separate call under its own, much
        // longer, deadline.
        self.open()?.handshake_complete();
        self.negotiated = Some(capabilities.clone());

        Ok(InitializeOutcome {
            protocol_version: selected,
            capabilities,
            agent_info,
            elapsed: started.elapsed(),
        })
    }

    /// Authenticates with one of the methods the agent advertised.
    ///
    /// The advertisement is checked *before* anything is written: an agent that
    /// offered no method wants no authentication, and asking it anyway is a
    /// request Harkness should not have made rather than a question for the
    /// peer. No credential material passes through this crate — v1's one method
    /// shape has the agent handle authentication itself, and Harkness only names
    /// which of the offered ways to use.
    ///
    /// # Errors
    ///
    /// [`AcpError::AuthMethodNotAdvertised`] when the agent did not offer
    /// `method`, [`AcpError::NotInitialized`] before a handshake,
    /// [`AcpError::AuthenticationFailed`] when the agent rejected the attempt,
    /// and the connection's own failure through [`AcpError::Transport`].
    pub fn authenticate(&mut self, method: &AuthMethodId) -> Result<AuthenticateOutcome, AcpError> {
        let started = Instant::now();
        self.ensure_open()?;
        let Some(negotiated) = self.negotiated.as_ref() else {
            return Err(AcpError::NotInitialized {
                method: wire::AUTHENTICATE,
            });
        };
        if !negotiated.advertises(method) {
            return Err(AcpError::AuthMethodNotAdvertised {
                requested: method.clone(),
                advertised: negotiated.auth_method_ids(),
            });
        }

        let request = wire::AuthenticateRequest::new(method.as_str().to_owned());
        let params = wire::encode(wire::AUTHENTICATE, &request)?;
        let deadline = deadline_from(started, self.timeouts.authenticate);

        let answered = {
            let connection = self.open()?;
            connection.request(wire::AUTHENTICATE, Some(params), deadline)
        };
        let attempted = method.clone();
        // The result is deliberately not decoded, and this is the one place that
        // is true. An `authenticate` response carries nothing but `_meta`, which
        // is ignored, so every field is optional and decoding could only refuse a
        // result that is not a JSON object — and `null` is what a great many
        // JSON-RPC peers write for a void result. Closing a working connection
        // over that spelling would be Harkness inventing a conformance rule the
        // specification does not have. An `initialize` response is different and
        // is decoded, because `protocolVersion` is a field the negotiation needs.
        let _ignored = self.collect(wire::AUTHENTICATE, answered, move |method, refusal| {
            match refusal.code {
                // An agent that advertised this method and then does not
                // implement the call is not refusing a credential — it is not
                // serving `authenticate` at all, and telling a caller its
                // credentials were rejected sends it to re-prompt a person over
                // a conformance bug no answer of theirs can fix.
                wire::METHOD_NOT_FOUND_CODE => AcpError::MethodNotSupported { method },
                _ => AcpError::AuthenticationFailed {
                    method_id: attempted,
                    refusal: Box::new(refused(refusal)),
                },
            }
        })?;

        Ok(AuthenticateOutcome {
            method: method.clone(),
            elapsed: started.elapsed(),
        })
    }

    /// What the agent said it can do, once `initialize` has succeeded.
    #[must_use]
    pub fn capabilities(&self) -> Option<&AcpAgentCapabilities> {
        self.negotiated.as_ref()
    }

    /// Whether the conversation has ended.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        matches!(self.link, Link::Closed { .. })
    }

    /// [`AcpError::kind`] of the failure that closed the connection, if one did.
    #[must_use]
    pub fn closed_because(&self) -> Option<&'static str> {
        match &self.link {
            Link::Open(_) => None,
            Link::Closed { because, .. } => Some(because),
        }
    }

    /// Tears the connection down and reports how far the teardown had to go.
    ///
    /// A connection an earlier failure already closed reports that teardown's
    /// outcome rather than running a second one, so "this agent had to be
    /// killed" survives however the conversation ended.
    #[must_use]
    pub fn shutdown(self) -> ShutdownOutcome {
        match self.link {
            Link::Open(connection) => connection.shutdown(self.timeouts.shutdown_grace),
            Link::Closed { outcome, .. } => outcome,
        }
    }

    /// The open connection, or why there is not one.
    fn open(&self) -> Result<&Connection, AcpError> {
        match &self.link {
            Link::Open(connection) => Ok(connection),
            Link::Closed { because, .. } => Err(AcpError::ConnectionClosed { because }),
        }
    }

    /// Refuses before anything else when the conversation has already ended.
    ///
    /// Every entry point asks this first, so a closed connection is reported as
    /// closed rather than as whatever else is also true of it — that a handshake
    /// is missing, or that one already happened. Both of those are accurate and
    /// neither is the answer a caller can act on: only this one says the agent
    /// is gone.
    fn ensure_open(&self) -> Result<(), AcpError> {
        self.open().map(|_| ())
    }

    /// Reduces one transport answer to a response body.
    ///
    /// The peer-error mapping is the caller's, because `authenticate` reports a
    /// rejection as an authentication failure while every other method reports
    /// one as a refusal — a caller has to tell "your credentials were refused"
    /// from "that call was declined" to decide between re-prompting a human and
    /// giving up.
    fn collect(
        &mut self,
        method: &'static str,
        answered: Result<Result<serde_json::Value, PeerError>, TransportError>,
        on_refusal: impl FnOnce(&'static str, PeerError) -> AcpError,
    ) -> Result<serde_json::Value, AcpError> {
        match answered {
            Ok(Ok(body)) => Ok(body),
            // The agent is running and answered; it declined the call. Nothing
            // about the connection changed, so nothing is closed.
            Ok(Err(refusal)) => Err(on_refusal(method, refusal)),
            Err(transport) => {
                let error = AcpError::from(transport);
                if error.is_terminal() {
                    self.close(error.kind());
                }
                Err(error)
            }
        }
    }

    /// Re-reads a handshake failure the connection called recoverable.
    ///
    /// `peer_queue_full` is not terminal in general, and correctly so: the
    /// connection resumes the moment somebody drains it. Nobody can drain it
    /// here. ACP gives an agent nothing to send before `initialize` returns, so
    /// a queue that filled holds `capacity` messages that should not exist, and
    /// this crate offers no way to read them because there is nothing legitimate
    /// in there to read. Left as the transport reported it, the failure would
    /// tell every caller the agent is fine while no retry could ever get past
    /// the same full queue and the child process was never torn down.
    ///
    /// It is reported as the protocol violation it is — one message arriving
    /// during the handshake is already a violation, and this is `capacity` of
    /// them — rather than as a new kind, so a caller has one thing to recognize.
    fn refuse_handshake_stall(&mut self, during: &'static str, error: AcpError) -> AcpError {
        let Some(TransportError::PeerQueueFull { capacity }) = error.transport() else {
            return error;
        };
        let violation = AcpError::ProtocolViolation {
            during,
            detail: format!("{capacity} messages before answering"),
        };
        self.close(violation.kind());
        violation
    }

    /// Refuses an agent that called a method before the handshake finished.
    ///
    /// ACP has nothing an agent may send before `initialize` returns: there is
    /// no session to update, no file to read, and no terminal to create. The
    /// check is exact rather than a heuristic because the transport delivers one
    /// ordered stream through one pump — everything the agent wrote before its
    /// response was routed before its response was — so a queue that is empty
    /// here is proof the agent sent nothing, and anything in it arrived early.
    fn refuse_early_peer_traffic(&mut self, during: &'static str) -> Result<(), AcpError> {
        let observed = {
            let connection = self.open()?;
            connection.next_peer_message(Instant::now())
        };
        match observed {
            Ok(None) => Ok(()),
            Ok(Some(message)) => {
                let error = AcpError::ProtocolViolation {
                    during,
                    detail: describe(&message),
                };
                self.close(error.kind());
                Err(error)
            }
            Err(transport) => {
                let error = AcpError::from(transport);
                if error.is_terminal() {
                    self.close(error.kind());
                }
                Err(error)
            }
        }
    }

    /// Ends the conversation, keeping the reason the first failure gave.
    fn close(&mut self, because: &'static str) {
        let grace = self.timeouts.shutdown_grace;
        let previous = mem::replace(
            &mut self.link,
            Link::Closed {
                because,
                outcome: PENDING_TEARDOWN,
            },
        );
        self.link = match previous {
            Link::Open(connection) => Link::Closed {
                because,
                outcome: connection.shutdown(grace),
            },
            // Already closed. The first failure keeps the reason, exactly as the
            // transport gives a fault to the thread that observed it and every
            // later caller the quarantine naming it.
            settled => settled,
        };
    }
}

/// Maps one JSON-RPC error object into this crate's vocabulary.
///
/// Two codes get variants of their own because they lead somewhere different
/// from an ordinary refusal: `-32601` for a method ACP requires means the
/// program is not an ACP agent, and `-32000` means the call would succeed after
/// authentication. Every other code is carried whole — code, message, and data —
/// because this crate has no vocabulary for what an agent's own error numbers
/// mean and inventing one would be guessing on a caller's behalf.
fn peer_error(method: &'static str, error: PeerError) -> AcpError {
    match error.code {
        wire::AUTH_REQUIRED_CODE => AcpError::AuthenticationRequired { method },
        wire::METHOD_NOT_FOUND_CODE => AcpError::MethodNotSupported { method },
        _ => AcpError::AgentRejectedRequest {
            method,
            refusal: Box::new(refused(error)),
        },
    }
}

/// Carries one JSON-RPC error object across the transport boundary unchanged.
fn refused(error: PeerError) -> AgentRefusal {
    AgentRefusal {
        code: error.code,
        message: error.message,
        data: error.data,
    }
}

/// Names one peer-initiated message by its method, bounded.
fn describe(message: &PeerMessage) -> String {
    let (shape, method) = match message {
        PeerMessage::Request(request) => ("request", request.method.as_str()),
        PeerMessage::Notification(notification) => ("notification", notification.method.as_str()),
    };
    format!("a '{}' {shape}", quoted(method))
}

#[cfg(test)]
mod tests {
    use std::{thread, time::Duration};

    use harkness_transport::{
        Cancellation, DesyncDetail, DisconnectKind, Message, PeerError, RequestId, ShutdownRung,
        TransportError,
    };
    use serde_json::{Value, json};

    use super::{AcpConnection, AcpTimeouts, InitializeOutcome};
    use crate::{
        AcpAgentCapabilities, AcpError, AdvertisedClientCapabilities, AgentDescription,
        AgentRefusal, AuthCapabilities, AuthMethod, AuthMethodId, ClientIdentity, McpCapabilities,
        PromptCapabilities, SessionCapabilities,
        error::{MAX_QUOTED_AGENT_BYTES, quoted},
        testing::{Recorded, ScriptedAgent, Step, scripted},
    };

    /// One case of the drop-a-capability sweep: a JSON Pointer into the frozen
    /// response, and the snapshot field its absence must clear.
    type CapabilityCase = (&'static str, Box<dyn Fn(&mut AcpAgentCapabilities)>);

    /// The full `initialize` request Harkness produces, frozen.
    const REQUEST_FIXTURE: &str = include_str!("fixtures/initialize-request-v1.json");
    /// The request produced when #153 permits nothing, frozen.
    const MINIMAL_REQUEST_FIXTURE: &str =
        include_str!("fixtures/initialize-request-minimal-v1.json");
    /// An agent advertising everything v1 has, frozen.
    const RESPONSE_FIXTURE: &str = include_str!("fixtures/initialize-response-v1.json");
    /// The smallest conformant v1 response, frozen and hand-maintained: this
    /// build cannot produce it, because serializing a response always writes the
    /// defaults an agent is free to leave out.
    const MINIMAL_RESPONSE_FIXTURE: &str =
        include_str!("fixtures/initialize-response-minimal-v1.json");
    /// The ADR-0014 boundary on the wire, frozen and hand-maintained for the
    /// same reason: Harkness never produces a v2 response.
    const V2_RESPONSE_FIXTURE: &str = include_str!("fixtures/initialize-response-v2-v1.json");

    fn harkness() -> ClientIdentity {
        ClientIdentity::new("harkness", "0.1.0").title("Harkness")
    }

    fn everything() -> AdvertisedClientCapabilities {
        AdvertisedClientCapabilities {
            fs_read_text_file: true,
            fs_write_text_file: true,
            terminal: true,
        }
    }

    fn fixture(text: &str) -> Value {
        serde_json::from_str(text).expect("a committed fixture parses")
    }

    /// The agent every negotiation test that is not about negotiation uses.
    fn agreeing() -> Vec<Step> {
        vec![Step::Reply(fixture(RESPONSE_FIXTURE))]
    }

    fn negotiated() -> (AcpConnection, Recorded) {
        let (mut agent, recorded) = scripted(agreeing());
        agent
            .initialize(&harkness(), &everything())
            .expect("the fixture agent agrees on version 1");
        (agent, recorded)
    }

    // -- What Harkness sends -------------------------------------------------

    /// The request carries the offered version, the client's name, and exactly
    /// the three flags #153 handed the adapter. The comparison is byte for byte
    /// against a committed fixture because every one of those is something an
    /// agent reads and acts on.
    ///
    /// Key order is part of the comparison and is stable rather than incidental:
    /// `agent-client-protocol-schema` requires `serde_json/preserve_order`, so a
    /// `Value` here keeps the order the wire types declare their fields in. An
    /// upstream release reordering one would change the bytes an agent in the
    /// field receives, which is a thing to notice rather than to normalize away.
    #[test]
    fn the_initialize_request_is_the_frozen_wire_form() {
        let (mut agent, recorded) = scripted(agreeing());
        agent.initialize(&harkness(), &everything()).unwrap();

        let sent = recorded
            .request_params("initialize")
            .expect("initialize was sent")
            .expect("initialize carries params");
        assert_eq!(
            serde_json::to_string_pretty(&sent).unwrap(),
            REQUEST_FIXTURE.trim_end(),
        );
    }

    /// Advertising nothing is the default and has to stay expressible: the three
    /// capabilities are promises of mediation #153 owns, and a client that
    /// cannot decline them is a client that always promises.
    #[test]
    fn an_advertisement_of_nothing_is_sent_as_nothing() {
        let (mut agent, recorded) = scripted(agreeing());
        agent
            .initialize(
                &ClientIdentity::new("harkness", "0.1.0"),
                &AdvertisedClientCapabilities::default(),
            )
            .unwrap();

        let sent = recorded.request_params("initialize").unwrap().unwrap();
        assert_eq!(
            serde_json::to_string_pretty(&sent).unwrap(),
            MINIMAL_REQUEST_FIXTURE.trim_end(),
        );
        // The absent title is absent rather than empty: a fallback is the
        // agent's to choose, and an empty string is a name.
        assert!(!MINIMAL_REQUEST_FIXTURE.contains("title"));
    }

    /// Each flag reaches its own field. A test that only checked the all-on and
    /// all-off shapes would pass with two of them wired to each other.
    #[test]
    fn each_advertised_capability_reaches_its_own_field() {
        for (advertised, expected) in [
            (
                AdvertisedClientCapabilities {
                    fs_read_text_file: true,
                    ..AdvertisedClientCapabilities::default()
                },
                json!({"fs": {"readTextFile": true, "writeTextFile": false}, "terminal": false}),
            ),
            (
                AdvertisedClientCapabilities {
                    fs_write_text_file: true,
                    ..AdvertisedClientCapabilities::default()
                },
                json!({"fs": {"readTextFile": false, "writeTextFile": true}, "terminal": false}),
            ),
            (
                AdvertisedClientCapabilities {
                    terminal: true,
                    ..AdvertisedClientCapabilities::default()
                },
                json!({"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": true}),
            ),
        ] {
            let (mut agent, recorded) = scripted(agreeing());
            agent.initialize(&harkness(), &advertised).unwrap();
            let sent = recorded.request_params("initialize").unwrap().unwrap();
            assert_eq!(sent["clientCapabilities"], expected);
        }
    }

    // -- Negotiation ---------------------------------------------------------

    /// Version 1 proceeds, and the snapshot is the response's own answer rather
    /// than a reading of it.
    #[test]
    fn an_agreed_version_yields_the_agents_capability_snapshot() {
        let (mut agent, recorded) = scripted(agreeing());
        let outcome = agent.initialize(&harkness(), &everything()).unwrap();

        assert_eq!(outcome.protocol_version, 1);
        assert_eq!(
            outcome.capabilities,
            AcpAgentCapabilities {
                load_session: true,
                prompt: PromptCapabilities {
                    image: true,
                    audio: true,
                    embedded_context: true,
                },
                mcp: McpCapabilities {
                    http: true,
                    sse: true,
                },
                session: SessionCapabilities {
                    list: true,
                    delete: true,
                    additional_directories: true,
                    resume: true,
                    close: true,
                },
                auth: AuthCapabilities { logout: true },
                auth_methods: vec![
                    AuthMethod {
                        id: AuthMethodId::new("oauth"),
                        name: "Sign in with the browser".to_owned(),
                        description: Some("Opens a browser window".to_owned()),
                    },
                    AuthMethod {
                        id: AuthMethodId::new("api-key"),
                        name: "API key".to_owned(),
                        description: None,
                    },
                ],
            }
        );
        assert_eq!(
            outcome.agent_info,
            Some(AgentDescription {
                name: "example-agent".to_owned(),
                title: Some("Example Agent".to_owned()),
                version: "3.2.1".to_owned(),
            })
        );
        assert_eq!(agent.capabilities(), Some(&outcome.capabilities));
        assert!(!agent.is_closed());
        // The startup window closes once the peer has proven it speaks ACP, and
        // not before: #147 cannot recognize a handshake on its own.
        assert_eq!(recorded.handshakes(), 1);
    }

    /// ADR-0014's boundary. The version is recorded verbatim, the connection is
    /// closed, and nothing else is asked of the agent — a mismatch is permanent
    /// until software changes, so a retry is a way to spawn a program twice for
    /// no reason.
    #[test]
    fn an_unsupported_version_closes_the_connection_and_asks_nothing_more() {
        let (mut agent, recorded) = scripted(vec![Step::Reply(fixture(V2_RESPONSE_FIXTURE))]);
        let error = agent.initialize(&harkness(), &everything()).unwrap_err();

        assert_eq!(error.kind(), "unsupported_protocol_version");
        assert!(matches!(
            error,
            AcpError::UnsupportedProtocolVersion { agent_selected: 2 }
        ));
        assert!(error.is_terminal());
        assert!(agent.is_closed());
        assert_eq!(agent.closed_because(), Some("unsupported_protocol_version"));
        assert_eq!(recorded.request_count(), 1);
        assert_eq!(recorded.handshakes(), 0);
        assert_eq!(recorded.shutdowns(), 1);

        // Every later call reports the close rather than reaching for a
        // connection that is gone — including a second handshake, which is also
        // `already_initialized` on a connection that negotiated and is neither
        // the actionable answer nor true here.
        let refused = agent.authenticate(&AuthMethodId::new("oauth")).unwrap_err();
        assert_eq!(refused.kind(), "connection_closed");
        let again = agent.initialize(&harkness(), &everything()).unwrap_err();
        assert_eq!(again.kind(), "connection_closed");
        assert_eq!(recorded.request_count(), 1);
    }

    /// A connection that negotiated and *then* failed reports the close too. Its
    /// handshake did happen, so `already_initialized` is accurate — and it is
    /// the wrong thing to say, because it describes a connection a caller could
    /// still use.
    #[test]
    fn a_call_after_a_late_failure_reports_the_close_and_not_the_handshake() {
        let (mut agent, _recorded) = scripted(vec![
            Step::Reply(fixture(RESPONSE_FIXTURE)),
            Step::Fault(TransportError::Disconnected {
                kind: DisconnectKind::Idle,
            }),
        ]);
        agent.initialize(&harkness(), &everything()).unwrap();
        agent.authenticate(&AuthMethodId::new("oauth")).unwrap_err();

        assert!(agent.is_closed());
        assert_eq!(
            agent
                .initialize(&harkness(), &everything())
                .unwrap_err()
                .kind(),
            "connection_closed",
        );
    }

    /// The version question is decided before any capability is read, so an
    /// agent speaking a version Harkness does not know is refused for the
    /// version rather than for a capability shape that version was free to
    /// change.
    #[test]
    fn a_future_version_is_refused_before_its_capabilities_are_interpreted() {
        let mut response = fixture(V2_RESPONSE_FIXTURE);
        response["agentCapabilities"] = json!("a shape this build has never seen");
        let (mut agent, _recorded) = scripted(vec![Step::Reply(response)]);

        assert_eq!(
            agent
                .initialize(&harkness(), &everything())
                .unwrap_err()
                .kind(),
            "unsupported_protocol_version",
        );
    }

    /// One handshake per connection. A second would re-negotiate a version and a
    /// capability set that sessions on this connection were created against.
    #[test]
    fn a_second_handshake_is_refused_without_reaching_the_agent() {
        let (mut agent, recorded) = negotiated();
        let error = agent.initialize(&harkness(), &everything()).unwrap_err();

        assert_eq!(error.kind(), "already_initialized");
        assert!(!error.is_terminal());
        assert_eq!(recorded.request_count(), 1);
    }

    // -- Omitted means unsupported -------------------------------------------

    /// The smallest conformant response. Every capability the agent left out
    /// reads as unsupported, which is what ACP requires and what stops Harkness
    /// calling `session/load` against an agent that never claimed it.
    #[test]
    fn every_omitted_capability_decodes_as_unsupported() {
        let (mut agent, _recorded) = scripted(vec![Step::Reply(fixture(MINIMAL_RESPONSE_FIXTURE))]);
        let outcome = agent.initialize(&harkness(), &everything()).unwrap();

        assert_eq!(outcome.protocol_version, 1);
        assert_eq!(outcome.capabilities, AcpAgentCapabilities::default());
        assert!(!outcome.capabilities.load_session);
        assert!(outcome.capabilities.auth_methods.is_empty());
        assert_eq!(outcome.agent_info, None);
    }

    /// Each capability is decoded from its own field. Every one is dropped from
    /// the full response in turn, so a mapping that read two of them from one
    /// place fails here rather than silently reporting a feature the agent never
    /// advertised.
    #[test]
    fn dropping_one_capability_changes_exactly_that_capability() {
        let full = fixture(RESPONSE_FIXTURE);
        let baseline = {
            let (mut agent, _recorded) = scripted(vec![Step::Reply(full.clone())]);
            agent
                .initialize(&harkness(), &everything())
                .unwrap()
                .capabilities
        };

        let cases: Vec<CapabilityCase> = vec![
            (
                "/agentCapabilities/loadSession",
                Box::new(|snapshot: &mut AcpAgentCapabilities| snapshot.load_session = false),
            ),
            (
                "/agentCapabilities/promptCapabilities/image",
                Box::new(|snapshot: &mut AcpAgentCapabilities| snapshot.prompt.image = false),
            ),
            (
                "/agentCapabilities/promptCapabilities/audio",
                Box::new(|snapshot: &mut AcpAgentCapabilities| snapshot.prompt.audio = false),
            ),
            (
                "/agentCapabilities/promptCapabilities/embeddedContext",
                Box::new(|snapshot: &mut AcpAgentCapabilities| {
                    snapshot.prompt.embedded_context = false;
                }),
            ),
            (
                "/agentCapabilities/mcpCapabilities/http",
                Box::new(|snapshot: &mut AcpAgentCapabilities| snapshot.mcp.http = false),
            ),
            (
                "/agentCapabilities/mcpCapabilities/sse",
                Box::new(|snapshot: &mut AcpAgentCapabilities| snapshot.mcp.sse = false),
            ),
            (
                "/agentCapabilities/sessionCapabilities/list",
                Box::new(|snapshot: &mut AcpAgentCapabilities| snapshot.session.list = false),
            ),
            (
                "/agentCapabilities/sessionCapabilities/delete",
                Box::new(|snapshot: &mut AcpAgentCapabilities| snapshot.session.delete = false),
            ),
            (
                "/agentCapabilities/sessionCapabilities/additionalDirectories",
                Box::new(|snapshot: &mut AcpAgentCapabilities| {
                    snapshot.session.additional_directories = false;
                }),
            ),
            (
                "/agentCapabilities/sessionCapabilities/resume",
                Box::new(|snapshot: &mut AcpAgentCapabilities| snapshot.session.resume = false),
            ),
            (
                "/agentCapabilities/sessionCapabilities/close",
                Box::new(|snapshot: &mut AcpAgentCapabilities| snapshot.session.close = false),
            ),
            (
                "/agentCapabilities/auth/logout",
                Box::new(|snapshot: &mut AcpAgentCapabilities| snapshot.auth.logout = false),
            ),
            (
                "/authMethods",
                Box::new(|snapshot: &mut AcpAgentCapabilities| snapshot.auth_methods.clear()),
            ),
        ];

        for (pointer, unset) in cases {
            let mut response = full.clone();
            remove(&mut response, pointer);

            let (mut agent, _recorded) = scripted(vec![Step::Reply(response)]);
            let observed = agent
                .initialize(&harkness(), &everything())
                .unwrap()
                .capabilities;

            let mut expected = baseline.clone();
            unset(&mut expected);
            assert_eq!(
                observed, expected,
                "dropping {pointer} changed the wrong field"
            );
        }
    }

    /// An optional capability *object* means presence, and nothing inside it is
    /// read. Both spellings of absence — the key missing and the key null — have
    /// to answer the same way, because an agent is free to write either.
    #[test]
    fn a_null_capability_object_is_absence_rather_than_presence() {
        let mut response = fixture(RESPONSE_FIXTURE);
        response["agentCapabilities"]["sessionCapabilities"]["resume"] = Value::Null;
        response["agentCapabilities"]["auth"]["logout"] = Value::Null;

        let (mut agent, _recorded) = scripted(vec![Step::Reply(response)]);
        let capabilities = agent
            .initialize(&harkness(), &everything())
            .unwrap()
            .capabilities;

        assert!(!capabilities.session.resume);
        assert!(!capabilities.auth.logout);
        assert!(capabilities.session.close, "the siblings are unaffected");
    }

    /// Every combination of the four optional top-level fields, present or not,
    /// decodes without panicking and reports exactly what was there. Sixteen
    /// shapes rather than the two a hand-written pair of cases would cover.
    #[test]
    fn any_combination_of_optional_fields_decodes_to_what_was_present() {
        let full = fixture(RESPONSE_FIXTURE);
        let optional = ["agentCapabilities", "authMethods", "agentInfo", "_meta"];

        for present in 0..(1 << optional.len()) {
            let mut response = full.clone();
            for (bit, field) in optional.iter().enumerate() {
                if present & (1 << bit) == 0 {
                    response
                        .as_object_mut()
                        .expect("the fixture is an object")
                        .remove(*field);
                }
            }
            let expect_capabilities = present & 1 != 0;
            let expect_methods = present & 2 != 0;
            let expect_info = present & 4 != 0;

            let (mut agent, _recorded) = scripted(vec![Step::Reply(response)]);
            let outcome = agent
                .initialize(&harkness(), &everything())
                .unwrap_or_else(|error| panic!("shape {present} was refused: {error}"));

            assert_eq!(outcome.capabilities.load_session, expect_capabilities);
            assert_eq!(
                !outcome.capabilities.auth_methods.is_empty(),
                expect_methods
            );
            assert_eq!(outcome.agent_info.is_some(), expect_info);
        }
    }

    /// Upstream decodes a wrong-typed capability to its default, which is the
    /// same answer as omitting it — and that is the correct answer rather than a
    /// leniency to work around. A capability object nobody can read is an agent
    /// with fewer features, not an agent that failed to answer, and refusing the
    /// whole handshake over one would take a working agent away from a user.
    #[test]
    fn a_capability_nobody_can_read_is_unsupported_rather_than_a_refusal() {
        let mut response = fixture(RESPONSE_FIXTURE);
        response["agentCapabilities"]["promptCapabilities"] = json!("yes please");
        response["agentCapabilities"]["loadSession"] = json!(["maybe"]);

        let (mut agent, _recorded) = scripted(vec![Step::Reply(response)]);
        let capabilities = agent
            .initialize(&harkness(), &everything())
            .unwrap()
            .capabilities;

        assert_eq!(capabilities.prompt, PromptCapabilities::default());
        assert!(!capabilities.load_session);
        assert!(capabilities.mcp.http, "the siblings are unaffected");
    }

    /// `protocolVersion` is the one field with no default, because the whole
    /// negotiation is about it. A response without one is not an ACP response.
    #[test]
    fn a_response_without_a_version_is_refused_by_name() {
        for body in [json!({}), json!({"protocolVersion": "one"})] {
            let (mut agent, recorded) = scripted(vec![Step::Reply(body)]);
            let error = agent.initialize(&harkness(), &everything()).unwrap_err();

            assert_eq!(error.kind(), "malformed_response");
            assert!(
                error.to_string().contains("protocolVersion"),
                "the field that was wrong is named: {error}",
            );
            assert!(error.is_terminal());
            assert!(agent.is_closed());
            assert_eq!(recorded.shutdowns(), 1);
        }
    }

    // -- What an agent refuses -----------------------------------------------

    /// A JSON-RPC error object is the agent declining a call over a working
    /// connection, and code, message, and data are carried whole: this crate has
    /// no vocabulary for what an agent's own error numbers mean.
    #[test]
    fn a_json_rpc_error_is_carried_whole_and_leaves_the_connection_open() {
        let (mut agent, recorded) = scripted(vec![Step::Refuse(PeerError {
            code: -32603,
            message: "the model provider is unreachable".to_owned(),
            data: Some(json!({"retryAfter": 30})),
        })]);
        let error = agent.initialize(&harkness(), &everything()).unwrap_err();

        assert_eq!(error.kind(), "agent_rejected_request");
        match &error {
            AcpError::AgentRejectedRequest { method, refusal } => {
                assert_eq!(*method, "initialize");
                assert_eq!(
                    **refusal,
                    AgentRefusal {
                        code: -32603,
                        message: "the model provider is unreachable".to_owned(),
                        data: Some(json!({"retryAfter": 30})),
                    }
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert!(!error.is_terminal());
        assert!(!agent.is_closed());
        assert_eq!(recorded.shutdowns(), 0);
    }

    /// Two codes lead somewhere different from an ordinary refusal, so they get
    /// names of their own: one says the program is not an ACP agent, the other
    /// says the call would succeed after authentication.
    #[test]
    fn the_two_codes_that_change_what_a_caller_does_are_named() {
        for (code, expected) in [
            (-32601_i64, "method_not_supported"),
            (-32000, "authentication_required"),
        ] {
            let (mut agent, _recorded) = scripted(vec![Step::Refuse(PeerError {
                code,
                message: "no".to_owned(),
                data: None,
            })]);
            let error = agent.initialize(&harkness(), &everything()).unwrap_err();
            assert_eq!(error.kind(), expected);
            assert!(!error.is_terminal());
        }
    }

    // -- Authentication ------------------------------------------------------

    /// The gate is the agent's own advertisement, and it is asked before
    /// anything is written: an agent that offered nothing wants no
    /// authentication.
    #[test]
    fn authentication_against_an_agent_that_offered_nothing_writes_nothing() {
        let (mut agent, recorded) = scripted(vec![Step::Reply(fixture(MINIMAL_RESPONSE_FIXTURE))]);
        agent.initialize(&harkness(), &everything()).unwrap();

        let error = agent.authenticate(&AuthMethodId::new("oauth")).unwrap_err();

        assert_eq!(error.kind(), "auth_method_not_advertised");
        assert!(error.to_string().contains("it offers none"), "{error}");
        assert_eq!(recorded.request_count(), 1, "only initialize was sent");
        assert!(!agent.is_closed());
    }

    /// The same gate for a method the agent did not list, which is the case a
    /// caller reaches by remembering a method from another agent.
    #[test]
    fn authentication_with_a_method_nobody_offered_writes_nothing() {
        let (mut agent, recorded) = negotiated();
        let error = agent
            .authenticate(&AuthMethodId::new("kerberos"))
            .unwrap_err();

        assert_eq!(error.kind(), "auth_method_not_advertised");
        match error {
            AcpError::AuthMethodNotAdvertised {
                requested,
                advertised,
            } => {
                assert_eq!(requested, AuthMethodId::new("kerberos"));
                assert_eq!(
                    advertised,
                    vec![AuthMethodId::new("oauth"), AuthMethodId::new("api-key")],
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(recorded.request_count(), 1);
    }

    /// An accepted authentication names the method on the wire exactly as the
    /// agent spelled it, and reports it back: an identifier this crate reworded
    /// on either leg would be one no agent recognizes.
    #[test]
    fn an_accepted_authentication_sends_and_reports_the_method_verbatim() {
        let (mut agent, recorded) = scripted(vec![
            Step::Reply(fixture(RESPONSE_FIXTURE)),
            Step::Reply(json!({})),
        ]);
        agent.initialize(&harkness(), &everything()).unwrap();

        let outcome = agent
            .authenticate(&AuthMethodId::new("api-key"))
            .expect("the agent accepted the method it advertised");

        assert_eq!(
            recorded.request_params("authenticate"),
            Some(Some(json!({"methodId": "api-key"}))),
        );
        assert_eq!(outcome.method, AuthMethodId::new("api-key"));
        assert_eq!(recorded.request_count(), 2);
        assert!(!agent.is_closed());
    }

    /// An `authenticate` response carries nothing Harkness reads, so any answer
    /// at all is an acceptance — `null` included, which is what a great many
    /// JSON-RPC peers write for a void result. Refusing that spelling would be a
    /// conformance rule the specification does not have, enforced against a user
    /// whose agent had just authenticated them.
    #[test]
    fn any_answer_to_authenticate_is_an_acceptance() {
        for result in [json!({}), Value::Null, json!({"_meta": {"a": 1}})] {
            let (mut agent, _recorded) = scripted(vec![
                Step::Reply(fixture(RESPONSE_FIXTURE)),
                Step::Reply(result.clone()),
            ]);
            agent.initialize(&harkness(), &everything()).unwrap();

            agent
                .authenticate(&AuthMethodId::new("oauth"))
                .unwrap_or_else(|error| panic!("{result} was refused: {error}"));
            assert!(!agent.is_closed());
        }
    }

    /// "Your credentials were refused" and "the agent died mid-call" are the
    /// same outcome to a caller that only checks for `Err`, and #150 has to tell
    /// them apart to choose between re-prompting a human and relaunching a
    /// program.
    #[test]
    fn a_rejected_attempt_is_told_apart_from_a_connection_that_failed() {
        let (mut agent, _recorded) = scripted(vec![
            Step::Reply(fixture(RESPONSE_FIXTURE)),
            Step::Refuse(PeerError {
                code: -32000,
                message: "that key has been revoked".to_owned(),
                data: None,
            }),
        ]);
        agent.initialize(&harkness(), &everything()).unwrap();

        let rejected = agent
            .authenticate(&AuthMethodId::new("api-key"))
            .unwrap_err();
        assert_eq!(rejected.kind(), "authentication_failed");
        match &rejected {
            AcpError::AuthenticationFailed { method_id, refusal } => {
                assert_eq!(*method_id, AuthMethodId::new("api-key"));
                assert_eq!(refusal.code, -32000);
                assert_eq!(refusal.message, "that key has been revoked");
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert!(!rejected.is_terminal(), "the agent is still running");

        let (mut agent, _recorded) = scripted(vec![
            Step::Reply(fixture(RESPONSE_FIXTURE)),
            Step::Fault(TransportError::Disconnected {
                kind: DisconnectKind::MidResponse,
            }),
        ]);
        agent.initialize(&harkness(), &everything()).unwrap();
        let died = agent
            .authenticate(&AuthMethodId::new("api-key"))
            .unwrap_err();
        assert_eq!(died.kind(), "disconnected");
        assert!(died.is_terminal());
    }

    /// An agent that advertised a method and then does not implement the call is
    /// not refusing a credential. Reporting that as `authentication_failed`
    /// would send a caller to re-prompt a person over a conformance bug no
    /// answer of theirs can fix.
    #[test]
    fn an_agent_that_does_not_serve_authenticate_is_not_refusing_a_credential() {
        let (mut agent, _recorded) = scripted(vec![
            Step::Reply(fixture(RESPONSE_FIXTURE)),
            Step::Refuse(PeerError {
                code: -32601,
                message: "unknown method".to_owned(),
                data: None,
            }),
        ]);
        agent.initialize(&harkness(), &everything()).unwrap();

        let error = agent.authenticate(&AuthMethodId::new("oauth")).unwrap_err();
        assert_eq!(error.kind(), "method_not_supported");
        assert!(!error.is_terminal());
        assert!(!agent.is_closed());
    }

    /// Authentication before a handshake has no advertisement to check against,
    /// so it is refused rather than guessed at.
    #[test]
    fn authentication_before_the_handshake_is_refused() {
        let (mut agent, recorded) = scripted(agreeing());
        let error = agent.authenticate(&AuthMethodId::new("oauth")).unwrap_err();

        assert_eq!(error.kind(), "not_initialized");
        assert!(!error.is_terminal());
        assert_eq!(recorded.request_count(), 0);
    }

    // -- What the connection reports -----------------------------------------

    /// A transport fault keeps the discriminant #147 gave it all the way up, so
    /// a caller reads one vocabulary rather than two names for one event.
    #[test]
    fn a_transport_fault_keeps_its_own_kind_and_closes_the_connection() {
        for (fault, expected) in [
            (
                TransportError::Desynchronized {
                    detail: DesyncDetail::NonJsonLine {
                        detail: "expected value at line 1".to_owned(),
                    },
                },
                "desynchronized",
            ),
            (
                TransportError::Desynchronized {
                    detail: DesyncDetail::DuplicateResponseId {
                        id: RequestId::Number(1),
                    },
                },
                "desynchronized",
            ),
            (
                TransportError::MessageTooLarge {
                    bytes: 17 * 1024 * 1024,
                    limit: 16 * 1024 * 1024,
                },
                "message_too_large",
            ),
            (
                TransportError::Disconnected {
                    kind: DisconnectKind::ExitBeforeResponse,
                },
                "disconnected",
            ),
        ] {
            let (mut agent, recorded) = scripted(vec![Step::Fault(fault)]);
            let error = agent.initialize(&harkness(), &everything()).unwrap_err();

            assert_eq!(error.kind(), expected);
            assert!(error.is_terminal());
            assert!(error.transport().is_some());
            assert!(agent.is_closed());
            assert_eq!(recorded.shutdowns(), 1);
        }
    }

    /// An answer to a request nobody sent means the stream's position is no
    /// longer known, so the connection is quarantined rather than resynchronized
    /// — guessing where the next message starts is how one bad line becomes a
    /// wrong answer. What reaches the caller is the transport's own kind.
    #[test]
    fn an_answer_to_a_request_nobody_sent_quarantines_the_connection() {
        let (mut agent, recorded) = scripted(vec![
            Step::Peer(Message::result(
                RequestId::Number(4242),
                json!({"protocolVersion": 1}),
            )),
            Step::Reply(fixture(RESPONSE_FIXTURE)),
        ]);
        let error = agent.initialize(&harkness(), &everything()).unwrap_err();

        assert_eq!(error.kind(), "desynchronized");
        assert!(error.is_terminal());
        assert_eq!(recorded.quarantined().as_deref(), Some("desynchronized"));
        assert!(agent.is_closed());
        assert_eq!(recorded.handshakes(), 0);
    }

    /// ACP has nothing an agent may send before `initialize` returns: no session
    /// to update, no file to read, no terminal to create. The check is exact
    /// because the transport delivers one ordered stream — anything in the peer
    /// queue when the response arrives was written before it.
    #[test]
    fn an_agent_that_speaks_before_the_handshake_is_refused() {
        for early in [
            Message::request(RequestId::Number(7), "fs/read_text_file", None),
            Message::notification("session/update", Some(json!({"sessionId": "s"}))),
        ] {
            let expected = match &early {
                Message::Request(request) => format!("a '{}' request", request.method),
                Message::Notification(notification) => {
                    format!("a '{}' notification", notification.method)
                }
                Message::Response(_) => unreachable!("the script sends no response"),
            };

            let (mut agent, recorded) = scripted(vec![
                Step::Peer(early),
                Step::Reply(fixture(RESPONSE_FIXTURE)),
            ]);
            let error = agent.initialize(&harkness(), &everything()).unwrap_err();

            assert_eq!(error.kind(), "protocol_violation");
            assert!(error.to_string().contains(&expected), "{error}");
            assert!(error.is_terminal());
            assert!(agent.is_closed());
            assert_eq!(recorded.handshakes(), 0);
        }
    }

    /// A peer queue that fills during the handshake is a stall nobody can clear:
    /// nothing legitimate is in there to read, so `peer_queue_full`'s promise
    /// that "the connection resumes the moment somebody drains" is one this
    /// crate cannot keep. Reported as it arrives, it would tell every retry the
    /// agent is fine while no retry could get past the same full queue and the
    /// child was never torn down.
    #[test]
    fn a_peer_queue_that_fills_during_the_handshake_is_the_violation_it_is() {
        let (mut agent, recorded) = scripted(vec![
            Step::Fault(TransportError::PeerQueueFull { capacity: 4096 }),
            Step::Reply(fixture(RESPONSE_FIXTURE)),
        ]);
        let error = agent.initialize(&harkness(), &everything()).unwrap_err();

        assert_eq!(error.kind(), "protocol_violation");
        assert!(
            error.is_terminal(),
            "no retry can drain what nobody may read"
        );
        assert!(error.to_string().contains("4096"), "{error}");
        assert!(agent.is_closed());
        assert_eq!(recorded.shutdowns(), 1, "the child is torn down");
    }

    /// Every field of `AcpTimeouts` is public, so a caller reaching for "wait
    /// indefinitely" will write `Duration::MAX` — and `Instant + Duration`
    /// panics on overflow. The adapter is not a place to panic from.
    #[test]
    fn an_unaddable_timeout_becomes_a_long_wait_rather_than_a_panic() {
        let (connection, _recorded) = ScriptedAgent::connect(agreeing());
        let mut agent = AcpConnection::with_timeouts(
            connection,
            AcpTimeouts {
                initialize: Duration::MAX,
                authenticate: Duration::MAX,
                shutdown_grace: Duration::MAX,
            },
        );

        agent
            .initialize(&harkness(), &everything())
            .expect("an unaddable deadline is still a deadline");
    }

    /// A peer choosing a method name a megabyte long must not choose how long a
    /// Harkness diagnostic is.
    #[test]
    fn an_agents_method_name_is_bounded_in_a_diagnostic() {
        let long = "session/".to_owned() + &"u".repeat(4096);
        let (mut agent, _recorded) = scripted(vec![
            Step::Peer(Message::notification(long, None)),
            Step::Reply(fixture(RESPONSE_FIXTURE)),
        ]);
        let error = agent.initialize(&harkness(), &everything()).unwrap_err();

        assert_eq!(error.kind(), "protocol_violation");
        assert!(
            error.to_string().len() < 512,
            "the diagnostic grew with the name: {} bytes",
            error.to_string().len(),
        );
    }

    /// A multi-byte name is cut on a character boundary rather than in the
    /// middle of one, because the result is a `String`.
    #[test]
    fn quoted_agent_text_is_cut_on_a_character_boundary() {
        let short = "session/prompt";
        assert_eq!(quoted(short), short);

        let wide = "é".repeat(MAX_QUOTED_AGENT_BYTES);
        let clamped = quoted(&wide);
        assert!(clamped.len() <= MAX_QUOTED_AGENT_BYTES + '…'.len_utf8());
        assert!(clamped.ends_with('…'));
    }

    /// The same bound covers an agent's JSON-RPC message, which reaches a log
    /// line, a CLI envelope, and a `{error}` in a panic through `Display`. The
    /// field beside it keeps the whole thing — that split is the promise, and
    /// interpolating the message into the sentence would have quietly broken the
    /// half of it that matters.
    #[test]
    fn an_agents_message_is_bounded_in_a_diagnostic_and_whole_in_its_field() {
        let sprawling = "x".repeat(1_000_000);
        let (mut agent, _recorded) = scripted(vec![Step::Refuse(PeerError {
            code: -32603,
            message: sprawling.clone(),
            data: None,
        })]);
        let error = agent.initialize(&harkness(), &everything()).unwrap_err();

        assert!(
            error.to_string().len() < 512,
            "the diagnostic grew with the agent's message: {} bytes",
            error.to_string().len(),
        );
        match &error {
            AcpError::AgentRejectedRequest { refusal, .. } => {
                assert_eq!(refusal.message, sprawling, "the field keeps it whole");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    /// A silent agent is bounded by the caller's deadline and nothing else, and
    /// the connection survives it: a short deadline is not evidence about the
    /// peer, so ending a working session over one would be Harkness's mistake.
    #[test]
    fn a_silent_agent_is_bounded_by_the_deadline_without_ending_the_session() {
        let (mut agent, recorded) = scripted(vec![Step::Silent]);
        let error = agent.initialize(&harkness(), &everything()).unwrap_err();

        assert_eq!(error.kind(), "request_timed_out");
        assert!(!error.is_terminal());
        assert!(!agent.is_closed());
        assert_eq!(recorded.shutdowns(), 0);
    }

    /// Cancellation reaches a pending handshake well inside the workspace's
    /// 250 ms visibility target, because the connection polls its token every
    /// 20 ms in every blocking phase.
    #[test]
    fn cancellation_reaches_a_pending_handshake_promptly() {
        let cancel = Cancellation::default();
        let (connection, recorded) =
            ScriptedAgent::connect_with(vec![Step::Silent], cancel.clone());
        let mut agent = AcpConnection::with_timeouts(
            connection,
            AcpTimeouts {
                // Far longer than the target, so a deadline cannot masquerade as
                // a prompt cancellation.
                initialize: Duration::from_secs(30),
                ..AcpTimeouts::default()
            },
        );

        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            cancel.cancel();
        });

        let started = std::time::Instant::now();
        let error = agent.initialize(&harkness(), &everything()).unwrap_err();
        let elapsed = started.elapsed();
        canceller.join().unwrap();

        assert_eq!(error.kind(), "cancelled");
        assert!(
            elapsed < Duration::from_millis(250),
            "cancellation took {elapsed:?}",
        );
        assert!(agent.is_closed());
        assert_eq!(recorded.shutdowns(), 1);
    }

    /// "This agent had to be killed" is a bug report, so it survives however the
    /// conversation ended — including a teardown an earlier failure ran.
    #[test]
    fn shutdown_reports_the_teardown_an_earlier_failure_ran() {
        let (mut agent, recorded) = scripted(vec![Step::Reply(fixture(V2_RESPONSE_FIXTURE))]);
        agent.initialize(&harkness(), &everything()).unwrap_err();

        let outcome = agent.shutdown();
        assert_eq!(outcome.rung, ShutdownRung::ClosedStdin);
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(recorded.shutdowns(), 1, "the second teardown is not run");
    }

    /// An ordinary teardown runs one.
    #[test]
    fn shutdown_tears_an_open_connection_down() {
        let (agent, recorded) = negotiated();
        let outcome = agent.shutdown();
        assert_eq!(outcome.rung, ShutdownRung::ClosedStdin);
        assert_eq!(recorded.shutdowns(), 1);
    }

    /// #150 runs an agent from a worker thread, so the whole conversation has to
    /// be able to move to one. Asserted rather than assumed, because a field
    /// added later — a cell, a raw pointer, an `Rc` — takes the property away
    /// silently and the failure lands in another crate.
    #[test]
    fn the_conversation_can_be_owned_by_another_thread() {
        const fn owned_by_a_thread<T: Send>() {}
        owned_by_a_thread::<AcpConnection>();
        owned_by_a_thread::<AcpError>();
        owned_by_a_thread::<InitializeOutcome>();
    }

    // -- Fixtures ------------------------------------------------------------

    /// The committed response fixtures are what every decoding test above is
    /// written against, so they have to keep meaning what they meant. The full
    /// one round-trips byte for byte through the wire types; the two
    /// hand-maintained ones are pinned by what they decode to, because this
    /// build cannot produce either.
    #[test]
    fn the_frozen_response_fixtures_still_describe_these_wire_forms() {
        let full = fixture(RESPONSE_FIXTURE);
        assert_eq!(
            serde_json::to_string_pretty(&full).unwrap(),
            RESPONSE_FIXTURE.trim_end(),
            "the committed fixture is not canonical JSON",
        );

        assert_eq!(
            fixture(MINIMAL_RESPONSE_FIXTURE),
            json!({"protocolVersion": 1})
        );
        assert_eq!(
            fixture(V2_RESPONSE_FIXTURE)["protocolVersion"],
            json!(2),
            "the ADR-0014 boundary fixture must keep selecting a version Harkness refuses",
        );
    }

    /// Removes one JSON Pointer's target, so a test can say "the agent did not
    /// mention this" without rebuilding the whole response around the gap.
    fn remove(value: &mut Value, pointer: &str) {
        let (parent, key) = pointer
            .rsplit_once('/')
            .expect("a pointer names a parent and a key");
        value
            .pointer_mut(parent)
            .expect("the pointer's parent exists")
            .as_object_mut()
            .expect("the pointer's parent is an object")
            .remove(key)
            .expect("the pointer's target exists");
    }

    /// Rewrites the two fixtures this build produces.
    ///
    /// Run only when the request Harkness sends genuinely changes, and never to
    /// make a failing comparison pass: these files are the evidence that an
    /// agent in the field still receives what it received before.
    ///
    /// ```sh
    /// cargo test -p harkness-acp -- --ignored regenerate_the_frozen_v1_fixtures
    /// ```
    #[test]
    #[ignore = "rewrites committed fixtures; run deliberately"]
    fn regenerate_the_frozen_v1_fixtures() {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/fixtures");
        std::fs::create_dir_all(&directory).unwrap();

        for (name, client, advertised) in [
            ("initialize-request-v1.json", harkness(), everything()),
            (
                "initialize-request-minimal-v1.json",
                ClientIdentity::new("harkness", "0.1.0"),
                AdvertisedClientCapabilities::default(),
            ),
        ] {
            let (mut agent, recorded) = scripted(agreeing());
            agent.initialize(&client, &advertised).unwrap();
            let sent = recorded.request_params("initialize").unwrap().unwrap();
            let json = serde_json::to_string_pretty(&sent).unwrap();
            std::fs::write(directory.join(name), format!("{json}\n")).unwrap();
        }

        // `initialize-response-minimal-v1.json` and
        // `initialize-response-v2-v1.json` are deliberately not regenerated.
        // Neither is a wire form this build can produce — one omits every field
        // serialization always writes, the other selects a version Harkness
        // refuses — so both are hand-maintained beside the frozen set they
        // probe. `initialize-response-v1.json` is hand-maintained too: it is an
        // *agent's* answer, and writing it out from this crate's own encoder
        // would test the encoder against itself.
    }
}
