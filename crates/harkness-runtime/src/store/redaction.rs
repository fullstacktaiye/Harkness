//! The single choke point every recorded byte passes through.
//!
//! Everything the event log and the artifact store hold may later be shown to a
//! user or handed back to an agent as an observation: process output, Git
//! stderr, a tool's own error text. A secret that reaches either of them is
//! durable, so scrubbing has to happen *before* persistence rather than at each
//! display site, where one forgotten renderer leaks everything.
//!
//! This module supplies the hook, not the rules. The default
//! [`PassThrough`] does nothing, which is deliberate: the point of landing the
//! hook first is that every write path already routes through it, so supplying
//! real rules later is a change in one place instead of an audit of every
//! caller.
//!
//! # Two methods because there are two shapes
//!
//! An event payload is a small structured value held in memory;
//! [`Redactor::redact_text`] rewrites the strings inside it and can borrow when
//! there is nothing to change. An artifact is a stream that may be far larger
//! than memory, so [`Redactor::wrap_stream`] interposes on the bytes as they are
//! written instead of asking for them all at once.
//!
//! Only string *values* of a payload are rewritten, never object keys. A key is
//! a field name from a published schema: rewriting it would change what the
//! record means to every consumer, and a secret is a value, not a field name.

use std::borrow::Cow;
use std::fmt;
use std::io::Write;

use serde_json::Value;

/// Rewrites content on its way into durable storage.
///
/// Implementations must be cheap enough to sit on every write and must not
/// panic: a redactor is not a validation layer, and a run that cannot record
/// what happened is worse than one that records a little too much.
pub trait Redactor: fmt::Debug + Send + Sync {
    /// Returns `text` with anything that must not be persisted rewritten.
    ///
    /// Borrowing when nothing changes keeps the common case allocation-free.
    fn redact_text<'a>(&self, text: &'a str) -> Cow<'a, str>;

    /// Wraps an artifact sink so bytes are rewritten as they stream past.
    ///
    /// The wrapper sits *above* the hashing and counting layer, so the recorded
    /// size and SHA-256 describe the bytes that actually landed on disk. An
    /// implementation must not retain the sink it is handed beyond the wrapper
    /// it returns.
    ///
    /// **The returned writer's `write` must report how much of its *input* it
    /// consumed, not how much it passed downstream.** A rule that shortens what
    /// it rewrites — masking a long secret with a short marker — would otherwise
    /// return fewer bytes than it was given, and `write_all` would resend the
    /// difference, duplicating content in the file and in its digest. Writing
    /// the transformed bytes with `write_all` and returning `buffer.len()` is
    /// the shape that is always correct.
    fn wrap_stream(&self, sink: Box<dyn Write + Send>) -> Box<dyn Write + Send>;
}

/// The redactor that changes nothing.
///
/// The v0.3 default. It exists so the hook can be mandatory before any rules
/// are written: a store built with it behaves exactly as one with no redaction
/// at all, while every write path is already committed to going through one.
#[derive(Clone, Copy, Debug, Default)]
pub struct PassThrough;

impl Redactor for PassThrough {
    fn redact_text<'a>(&self, text: &'a str) -> Cow<'a, str> {
        Cow::Borrowed(text)
    }

    fn wrap_stream(&self, sink: Box<dyn Write + Send>) -> Box<dyn Write + Send> {
        sink
    }
}

/// Rewrites every string value in a payload, leaving its structure intact.
///
/// Applying the redactor to the *encoded* JSON instead would be simpler and
/// wrong: a rule that rewrites a quote or a backslash would produce a column
/// that no longer parses, turning a redaction into a corrupt row.
pub(crate) fn redact_payload(redactor: &dyn Redactor, payload: &Value) -> Value {
    match payload {
        Value::String(text) => Value::String(redactor.redact_text(text).into_owned()),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| redact_payload(redactor, item))
                .collect(),
        ),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(name, value)| (name.clone(), redact_payload(redactor, value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::borrow::Cow;
    use std::io::Write;

    use serde_json::json;

    use super::{PassThrough, Redactor, redact_payload};

    /// A redactor whose effect is impossible to miss.
    ///
    /// Uppercasing is not a redaction rule anybody wants; it is a rule a test
    /// can assert reached every byte, which is the property this hook has to
    /// hold before #103 supplies rules worth having.
    #[derive(Clone, Copy, Debug, Default)]
    pub(in crate::store) struct Shouting;

    impl Redactor for Shouting {
        fn redact_text<'a>(&self, text: &'a str) -> Cow<'a, str> {
            Cow::Owned(text.to_uppercase())
        }

        fn wrap_stream(&self, sink: Box<dyn Write + Send>) -> Box<dyn Write + Send> {
            Box::new(ShoutingStream(sink))
        }
    }

    /// A redactor shaped like a real rule: it scrubs values and leaves streams
    /// alone.
    ///
    /// The trait permits exactly this — a rule about JSON string values has
    /// nothing to say about arbitrary bytes — so it is the shape that catches a
    /// store relying on `wrap_stream` to scrub something it had already decided
    /// to redact by value.
    #[derive(Clone, Copy, Debug, Default)]
    pub(in crate::store) struct Masking;

    /// A value-only rule whose visible wrapper proves it ran exactly once.
    #[derive(Clone, Copy, Debug, Default)]
    pub(in crate::store) struct NonIdempotentValueOnly;

    /// The one thing [`Masking`] knows to look for.
    pub(in crate::store) const SECRET: &str = "hunter2";

    /// What [`Masking`] leaves in its place.
    pub(in crate::store) const MASK: &str = "[redacted]";

    impl Redactor for Masking {
        fn redact_text<'a>(&self, text: &'a str) -> Cow<'a, str> {
            if text.contains(SECRET) {
                return Cow::Owned(text.replace(SECRET, MASK));
            }
            Cow::Borrowed(text)
        }

        fn wrap_stream(&self, sink: Box<dyn Write + Send>) -> Box<dyn Write + Send> {
            sink
        }
    }

    impl Redactor for NonIdempotentValueOnly {
        fn redact_text<'a>(&self, text: &'a str) -> Cow<'a, str> {
            Cow::Owned(format!("R({})", text.replace(SECRET, MASK)))
        }

        fn wrap_stream(&self, sink: Box<dyn Write + Send>) -> Box<dyn Write + Send> {
            sink
        }
    }

    struct ShoutingStream(Box<dyn Write + Send>);

    impl Write for ShoutingStream {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            let shouted = buffer.to_ascii_uppercase();
            self.0.write_all(&shouted)?;
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.0.flush()
        }
    }

    #[test]
    fn redaction_rewrites_string_values_and_leaves_the_shape_alone() {
        let payload = json!({
            "token": "secret",
            "counts": [1, 2],
            "nested": {"note": "quiet"},
            "flag": true,
        });

        let redacted = redact_payload(&Shouting, &payload);

        assert_eq!(
            redacted,
            json!({
                "token": "SECRET",
                "counts": [1, 2],
                "nested": {"note": "QUIET"},
                "flag": true,
            }),
            "keys, numbers and booleans must survive unchanged"
        );
    }

    #[test]
    fn the_pass_through_redactor_borrows_instead_of_copying() {
        assert!(matches!(
            PassThrough.redact_text("unchanged"),
            Cow::Borrowed("unchanged")
        ));
        assert_eq!(
            redact_payload(&PassThrough, &json!({"token": "secret"})),
            json!({"token": "secret"})
        );
    }
}
