//! The [`Redactor`] every Harkness store installs.
//!
//! [`store::redaction`](crate::store::redaction) owns the *hook* — the trait,
//! and the guarantee that no write path bypasses it. This module owns the
//! *rules*, which is why it lives beside the diagnostic log rather than inside
//! the store: the same rules have to reach a log line the store never sees, and
//! a rule set that lived under `store` would be reachable only by things that
//! persist rows.
//!
//! # Two shapes, one rule set
//!
//! [`StandardRedactor::redact_text`] is for values held in memory — an event
//! payload's strings, an approval summary, an artifact's label. It applies every
//! rule, including the one that spans lines.
//!
//! [`StandardRedactor::wrap_stream`] is for an artifact's bytes, which may be
//! larger than memory and are not required to be UTF-8. It filters a line at a
//! time, on bytes, applying every rule
//! [`RedactionRule::covers_streams`](super::RedactionRule::covers_streams)
//! admits. Two limits follow from that and are stated rather than hidden:
//!
//! - A rule needing to see across a newline is not attempted on a stream. Only
//!   [`PrivateKeyBlock`](super::RedactionRule::PrivateKeyBlock) is such a rule.
//! - A line longer than [`MAX_FILTERED_LINE_BYTES`] is redacted and emitted in
//!   bounded chunks rather than buffered whole, so a credential straddling a
//!   chunk boundary is not seen. The bound is what keeps a streaming artifact
//!   streaming; an unbounded line buffer would defeat the reason the artifact
//!   store exists.
//!
//! `docs/observability.md` carries the same boundary as a channel-by-rule table.

use std::borrow::Cow;
use std::io::{self, Write};

use crate::store::Redactor;

use super::rules::{self, RedactionRule};
use super::secret::SecretRegistry;

/// Most bytes of one artifact line held before the filter gives up on finding a
/// newline and processes what it has.
///
/// 64 KiB is the store's inline-payload threshold, reused deliberately: it is
/// already the size at which this workspace decides a value belongs in a file
/// rather than in memory, and a second, differently argued number would be one
/// more thing to keep in step.
pub const MAX_FILTERED_LINE_BYTES: usize = 64 * 1024;

/// The redaction rules Harkness applies to everything it writes down.
///
/// Cheap to clone and safe to share: the pattern engines are compiled once for
/// the process, and the declared-secret set is a handle to one append-only
/// registry.
#[derive(Clone, Debug)]
pub struct StandardRedactor {
    secrets: SecretRegistry,
}

impl StandardRedactor {
    /// The redactor a production store installs.
    ///
    /// Bound to [`SecretRegistry::process`], so a value a process tool declares
    /// while a run is under way is redacted by a store that was opened before
    /// anybody knew the value existed.
    #[must_use]
    pub fn standard() -> Self {
        Self {
            secrets: SecretRegistry::process(),
        }
    }

    /// The same rules against a registry the caller owns.
    ///
    /// What a test uses, so declaring a secret in one test cannot change what
    /// another one observes.
    #[must_use]
    pub fn with_secrets(secrets: SecretRegistry) -> Self {
        Self { secrets }
    }

    /// The declared-secret registry this redactor consults.
    #[must_use]
    pub fn secrets(&self) -> &SecretRegistry {
        &self.secrets
    }

    /// Applies the declared-secret rule, then every shape rule.
    ///
    /// Declared values go first because they are literals: a rule that already
    /// replaced a secret cannot then be confused by whatever shape the rest of
    /// the line happens to have.
    fn apply(&self, text: &str, streams_only: bool) -> Option<String> {
        match self.secrets.redact(text, rules::declared_secret_marker()) {
            Some(rewritten) => {
                Some(rules::redact_shapes(&rewritten, streams_only).unwrap_or(rewritten))
            }
            None => rules::redact_shapes(text, streams_only),
        }
    }
}

impl Default for StandardRedactor {
    fn default() -> Self {
        Self::standard()
    }
}

impl Redactor for StandardRedactor {
    fn redact_text<'a>(&self, text: &'a str) -> Cow<'a, str> {
        match self.apply(text, false) {
            Some(rewritten) => Cow::Owned(rewritten),
            None => Cow::Borrowed(text),
        }
    }

    fn wrap_stream(&self, sink: Box<dyn Write + Send>) -> Box<dyn Write + Send> {
        Box::new(LineFilter {
            sink,
            secrets: self.secrets.clone(),
            buffer: Vec::new(),
        })
    }
}

/// An artifact sink that scrubs each line on its way past.
///
/// The contract [`Redactor::wrap_stream`] states is easy to get wrong and is
/// worth repeating at the implementation: `write` reports how much of its
/// *input* it consumed, never how much reached the sink. A rule that shortens
/// what it rewrites would otherwise make `write_all` resend the difference and
/// duplicate content in the file and in its digest.
struct LineFilter {
    sink: Box<dyn Write + Send>,
    secrets: SecretRegistry,
    buffer: Vec<u8>,
}

impl LineFilter {
    /// Redacts one chunk and writes it downstream.
    fn emit(&mut self, chunk: &[u8]) -> io::Result<()> {
        let declared = self
            .secrets
            .redact_bytes(chunk, rules::declared_secret_marker().as_bytes());
        let source: &[u8] = declared.as_deref().unwrap_or(chunk);
        match rules::redact_shape_bytes(source) {
            Some(rewritten) => self.sink.write_all(&rewritten),
            None => self.sink.write_all(source),
        }
    }

