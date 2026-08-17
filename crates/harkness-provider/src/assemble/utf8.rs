//! Turning a byte stream into text no sink can be handed half a character of.
//!
//! [`ModelEvent::TextDelta`](crate::contract::ModelEvent::TextDelta) carries a
//! `String`, so by the time an event exists the question is settled. The
//! question is real one layer down: an endpoint chops its response wherever its
//! transport happens to flush, and a three-byte character split across two
//! chunks is an ordinary occurrence rather than a corrupt stream. An adapter
//! decoding those chunks buffers the incomplete tail here rather than lossily
//! substituting it, which is what makes the guarantee above true rather than
//! merely typed.

use crate::contract::ProviderError;

/// Bytes an incomplete UTF-8 sequence can occupy while it waits for the rest.
///
/// Four is the longest encoded character, so at most three bytes are ever
/// pending: the buffer is bounded by the encoding, not by a policy.
pub const MAX_PENDING_UTF8_BYTES: usize = 3;

/// Accumulates bytes and releases only whole characters.
#[derive(Clone, Debug, Default)]
pub struct Utf8Accumulator {
    pending: Vec<u8>,
}

impl Utf8Accumulator {
    /// Builds an empty accumulator.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Absorbs `bytes` and returns every character they completed.
    ///
    /// The returned text may be empty — that is what a chunk landing inside a
    /// character means, and it is not an error.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::MalformedResponse`] for bytes that are not the
    /// start of any valid sequence. An incomplete tail is not that: it is held
    /// for the next chunk.
    pub fn push(&mut self, bytes: &[u8]) -> Result<String, ProviderError> {
        self.pending.extend_from_slice(bytes);
        match std::str::from_utf8(&self.pending) {
            Ok(text) => {
                let complete = text.to_owned();
                self.pending.clear();
                Ok(complete)
            }
            Err(error) if error.error_len().is_none() => {
                let boundary = error.valid_up_to();
                let complete = std::str::from_utf8(&self.pending[..boundary])
                    .expect("the prefix up to valid_up_to is valid by construction")
                    .to_owned();
                self.pending.drain(..boundary);
                Ok(complete)
            }
            Err(error) => Err(ProviderError::malformed_response(format!(
                "the provider sent bytes that are not UTF-8 at offset {}",
                error.valid_up_to()
            ))),
        }
    }

    /// How many bytes are waiting for the rest of their character.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Ends the stream.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::MalformedResponse`] when a character was left
    /// half-sent. A truncated character is evidence the stream was cut, and
    /// silently dropping it would turn that into text the model never wrote.
    pub fn finish(self) -> Result<(), ProviderError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        Err(ProviderError::malformed_response(format!(
            "the stream ended with {} bytes of an incomplete UTF-8 character",
            self.pending.len()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_PENDING_UTF8_BYTES, Utf8Accumulator};

    /// The property every splitting scenario depends on: however the bytes are
    /// chopped, the text that comes out is the text that went in.
    #[test]
    fn every_split_of_a_multibyte_string_reassembles_to_the_same_text() {
        let text = "café ☕ — done";
        let bytes = text.as_bytes();
        for split in 0..=bytes.len() {
            let mut accumulator = Utf8Accumulator::new();
            let mut assembled = accumulator.push(&bytes[..split]).unwrap();
            assembled.push_str(&accumulator.push(&bytes[split..]).unwrap());
            accumulator.finish().unwrap();
            assert_eq!(assembled, text, "split at {split}");
        }
    }

    #[test]
    fn a_chunk_landing_inside_a_character_releases_nothing_and_holds_the_tail() {
        let mut accumulator = Utf8Accumulator::new();
        let coffee = "☕".as_bytes();
        assert_eq!(accumulator.push(&coffee[..1]).unwrap(), "");
        assert_eq!(accumulator.pending(), 1);
        assert_eq!(accumulator.push(&coffee[1..2]).unwrap(), "");
        assert_eq!(accumulator.pending(), 2);
        assert!(accumulator.pending() <= MAX_PENDING_UTF8_BYTES);
        assert_eq!(accumulator.push(&coffee[2..]).unwrap(), "☕");
        assert_eq!(accumulator.pending(), 0);
    }

    #[test]
    fn a_truncated_character_at_the_end_of_a_stream_is_a_malformed_response() {
        let mut accumulator = Utf8Accumulator::new();
        assert_eq!(accumulator.push(&"☕".as_bytes()[..2]).unwrap(), "");
        let error = accumulator.finish().unwrap_err();
        assert_eq!(error.kind(), "malformed_response");
        assert!(error.to_string().contains("incomplete UTF-8"), "{error}");
    }

    #[test]
    fn bytes_that_start_no_sequence_are_refused_rather_than_substituted() {
        let mut accumulator = Utf8Accumulator::new();
        let error = accumulator.push(&[0x61, 0xff, 0x62]).unwrap_err();
        assert_eq!(error.kind(), "malformed_response");
        assert!(error.to_string().contains("offset 1"), "{error}");
    }
}
