//! The state machine that turns events into one assistant turn.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use crate::contract::{
    ErrorDetail, ModelEvent, ProviderError, ProviderToolCallId, StopReason, TokenUsage, TurnOutcome,
};

use super::{
    clock::{MonotonicTurnClock, TurnClock},
    turn::{AssembledToolCall, AssemblyDiagnostics, AssistantTurn, IdProvenance, ToolCallDefect},
};

/// Argument bytes one tool call may accumulate.
pub const MAX_TOOL_CALL_ARGUMENT_BYTES: usize = 1024 * 1024;

/// Text bytes one turn may accumulate.
pub const MAX_TURN_TEXT_BYTES: usize = 8 * 1024 * 1024;

/// Tool calls one turn may contain.
pub const MAX_TOOL_CALLS_PER_TURN: usize = 256;

/// What an assembler will hold for one turn.
///
/// Every bound is on something the *provider* chooses the size of, which is the
/// same reason the JSON-RPC transport bounds a peer's line length: a stream is
/// somebody else's output arriving on this process's heap. Exceeding one is a
/// [`ProviderError::MalformedResponse`] naming the cap, never a truncation —
/// silently keeping the first megabyte of a tool call would produce arguments
/// the model did not write, which is worse than refusing the turn.
///
/// The bounds are per accumulation, so what one turn may hold *at once* is
/// their product: 256 calls of a megabyte each beside eight megabytes of text,
/// or roughly 264 MiB from an endpoint determined to reach it. That ceiling is
/// deliberate rather than overlooked — the alternative is a turn-wide argument
/// budget, which refuses a legitimate turn on the strength of its neighbours —
/// and a caller that needs a tighter one sets its own limits here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AssemblyLimits {
    /// Argument bytes one call may accumulate.
    pub max_tool_call_argument_bytes: usize,
    /// Text bytes one turn may accumulate.
    pub max_turn_text_bytes: usize,
    /// Calls one turn may contain.
    pub max_tool_calls: usize,
}

impl Default for AssemblyLimits {
    fn default() -> Self {
        Self {
            max_tool_call_argument_bytes: MAX_TOOL_CALL_ARGUMENT_BYTES,
            max_turn_text_bytes: MAX_TURN_TEXT_BYTES,
            max_tool_calls: MAX_TOOL_CALLS_PER_TURN,
        }
    }
}

/// What the assembler did with one event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Absorbed {
    /// The event contributed to the turn.
    Recorded,
    /// The turn had already completed, so the event was counted and dropped.
    /// A caller must not deliver it onwards: a sink that has seen
    /// [`TurnCompleted`](ModelEvent::TurnCompleted) has seen the turn end.
    IgnoredAfterCompletion,
}

/// A tool call still accumulating.
#[derive(Clone, Debug)]
struct OpenCall {
    index: u32,
    id: ProviderToolCallId,
    id_provenance: IdProvenance,
    duplicate_of: Option<ProviderToolCallId>,
    name: Option<String>,
    raw_arguments: String,
}

/// One index's state within a turn.
#[derive(Clone, Debug)]
enum CallSlot {
    Open(OpenCall),
    Finalized(AssembledToolCall),
}

/// Reads a stream of [`ModelEvent`]s into one [`AssistantTurn`].
///
/// Push events with [`observe`](Self::observe) and end with
/// [`finish`](Self::finish). Most implementations should not do that directly:
/// [`TurnDriver`](super::TurnDriver) wraps an assembler with the sink and
/// cancellation rules the contract also requires, and is what the scripted
/// provider itself runs on.
pub struct TurnAssembler {
    limits: AssemblyLimits,
    clock: Box<dyn TurnClock>,
    started: bool,
    completed: bool,
    text: String,
    calls: BTreeMap<u32, CallSlot>,
    seen_ids: BTreeSet<ProviderToolCallId>,
    synthesized: u32,
    usage: Option<TokenUsage>,
    stop: Option<StopReason>,
    provider_request_id: Option<String>,
    diagnostics: AssemblyDiagnostics,
    event_count: u32,
    first_event_latency: Option<std::time::Duration>,
}

