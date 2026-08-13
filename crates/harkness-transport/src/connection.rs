//! Request/response correlation over any [`JsonRpcTransport`].
//!
//! A transport moves messages. A [`Connection`] is what turns them back into
//! calls: it allocates a fresh id for every request, remembers which caller is
//! waiting for which id, and hands peer-initiated requests and notifications to
//! whoever is consuming them. Nothing here knows what a method means, and
//! nothing here touches a process — which is why an adapter written against a
//! `Connection` is testable with a scripted transport and no `fork`.
//!
//! # Why there is no dispatcher thread
//!
//! Something has to read from the transport, and the obvious answer is a thread
//! per connection that reads in a loop and routes. This does not have one: the
//! callers already blocked on an answer do the reading, one at a time. Whichever
//! thread holds the pump reads in [`POLL_INTERVAL`]-sized slices and routes what
//! it finds — into another caller's reply slot, or into the peer-message queue —
//! then re-offers the pump. A caller that is not leading waits on a condition
//! variable with the same slice, so it becomes the leader within one interval of
//! the leader departing.
//!
//! The property this buys is that a connection reads exactly when somebody is
//! waiting for something. A dispatcher thread reads always, which sounds
//! harmless until the consumer stops consuming: the thread then blocks pushing
//! into a full queue and every response behind that message stops flowing, which
//! is the same stall with a fourth thread paid for. Three threads per connection
//! remains the whole cost, and they are the transport's three.
//!
//! An adapter that wants a dedicated reader still has one — it calls
//! [`Connection::next_peer_message`] in a loop on its own thread, and that thread
//! is the leader most of the time. That is the ACP session-update shape, and it
//! composes with concurrent [`request`](Connection::request) calls without either
//! side knowing about the other.

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Condvar, Mutex,
        atomic::{AtomicI64, Ordering},
    },
    time::{Duration, Instant},
};

use harkness_git::Cancellation;
use serde_json::Value;

use crate::{
    error::{DesyncDetail, DisconnectKind, TransportError},
    message::{Message, Notification, PeerError, Request, RequestId, Response},
    spawn::SpawnSpec,
    stdio::StdioTransport,
    transport::{Counters, JsonRpcTransport, SendRejection, ShutdownOutcome},
};

/// How often a waiting caller re-checks its deadline, its token, and the pump.
///
/// Matches the transport's own interval, and both are sized against the
/// workspace's 250 ms cancellation-visibility target with an order of magnitude
/// to spare.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Peer-initiated messages held for an adapter that has not taken them yet.
///
/// Bounded, like everything else a peer controls the size of, and generous
/// enough for one streamed agent turn: a `session/prompt` that reports its work
/// as it goes emits updates in the hundreds, and a bound an ordinary turn
/// crossed would make [`TransportError::PeerQueueFull`] a routine event rather
/// than a diagnosis.
const PEER_CAPACITY: usize = 4096;

/// Bytes of peer-initiated content held for an adapter that has not taken it.
///
/// A count is not a bound on memory, and the two have to be stated separately or
/// the product of them is the real answer: `PEER_CAPACITY` messages each just
/// under [`DEFAULT_MAX_MESSAGE_BYTES`] is tens of gigabytes, reached with every
/// per-message bound respected. A peer chooses both how many messages it sends
/// and how large each one is, so it chooses that product, which makes the
/// aggregate exactly the kind of thing a peer must not be allowed to pick.
/// Whichever bound is reached first stops the pump.
///
/// [`DEFAULT_MAX_MESSAGE_BYTES`]: crate::DEFAULT_MAX_MESSAGE_BYTES
const PEER_BYTE_BUDGET: usize = 64 * 1024 * 1024;

/// Request ids remembered after their caller is finished with them.
///
/// A retired id is what tells a *late* answer — to a request whose caller gave
/// up — apart from an answer to a request that was never sent. Bounded, because
/// the alternative is a table that grows for the life of a long-lived server
/// connection; an answer arriving after its id has been evicted reads as an
/// unknown id, which is the same conclusion reached a little later.
const RETAINED_REQUEST_IDS: usize = 1024;

/// A peer-initiated message, which no correlation table can answer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PeerMessage {
    /// A call the peer expects this side to answer, with
    /// [`Connection::respond`].
    Request(Request),
    /// A call the peer expects no answer to.
    Notification(Notification),
}

impl PeerMessage {
    /// Roughly what holding this message costs, for the aggregate bound.
    fn retained_bytes(&self) -> usize {
        let (method, params) = match self {
            Self::Request(request) => (&request.method, request.params.as_ref()),
            Self::Notification(notification) => {
                (&notification.method, notification.params.as_ref())
            }
        };
        method.len() + params.map_or(0, retained_bytes)
    }
}

/// One correlated JSON-RPC conversation.
pub struct Connection {
    transport: Box<dyn JsonRpcTransport>,
    cancel: Cancellation,
    next_id: AtomicI64,
    /// Held for the duration of one read from the transport, and never taken
    /// while `pending` or `peers` is held. That is what makes the lock order
    /// pump → table impossible to invert.
    pump: Mutex<()>,
    pending: Mutex<Pending>,
    answered: Condvar,
    peers: Mutex<PeerQueue>,
    arrived: Condvar,
}

/// Peer-initiated messages waiting for an adapter, bounded two ways.
#[derive(Default)]
struct PeerQueue {
    waiting: VecDeque<(PeerMessage, usize)>,
    bytes: usize,
}

impl PeerQueue {
    /// Whether the pump must stop reading.
    fn is_full(&self) -> bool {
        self.waiting.len() >= PEER_CAPACITY || self.bytes >= PEER_BYTE_BUDGET
    }

