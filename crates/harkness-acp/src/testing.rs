//! A scripted agent with no process behind it.
//!
//! ADR-0012 promises the conformance suites will be written against the
//! transport seam rather than against a child process, and this is the shape
//! that promise takes for ACP: a [`JsonRpcTransport`] that answers each request
//! from a script and records everything sent to it. Negotiation, capability
//! decoding, the error taxonomy, and the peer-traffic refusal are all exercised
//! with no executable, no timing, and no platform involved — which is what makes
//! them assertions rather than observations.
//!
//! [#156] grows this into a fake agent that serves whole sessions. What is here
//! is the handshake's worth of it.
//!
//! [#156]: https://github.com/fullstacktaiye/harkness/issues/156

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use harkness_transport::{
    Cancellation, Connection, Counters, JsonRpcTransport, Message, PeerError, SendRejection,
    ShutdownOutcome, ShutdownRung, TransportError,
};
use serde_json::Value;

use crate::{AcpConnection, AcpTimeouts};

/// One thing a scripted agent does when it is next asked something.
pub(crate) enum Step {
    /// Answer the pending request with this result value.
    Reply(Value),
    /// Answer the pending request with this JSON-RPC error.
    Refuse(PeerError),
    /// Put a message on the wire before answering.
    ///
    /// Queued ahead of whatever terminal step follows it, which is what makes
    /// "the agent spoke during the handshake" reproducible: the transport
    /// delivers one ordered stream, so a message queued first is routed first.
    /// Any message, not only a peer-initiated one — a response naming an id
    /// nobody sent is how the correlation failures are reached.
    Peer(Message),
    /// Raise a transport fault instead of answering.
    Fault(TransportError),
    /// Say nothing at all, ever.
    Silent,
}

/// What a scripted agent was told, readable after the connection has taken it.
#[derive(Clone, Default)]
pub(crate) struct Recorded {
    sent: Arc<Mutex<Vec<Message>>>,
    quarantined: Arc<Mutex<Option<String>>>,
    handshakes: Arc<Mutex<usize>>,
    shutdowns: Arc<Mutex<usize>>,
}

impl Recorded {
    /// Every message handed to the agent, in order.
    pub(crate) fn sent(&self) -> Vec<Message> {
        self.sent.lock().unwrap().clone()
    }

    /// The parameters of the `method` request, if one was sent.
    pub(crate) fn request_params(&self, method: &str) -> Option<Value> {
        self.sent().into_iter().find_map(|message| match message {
            Message::Request(request) if request.method == method => request.params,
            _ => None,
        })
    }

    /// How many requests were handed to the agent.
    pub(crate) fn request_count(&self) -> usize {
        self.sent()
            .iter()
            .filter(|message| matches!(message, Message::Request(_)))
            .count()
    }

    /// [`TransportError::kind`] of the fault that quarantined the connection.
    pub(crate) fn quarantined(&self) -> Option<String> {
        self.quarantined.lock().unwrap().clone()
    }

    /// Whether the adapter declared the startup window closed.
    pub(crate) fn handshakes(&self) -> usize {
        *self.handshakes.lock().unwrap()
    }

    /// How many times the agent was torn down.
    pub(crate) fn shutdowns(&self) -> usize {
        *self.shutdowns.lock().unwrap()
    }
}

/// A transport that answers from a script.
pub(crate) struct ScriptedAgent {
    steps: Mutex<VecDeque<Step>>,
    inbound: Mutex<VecDeque<Result<Message, TransportError>>>,
    recorded: Recorded,
}

impl ScriptedAgent {
    /// Builds a connection that runs `steps`, and the record of what it is told.
    pub(crate) fn connect(steps: Vec<Step>) -> (Connection, Recorded) {
        Self::connect_with(steps, Cancellation::default())
    }

    /// As [`connect`](Self::connect), with a token the caller can trip.
    pub(crate) fn connect_with(steps: Vec<Step>, cancel: Cancellation) -> (Connection, Recorded) {
        let recorded = Recorded::default();
        let agent = Box::new(Self {
            steps: Mutex::new(steps.into()),
            inbound: Mutex::new(VecDeque::new()),
            recorded: recorded.clone(),
        });
        (Connection::new(agent, cancel), recorded)
    }

    /// Queues everything one request provokes, up to and including its answer.
    fn perform(&self, answering: &Message) {
        let Message::Request(request) = answering else {
            return;
        };
        let mut steps = self.steps.lock().unwrap();
        let mut inbound = self.inbound.lock().unwrap();
        while let Some(step) = steps.pop_front() {
            match step {
                Step::Peer(message) => inbound.push_back(Ok(message)),
                Step::Reply(result) => {
                    inbound.push_back(Ok(Message::result(request.id.clone(), result)));
                    return;
                }
                Step::Refuse(error) => {
                    inbound.push_back(Ok(Message::failure(request.id.clone(), error)));
                    return;
                }
                Step::Fault(error) => {
                    inbound.push_back(Err(error));
                    return;
                }
                Step::Silent => return,
            }
        }
    }
}

impl JsonRpcTransport for ScriptedAgent {
    fn send(&self, message: Message, _deadline: Instant) -> Result<(), TransportError> {
        match self.try_send(message) {
            Ok(()) => Ok(()),
            Err(SendRejection::NoRoom(_)) => Err(TransportError::SendTimedOut),
            Err(SendRejection::Failed(error)) => Err(error),
        }
    }

    fn try_send(&self, message: Message) -> Result<(), SendRejection> {
        self.perform(&message);
        self.recorded.sent.lock().unwrap().push(message);
        Ok(())
    }

    fn recv_deadline(&self, deadline: Instant) -> Result<Option<Message>, TransportError> {
        if let Some(next) = self.inbound.lock().unwrap().pop_front() {
            return next.map(Some);
        }
        // A quiet agent has to be quiet for the caller's slice rather than
        // returning instantly, or a waiting connection spins its poll loop
        // instead of sleeping in it. The slice is the connection's own poll
        // interval, so cancellation is still observed an order of magnitude
        // inside the workspace's 250 ms target.
        thread::sleep(deadline.saturating_duration_since(Instant::now()));
        Ok(None)
    }

    fn quarantine(&self, fault: &TransportError) {
        let mut quarantined = self.recorded.quarantined.lock().unwrap();
        if quarantined.is_none() {
            *quarantined = Some(fault.kind().to_owned());
        }
    }

    fn handshake_complete(&self) {
        *self.recorded.handshakes.lock().unwrap() += 1;
    }

    fn counters(&self) -> Counters {
        Counters::default()
    }

    fn shutdown(self: Box<Self>, _grace: Duration) -> ShutdownOutcome {
        *self.recorded.shutdowns.lock().unwrap() += 1;
        ShutdownOutcome {
            rung: ShutdownRung::ClosedStdin,
            exit_code: Some(0),
            stderr_bytes: 0,
        }
    }
}

/// Timeouts short enough that a test waiting one out finishes.
pub(crate) fn brisk() -> AcpTimeouts {
    AcpTimeouts {
        initialize: Duration::from_millis(200),
        authenticate: Duration::from_millis(200),
        shutdown_grace: Duration::from_millis(10),
    }
}

/// An adapter over a scripted agent, with the brisk timeouts above.
pub(crate) fn scripted(steps: Vec<Step>) -> (AcpConnection, Recorded) {
    let (connection, recorded) = ScriptedAgent::connect(steps);
    (AcpConnection::with_timeouts(connection, brisk()), recorded)
}
