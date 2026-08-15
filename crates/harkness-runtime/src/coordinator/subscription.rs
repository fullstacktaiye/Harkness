use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::domain::RunId;
use crate::store::{DEFAULT_EVENT_PAGE_LIMIT, EventSeq, Store, StoredEvent};

/// Maximum live events retained for one subscriber.
pub const SUBSCRIBER_CAPACITY: usize = 64;

/// One item delivered by a run subscription.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum EventDelivery {
    /// One persisted event, in sequence order.
    Event(StoredEvent),
    /// This subscriber fell behind and was disconnected.
    Lagged {
        /// Run whose live stream was lost.
        run: RunId,
        /// Last persisted position offered before disconnection.
        last_available: EventSeq,
    },
}

/// Error returned by a nonblocking event receive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TryReceiveError {
    /// No item is currently ready.
    Empty,
    /// The run ended or this subscriber was disconnected.
    Disconnected,
}

/// Error returned by a bounded event wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiveTimeoutError {
    /// The requested duration elapsed.
    Timeout,
    /// The run ended or this subscriber was disconnected.
    Disconnected,
}

#[derive(Debug, Default)]
struct QueueState {
    items: VecDeque<EventDelivery>,
    closed: bool,
}

#[derive(Debug)]
pub(super) struct Subscriber {
    state: Mutex<QueueState>,
    ready: Condvar,
    live_after: Option<EventSeq>,
}

impl Subscriber {
    pub(super) fn new(live_after: Option<EventSeq>) -> Self {
        Self {
            state: Mutex::new(QueueState::default()),
            ready: Condvar::new(),
            live_after,
        }
    }

    pub(super) fn push(&self, event: StoredEvent) {
        if self.live_after.is_some_and(|tip| event.seq <= tip) {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.closed {
            return;
        }
        if state.items.len() >= SUBSCRIBER_CAPACITY {
            state.items.clear();
            state.items.push_back(EventDelivery::Lagged {
                run: event.run_id,
                last_available: event.seq,
            });
            state.closed = true;
        } else {
            state.items.push_back(EventDelivery::Event(event));
        }
        drop(state);
        self.ready.notify_one();
    }

    pub(super) fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.closed = true;
        drop(state);
        self.ready.notify_all();
    }
}

/// Bounded live view of one run's persisted event stream.
#[derive(Debug)]
pub struct EventReceiver {
    subscriber: Arc<Subscriber>,
    replay: Mutex<ReplayState>,
}

#[derive(Debug)]
struct ReplayState {
    store: Arc<Store>,
    run: RunId,
    tip: Option<EventSeq>,
    cursor: Option<EventSeq>,
    items: VecDeque<StoredEvent>,
    exhausted: bool,
}

impl EventReceiver {
    pub(super) fn new(
        subscriber: Arc<Subscriber>,
        store: Arc<Store>,
        run: RunId,
        tip: Option<EventSeq>,
    ) -> Self {
        Self {
            subscriber,
            replay: Mutex::new(ReplayState {
                store,
                run,
                tip,
                cursor: None,
                items: VecDeque::new(),
                exhausted: tip.is_none(),
            }),
        }
    }

    fn replay_next(&self) -> Option<StoredEvent> {
        let mut replay = self.replay.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(event) = replay.items.pop_front() {
            return Some(event);
        }
        if replay.exhausted {
            return None;
        }
        let page = match replay
            .store
            .events(replay.run, replay.cursor, DEFAULT_EVENT_PAGE_LIMIT)
        {
            Ok(page) => page,
            Err(_) => {
                replay.exhausted = true;
                self.subscriber.close();
                return None;
            }
        };
        let tip = replay.tip.expect("a non-empty replay has a tip");
        replay
            .items
            .extend(page.into_iter().take_while(|event| event.seq <= tip));
        replay.cursor = replay.items.back().map(|event| event.seq).or(replay.cursor);
        replay.exhausted = replay.cursor.is_none_or(|cursor| cursor >= tip);
        replay.items.pop_front()
    }

    /// Waits until an event is available or the stream closes.
    pub fn recv(&self) -> Result<EventDelivery, ReceiveTimeoutError> {
        if let Some(event) = self.replay_next() {
            return Ok(EventDelivery::Event(event));
        }
        let mut state = self
            .subscriber
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(item) = state.items.pop_front() {
                return Ok(item);
            }
            if state.closed {
                return Err(ReceiveTimeoutError::Disconnected);
            }
            state = self
                .subscriber
                .ready
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Waits no longer than `limit` for an event.
    pub fn recv_timeout(&self, limit: Duration) -> Result<EventDelivery, ReceiveTimeoutError> {
        if let Some(event) = self.replay_next() {
            return Ok(EventDelivery::Event(event));
        }
        let deadline = Instant::now() + limit;
        let mut state = self
            .subscriber
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(item) = state.items.pop_front() {
                return Ok(item);
            }
            if state.closed {
                return Err(ReceiveTimeoutError::Disconnected);
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(ReceiveTimeoutError::Timeout);
            };
            state = self
                .subscriber
                .ready
                .wait_timeout(state, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Returns immediately with the next available event.
    pub fn try_recv(&self) -> Result<EventDelivery, TryReceiveError> {
        if let Some(event) = self.replay_next() {
            return Ok(EventDelivery::Event(event));
        }
        let mut state = self
            .subscriber
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(item) = state.items.pop_front() {
            Ok(item)
        } else if state.closed {
            Err(TryReceiveError::Disconnected)
        } else {
            Err(TryReceiveError::Empty)
        }
    }
}