    fn push(&mut self, message: PeerMessage) {
        let bytes = message.retained_bytes();
        self.bytes = self.bytes.saturating_add(bytes);
        self.waiting.push_back((message, bytes));
    }

    fn pop(&mut self) -> Option<PeerMessage> {
        let (message, bytes) = self.waiting.pop_front()?;
        self.bytes = self.bytes.saturating_sub(bytes);
        Some(message)
    }
}

/// Roughly what a parsed value costs to keep, for the aggregate bound.
///
/// An estimate rather than a measurement: re-serializing every message to size
/// it would double the parsing this engine already did, and the bound only has
/// to be the right order of magnitude to stop a peer choosing how much memory
/// this process holds. `serde_json` caps nesting depth while parsing, so the
/// recursion here is over a structure whose depth is already bounded.
fn retained_bytes(value: &Value) -> usize {
    const OVERHEAD: usize = 16;
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => OVERHEAD,
        Value::String(text) => OVERHEAD + text.len(),
        Value::Array(items) => OVERHEAD + items.iter().map(retained_bytes).sum::<usize>(),
        Value::Object(fields) => {
            OVERHEAD
                + fields
                    .iter()
                    .map(|(name, value)| OVERHEAD + name.len() + retained_bytes(value))
                    .sum::<usize>()
        }
    }
}

#[derive(Default)]
struct Pending {
    slots: HashMap<RequestId, Slot>,
    /// Retirement order, so the retained ids stay bounded. An entry may name a
    /// slot that is already gone; eviction tolerates that rather than paying a
    /// linear removal to keep the two exactly in step.
    retired: VecDeque<RequestId>,
    /// Set once the conversation ends, so a caller waiting on a slot that will
    /// never be filled is told why rather than waiting out its deadline.
    terminal: Option<TerminalFault>,
}

/// What is known about one request this connection sent.
#[derive(Clone)]
enum Slot {
    /// Sent, unanswered, with a caller waiting on it.
    Waiting,
    /// Answered, and the answer not yet collected.
    Answered(Result<Value, PeerError>),
    /// The caller is gone — it collected its answer, or it gave up waiting.
    ///
    /// Kept rather than removed, and that is the difference between a connection
    /// that survives a slow peer and one that does not. A request that passed
    /// its deadline may still be answered; if its id were forgotten, that
    /// perfectly correct late answer would read as a response to a request
    /// nobody sent and quarantine a live session over a deadline *this* side
    /// chose. `answered` records whether the one permitted answer has arrived,
    /// so a genuine second answer is still a duplicate.
    Retired { answered: bool },
}

impl Pending {
    /// Requests that have been sent and whose caller is still waiting.
    fn outstanding(&self) -> usize {
        self.slots
            .values()
            .filter(|slot| !matches!(slot, Slot::Retired { .. }))
            .count()
    }

    /// Marks `id` finished with, keeping it recognizable for a bounded while.
    fn retire(&mut self, id: &RequestId, answered: bool) {
        self.slots.insert(id.clone(), Slot::Retired { answered });
        self.retired.push_back(id.clone());
        while self.retired.len() > RETAINED_REQUEST_IDS {
            if let Some(evicted) = self.retired.pop_front() {
                self.slots.remove(&evicted);
            }
        }
    }
}

/// Why the conversation ended, remembered for the callers who arrive after.
#[derive(Clone)]
enum TerminalFault {
    Disconnected(DisconnectKind),
    Cancelled,
    Fault { kind: &'static str, detail: String },
}

impl TerminalFault {
    fn from(error: &TransportError) -> Self {
        match error {
            TransportError::Disconnected { kind } => Self::Disconnected(*kind),
            TransportError::Cancelled => Self::Cancelled,
            other => Self::Fault {
                kind: other.kind(),
                detail: other.to_string(),
            },
        }
    }

    /// Rebuilds the failure for a caller that arrived after it happened.
    ///
    /// A clean exit observed while this caller had a request outstanding is
    /// reported as [`DisconnectKind::ExitBeforeResponse`], which the transport
    /// could not have said: it does not know what anybody asked for. That
    /// refinement is the whole reason the disconnect kind is finished here
    /// rather than beneath.
    fn error(&self, had_outstanding: bool) -> TransportError {
        match self {
            Self::Disconnected(DisconnectKind::Idle) if had_outstanding => {
                TransportError::Disconnected {
                    kind: DisconnectKind::ExitBeforeResponse,
                }
            }
            Self::Disconnected(kind) => TransportError::Disconnected { kind: *kind },
            Self::Cancelled => TransportError::Cancelled,
            Self::Fault { kind, detail } => TransportError::Quarantined {
                fault_kind: kind,
                detail: detail.clone(),
            },
        }
    }
}

impl Connection {
    /// Launches `spec`'s program and starts a correlated conversation with it.
    ///
    /// # Errors
    ///
    /// As [`StdioTransport::spawn`].
    pub fn spawn(spec: SpawnSpec, cancel: Cancellation) -> Result<Self, TransportError> {
        let transport = StdioTransport::spawn(spec, cancel.clone())?;
        Ok(Self::new(Box::new(transport), cancel))
    }

    /// Correlates calls over an existing transport.
    ///
    /// This is the seam the conformance suites are built on: a transport that
    /// replays a scripted message sequence exercises an adapter's negotiation,
    /// session lifecycle, and error taxonomy with no child process involved.
    #[must_use]
    pub fn new(transport: Box<dyn JsonRpcTransport>, cancel: Cancellation) -> Self {
        Self {
            transport,
            cancel,
            next_id: AtomicI64::new(1),
            pump: Mutex::new(()),
            pending: Mutex::new(Pending::default()),
            answered: Condvar::new(),
            peers: Mutex::new(PeerQueue::default()),
            arrived: Condvar::new(),
        }
    }