impl Default for TurnAssembler {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for TurnAssembler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TurnAssembler")
            .field("limits", &self.limits)
            .field("completed", &self.completed)
            .field("text_bytes", &self.text.len())
            .field("calls", &self.calls.len())
            .field("event_count", &self.event_count)
            .field("diagnostics", &self.diagnostics)
            .finish()
    }
}

impl TurnAssembler {
    /// Builds an assembler on a monotonic clock and the default limits.
    #[must_use]
    pub fn new() -> Self {
        Self::with_clock(Box::new(MonotonicTurnClock::started_now()))
    }

    /// Builds an assembler timed by `clock`.
    #[must_use]
    pub fn with_clock(clock: Box<dyn TurnClock>) -> Self {
        Self {
            limits: AssemblyLimits::default(),
            clock,
            started: false,
            completed: false,
            text: String::new(),
            calls: BTreeMap::new(),
            seen_ids: BTreeSet::new(),
            synthesized: 0,
            usage: None,
            stop: None,
            provider_request_id: None,
            diagnostics: AssemblyDiagnostics::default(),
            event_count: 0,
            first_event_latency: None,
        }
    }

    /// Replaces the accumulation bounds.
    #[must_use]
    pub fn with_limits(mut self, limits: AssemblyLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The bounds in force.
    #[must_use]
    pub const fn limits(&self) -> AssemblyLimits {
        self.limits
    }

    /// Whether the provider has said why the turn ended.
    #[must_use]
    pub const fn is_completed(&self) -> bool {
        self.completed
    }

    /// How many events have been observed, ignored ones included.
    #[must_use]
    pub const fn event_count(&self) -> u32 {
        self.event_count
    }

    /// Absorbs one event.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::MalformedResponse`] for a stream this contract
    /// cannot interpret: a delta naming an index no call started, a second call
    /// at one index, a second [`TurnStarted`](ModelEvent::TurnStarted), or an
    /// accumulation past one of the [`AssemblyLimits`].
    pub fn observe(&mut self, event: &ModelEvent) -> Result<Absorbed, ProviderError> {
        self.event_count = self.event_count.saturating_add(1);
        if self.first_event_latency.is_none() {
            self.first_event_latency = Some(self.clock.elapsed());
        }
        if self.completed {
            self.diagnostics.ignored_after_completion =
                self.diagnostics.ignored_after_completion.saturating_add(1);
            return Ok(Absorbed::IgnoredAfterCompletion);
        }

        match event {
            ModelEvent::TurnStarted {
                provider_request_id,
            } => {
                if self.started {
                    return Err(ProviderError::malformed_response(
                        "the provider started one turn twice",
                    ));
                }
                self.started = true;
                self.provider_request_id.clone_from(provider_request_id);
            }
            ModelEvent::TextDelta { text } => {
                if self.text.len() + text.len() > self.limits.max_turn_text_bytes {
                    return Err(ProviderError::malformed_response(format!(
                        "the turn's text exceeds the {} byte cap",
                        self.limits.max_turn_text_bytes
                    )));
                }
                self.text.push_str(text);
            }
            ModelEvent::ToolCallStarted { index, id, name } => {
                self.start_call(*index, id.clone(), name.clone())?;
            }
            ModelEvent::ToolCallArgumentsDelta { index, fragment } => {
                self.extend_call(*index, fragment)?;
            }
            ModelEvent::ToolCallReady { index } => self.ready_call(*index)?,
            ModelEvent::Usage {
                input_tokens,
                output_tokens,
                exact,
            } => {
                let reported = TokenUsage {
                    input_tokens: *input_tokens,
                    output_tokens: *output_tokens,
                    exact: *exact,
                };
                match &mut self.usage {
                    Some(usage) => usage.absorb(reported),
                    None => self.usage = Some(reported),
                }
            }
            ModelEvent::TurnCompleted { stop } => {
                self.finalize_open_calls(false);
                self.stop = Some(stop.clone());
                self.completed = true;
            }
        }
        Ok(Absorbed::Recorded)
    }

    fn start_call(
        &mut self,
        index: u32,
        id: Option<ProviderToolCallId>,
        name: Option<String>,
    ) -> Result<(), ProviderError> {
        if self.calls.contains_key(&index) {
            return Err(ProviderError::malformed_response(format!(
                "the provider started a second tool call at index {index}"
            )));
        }
        if self.calls.len() >= self.limits.max_tool_calls {
            return Err(ProviderError::malformed_response(format!(
                "the turn exceeds the {} tool call cap",
                self.limits.max_tool_calls
            )));
        }

        let (id, id_provenance) = match id {
            Some(id) => (id, IdProvenance::Provider),
            None => {
                self.synthesized = self.synthesized.saturating_add(1);
                self.diagnostics.synthesized_ids =
                    self.diagnostics.synthesized_ids.saturating_add(1);
                (
                    ProviderToolCallId::synthesized(self.synthesized),
                    IdProvenance::Synthesized,
                )
            }
        };
        let duplicate_of = if self.seen_ids.insert(id.clone()) {
            None
        } else {
            self.diagnostics.duplicate_ids = self.diagnostics.duplicate_ids.saturating_add(1);
            Some(id.clone())
        };

        self.calls.insert(
            index,
            CallSlot::Open(OpenCall {
                index,
                id,
                id_provenance,
                duplicate_of,
                name,
                raw_arguments: String::new(),
            }),
        );
        Ok(())
    }

    fn extend_call(&mut self, index: u32, fragment: &str) -> Result<(), ProviderError> {
        let limit = self.limits.max_tool_call_argument_bytes;
        match self.calls.get_mut(&index) {
            Some(CallSlot::Open(call)) => {
                if call.raw_arguments.len() + fragment.len() > limit {
                    return Err(ProviderError::malformed_response(format!(
                        "tool call {index} exceeds the {limit} byte argument cap"
                    )));
                }
                call.raw_arguments.push_str(fragment);
                Ok(())
            }
            Some(CallSlot::Finalized(_)) => Err(ProviderError::malformed_response(format!(
                "the provider sent arguments for tool call {index} after it was ready"
            ))),
            None => Err(ProviderError::malformed_response(format!(
                "the provider sent arguments for tool call {index}, which no call started"
            ))),
        }
    }

    fn ready_call(&mut self, index: u32) -> Result<(), ProviderError> {
        match self.calls.remove(&index) {
            Some(CallSlot::Open(call)) => {
                self.calls
                    .insert(index, CallSlot::Finalized(finalize(call, false)));
                Ok(())
            }
            Some(finalized @ CallSlot::Finalized(_)) => {
                self.calls.insert(index, finalized);
                Err(ProviderError::malformed_response(format!(
                    "the provider readied tool call {index} twice"
                )))
            }
            None => Err(ProviderError::malformed_response(format!(
                "the provider readied tool call {index}, which no call started"
            ))),
        }
    }

    /// Closes every call still accumulating.
    ///
    /// `truncated` says whether the stream ended under the call rather than
    /// after it, which is the difference between "the model wrote this" and
    /// "this is as much as arrived".
    fn finalize_open_calls(&mut self, truncated: bool) {
        let open = self
            .calls
            .iter()
            .filter_map(|(index, slot)| matches!(slot, CallSlot::Open(_)).then_some(*index))
            .collect::<Vec<_>>();
        for index in open {
            let Some(CallSlot::Open(call)) = self.calls.remove(&index) else {
                continue;
            };
            self.calls
                .insert(index, CallSlot::Finalized(finalize(call, truncated)));
        }
    }

    /// Ends the turn.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::EmptyResponse`] when nothing arrived at all,
    /// and [`ProviderError::Disconnected`] — carrying the partial turn — when
    /// the stream ended without the provider saying why.
    pub fn finish(self) -> Result<TurnOutcome, ProviderError> {
        if self.completed {
            let stop = self.stop.clone().unwrap_or(StopReason::EndTurn);
            return Ok(self.into_outcome(stop, false));
        }
        if self.event_count == 0 {
            return Err(ProviderError::EmptyResponse);
        }
        Err(ProviderError::Disconnected {
            detail: ErrorDetail::new("the stream ended before the turn completed"),
            partial: Some(Box::new(self.into_partial())),
        })
    }

    /// Ends the turn because the sink asked it to.
    ///
    /// Not an error: the caller stopped its own turn, so what it gets back is
    /// the turn as far as it went, stopped [`AbortedBySink`](StopReason::AbortedBySink).
    #[must_use]
    pub fn abort_by_sink(self) -> TurnOutcome {
        self.into_outcome(StopReason::AbortedBySink, true)
    }

    /// The turn as far as it went, for attaching to a failure.
    #[must_use]
    pub fn into_partial(mut self) -> AssistantTurn {
        self.take_turn(true)
    }

    fn into_outcome(mut self, stop: StopReason, truncated: bool) -> TurnOutcome {
        let elapsed = self.clock.elapsed();
        let event_count = self.event_count;
        let first_event_latency = self.first_event_latency;
        let diagnostics = self.diagnostics;
        let provider_request_id = self.provider_request_id.take();
        let usage = self.usage;
        TurnOutcome {
            turn: self.take_turn(truncated),
            stop,
            usage,
            provider_request_id,
            elapsed,
            first_event_latency,
            event_count,
            diagnostics,
        }
    }

    fn take_turn(&mut self, truncated: bool) -> AssistantTurn {
        self.finalize_open_calls(truncated);
        let tool_calls = std::mem::take(&mut self.calls)
            .into_values()
            .map(|slot| match slot {
                CallSlot::Finalized(call) => call,
                // `finalize_open_calls` ran immediately above, so every slot is
                // finalized. Closing an open one here rather than unwrapping
                // keeps the impossible case from being a panic in a library.
                CallSlot::Open(call) => finalize(call, truncated),
            })
            .collect();
        AssistantTurn {
            text: std::mem::take(&mut self.text),
            tool_calls,
            usage: self.usage,
            stop: self.stop.clone(),
        }
    }
}

/// Turns one accumulated call into its record.
///
/// Defect precedence is fixed here, highest first: a truncated stream, then a
/// call the provider never named, then arguments that are not one JSON value.
/// Half an object failing to parse *because* the stream was cut is one finding,
/// not two, and reporting the parse error would describe the symptom.
fn finalize(call: OpenCall, truncated: bool) -> AssembledToolCall {
    let OpenCall {
        index,
        id,
        id_provenance,
        duplicate_of,
        name,
        raw_arguments,
    } = call;

    if truncated {
        return AssembledToolCall::Invalid {
            index,
            id,
            id_provenance,
            duplicate_of,
            name,
            raw_arguments,
            defect: ToolCallDefect::Truncated,
        };
    }
    let Some(named) = name else {
        return AssembledToolCall::Invalid {
            index,
            id,
            id_provenance,
            duplicate_of,
            name: None,
            raw_arguments,
            defect: ToolCallDefect::MissingName,
        };
    };
    // A call whose arguments never received a fragment is a call with no
    // arguments: providers omit the field entirely for a tool that takes none,
    // and reading that as a parse failure would refuse the commonest call there
    // is. A *truncated* stream is handled above, so this cannot absorb one.
    let parsed = if raw_arguments.trim().is_empty() {
        Ok(Value::Object(Map::new()))
    } else {
        serde_json::from_str::<Value>(&raw_arguments)
    };
    match parsed {
        Ok(arguments) => AssembledToolCall::Ready {
            index,
            id,
            id_provenance,
            duplicate_of,
            name: named,
            arguments,
        },
        Err(error) => AssembledToolCall::Invalid {
            index,
            id,
            id_provenance,
            duplicate_of,
            name: Some(named),
            raw_arguments,
            defect: ToolCallDefect::UnparsableArguments {
                detail: ErrorDetail::new(error.to_string()),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        AssemblyLimits, MAX_TOOL_CALL_ARGUMENT_BYTES, MAX_TOOL_CALLS_PER_TURN, MAX_TURN_TEXT_BYTES,
        TurnAssembler,
    };
    use crate::{
        assemble::{IdProvenance, ToolCallDefect, Utf8Accumulator},
        contract::{ModelEvent, ProviderToolCallId, StopReason, TurnOutcome},
    };

    fn started(index: u32, id: Option<&str>, name: &str) -> ModelEvent {
        ModelEvent::ToolCallStarted {
            index,
            id: id.map(|id| ProviderToolCallId::new(id).unwrap()),
            name: Some(name.to_owned()),
        }
    }

    fn fragment(index: u32, text: &str) -> ModelEvent {
        ModelEvent::ToolCallArgumentsDelta {
            index,
            fragment: text.to_owned(),
        }
    }

    fn completed() -> ModelEvent {
        ModelEvent::TurnCompleted {
            stop: StopReason::ToolUse,
        }
    }

    fn assemble(events: &[ModelEvent]) -> TurnOutcome {
        let mut assembler = TurnAssembler::new();
        for event in events {
            assembler.observe(event).expect("a well-formed stream");
        }
        assembler.finish().expect("a completed turn")
    }

    /// The acceptance criterion, exhaustively rather than by sampling: every
    /// byte offset, including the ones inside the multi-byte characters, and
    /// then one byte at a time.
    #[test]
    fn a_call_split_at_every_byte_offset_assembles_identically() {
        let arguments = r#"{"query":"café ☕","limit":5}"#;
        let unsplit = assemble(&[
            started(0, Some("call_1"), "workspace.search"),
            fragment(0, arguments),
            ModelEvent::ToolCallReady { index: 0 },
            completed(),
        ]);

        let bytes = arguments.as_bytes();
        for offset in 0..=bytes.len() {
            let mut accumulator = Utf8Accumulator::new();
            let first = accumulator.push(&bytes[..offset]).unwrap();
            let second = accumulator.push(&bytes[offset..]).unwrap();
            accumulator.finish().unwrap();

            let split = assemble(&[
                started(0, Some("call_1"), "workspace.search"),
                fragment(0, &first),
                fragment(0, &second),
                ModelEvent::ToolCallReady { index: 0 },
                completed(),
            ]);
            assert_eq!(
                split.turn.tool_calls, unsplit.turn.tool_calls,
                "split at byte {offset}"
            );
        }

        let mut accumulator = Utf8Accumulator::new();
        let mut events = vec![started(0, Some("call_1"), "workspace.search")];
        for byte in bytes {
            let released = accumulator.push(&[*byte]).unwrap();
            if !released.is_empty() {
                events.push(fragment(0, &released));
            }
        }
        accumulator.finish().unwrap();
        events.push(ModelEvent::ToolCallReady { index: 0 });
        events.push(completed());
        assert_eq!(assemble(&events).turn.tool_calls, unsplit.turn.tool_calls);
    }

    #[test]
    fn interleaved_calls_accumulate_independently_and_report_in_index_order() {
        let outcome = assemble(&[
            started(1, Some("call_b"), "fs.read"),
            started(0, Some("call_a"), "workspace.search"),
            fragment(1, "{\"path\":"),
            fragment(0, "{\"query\":"),
            fragment(1, "\"src/lib.rs\"}"),
            fragment(0, "\"needle\"}"),
            ModelEvent::ToolCallReady { index: 1 },
            ModelEvent::ToolCallReady { index: 0 },
            completed(),
        ]);

        let calls = &outcome.turn.tool_calls;
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].index(), 0, "index order, not arrival order");
        assert_eq!(calls[0].arguments(), Some(&json!({"query": "needle"})));
        assert_eq!(calls[1].arguments(), Some(&json!({"path": "src/lib.rs"})));
    }

    #[test]
    fn a_call_the_provider_never_named_is_given_a_deterministic_id() {
        let outcome = assemble(&[
            ModelEvent::ToolCallStarted {
                index: 0,
                id: None,
                name: Some("fs.read".to_owned()),
            },
            fragment(0, "{}"),
            ModelEvent::ToolCallReady { index: 0 },
            completed(),
        ]);

        let call = &outcome.turn.tool_calls[0];
        assert_eq!(call.id().as_str(), "harkness-synth-1");
        assert_eq!(call.id_provenance(), IdProvenance::Synthesized);
        assert!(call.is_ready());
        assert_eq!(outcome.diagnostics.synthesized_ids, 1);
    }

    #[test]
    fn a_repeated_identity_is_marked_and_never_merged() {
        let outcome = assemble(&[
            started(0, Some("call_1"), "fs.read"),
            fragment(0, "{\"path\":\"a\"}"),
            ModelEvent::ToolCallReady { index: 0 },
            started(1, Some("call_1"), "fs.read"),
            fragment(1, "{\"path\":\"b\"}"),
            ModelEvent::ToolCallReady { index: 1 },
            completed(),
        ]);

        assert_eq!(outcome.turn.tool_calls.len(), 2, "two calls, not one");
        assert_eq!(outcome.turn.tool_calls[0].duplicate_of(), None);
        assert_eq!(
            outcome.turn.tool_calls[1]
                .duplicate_of()
                .map(|id| id.as_str()),
            Some("call_1")
        );
        assert_eq!(
            outcome.turn.tool_calls[1].arguments(),
            Some(&json!({"path": "b"})),
            "the second call keeps its own arguments"
        );
        assert_eq!(outcome.diagnostics.duplicate_ids, 1);
    }

    #[test]
    fn none_of_the_three_broken_shapes_is_dropped_from_the_turn() {
        let outcome = assemble(&[
            // Unparsable arguments.
            started(0, Some("call_1"), "fs.read"),
            fragment(0, "{\"path\":"),
            ModelEvent::ToolCallReady { index: 0 },
            // No identity.
            ModelEvent::ToolCallStarted {
                index: 1,
                id: None,
                name: Some("fs.read".to_owned()),
            },
            fragment(1, "{}"),
            ModelEvent::ToolCallReady { index: 1 },
            // A repeated identity.
            started(2, Some("call_1"), "fs.read"),
            fragment(2, "{}"),
            ModelEvent::ToolCallReady { index: 2 },
            completed(),
        ]);

        assert_eq!(outcome.turn.tool_calls.len(), 3);
        assert!(matches!(
            outcome.turn.tool_calls[0].defect(),
            Some(ToolCallDefect::UnparsableArguments { .. })
        ));
        assert_eq!(
            outcome.turn.tool_calls[1].id_provenance(),
            IdProvenance::Synthesized
        );
        assert!(outcome.turn.tool_calls[2].duplicate_of().is_some());
        assert_eq!(outcome.turn.invalid_calls().count(), 1);
        assert_eq!(outcome.turn.ready_calls().count(), 2);
    }

    #[test]
    fn a_call_the_provider_never_named_at_all_cannot_be_executed() {
        let outcome = assemble(&[
            ModelEvent::ToolCallStarted {
                index: 0,
                id: Some(ProviderToolCallId::new("call_1").unwrap()),
                name: None,
            },
            fragment(0, "{}"),
            ModelEvent::ToolCallReady { index: 0 },
            completed(),
        ]);
        assert_eq!(
            outcome.turn.tool_calls[0].defect(),
            Some(&ToolCallDefect::MissingName)
        );
    }

    #[test]
    fn a_call_with_no_arguments_at_all_is_a_call_with_no_arguments() {
        let outcome = assemble(&[
            started(0, Some("call_1"), "workspace.inspect"),
            ModelEvent::ToolCallReady { index: 0 },
            completed(),
        ]);
        assert_eq!(outcome.turn.tool_calls[0].arguments(), Some(&json!({})));
    }

    #[test]
    fn a_call_left_open_when_the_turn_completes_is_still_finalized() {
        let outcome = assemble(&[
            started(0, Some("call_1"), "fs.read"),
            fragment(0, "{\"path\":\"a\"}"),
            completed(),
        ]);
        assert!(
            outcome.turn.tool_calls[0].is_ready(),
            "a provider that omits the ready event still described one call"
        );
    }

    #[test]
    fn a_delta_naming_an_index_no_call_started_is_a_malformed_response() {
        let mut assembler = TurnAssembler::new();
        let error = assembler.observe(&fragment(3, "{}")).unwrap_err();
        assert_eq!(error.kind(), "malformed_response");
        assert!(error.to_string().contains("tool call 3"), "{error}");
    }

    #[test]
    fn a_second_call_at_one_index_is_a_malformed_response() {
        let mut assembler = TurnAssembler::new();
        assembler
            .observe(&started(0, Some("call_1"), "fs.read"))
            .unwrap();
        let error = assembler
            .observe(&started(0, Some("call_2"), "fs.read"))
            .unwrap_err();
        assert_eq!(error.kind(), "malformed_response");
        assert!(error.to_string().contains("index 0"), "{error}");
    }

    #[test]
    fn a_second_turn_start_is_a_malformed_response() {
        let mut assembler = TurnAssembler::new();
        let start = ModelEvent::TurnStarted {
            provider_request_id: None,
        };
        assembler.observe(&start).unwrap();
        let error = assembler.observe(&start).unwrap_err();
        assert_eq!(error.kind(), "malformed_response");
    }

    #[test]
    fn arguments_after_a_call_is_ready_are_a_malformed_response() {
        let mut assembler = TurnAssembler::new();
        assembler
            .observe(&started(0, Some("call_1"), "fs.read"))
            .unwrap();
        assembler
            .observe(&ModelEvent::ToolCallReady { index: 0 })
            .unwrap();
        let error = assembler.observe(&fragment(0, "{}")).unwrap_err();
        assert_eq!(error.kind(), "malformed_response");
        assert!(error.to_string().contains("after it was ready"), "{error}");
    }

    #[test]
    fn exceeding_the_argument_cap_names_the_cap() {
        let mut assembler = TurnAssembler::new();
        assembler
            .observe(&started(0, Some("call_1"), "fs.read"))
            .unwrap();
        let oversized = "x".repeat(MAX_TOOL_CALL_ARGUMENT_BYTES + 1);
        let error = assembler.observe(&fragment(0, &oversized)).unwrap_err();
        assert_eq!(error.kind(), "malformed_response");
        assert!(
            error
                .to_string()
                .contains(&format!("{MAX_TOOL_CALL_ARGUMENT_BYTES} byte argument cap")),
            "{error}"
        );
    }

    #[test]
    fn exceeding_the_turn_text_cap_names_the_cap() {
        let mut assembler = TurnAssembler::new();
        let oversized = "x".repeat(MAX_TURN_TEXT_BYTES + 1);
        let error = assembler
            .observe(&ModelEvent::TextDelta { text: oversized })
            .unwrap_err();
        assert_eq!(error.kind(), "malformed_response");
        assert!(
            error
                .to_string()
                .contains(&format!("{MAX_TURN_TEXT_BYTES} byte cap")),
            "{error}"
        );
    }

    /// The cap is checked against what *would* be held, so the assembler never
    /// grows past it and then complains.
    #[test]
    fn a_cap_is_refused_before_the_bytes_are_held() {
        let limits = AssemblyLimits {
            max_turn_text_bytes: 8,
            ..AssemblyLimits::default()
        };
        let mut assembler = TurnAssembler::new().with_limits(limits);
        assembler
            .observe(&ModelEvent::TextDelta {
                text: "12345678".to_owned(),
            })
            .unwrap();
        assert!(
            assembler
                .observe(&ModelEvent::TextDelta {
                    text: "9".to_owned()
                })
                .is_err()
        );
    }

    #[test]
    fn exceeding_the_call_count_cap_names_the_cap() {
        let mut assembler = TurnAssembler::new();
        for index in 0..MAX_TOOL_CALLS_PER_TURN {
            assembler
                .observe(&started(
                    u32::try_from(index).unwrap(),
                    Some(&format!("call_{index}")),
                    "fs.read",
                ))
                .unwrap();
        }
        let error = assembler
            .observe(&started(9_999, Some("call_over"), "fs.read"))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&format!("{MAX_TOOL_CALLS_PER_TURN} tool call cap")),
            "{error}"
        );
    }

    #[test]
    fn usage_reported_in_pieces_is_merged_into_one_report() {
        let outcome = assemble(&[
            ModelEvent::Usage {
                input_tokens: Some(400),
                output_tokens: None,
                exact: true,
            },
            ModelEvent::Usage {
                input_tokens: None,
                output_tokens: Some(90),
                exact: false,
            },
            completed(),
        ]);
        let usage = outcome.usage.expect("the provider reported usage");
        assert_eq!(usage.input_tokens, Some(400));
        assert_eq!(usage.output_tokens, Some(90));
        assert!(!usage.exact);
        assert_eq!(outcome.turn.usage, Some(usage));
    }

    /// Issue [#111] budgets per-event assembly overhead at ten microseconds in
    /// a release build, because every token of every turn passes through here.
    ///
    /// [#111]: https://github.com/fullstacktaiye/harkness/issues/111
    #[test]
    #[ignore = "latency target; meaningful only in a release build"]
    fn event_dispatch_meets_the_latency_target() {
        const EVENTS: usize = 200_000;

        let text = ModelEvent::TextDelta {
            text: "a moderately sized token".to_owned(),
        };
        let mut assembler = TurnAssembler::new().with_limits(AssemblyLimits {
            max_turn_text_bytes: usize::MAX,
            ..AssemblyLimits::default()
        });

        let started = std::time::Instant::now();
        for _ in 0..EVENTS {
            assembler.observe(&text).unwrap();
        }
        let per_event = started.elapsed() / u32::try_from(EVENTS).unwrap();
        harkness_test_fixtures::latency::record(
            "provider::assemble_text_delta",
            per_event,
            std::time::Duration::from_micros(10),
        );

        let mut assembler = TurnAssembler::new().with_limits(AssemblyLimits {
            max_tool_call_argument_bytes: usize::MAX,
            ..AssemblyLimits::default()
        });
        assembler
            .observe(&started_call_for_benchmark())
            .expect("one call opens");
        let delta = fragment(0, "{\"chunk\":\"of arguments\"},");
        let began = std::time::Instant::now();
        for _ in 0..EVENTS {
            assembler.observe(&delta).unwrap();
        }
        let per_argument_event = began.elapsed() / u32::try_from(EVENTS).unwrap();
        harkness_test_fixtures::latency::record(
            "provider::assemble_tool_call_argument_delta",
            per_argument_event,
            std::time::Duration::from_micros(10),
        );
    }

    #[cfg(test)]
    fn started_call_for_benchmark() -> ModelEvent {
        started(0, Some("call_1"), "workspace.search")
    }
}
