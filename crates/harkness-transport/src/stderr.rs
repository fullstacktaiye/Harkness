//! Where a peer's standard error goes.
//!
//! The MCP specification is explicit that a server's standard error is free-form
//! logging a client MAY capture and SHOULD NOT assume is errors, and an ACP
//! agent's is no different. So this engine captures it and draws no conclusion
//! from it: nothing a peer writes here can fail a request, quarantine a
//! connection, or change a [`ShutdownOutcome`](crate::ShutdownOutcome). What it
//! is for is the moment *after* a failure, when the only account of why an agent
//! died is the thing it printed on its way out.
//!
//! The sink is a trait rather than a path because the destination belongs to the
//! layer above: `harkness-runtime` owns the artifact store, and this crate sits
//! below it and cannot name one. A runtime-side implementation streams into
//! `<data_dir>/artifacts/<run_id>/<artifact_id>` through the redacting writer
//! that every other durable byte passes through; this crate ships only what a
//! test and a diagnostic need.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

/// A peer's standard-error stream, one chunk at a time.
///
/// Implementations must not block for long and must not fail: the sink runs on
/// the connection's stderr thread, and a destination that stopped accepting
/// bytes is a reason to stop capturing, never a reason to disturb a working
/// conversation. That is why no method returns a `Result`.
pub trait StderrSink: Send {
    /// Accepts the next chunk exactly as the peer wrote it.
    fn write(&mut self, chunk: &[u8]);

    /// Signals that the peer closed the stream.
    ///
    /// Called once, from the stderr thread, before the connection is torn down.
    /// A destination that has to be finalized — an artifact that becomes durable
    /// only when it is sealed — does it here.
    fn finish(&mut self) {}
}

/// A sink that keeps nothing.
///
/// The default, and the right one for a peer whose logging has no owner: a
/// connection with no run behind it has nowhere to put an artifact, and
/// buffering output nobody will read is how a long-lived server becomes a memory
/// leak.
#[derive(Clone, Copy, Debug, Default)]
pub struct DiscardedStderr;

impl StderrSink for DiscardedStderr {
    fn write(&mut self, _chunk: &[u8]) {}
}

/// A bounded, shareable tail of what a peer wrote to standard error.
///
/// Bounded because the peer chooses how much it writes, and shareable because
/// the thread that wants to read it is never the thread that filled it: a clone
/// handed to [`SpawnSpec::stderr_sink`](crate::SpawnSpec::stderr_sink) is the
/// same buffer the caller keeps, so a failure can quote the peer's own last
/// words without a channel between them.
#[derive(Clone, Debug)]
pub struct StderrTail {
    limit: usize,
    retained: Arc<Mutex<Retained>>,
}

#[derive(Debug, Default)]
struct Retained {
    bytes: VecDeque<u8>,
    total: u64,
    finished: bool,
}

impl StderrTail {
    /// Retains at most `limit` bytes of the peer's standard error.
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            retained: Arc::new(Mutex::new(Retained::default())),
        }
    }

    /// The retained tail, decoded leniently.
    ///
    /// Lossy because a tail starts wherever the bound put it, which can be
    /// mid-character, and because a peer's logging is not required to be UTF-8
    /// at all. A diagnostic that refused to render is worse than one with a
    /// replacement character in it.
    #[must_use]
    pub fn text(&self) -> String {
        let retained = self.retained.lock().expect("stderr tail is not poisoned");
        String::from_utf8_lossy(&retained.bytes.iter().copied().collect::<Vec<_>>()).into_owned()
    }

    /// Everything the peer wrote, including what the bound discarded.
    #[must_use]
    pub fn byte_len(&self) -> u64 {
        self.retained
            .lock()
            .expect("stderr tail is not poisoned")
            .total
    }

    /// Whether the peer has closed its standard error.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.retained
            .lock()
            .expect("stderr tail is not poisoned")
            .finished
    }
}

impl StderrSink for StderrTail {
    fn write(&mut self, chunk: &[u8]) {
        let mut retained = self.retained.lock().expect("stderr tail is not poisoned");
        retained.total = retained.total.saturating_add(chunk.len() as u64);
        // Only the tail can matter, so a chunk larger than the whole bound is
        // trimmed before it is copied rather than pushed and popped byte by
        // byte.
        let keep = chunk.len().min(self.limit);
        retained.bytes.extend(&chunk[chunk.len() - keep..]);
        while retained.bytes.len() > self.limit {
            retained.bytes.pop_front();
        }
    }

    fn finish(&mut self) {
        self.retained
            .lock()
            .expect("stderr tail is not poisoned")
            .finished = true;
    }
}

#[cfg(test)]
mod tests {
    use super::{DiscardedStderr, StderrSink, StderrTail};

    #[test]
    fn a_tail_keeps_the_end_and_counts_the_whole() {
        let tail = StderrTail::new(8);
        let mut sink = tail.clone();

        sink.write(b"starting up\n");
        sink.write(b"fatal: no\n");
        sink.finish();

        assert_eq!(tail.text(), "fatal: no\n"[2..].to_owned());
        assert_eq!(tail.byte_len(), 22);
        assert!(tail.is_finished());
    }

    /// A single chunk larger than the bound is the case a naive push-then-trim
    /// gets quadratically wrong, and it is exactly what a server dumping a
    /// backtrace produces.
    #[test]
    fn one_oversized_chunk_is_trimmed_before_it_is_retained() {
        let tail = StderrTail::new(4);
        let mut sink = tail.clone();

        sink.write(&vec![b'x'; 1024]);
        sink.write(b"end");

        assert_eq!(tail.text(), "xend");
        assert_eq!(tail.byte_len(), 1027);
    }

    #[test]
    fn a_tail_renders_output_that_is_not_utf8() {
        let tail = StderrTail::new(8);
        let mut sink = tail.clone();

        sink.write(&[0xff, 0xfe]);

        assert_eq!(tail.text(), "\u{fffd}\u{fffd}");
    }

    #[test]
    fn a_discarding_sink_keeps_nothing() {
        let mut sink = DiscardedStderr;
        sink.write(b"anything at all");
        sink.finish();
    }
}