    /// Declares the peer's handshake finished, ending the startup deadline.
    pub fn handshake_complete(&self) {
        self.transport.handshake_complete();
    }

    /// Calls `method` and waits for its single answer.
    ///
    /// The id is allocated here and never reused within the connection, so a
    /// duplicate outbound id is unrepresentable rather than guarded against.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::RequestTimedOut`] when `deadline` passes with
    /// the peer still alive, [`TransportError::Cancelled`] when the token is
    /// tripped, and the connection's terminal failure — a disconnect, a
    /// desynchronization, an oversized message — when there will be no answer.
    /// An `Err(PeerError)` *inside* the `Ok` is the peer reporting that the
    /// method failed, which is a working connection and never a transport error.
    pub fn request(
        &self,
        method: &str,
        params: Option<Value>,
        deadline: Instant,
    ) -> Result<Result<Value, PeerError>, TransportError> {
        let id = RequestId::Number(self.next_id.fetch_add(1, Ordering::Relaxed));
        {
            let mut pending = self.pending.lock().expect("pending table is not poisoned");
            if let Some(fault) = pending.terminal.clone() {
                return Err(fault.error(false));
            }
            pending.slots.insert(id.clone(), Slot::Waiting);
        }

        if let Err(error) =
            self.send_pumping(Message::request(id.clone(), method, params), deadline)
        {
            self.retire(&id, false);
            return Err(error);
        }

        loop {
            {
                let mut pending = self.pending.lock().expect("pending table is not poisoned");
                if let Some(Slot::Answered(answer)) = pending.slots.get(&id).cloned() {
                    pending.retire(&id, true);
                    return Ok(answer);
                }
                if let Some(fault) = pending.terminal.clone() {
                    pending.retire(&id, false);
                    return Err(fault.error(true));
                }
            }
            if self.cancel.is_cancelled() {
                self.retire(&id, false);
                self.give_up(&TransportError::Cancelled);
                return Err(TransportError::Cancelled);
            }
            if Instant::now() >= deadline {
                // Retired rather than forgotten. The peer may still answer, and
                // it did nothing wrong — this side chose the deadline — so its
                // late answer is dropped quietly instead of reading as a
                // response to a request nobody sent. `request_timed_out` claims
                // the connection is still healthy, and this is what makes that
                // claim true.
                self.retire(&id, false);
                return Err(TransportError::RequestTimedOut { id });
            }
            if let Err(error) = self.pump_once(deadline, &self.answered, &self.pending) {
                self.retire(&id, false);
                // This caller had a request outstanding, which the transport
                // could not know when it called the exit idle.
                return Err(match error {
                    TransportError::Disconnected {
                        kind: DisconnectKind::Idle,
                    } => TransportError::Disconnected {
                        kind: DisconnectKind::ExitBeforeResponse,
                    },
                    other => other,
                });
            }
        }
    }

    /// Calls `method` and expects no answer, giving up at `deadline`.
    ///
    /// # Errors
    ///
    /// As [`JsonRpcTransport::send`].
    pub fn notify(
        &self,
        method: &str,
        params: Option<Value>,
        deadline: Instant,
    ) -> Result<(), TransportError> {
        self.send_pumping(Message::notification(method, params), deadline)
    }

    /// Answers a peer-initiated request, giving up at `deadline`.
    ///
    /// # Errors
    ///
    /// As [`JsonRpcTransport::send`].
    pub fn respond(
        &self,
        id: RequestId,
        outcome: Result<Value, PeerError>,
        deadline: Instant,
    ) -> Result<(), TransportError> {
        self.send_pumping(Message::Response(Response { id, outcome }), deadline)
    }

