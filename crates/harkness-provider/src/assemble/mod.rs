//! Turning a stream of events into one validated assistant turn.
//!
//! A provider streams fragments: text in pieces, a tool call's name before its
//! arguments, those arguments in chunks that are not JSON on their own, and
//! several calls interleaved. Assembly is what turns that into a value the
//! runtime can act on — and, just as importantly, what refuses to turn a broken
//! stream into a plausible-looking one.
//!
//! # Guarantees
//!
//! - **Nothing is silently dropped.** A call whose arguments do not parse, one
//!   the provider never named, and one cut off by a disconnect all appear in
//!   [`AssistantTurn::tool_calls`] as [`AssembledToolCall::Invalid`]. They are
//!   never executed and never omitted: a turn that quietly lost a call would
//!   describe a conversation that did not happen.
//! - **Identity is recorded, not inferred.** A call the provider left unnamed
//!   gets a deterministic turn-scoped id and is marked
//!   [`IdProvenance::Synthesized`]. A call repeating an id an earlier call in
//!   the same turn used keeps both entries and marks the second
//!   [`duplicate_of`](AssembledToolCall::duplicate_of) — merging them would run
//!   one call twice or not at all.
//! - **Fragments are position-independent.** Argument text is keyed by the
//!   call's `index`, so interleaved calls accumulate separately and a stream
//!   chopped at any byte offset assembles to the same call as an unchopped one.
//!   [`Utf8Accumulator`] is what makes the byte-level half of that true for an
//!   adapter decoding a transport.
//! - **Everything is bounded.** [`AssemblyLimits`] caps argument bytes per
//!   call, text bytes per turn, and calls per turn. A provider chooses all
//!   three sizes, so all three are refused rather than absorbed.
//!
//! # Using it
//!
//! [`TurnDriver`] is the entry point an implementation of
//! [`ModelProvider`](crate::contract::ModelProvider) uses; it owns a
//! [`TurnAssembler`] and adds the sink and cancellation rules. Reach for the
//! assembler directly only to assemble events that are already in hand.
//!
//! ```
//! use harkness_provider::{
//!     assemble::TurnAssembler,
//!     contract::{ModelEvent, StopReason},
//! };
//!
//! let mut assembler = TurnAssembler::new();
//! assembler.observe(&ModelEvent::TextDelta { text: "Hello, ".to_owned() })?;
//! assembler.observe(&ModelEvent::TextDelta { text: "world.".to_owned() })?;
//! assembler.observe(&ModelEvent::TurnCompleted { stop: StopReason::EndTurn })?;
//!
//! let outcome = assembler.finish()?;
//! assert_eq!(outcome.turn.text, "Hello, world.");
//! # Ok::<(), harkness_provider::contract::ProviderError>(())
//! ```

mod assembler;
mod clock;
mod driver;
mod turn;
mod utf8;

pub use assembler::{
    Absorbed, AssemblyLimits, MAX_TOOL_CALL_ARGUMENT_BYTES, MAX_TOOL_CALLS_PER_TURN,
    MAX_TURN_TEXT_BYTES, TurnAssembler,
};
pub use clock::{ManualTurnClock, MonotonicTurnClock, TurnClock};
pub use driver::TurnDriver;
pub use turn::{
    AssembledToolCall, AssemblyDiagnostics, AssistantTurn, IdProvenance, ToolCallDefect,
};
pub use utf8::{MAX_PENDING_UTF8_BYTES, Utf8Accumulator};