    /// Emits every complete line the buffer holds, and forces a chunk out when
    /// one line has grown past the bound.
    fn drain_lines(&mut self) -> io::Result<()> {
        while let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=position).collect();
            self.emit(&line)?;
        }
        if self.buffer.len() >= MAX_FILTERED_LINE_BYTES {
            let chunk = std::mem::take(&mut self.buffer);
            self.emit(&chunk)?;
        }
        Ok(())
    }
}

impl Write for LineFilter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buffer);
        self.drain_lines()?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.buffer.is_empty() {
            let remainder = std::mem::take(&mut self.buffer);
            self.emit(&remainder)?;
        }
        self.sink.flush()
    }
}

impl Drop for LineFilter {
    fn drop(&mut self) {
        // A sink dropped without a flush would otherwise lose whatever had not
        // reached a newline. The store always seals through `flush`, so this is
        // the abandoned-sink path, where losing the tail would make the digest
        // describe fewer bytes than the caller wrote.
        if !self.buffer.is_empty() {
            let remainder = std::mem::take(&mut self.buffer);
            let _ = self.emit(&remainder);
        }
        let _ = self.sink.flush();
    }
}

/// The rules that reach an artifact's byte stream, for documentation and tests.
#[must_use]
pub fn stream_rules() -> Vec<RedactionRule> {
    RedactionRule::all()
        .iter()
        .copied()
        .filter(|rule| rule.covers_streams())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use crate::store::Redactor;

    use super::super::secret::SecretRegistry;
    use super::{MAX_FILTERED_LINE_BYTES, StandardRedactor};

    /// A sink a test can read back.
    #[derive(Clone, Default)]
    struct Collected(Arc<Mutex<Vec<u8>>>);

    impl Write for Collected {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn hermetic() -> StandardRedactor {
        StandardRedactor::with_secrets(SecretRegistry::new())
    }

    fn streamed(redactor: &StandardRedactor, chunks: &[&[u8]]) -> Vec<u8> {
        let collected = Collected::default();
        let mut stream = redactor.wrap_stream(Box::new(collected.clone()));
        for chunk in chunks {
            assert_eq!(
                stream.write(chunk).unwrap(),
                chunk.len(),
                "write reports input consumed, never output produced"
            );
        }
        stream.flush().unwrap();
        drop(stream);
        collected.0.lock().unwrap().clone()
    }

    #[test]
    fn clean_text_is_borrowed_rather_than_copied() {
        assert!(matches!(
            hermetic().redact_text("nothing here"),
            std::borrow::Cow::Borrowed("nothing here")
        ));
    }

    #[test]
    fn a_declared_secret_is_replaced_alongside_the_shape_rules() {
        let registry = SecretRegistry::new();
        registry.declare("opaque-passphrase");
        let redactor = StandardRedactor::with_secrets(registry);

        let redacted = redactor.redact_text("used opaque-passphrase for https://u:p@host/x");

        assert_eq!(
            redacted,
            "used «redacted:declared_secret» for https://«redacted:url_userinfo»@host/x"
        );
    }

    #[test]
    fn a_stream_is_filtered_a_line_at_a_time_across_chunk_boundaries() {
        let redactor = hermetic();

        let bytes = streamed(
            &redactor,
            &[
                b"cloning https://user:hun",
                b"ter2@example.com\nnext line\n",
            ],
        );

        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "cloning https://«redacted:url_userinfo»@example.com\nnext line\n",
            "a secret split across two writes is still one line to the filter"
        );
    }

    #[test]
    fn a_stream_with_no_trailing_newline_still_emits_its_last_line() {
        let bytes = streamed(&hermetic(), &[b"token=abcdefgh"]);

        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "token=«redacted:credential_parameter»"
        );
    }

    #[test]
    fn a_declared_secret_reaches_the_stream_too() {
        let registry = SecretRegistry::new();
        registry.declare("streamed-secret");
        let redactor = StandardRedactor::with_secrets(registry);

        let bytes = streamed(&redactor, &[b"saw streamed-secret once\n"]);

        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "saw «redacted:declared_secret» once\n"
        );
    }

    #[test]
    fn binary_content_reaches_the_sink_byte_for_byte() {
        let binary: Vec<u8> = (0u8..=255).collect();

        let bytes = streamed(&hermetic(), &[&binary]);

        assert_eq!(
            bytes, binary,
            "an artifact that is not text must not be rewritten"
        );
    }

    #[test]
    fn a_line_past_the_bound_is_emitted_rather_than_buffered_without_end() {
        let redactor = hermetic();
        let long = vec![b'x'; MAX_FILTERED_LINE_BYTES + 32];

        let bytes = streamed(&redactor, &[&long]);

        assert_eq!(bytes.len(), long.len());
        assert_eq!(bytes, long);
    }

    #[test]
    fn an_abandoned_stream_still_delivers_what_it_was_given() {
        let redactor = hermetic();
        let collected = Collected::default();
        let mut stream = redactor.wrap_stream(Box::new(collected.clone()));
        stream.write_all(b"no newline here").unwrap();

        drop(stream);

        assert_eq!(
            String::from_utf8(collected.0.lock().unwrap().clone()).unwrap(),
            "no newline here"
        );
    }
}
