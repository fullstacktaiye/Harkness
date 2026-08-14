//! Newline-delimited framing.
//!
//! Both specifications this engine serves describe the same frame: exactly one
//! JSON message per line, with no embedded newline. That makes the reader's job
//! two separable questions — where does a line end, and is that line one message
//! — and this module owns the first. [`Message::decode`](crate::Message::decode)
//! owns the second.
//!
//! The size bound lives here rather than beside the parser for a reason that
//! only matters under attack: a limit checked after a line is assembled is not a
//! limit. [`LineSplitter`] refuses a pending line the moment it crosses the
//! bound, so the most this process ever holds for a peer that never writes a
//! newline is the limit plus the read chunk that crossed it.

use crate::error::TransportError;

/// Splits a byte stream into lines under a hard size bound.
pub(crate) struct LineSplitter {
    limit: usize,
    pending: Vec<u8>,
    /// A carriage return held back because it may be the first half of a `\r\n`
    /// terminator.
    ///
    /// Held rather than appended so that it is not charged against the bound
    /// when it turns out to be part of a terminator. Appending it and trimming
    /// it at the newline would make the effective limit one byte smaller for a
    /// peer writing Windows line endings — and only for those peers, which is
    /// the kind of difference nobody finds until a message lands on the
    /// boundary.
    deferred_carriage_return: bool,
}

impl LineSplitter {
    /// Splits lines of at most `limit` bytes, excluding the terminator.
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit,
            pending: Vec::new(),
            deferred_carriage_return: false,
        }
    }

    /// Feeds `chunk`, calling `on_line` once per complete line.
    ///
    /// A blank line is skipped rather than reported: it carries no message, and
    /// a peer that separates its output with one has not desynchronized
    /// anything. `on_line` receives the line without its terminator, and a
    /// `\r\n` terminator is consumed whole so a peer writing Windows line
    /// endings does not hand the parser a stray carriage return.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::MessageTooLarge`] as soon as the pending line
    /// crosses the bound. The splitter keeps nothing after that: the connection
    /// is quarantined, so there is no next line to read.
    /// Copied a run at a time rather than a byte at a time: the limit is
    /// megabytes, and a per-byte bound check would run millions of iterations
    /// for one legitimate large message. The bound is still exact, because a run
    /// is appended only after checking that it fits.
    pub(crate) fn feed(
        &mut self,
        mut chunk: &[u8],
        on_line: &mut impl FnMut(&[u8]),
    ) -> Result<(), TransportError> {
        while let Some(offset) = chunk.iter().position(|&byte| byte == b'\n') {
            self.absorb(&chunk[..offset])?;
            // A held-back carriage return was the first half of this terminator,
            // and goes with it.
            self.deferred_carriage_return = false;
            if !self.pending.is_empty() {
                on_line(&self.pending);
            }
            self.pending.clear();
            chunk = &chunk[offset + 1..];
        }
        self.absorb(chunk)
    }

    /// Appends `run` to the pending line, holding back a trailing carriage
    /// return in case a newline follows it.
    fn absorb(&mut self, run: &[u8]) -> Result<(), TransportError> {
        if run.is_empty() {
            return Ok(());
        }
        // Something other than a newline follows the held-back carriage return,
        // so it was content after all.
        if std::mem::take(&mut self.deferred_carriage_return) {
            self.extend(b"\r")?;
        }
        match run.split_last() {
            Some((b'\r', head)) => {
                self.extend(head)?;
                self.deferred_carriage_return = true;
                Ok(())
            }
            _ => self.extend(run),
        }
    }

    /// Appends `run` to the pending line, or refuses the line for exceeding the
    /// bound and keeps none of it.
    fn extend(&mut self, run: &[u8]) -> Result<(), TransportError> {
        if self.pending.len() + run.len() > self.limit {
            // Reported as the first size that breached the bound rather than as
            // the whole run, so the number describes where reading stopped
            // rather than how much happened to arrive in one read.
            let bytes = self.limit + 1;
            self.pending = Vec::new();
            return Err(TransportError::MessageTooLarge {
                bytes,
                limit: self.limit,
            });
        }
        self.pending.extend_from_slice(run);
        Ok(())
    }

    /// Whether the stream ended part-way through a line.
    ///
    /// This is the whole evidence for [`DisconnectKind::MidResponse`]: a peer
    /// that died while writing leaves an unterminated line behind, and one that
    /// exited cleanly does not.
    ///
    /// [`DisconnectKind::MidResponse`]: crate::DisconnectKind::MidResponse
    pub(crate) fn has_partial_line(&self) -> bool {
        !self.pending.is_empty() || self.deferred_carriage_return
    }
}

/// Frames `encoded` as one line.
///
/// The newline check is a guard rather than a formality. `serde_json` escapes
/// every control character, so a message this crate encoded cannot carry one —
/// and that is precisely the property a future writer, a pretty-printer, or a
/// caller framing something it built itself could break silently, delivering two
/// messages where one was written.
///
/// # Errors
///
/// Returns [`TransportError::UnencodableMessage`] when `encoded` contains a line
/// terminator.
pub(crate) fn frame(encoded: &str) -> Result<String, TransportError> {
    if let Some(offset) = encoded.find(['\n', '\r']) {
        return Err(TransportError::UnencodableMessage {
            detail: format!(
                "the encoding contains a line terminator at byte {offset}, which \
                 newline-delimited framing would deliver as two messages"
            ),
        });
    }
    let mut framed = String::with_capacity(encoded.len() + 1);
    framed.push_str(encoded);
    framed.push('\n');
    Ok(framed)
}