    /// Hands a message over, draining what the peer sent while waiting for room.
    ///
    /// A blocking send that does not read is half of a deadlock. The other half
    /// is a peer that floods its own output and stops reading its input until
    /// somebody drains it: this side's reader fills the inbound queue and parks,
    /// the peer's pipe fills, the peer stops reading, and the two wait for each
    /// other until a deadline breaks the tie. Pumping between attempts is what
    /// makes that unreachable rather than merely bounded, and it is why
    /// [`JsonRpcTransport::try_send`] hands the message back instead of taking
    /// it.
    fn send_pumping(&self, message: Message, deadline: Instant) -> Result<(), TransportError> {
        let mut message = message;
        loop {
            match self.transport.try_send(message) {
                Ok(()) => return Ok(()),
                Err(SendRejection::NoRoom(returned)) => message = returned,
                Err(SendRejection::Failed(error)) => {
                    self.remember(&error);
                    return Err(error);
                }
            }
            if self.cancel.is_cancelled() {
                self.give_up(&TransportError::Cancelled);
                return Err(TransportError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(TransportError::SendTimedOut);
            }
            self.pump_once(deadline, &self.answered, &self.pending)?;
        }
    }

    /// Takes the next peer-initiated message, waiting until `deadline`.
    ///
    /// Returns `Ok(None)` when the deadline passes with nothing to report, which
    /// is the ordinary answer for a quiet peer and not a failure.
    ///
    /// # Errors
    ///
    /// Returns the connection's terminal failure when the conversation has
    /// ended, and [`TransportError::Cancelled`] when the token is tripped.
    pub fn next_peer_message(
        &self,
        deadline: Instant,
    ) -> Result<Option<PeerMessage>, TransportError> {
        loop {
            if let Some(message) = self.peers.lock().expect("peer queue is not poisoned").pop() {
                return Ok(Some(message));
            }
            {
                let pending = self.pending.lock().expect("pending table is not poisoned");
                if let Some(fault) = pending.terminal.clone() {
                    let had_outstanding = pending.outstanding() > 0;
                    drop(pending);
                    return Err(fault.error(had_outstanding));
                }
            }
            if self.cancel.is_cancelled() {
                self.give_up(&TransportError::Cancelled);
                return Err(TransportError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            self.pump_once(deadline, &self.arrived, &self.peers)?;
        }
    }

    /// What this connection has moved and is holding.
    #[must_use]
    pub fn counters(&self) -> Counters {
        let mut counters = self.transport.counters();
        counters.outstanding_requests = self
            .pending
            .lock()
            .expect("pending table is not poisoned")
            .outstanding();
        counters.peer_depth = self
            .peers
            .lock()
            .expect("peer queue is not poisoned")
            .waiting
            .len();
        counters
    }

    /// Tears the connection down and reports how far it had to go.
    #[must_use]
    pub fn shutdown(self, grace: Duration) -> ShutdownOutcome {
        self.transport.shutdown(grace)
    }

    /// Reads one slice from the transport, or waits one slice for the leader to.
    ///
    /// `waited_on` and `guard` are the condition variable and mutex this caller
    /// sleeps against when another thread already holds the pump. They differ
    /// between the two waiting shapes — a reply slot and the peer queue — and
    /// each is notified by whichever routing step could satisfy it.
    fn pump_once<T>(
        &self,
        deadline: Instant,
        waited_on: &Condvar,
        guard: &Mutex<T>,
    ) -> Result<(), TransportError> {
        let slice = POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now()));
        let Ok(_leader) = self.pump.try_lock() else {
            let locked = guard.lock().expect("connection state is not poisoned");
            let _woken = waited_on
                .wait_timeout(locked, slice)
                .expect("connection state is not poisoned");
            return Ok(());
        };

        // Nothing is ever discarded here: when the peer queue is full the pump
        // stops reading, the message stays in the transport's own bounded queue,
        // and the stall reaches the peer's own pipe. But a caller waiting for a
        // *response* cannot wait that out — in-order delivery puts its answer
        // behind the unread messages, and it is the only pump — so it is told by
        // name rather than left to time out on a stall it could not diagnose.
        // No implementation escapes the underlying choice: one ordered stream,
        // an application queue with a bound, and an answer behind unconsumed
        // messages leaves grow, discard, and fail as the whole of it.
        if self
            .peers
            .lock()
            .expect("peer queue is not poisoned")
            .is_full()
        {
            return Err(TransportError::PeerQueueFull {
                capacity: PEER_CAPACITY,
            });
        }

        match self.transport.recv_deadline(Instant::now() + slice) {
            Ok(Some(message)) => self.route(message),
            Ok(None) => Ok(()),
            Err(error) => {
                self.remember(&error);
                Err(error)
            }
        }
    }

    /// Delivers one message to whoever it belongs to.
    fn route(&self, message: Message) -> Result<(), TransportError> {
        let response = match message {
            Message::Response(response) => response,
            Message::Request(request) => return self.enqueue_peer(PeerMessage::Request(request)),
            Message::Notification(notification) => {
                return self.enqueue_peer(PeerMessage::Notification(notification));
            }
        };

        let mut pending = self.pending.lock().expect("pending table is not poisoned");
        let desync = match pending.slots.get_mut(&response.id) {
            Some(slot @ Slot::Waiting) => {
                *slot = Slot::Answered(response.outcome);
                drop(pending);
                self.answered.notify_all();
                return Ok(());
            }
            // A late answer to a request whose caller gave up. The peer did
            // nothing wrong, so it is dropped quietly, and the id stays retired
            // so a *second* answer is still the duplicate it would be for any
            // other request.
            Some(slot @ Slot::Retired { answered: false }) => {
                *slot = Slot::Retired { answered: true };
                return Ok(());
            }
            // Answering twice is not a retry: one answer has already been taken
            // as the truth about that call, so the stream's position is no
            // longer known and there is nothing to resynchronize to.
            Some(Slot::Answered(_) | Slot::Retired { answered: true }) => {
                DesyncDetail::DuplicateResponseId { id: response.id }
            }
            None => DesyncDetail::UnknownResponseId { id: response.id },
        };
        drop(pending);

        let error = TransportError::Desynchronized { detail: desync };
        self.give_up(&error);
        Err(error)
    }

    /// Hands a peer-initiated message to whoever is consuming them.
    ///
    /// Always accepted. The bound is enforced by the pump refusing to *read*
    /// when the queue is full, so this can never be asked to discard one: a
    /// dropped peer request is one an adapter never answers, and a dropped
    /// notification is history that silently did not happen.
    fn enqueue_peer(&self, message: PeerMessage) -> Result<(), TransportError> {
        self.peers
            .lock()
            .expect("peer queue is not poisoned")
            .push(message);
        self.arrived.notify_all();
        Ok(())
    }

    /// Records the conversation's terminal failure and wakes every waiter.
    fn remember(&self, error: &TransportError) {
        if !error.is_terminal() {
            return;
        }
        let mut pending = self.pending.lock().expect("pending table is not poisoned");
        if pending.terminal.is_none() {
            pending.terminal = Some(TerminalFault::from(error));
        }
        drop(pending);
        self.answered.notify_all();
        self.arrived.notify_all();
    }

    /// Ends the conversation and stops the transport doing any further I/O.
    fn give_up(&self, error: &TransportError) {
        self.transport.quarantine(error);
        self.remember(error);
    }

    /// Marks a request whose caller has stopped waiting.
    fn retire(&self, id: &RequestId, answered: bool) {
        self.pending
            .lock()
            .expect("pending table is not poisoned")
            .retire(id, answered);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };

    use harkness_git::Cancellation;
    use serde_json::{Value, json};

    use super::{Connection, PeerMessage};
    use crate::{
        error::{DisconnectKind, TransportError},
        message::{Message, PeerError, RequestId},
        transport::{Counters, JsonRpcTransport, SendRejection, ShutdownOutcome, ShutdownRung},
    };

    /// What a scripted transport recorded, readable after the connection that
    /// owns the transport has taken it.
    #[derive(Clone, Default)]
    struct Recorded {
        sent: Arc<Mutex<Vec<Message>>>,
        quarantined: Arc<Mutex<Option<String>>>,
    }

    /// A transport with no process behind it: everything it returns is scripted,
    /// and everything sent to it is recorded. This is the shape ADR-0012 promises
    /// the conformance suites will be written against, and it is what makes these
    /// tests deterministic rather than timing-dependent.
    struct ScriptedTransport {
        inbound: Mutex<Vec<Result<Message, TransportError>>>,
        recorded: Recorded,
        /// Answers each request as it is sent, which models a prompt peer with
        /// no thread and no ordering to get wrong.
        echo: bool,
        /// Refuses the first handover, so the pumping retry loop in
        /// `send_pumping` is exercised rather than merely compiled.
        refuse_once: AtomicBool,
    }

    impl ScriptedTransport {
        fn with(messages: Vec<Result<Message, TransportError>>) -> (Box<Self>, Recorded) {
            let recorded = Recorded::default();
            let transport = Box::new(Self {
                // Reversed so the script is consumed from the end, which is the
                // cheap end of a `Vec`.
                inbound: Mutex::new(messages.into_iter().rev().collect()),
                recorded: recorded.clone(),
                echo: false,
                refuse_once: AtomicBool::new(false),
            });
            (transport, recorded)
        }

        fn echoing() -> (Box<Self>, Recorded) {
            let (mut transport, recorded) = Self::with(Vec::new());
            transport.echo = true;
            (transport, recorded)
        }
    }

    impl JsonRpcTransport for ScriptedTransport {
        fn send(&self, message: Message, _deadline: Instant) -> Result<(), TransportError> {
            match self.try_send(message) {
                Ok(()) => Ok(()),
                Err(SendRejection::NoRoom(_)) => Err(TransportError::SendTimedOut),
                Err(SendRejection::Failed(error)) => Err(error),
            }
        }

        fn try_send(&self, message: Message) -> Result<(), SendRejection> {
            // A scripted peer that refuses one message and then accepts it, so
            // the pumping retry loop is exercised rather than merely compiled.
            if self.refuse_once.swap(false, Ordering::Relaxed) {
                return Err(SendRejection::NoRoom(message));
            }
            if self.echo
                && let Message::Request(request) = &message
            {
                self.inbound.lock().unwrap().insert(
                    0,
                    Ok(Message::result(request.id.clone(), json!({"ok": true}))),
                );
            }
            self.recorded.sent.lock().unwrap().push(message);
            Ok(())
        }

        fn recv_deadline(&self, _deadline: Instant) -> Result<Option<Message>, TransportError> {
            match self.inbound.lock().unwrap().pop() {
                Some(Ok(message)) => Ok(Some(message)),
                Some(Err(error)) => Err(error),
                None => Ok(None),
            }
        }

        fn quarantine(&self, fault: &TransportError) {
            let mut quarantined = self.recorded.quarantined.lock().unwrap();
            if quarantined.is_none() {
                *quarantined = Some(fault.kind().to_owned());
            }
        }

        fn counters(&self) -> Counters {
            Counters::default()
        }

        fn shutdown(self: Box<Self>, _grace: Duration) -> ShutdownOutcome {
            ShutdownOutcome {
                rung: ShutdownRung::ClosedStdin,
                exit_code: Some(0),
                stderr_bytes: 0,
            }
        }
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    #[test]
    fn a_request_is_correlated_to_its_answer() {
        let (transport, _recorded) = ScriptedTransport::echoing();
        let connection = Connection::new(transport, Cancellation::default());

        assert_eq!(
            connection.request("ping", None, soon()).unwrap(),
            Ok(json!({"ok": true}))
        );
        assert_eq!(connection.counters().outstanding_requests, 0);
    }

    /// A method that failed is a working connection. Folding a peer's error into
    /// a transport error would make an adapter tear an agent down because one
    /// tool call was rejected.
    #[test]
    fn a_peer_error_is_an_answer_rather_than_a_transport_failure() {
        let (transport, _recorded) = ScriptedTransport::with(vec![Ok(Message::failure(
            RequestId::Number(1),
            PeerError {
                code: -32601,
                message: "method not found".to_owned(),
                data: None,
            },
        ))]);
        let connection = Connection::new(transport, Cancellation::default());

        let answer = connection.request("nope", None, soon()).unwrap();

        assert_eq!(answer.unwrap_err().code, -32601);
    }

    /// Ids are allocated rather than chosen, so two requests cannot name one
    /// slot however many threads are asking.
    #[test]
    fn outbound_ids_are_allocated_and_never_repeat() {
        let (transport, recorded) = ScriptedTransport::echoing();
        let connection = Connection::new(transport, Cancellation::default());

        for _ in 0..8 {
            connection.request("ping", None, soon()).unwrap().unwrap();
        }

        let ids = recorded
            .sent
            .lock()
            .unwrap()
            .iter()
            .map(|message| match message {
                Message::Request(request) => request.id.clone(),
                other => panic!("only requests were sent, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            (1..=8).map(RequestId::Number).collect::<Vec<_>>(),
            "ids are fresh and monotonic"
        );
    }

    #[test]
    fn an_unknown_response_id_desynchronizes_the_connection() {
        let (transport, recorded) = ScriptedTransport::with(vec![Ok(Message::result(
            RequestId::Number(99),
            json!(null),
        ))]);
        let connection = Connection::new(transport, Cancellation::default());

        let error = connection
            .next_peer_message(Instant::now() + Duration::from_millis(200))
            .unwrap_err();

        assert_eq!(error.kind(), "desynchronized");
        assert_eq!(
            recorded.quarantined.lock().unwrap().as_deref(),
            Some("desynchronized"),
            "the transport is told to stop"
        );
        assert_eq!(
            connection
                .next_peer_message(Instant::now() + Duration::from_millis(50))
                .unwrap_err()
                .kind(),
            "quarantined"
        );
    }

    #[test]
    fn a_duplicate_response_id_desynchronizes_the_connection() {
        let (transport, _recorded) = ScriptedTransport::with(vec![
            Ok(Message::result(
                RequestId::Number(1),
                json!({"first": true}),
            )),
            Ok(Message::result(
                RequestId::Number(1),
                json!({"second": true}),
            )),
        ]);
        let connection = Connection::new(transport, Cancellation::default());

        assert_eq!(
            connection.request("once", None, soon()).unwrap(),
            Ok(json!({"first": true}))
        );
        let error = connection
            .next_peer_message(Instant::now() + Duration::from_millis(200))
            .unwrap_err();

        // The second answer arrives for an id that has been retired *answered*,
        // which is the duplicate case; an id that was never issued is the
        // unknown one. Both leave the stream at an unknown position. Retiring
        // rather than forgetting is what keeps this distinction available.
        assert_eq!(error.kind(), "desynchronized");
    }

    #[test]
    fn peer_requests_and_notifications_reach_their_consumer() {
        let (transport, _recorded) = ScriptedTransport::with(vec![
            Ok(Message::notification(
                "session/update",
                Some(json!({"n": 1})),
            )),
            Ok(Message::request(
                RequestId::from("peer-1"),
                "fs/read",
                Some(json!({"path": "a"})),
            )),
        ]);
        let connection = Connection::new(transport, Cancellation::default());

        let PeerMessage::Notification(update) =
            connection.next_peer_message(soon()).unwrap().unwrap()
        else {
            panic!("the first scripted message is a notification");
        };
        assert_eq!(update.method, "session/update");

        let PeerMessage::Request(request) = connection.next_peer_message(soon()).unwrap().unwrap()
        else {
            panic!("the second scripted message is a request");
        };
        assert_eq!(request.id, RequestId::from("peer-1"));
        connection
            .respond(request.id, Ok(json!({"content": ""})), soon())
            .unwrap();
    }

    /// A quiet peer is the ordinary case, not a failure, so a passed deadline is
    /// an empty answer rather than an error.
    #[test]
    fn a_quiet_peer_yields_nothing_rather_than_failing() {
        let (transport, _recorded) = ScriptedTransport::with(Vec::new());
        let connection = Connection::new(transport, Cancellation::default());

        assert_eq!(
            connection
                .next_peer_message(Instant::now() + Duration::from_millis(40))
                .unwrap(),
            None
        );
    }

    #[test]
    fn a_request_that_outlives_its_deadline_is_told_apart_from_a_disconnect() {
        let (transport, _recorded) = ScriptedTransport::with(Vec::new());
        let connection = Connection::new(transport, Cancellation::default());

        let error = connection
            .request("slow", None, Instant::now() + Duration::from_millis(60))
            .unwrap_err();

        assert!(matches!(error, TransportError::RequestTimedOut { .. }));
        assert!(!error.is_terminal(), "the peer is still there");
        assert_eq!(connection.counters().outstanding_requests, 0);
    }

    /// The transport cannot know whether anybody was waiting, so it reports a
    /// clean exit as `idle`. Refining that into `exit_before_response` is the
    /// correlation layer's job, and it is the distinction an adapter needs to
    /// decide whether relaunching would help.
    #[test]
    fn a_clean_exit_with_a_request_outstanding_is_reported_as_exit_before_response() {
        let (transport, _recorded) =
            ScriptedTransport::with(vec![Err(TransportError::Disconnected {
                kind: DisconnectKind::Idle,
            })]);
        let connection = Connection::new(transport, Cancellation::default());

        let error = connection.request("initialize", None, soon()).unwrap_err();

        assert!(matches!(
            error,
            TransportError::Disconnected {
                kind: DisconnectKind::ExitBeforeResponse
            }
        ));
    }

    #[test]
    fn a_clean_exit_with_nothing_outstanding_stays_idle() {
        let (transport, _recorded) =
            ScriptedTransport::with(vec![Err(TransportError::Disconnected {
                kind: DisconnectKind::Idle,
            })]);
        let connection = Connection::new(transport, Cancellation::default());

        let error = connection.next_peer_message(soon()).unwrap_err();

        assert!(matches!(
            error,
            TransportError::Disconnected {
                kind: DisconnectKind::Idle
            }
        ));
    }

    /// A peer that died part-way through a line is reported as such whether or
    /// not anybody was waiting: the evidence is in the stream, not in the table.
    #[test]
    fn a_mid_message_exit_keeps_its_kind() {
        let (transport, _recorded) =
            ScriptedTransport::with(vec![Err(TransportError::Disconnected {
                kind: DisconnectKind::MidResponse,
            })]);
        let connection = Connection::new(transport, Cancellation::default());

        let error = connection.request("initialize", None, soon()).unwrap_err();

        assert!(matches!(
            error,
            TransportError::Disconnected {
                kind: DisconnectKind::MidResponse
            }
        ));
    }

    #[test]
    fn cancellation_ends_a_pending_request_promptly() {
        let cancellation = Cancellation::default();
        let (transport, _recorded) = ScriptedTransport::with(Vec::new());
        let connection = Connection::new(transport, cancellation.clone());

        let tripping = cancellation.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            tripping.cancel();
        });

        let started = Instant::now();
        let error = connection
            .request("slow", None, Instant::now() + Duration::from_secs(30))
            .unwrap_err();

        assert_eq!(error.kind(), "cancelled");
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "cancellation took {:?}",
            started.elapsed()
        );
    }

    /// Two callers waiting at once is the case the pump exists for: one leads
    /// and reads, the other is woken by the routing step, and neither needs to
    /// know which it was.
    #[test]
    fn concurrent_callers_each_receive_their_own_answer() {
        let (transport, _recorded) = ScriptedTransport::echoing();
        let connection = Connection::new(transport, Cancellation::default());

        let answers = std::thread::scope(|scope| {
            let handles = (0..4)
                .map(|index| {
                    let connection = &connection;
                    scope.spawn(move || {
                        connection
                            .request(&format!("method-{index}"), None, soon())
                            .unwrap()
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });

        assert_eq!(answers.len(), 4);
        for answer in answers {
            assert_eq!(answer, Ok(json!({"ok": true})));
        }
        assert_eq!(connection.counters().outstanding_requests, 0);
    }

    #[test]
    fn a_notification_carries_no_id() {
        let (transport, recorded) = ScriptedTransport::with(Vec::new());
        let connection = Connection::new(transport, Cancellation::default());

        connection
            .notify("notifications/cancelled", Some(json!({"id": 1})), soon())
            .unwrap();

        assert!(matches!(
            recorded.sent.lock().unwrap().as_slice(),
            [Message::Notification(_)]
        ));
        assert_eq!(connection.counters().outstanding_requests, 0);
    }

    #[test]
    fn a_scalar_answer_is_delivered_verbatim() {
        let (transport, _recorded) = ScriptedTransport::with(vec![Ok(Message::result(
            RequestId::Number(1),
            Value::String("verbatim".to_owned()),
        ))]);
        let connection = Connection::new(transport, Cancellation::default());

        assert_eq!(
            connection.request("echo", None, soon()).unwrap(),
            Ok(Value::String("verbatim".to_owned()))
        );
    }

    #[test]
    fn shutdown_reports_the_transport_outcome() {
        let (transport, _recorded) = ScriptedTransport::with(Vec::new());
        let connection = Connection::new(transport, Cancellation::default());

        let outcome = connection.shutdown(Duration::from_millis(10));

        assert_eq!(outcome.rung, ShutdownRung::ClosedStdin);
    }

    /// The bound on the peer queue is enforced by *not reading*, so a peer that
    /// outruns its consumer stalls rather than losing an update — and a request
    /// stuck behind that stall is told exactly what happened rather than left to
    /// discover it when its deadline runs out. Nothing above the bound is
    /// discarded, and what was taken is still in order.
    #[test]
    fn an_unconsumed_peer_stream_names_itself_rather_than_dropping_messages() {
        let scripted = (0..super::PEER_CAPACITY * 2)
            .map(|n| Ok(Message::notification("tick", Some(json!({ "n": n })))))
            .collect::<Vec<_>>();
        let (transport, _recorded) = ScriptedTransport::with(scripted);
        let connection = Connection::new(transport, Cancellation::default());

        // Nobody is draining peer messages, so the only pump is this request,
        // whose own answer is behind everything it is queueing.
        let error = connection.request("slow", None, soon()).unwrap_err();
        assert_eq!(error.kind(), "peer_queue_full");
        assert!(!error.is_terminal(), "draining fixes it");

        let depth = connection.counters().peer_depth;
        assert!(
            (super::PEER_CAPACITY..=super::PEER_CAPACITY + 1).contains(&depth),
            "the peer queue holds {depth}, outside its {} bound",
            super::PEER_CAPACITY
        );

        for expected in 0..depth {
            let PeerMessage::Notification(tick) =
                connection.next_peer_message(soon()).unwrap().unwrap()
            else {
                panic!("the script holds only notifications");
            };
            assert_eq!(
                tick.params.unwrap()["n"],
                expected,
                "an update was lost or reordered"
            );
        }

        // Consuming the rest empties both the queue and the script, and the
        // connection is still there to be used: this is a diagnosis, not a
        // death.
        while connection
            .next_peer_message(Instant::now() + Duration::from_millis(50))
            .unwrap()
            .is_some()
        {}
        assert_eq!(
            connection
                .request("after", None, Instant::now() + Duration::from_millis(100))
                .unwrap_err()
                .kind(),
            "request_timed_out",
            "the script is exhausted, but the connection is alive"
        );
    }

    /// A peer answering after its caller gave up is a peer behaving correctly —
    /// this side chose the deadline. Quarantining the connection over it would
    /// make `request_timed_out` terminal in practice while claiming not to be,
    /// and would kill a live agent session over one slow call.
    #[test]
    fn a_late_answer_to_an_abandoned_request_does_not_poison_the_connection() {
        let (transport, _recorded) = ScriptedTransport::with(vec![
            // Nothing for the first request within its deadline, then its answer
            // arrives while the second request is the one waiting.
            Ok(Message::result(RequestId::Number(1), json!({"late": true}))),
            Ok(Message::result(RequestId::Number(2), json!({"ok": true}))),
        ]);
        let connection = Connection::new(transport, Cancellation::default());

        // A deadline in the past, so the request gives up without ever pumping.
        let timed_out = connection
            .request("first", None, Instant::now())
            .unwrap_err();
        assert!(matches!(timed_out, TransportError::RequestTimedOut { .. }));
        assert!(!timed_out.is_terminal());

        assert_eq!(
            connection.request("second", None, soon()).unwrap(),
            Ok(json!({"ok": true})),
            "the abandoned request's late answer was discarded, not quarantined"
        );
    }

    /// A send that waits without reading is half of a deadlock: a peer flooding
    /// its own output stops reading its input until somebody drains it. The
    /// retry has to pump, not merely sleep.
    #[test]
    fn a_send_with_no_room_drains_the_peer_while_it_waits() {
        let (transport, recorded) = ScriptedTransport::with(vec![
            Ok(Message::notification(
                "session/update",
                Some(json!({"n": 1})),
            )),
            Ok(Message::result(RequestId::Number(1), json!({"ok": true}))),
        ]);
        transport.refuse_once.store(true, Ordering::Relaxed);
        let connection = Connection::new(transport, Cancellation::default());

        // The first handover is refused, so the only way this completes is by
        // pumping between attempts and retrying with the message it got back.
        assert_eq!(
            connection.request("initialize", None, soon()).unwrap(),
            Ok(json!({"ok": true}))
        );
        assert_eq!(
            recorded.sent.lock().unwrap().len(),
            1,
            "the message was handed over once, not rebuilt and duplicated"
        );
        let PeerMessage::Notification(drained) =
            connection.next_peer_message(soon()).unwrap().unwrap()
        else {
            panic!("the update was drained while the send waited");
        };
        assert_eq!(drained.method, "session/update");
    }

    /// A count bounds messages, not memory. Sixteen-mebibyte notifications
    /// reach the byte budget thousands of messages before the count bound, and
    /// without the second bound every per-message limit would be respected while
    /// the process held tens of gigabytes.
    #[test]
    fn the_peer_queue_is_bounded_by_bytes_as_well_as_by_count() {
        let bulky = "x".repeat(4 * 1024 * 1024);
        let scripted = (0..64)
            .map(|_| {
                Ok(Message::notification(
                    "tick",
                    Some(json!({ "text": bulky })),
                ))
            })
            .collect::<Vec<_>>();
        let (transport, _recorded) = ScriptedTransport::with(scripted);
        let connection = Connection::new(transport, Cancellation::default());

        let error = connection.request("slow", None, soon()).unwrap_err();

        assert_eq!(error.kind(), "peer_queue_full");
        let depth = connection.counters().peer_depth;
        assert!(
            depth < super::PEER_CAPACITY,
            "{depth} messages queued, so the count bound stopped it rather than the byte budget"
        );
        assert!(
            connection.peers.lock().unwrap().bytes <= super::PEER_BYTE_BUDGET + bulky.len(),
            "the budget was passed by more than the message that reached it"
        );
    }

    /// The retained ids are bounded, so a connection whose caller times out
    /// forever does not accumulate a table for the life of the process.
    #[test]
    fn retired_request_ids_stay_bounded() {
        let (transport, _recorded) = ScriptedTransport::with(Vec::new());
        let connection = Connection::new(transport, Cancellation::default());

        for _ in 0..super::RETAINED_REQUEST_IDS + 64 {
            let error = connection
                .request("gone", None, Instant::now())
                .unwrap_err();
            assert!(matches!(error, TransportError::RequestTimedOut { .. }));
        }

        let pending = connection.pending.lock().unwrap();
        assert_eq!(pending.outstanding(), 0);
        assert!(
            pending.slots.len() <= super::RETAINED_REQUEST_IDS,
            "{} ids retained, past the {} bound",
            pending.slots.len(),
            super::RETAINED_REQUEST_IDS
        );
    }

    /// The two layers over a real child, which is the only thing that proves
    /// correlation, framing, and the subprocess compose the way each one's own
    /// tests claim in isolation.
    #[test]
    #[cfg(unix)]
    fn a_correlated_conversation_runs_over_a_spawned_peer() {
        use harkness_test_fixtures::Fixture;

        use crate::spawn::SpawnSpec;

        let fixture = Fixture::new();
        let workspace = fixture.directory("end-to-end");
        let peer = fixture.shim(
            "counting-peer",
            r#"#!/bin/sh
printf '{"jsonrpc":"2.0","method":"ready"}\n'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
  printf '{"jsonrpc":"2.0","id":%s,"result":{"seen":%s}}\n' "$id" "$id"
done
"#,
        );
        let connection = Connection::spawn(
            SpawnSpec::new(&peer, &workspace)
                .env("PATH", "/usr/bin:/bin")
                .startup_deadline(Duration::from_secs(10)),
            Cancellation::default(),
        )
        .unwrap();

        // The peer greets before it is asked anything, so the greeting is
        // waiting in the peer queue while the first request is correlated.
        assert_eq!(
            connection.request("first", None, soon()).unwrap(),
            Ok(json!({"seen": 1}))
        );
        connection.handshake_complete();
        assert_eq!(
            connection.request("second", None, soon()).unwrap(),
            Ok(json!({"seen": 2}))
        );

        let PeerMessage::Notification(greeting) =
            connection.next_peer_message(soon()).unwrap().unwrap()
        else {
            panic!("the peer greets with a notification");
        };
        assert_eq!(greeting.method, "ready");
        assert_eq!(connection.counters().outstanding_requests, 0);

        let outcome = connection.shutdown(Duration::from_secs(5));
        assert_eq!(outcome.rung, ShutdownRung::ClosedStdin);
    }
}
