//! Where a turn's timings come from.
//!
//! Behind a trait because the two things that need timings want opposite
//! properties: a live adapter wants a monotonic clock, and a test — or the
//! scripted provider — wants two replays of one script to produce the same
//! numbers. Reading `Instant::now` inside the assembler would make every
//! outcome unequal to itself and leave the latency rules asserted with sleeps.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// Elapsed time since one turn began.
///
/// `Send` because an assembler is moved onto whatever thread runs the turn.
pub trait TurnClock: Send {
    /// How long the turn has been running.
    fn elapsed(&self) -> Duration;
}

/// The clock a live adapter uses.
#[derive(Clone, Copy, Debug)]
pub struct MonotonicTurnClock {
    start: Instant,
}

impl MonotonicTurnClock {
    /// Starts a clock at this moment.
    #[must_use]
    pub fn started_now() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for MonotonicTurnClock {
    fn default() -> Self {
        Self::started_now()
    }
}

impl TurnClock for MonotonicTurnClock {
    fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }
}

/// A clock that only moves when something tells it to.
///
/// Cloning shares the same reading, so a caller keeps a handle while the
/// assembler owns one — which is how the scripted provider advances time
/// between steps without either side reaching a real clock.
#[derive(Clone, Debug, Default)]
pub struct ManualTurnClock {
    nanos: Arc<AtomicU64>,
}

impl ManualTurnClock {
    /// Starts a clock at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Moves the clock forward.
    ///
    /// Saturating, so a script cannot wrap the reading round to zero by
    /// advancing past 584 years.
    pub fn advance(&self, by: Duration) {
        let nanos = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX);
        let mut current = self.nanos.load(Ordering::Acquire);
        loop {
            let next = current.saturating_add(nanos);
            match self.nanos.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }
}

impl TurnClock for ManualTurnClock {
    fn elapsed(&self) -> Duration {
        Duration::from_nanos(self.nanos.load(Ordering::Acquire))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{ManualTurnClock, MonotonicTurnClock, TurnClock};

    #[test]
    fn a_manual_clock_moves_only_when_told_and_is_shared_by_its_clones() {
        let clock = ManualTurnClock::new();
        let handle = clock.clone();
        assert_eq!(clock.elapsed(), Duration::ZERO);
        handle.advance(Duration::from_millis(120));
        assert_eq!(clock.elapsed(), Duration::from_millis(120));
        clock.advance(Duration::from_millis(5));
        assert_eq!(handle.elapsed(), Duration::from_millis(125));
    }

    #[test]
    fn a_manual_clock_saturates_rather_than_wrapping() {
        let clock = ManualTurnClock::new();
        clock.advance(Duration::from_secs(u64::MAX));
        clock.advance(Duration::from_secs(u64::MAX));
        assert_eq!(clock.elapsed(), Duration::from_nanos(u64::MAX));
    }

    #[test]
    fn a_monotonic_clock_never_reports_a_negative_reading() {
        let clock = MonotonicTurnClock::started_now();
        assert!(clock.elapsed() < Duration::from_secs(60));
    }
}