#[cfg(test)]
mod tests {
    use super::{LineSplitter, frame};
    use crate::error::TransportError;

    fn lines(limit: usize, chunks: &[&[u8]]) -> Result<Vec<String>, TransportError> {
        let mut splitter = LineSplitter::new(limit);
        let mut collected = Vec::new();
        for chunk in chunks {
            splitter.feed(chunk, &mut |line| {
                collected.push(String::from_utf8_lossy(line).into_owned());
            })?;
        }
        Ok(collected)
    }

    #[test]
    fn lines_are_reassembled_across_read_boundaries() {
        assert_eq!(
            lines(64, &[b"{\"a\":", b"1}\n{\"b\"", b":2}\n"]).unwrap(),
            ["{\"a\":1}", "{\"b\":2}"]
        );
    }

    #[test]
    fn a_carriage_return_terminator_is_consumed_whole() {
        assert_eq!(lines(64, &[b"{\"a\":1}\r\n"]).unwrap(), ["{\"a\":1}"]);
        // Including when a read lands between its two halves.
        assert_eq!(lines(64, &[b"{\"a\":1}\r", b"\n"]).unwrap(), ["{\"a\":1}"]);
    }

    /// The bound is on the message, and a terminator is not part of the
    /// message. Charging the `\r` of a `\r\n` would make the effective limit one
    /// byte smaller for a peer writing Windows line endings, and for nobody
    /// else — a difference that only shows up on the boundary.
    #[test]
    fn a_carriage_return_terminator_is_not_charged_against_the_bound() {
        let exact = "x".repeat(16);
        for chunks in [
            vec![format!("{exact}\r\n").into_bytes()],
            vec![format!("{exact}\r").into_bytes(), b"\n".to_vec()],
            vec![exact.clone().into_bytes(), b"\r\n".to_vec()],
        ] {
            let borrowed = chunks.iter().map(Vec::as_slice).collect::<Vec<_>>();
            assert_eq!(lines(16, &borrowed).unwrap(), std::slice::from_ref(&exact));
        }
    }

    /// A carriage return that is *not* followed by a newline is content, and is
    /// charged like any other byte.
    #[test]
    fn a_carriage_return_inside_a_line_is_content() {
        assert_eq!(lines(64, &[b"a\rb\n"]).unwrap(), ["a\rb"]);
        assert_eq!(lines(64, &[b"a\r", b"b\n"]).unwrap(), ["a\rb"]);
        assert!(matches!(
            lines(2, &[b"a\rb\n"]),
            Err(TransportError::MessageTooLarge { .. })
        ));
    }

    /// A blank line carries no message, so reporting one would turn a peer's
    /// harmless spacing into a desynchronization.
    #[test]
    fn blank_lines_are_skipped() {
        assert_eq!(lines(64, &[b"\n\r\n{\"a\":1}\n\n"]).unwrap(), ["{\"a\":1}"]);
    }

    /// The bound is on the message, not on the framed line: a line of exactly
    /// the limit is delivered and the next byte is the one that breaches it.
    #[test]
    fn the_bound_is_inclusive_at_its_exact_boundary() {
        let exact = vec![b'x'; 16];
        let mut framed = exact.clone();
        framed.push(b'\n');
        assert_eq!(lines(16, &[&framed]).unwrap(), ["x".repeat(16)]);

        let mut over = vec![b'x'; 17];
        over.push(b'\n');
        assert!(matches!(
            lines(16, &[&over]),
            Err(TransportError::MessageTooLarge {
                bytes: 17,
                limit: 16
            })
        ));
    }

    /// The point of the incremental check: a peer that never writes a newline
    /// must not be able to make this process hold its output. Nothing survives
    /// the refusal, so the bound is what the reader ever holds rather than what
    /// it accepts.
    #[test]
    fn an_endless_line_is_refused_without_being_buffered() {
        let mut splitter = LineSplitter::new(8);
        let error = splitter
            .feed(&vec![b'x'; 4096], &mut |_| panic!("no line completes"))
            .unwrap_err();

        assert!(matches!(
            error,
            TransportError::MessageTooLarge { bytes: 9, limit: 8 }
        ));
        assert!(!splitter.has_partial_line());
    }

    #[test]
    fn an_unterminated_tail_is_reported_as_partial() {
        let mut splitter = LineSplitter::new(64);
        splitter.feed(b"{\"a\":1}\n{\"b\"", &mut |_| {}).unwrap();
        assert!(splitter.has_partial_line());

        splitter.feed(b":2}\n", &mut |_| {}).unwrap();
        assert!(!splitter.has_partial_line());
    }

    #[test]
    fn framing_appends_exactly_one_terminator() {
        assert_eq!(frame("{\"a\":1}").unwrap(), "{\"a\":1}\n");
    }

    #[test]
    fn framing_refuses_an_embedded_terminator() {
        for encoded in ["{\"a\":\n1}", "{\"a\":\r1}", "{\"a\":1}\n"] {
            assert!(
                matches!(
                    frame(encoded),
                    Err(TransportError::UnencodableMessage { .. })
                ),
                "framed {encoded:?}"
            );
        }
    }
}
