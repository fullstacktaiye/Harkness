//! What a provider streams back, and what one finished turn reports.
//!
//! # Event ordering
//!
//! The rules an implementation must hold to, and the assembler enforces:
//!
//! 1. [`TurnStarted`](ModelEvent::TurnStarted) arrives at most once, and by
//!    convention first. A second one is a
//!    [`malformed_response`](super::ProviderError::MalformedResponse); its
//!    *position* is not policed, because a late one costs a reader nothing and
//!    refusing it would fail turns over a detail this contract does not need.
//! 2. A tool call is introduced by [`ToolCallStarted`](ModelEvent::ToolCallStarted)
//!    at an `index` no other call in the turn uses, and every later
//!    [`ToolCallArgumentsDelta`](ModelEvent::ToolCallArgumentsDelta) and
//!    [`ToolCallReady`](ModelEvent::ToolCallReady) names that index. Indices
//!    interleave freely; a delta naming an index nothing started is a
//!    [`malformed_response`](super::ProviderError::MalformedResponse).
//! 3. [`TurnCompleted`](ModelEvent::TurnCompleted) is last. Anything after it is
//!    ignored and counted, because a turn that has said why it stopped has
//!    stopped.
//! 4. [`Usage`](ModelEvent::Usage) may arrive at any point and more than once;
//!    later reports override earlier ones field by field.

use std::{fmt, time::Duration};

use serde::{Deserialize, Serialize};

use crate::{
    assemble::{AssemblyDiagnostics, AssistantTurn},
    text::Preview,
};

/// Bytes of any one string a `Debug` rendering shows.
const DEBUG_TEXT_BYTES: usize = 48;

/// How often a blocking provider implementation must poll its cancellation.
///
/// The workspace's visibility target is 250 ms and every other blocking seam —
/// the Git runner, the tool executor, the JSON-RPC transport — polls at 20 ms,
/// so a provider that waits longer than this between checks is the one thing a
/// user notices when they press stop.
pub const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// One thing a provider said while producing a turn.
///
/// `#[non_exhaustive]`: a provider behavior nobody has seen yet is added here
/// without breaking an adapter or the assembler, which is the same additive
/// discipline the run event log uses for its kinds.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum ModelEvent {
    /// The provider accepted the request and began a turn.
    TurnStarted {
        /// The endpoint's own identifier for this request, when it gave one.
        /// Recorded so a support conversation about one turn can name it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_request_id: Option<String>,
    },
    /// More assistant text.
    TextDelta {
        /// The fragment. Always valid UTF-8: an adapter decoding a byte stream
        /// buffers an incomplete sequence rather than emitting one.
        text: String,
    },
    /// A tool call was introduced.
    ToolCallStarted {
        /// Position of this call within the turn.
        index: u32,
        /// The provider's identity for it, when it issued one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<super::ProviderToolCallId>,
        /// The tool named, when the provider named it up front.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// More argument text for one call.
    ToolCallArgumentsDelta {
        /// The call being described.
        index: u32,
        /// The fragment, which need not be valid JSON on its own.
        fragment: String,
    },
    /// One call's arguments are complete.
    ToolCallReady {
        /// The call that is complete.
        index: u32,
    },
    /// What the turn cost, as the provider counts it.
    Usage {
        /// Tokens the request occupied.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_tokens: Option<u64>,
        /// Tokens the turn produced.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_tokens: Option<u64>,
        /// Whether the provider counted rather than estimated. A `false` here
        /// travels all the way into [`TurnOutcome`], because a budget built on
        /// an estimate that was reported as exact is a budget that overruns.
        exact: bool,
    },
    /// The turn ended, and why.
    TurnCompleted {
        /// Why it ended.
        stop: StopReason,
    },
}

impl ModelEvent {
    /// Every stable event spelling, in declaration order.
    pub const KINDS: &'static [&'static str] = &[
        "turn_started",
        "text_delta",
        "tool_call_started",
        "tool_call_arguments_delta",
        "tool_call_ready",
        "usage",
        "turn_completed",
    ];

    /// Stable machine-readable spelling.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::TurnStarted { .. } => "turn_started",
            Self::TextDelta { .. } => "text_delta",
            Self::ToolCallStarted { .. } => "tool_call_started",
            Self::ToolCallArgumentsDelta { .. } => "tool_call_arguments_delta",
            Self::ToolCallReady { .. } => "tool_call_ready",
            Self::Usage { .. } => "usage",
            Self::TurnCompleted { .. } => "turn_completed",
        }
    }
}

/// Truncated on purpose, for the same reason a turn's rendering is: an event
/// *is* a piece of the conversation, and a sink implementation is the most
/// likely thing in the system to log one. Bounding the assembled turn while
/// leaving the events it was built from unbounded would move the same megabyte
/// one type earlier rather than removing it.
impl fmt::Debug for ModelEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TurnStarted {
                provider_request_id,
            } => formatter
                .debug_struct("TurnStarted")
                .field(
                    "provider_request_id",
                    &provider_request_id
                        .as_deref()
                        .map(|id| Preview::new(id, DEBUG_TEXT_BYTES)),
                )
                .finish(),
            Self::TextDelta { text } => formatter
                .debug_struct("TextDelta")
                .field("text", &Preview::new(text, DEBUG_TEXT_BYTES))
                .finish(),
            Self::ToolCallStarted { index, id, name } => formatter
                .debug_struct("ToolCallStarted")
                .field("index", index)
                .field("id", id)
                .field(
                    "name",
                    &name
                        .as_deref()
                        .map(|name| Preview::new(name, DEBUG_TEXT_BYTES)),
                )
                .finish(),
            Self::ToolCallArgumentsDelta { index, fragment } => formatter
                .debug_struct("ToolCallArgumentsDelta")
                .field("index", index)
                .field("fragment", &Preview::new(fragment, DEBUG_TEXT_BYTES))
                .finish(),
            Self::ToolCallReady { index } => formatter
                .debug_struct("ToolCallReady")
                .field("index", index)
                .finish(),
            Self::Usage {
                input_tokens,
                output_tokens,
                exact,
            } => formatter
                .debug_struct("Usage")
                .field("input_tokens", input_tokens)
                .field("output_tokens", output_tokens)
                .field("exact", exact)
                .finish(),
            Self::TurnCompleted { stop } => formatter
                .debug_struct("TurnCompleted")
                .field("stop", stop)
                .finish(),
        }
    }
}

/// Why a turn ended.
#[derive(Clone, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StopReason {
    /// The model finished what it had to say.
    EndTurn,
    /// The model is waiting for the tool calls it requested.
    ToolUse,
    /// The output bound was reached.
    MaxOutput,
    /// The model declined.
    Refusal,
    /// The caller's sink asked for the turn to stop. Not something a provider
    /// reports: it is what a turn Harkness itself ended is recorded as.
    AbortedBySink,
    /// A spelling this build does not define, carried verbatim.
    Other(String),
}

/// Previewed too. A spelling outside the defined table is whatever the endpoint
/// sent, and nothing bounds it on the way in.
impl fmt::Debug for StopReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Other(spelling) => formatter
                .debug_tuple("Other")
                .field(&Preview::new(spelling, DEBUG_TEXT_BYTES))
                .finish(),
            // `Other` is matched above, and the arm below still answers for it
            // rather than asserting it cannot arrive: a panic inside `Debug`
            // would take down whatever was trying to report a problem.
            defined => formatter.write_str(match defined {
                Self::EndTurn => "EndTurn",
                Self::ToolUse => "ToolUse",
                Self::MaxOutput => "MaxOutput",
                Self::Refusal => "Refusal",
                Self::AbortedBySink => "AbortedBySink",
                Self::Other(_) => "Other",
            }),
        }
    }
}

impl StopReason {
    /// Every spelling this build defines, in declaration order.
    ///
    /// [`Other`](Self::Other) is deliberately absent: it is what a spelling
    /// outside this table is carried as.
    pub const SPELLINGS: &'static [&'static str] = &[
        "end_turn",
        "tool_use",
        "max_output",
        "refusal",
        "aborted_by_sink",
    ];

    /// Stable machine-readable spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::EndTurn => "end_turn",
            Self::ToolUse => "tool_use",
            Self::MaxOutput => "max_output",
            Self::Refusal => "refusal",
            Self::AbortedBySink => "aborted_by_sink",
            Self::Other(spelling) => spelling,
        }
    }
}

/// What one turn cost.
///
/// `exact` travels with the numbers rather than beside them because the two are
/// only meaningful together: [#122] estimates when a provider reports nothing,
/// and an estimate presented as a count is what makes a context budget wrong in
/// the direction that overflows.
///
/// [#122]: https://github.com/fullstacktaiye/harkness/issues/122
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TokenUsage {
    /// Tokens the request occupied, when reported.
    pub input_tokens: Option<u64>,
    /// Tokens the turn produced, when reported.
    pub output_tokens: Option<u64>,
    /// Whether the provider counted rather than estimated.
    pub exact: bool,
}

impl TokenUsage {
    /// Absorbs a later report.
    ///
    /// A field the later report leaves unset keeps the earlier value, because
    /// providers that report input usage first and output usage last are
    /// describing one turn in two messages. `exact` is the conservative
    /// conjunction: a turn is exactly counted only if every report that
    /// contributed to it said so.
    pub fn absorb(&mut self, later: Self) {
        if later.input_tokens.is_some() {
            self.input_tokens = later.input_tokens;
        }
        if later.output_tokens.is_some() {
            self.output_tokens = later.output_tokens;
        }
        self.exact = self.exact && later.exact;
    }
}

/// What a sink asks the provider to do next.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SinkControl {
    /// Keep streaming.
    Continue,
    /// Stop the turn. The provider stops emitting and reports
    /// [`StopReason::AbortedBySink`] with the partial turn preserved.
    Stop,
}

/// Where streamed events go while a turn is in flight.
///
/// Synchronous by construction: there is no queue between a provider and its
/// sink, so backpressure is the sink blocking its provider rather than an
/// unbounded buffer reporting the past. A sink that needs to hand work to
/// another thread owes it a bounded channel of its own.
pub trait ModelEventSink {
    /// Delivers one event and answers whether the turn should continue.
    fn event(&mut self, event: ModelEvent) -> SinkControl;
}

impl<F: FnMut(ModelEvent) -> SinkControl> ModelEventSink for F {
    fn event(&mut self, event: ModelEvent) -> SinkControl {
        self(event)
    }
}

/// A sink for callers that want the assembled turn and nothing in between.
#[derive(Clone, Copy, Debug, Default)]
pub struct DiscardEvents;

impl ModelEventSink for DiscardEvents {
    fn event(&mut self, _event: ModelEvent) -> SinkControl {
        SinkControl::Continue
    }
}

/// Events a [`RecordedEvents`] holds before it stops the turn.
pub const DEFAULT_RECORDED_EVENT_CAPACITY: usize = 4_096;

/// A sink that keeps what it was given, up to a bound.
///
/// Bounded because a provider chooses how many events it sends: a recorder that
/// grew to match would be an unbounded buffer filled by somebody else. Reaching
/// the bound stops the turn — [`StopReason::AbortedBySink`] — rather than
/// dropping events, so what was recorded is always a prefix of what happened
/// and never a sample of it.
#[derive(Clone)]
pub struct RecordedEvents {
    events: Vec<ModelEvent>,
    capacity: usize,
}

/// Summarized rather than previewed: a recorder holds thousands of events, and
/// bounding each one still renders thousands of them.
impl fmt::Debug for RecordedEvents {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecordedEvents")
            .field("events", &self.events.len())
            .field("capacity", &self.capacity)
            .finish()
    }
}

impl RecordedEvents {
    /// Records up to `capacity` events.
    #[must_use]
    pub const fn with_capacity(capacity: usize) -> Self {
        Self {
            events: Vec::new(),
            capacity,
        }
    }

    /// What was recorded.
    #[must_use]
    pub fn events(&self) -> &[ModelEvent] {
        &self.events
    }

    /// Takes what was recorded.
    #[must_use]
    pub fn into_events(self) -> Vec<ModelEvent> {
        self.events
    }

    /// Whether the bound has been reached.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.events.len() >= self.capacity
    }
}

impl Default for RecordedEvents {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_RECORDED_EVENT_CAPACITY)
    }
}

impl ModelEventSink for RecordedEvents {
    fn event(&mut self, event: ModelEvent) -> SinkControl {
        if self.is_full() {
            return SinkControl::Stop;
        }
        self.events.push(event);
        if self.is_full() {
            SinkControl::Stop
        } else {
            SinkControl::Continue
        }
    }
}

/// Everything one finished model turn is worth recording.
///
/// Assembled once, by the provider, so [#126] persists one row per turn without
/// re-deriving anything a stream already said. The distinction worth keeping in
/// mind: `turn.stop` is what the *provider* said, and [`stop`](Self::stop) is
/// what the *call* concluded — they differ exactly when Harkness ended the turn
/// itself, where the provider said nothing and the outcome says
/// [`StopReason::AbortedBySink`].
///
/// [#126]: https://github.com/fullstacktaiye/harkness/issues/126
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TurnOutcome {
    /// The assembled turn.
    pub turn: AssistantTurn,
    /// Why the turn ended, from the caller's point of view.
    pub stop: StopReason,
    /// What it cost, when the provider said.
    pub usage: Option<TokenUsage>,
    /// The endpoint's identifier for the request, when it gave one.
    pub provider_request_id: Option<String>,
    /// How long the whole turn took.
    pub elapsed: Duration,
    /// How long the first event took to arrive, which is the number a user
    /// experiences as responsiveness. `None` when no event ever arrived.
    pub first_event_latency: Option<Duration>,
    /// How many events the provider produced, ignored ones included.
    pub event_count: u32,
    /// What assembly had to work around.
    pub diagnostics: AssemblyDiagnostics,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        DiscardEvents, ModelEvent, ModelEventSink, RecordedEvents, SinkControl, StopReason,
        TokenUsage, TurnOutcome,
    };
    use crate::{
        assemble::{AssemblyDiagnostics, AssistantTurn},
        contract::ProviderToolCallId,
    };

    #[test]
    fn every_event_kind_is_declared_in_order() {
        let cases = [
            (
                ModelEvent::TurnStarted {
                    provider_request_id: None,
                },
                "turn_started",
            ),
            (
                ModelEvent::TextDelta {
                    text: String::new(),
                },
                "text_delta",
            ),
            (
                ModelEvent::ToolCallStarted {
                    index: 0,
                    id: None,
                    name: None,
                },
                "tool_call_started",
            ),
            (
                ModelEvent::ToolCallArgumentsDelta {
                    index: 0,
                    fragment: String::new(),
                },
                "tool_call_arguments_delta",
            ),
            (ModelEvent::ToolCallReady { index: 0 }, "tool_call_ready"),
            (
                ModelEvent::Usage {
                    input_tokens: None,
                    output_tokens: None,
                    exact: false,
                },
                "usage",
            ),
            (
                ModelEvent::TurnCompleted {
                    stop: StopReason::EndTurn,
                },
                "turn_completed",
            ),
        ];
        let kinds = cases.iter().map(|(_, kind)| *kind).collect::<Vec<_>>();
        assert_eq!(kinds, ModelEvent::KINDS);
        for (event, expected) in cases {
            assert_eq!(event.kind(), expected);
        }
    }

    #[test]
    fn events_round_trip_through_serde() {
        let events = vec![
            ModelEvent::TurnStarted {
                provider_request_id: Some("req_1".to_owned()),
            },
            ModelEvent::TextDelta {
                text: "hello".to_owned(),
            },
            ModelEvent::ToolCallStarted {
                index: 3,
                id: Some(ProviderToolCallId::new("call_1").unwrap()),
                name: Some("fs.read".to_owned()),
            },
            ModelEvent::ToolCallArgumentsDelta {
                index: 3,
                fragment: "{\"path\"".to_owned(),
            },
            ModelEvent::ToolCallReady { index: 3 },
            ModelEvent::Usage {
                input_tokens: Some(10),
                output_tokens: Some(20),
                exact: true,
            },
            ModelEvent::TurnCompleted {
                stop: StopReason::Other("length".to_owned()),
            },
        ];
        let json = serde_json::to_string(&events).unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<ModelEvent>>(&json).unwrap(),
            events
        );
        assert!(json.contains("\"kind\":\"turn_started\""), "{json}");
    }

    #[test]
    fn an_optional_event_field_may_be_omitted_but_an_undefined_one_may_not() {
        let started: ModelEvent =
            serde_json::from_str("{\"kind\":\"tool_call_started\",\"index\":1}").unwrap();
        assert_eq!(
            started,
            ModelEvent::ToolCallStarted {
                index: 1,
                id: None,
                name: None
            }
        );
        assert!(
            serde_json::from_str::<ModelEvent>(
                "{\"kind\":\"tool_call_ready\",\"index\":1,\"reason\":\"done\"}"
            )
            .is_err()
        );
    }

    #[test]
    fn every_defined_stop_spelling_is_declared_in_order() {
        let cases = [
            (StopReason::EndTurn, "end_turn"),
            (StopReason::ToolUse, "tool_use"),
            (StopReason::MaxOutput, "max_output"),
            (StopReason::Refusal, "refusal"),
            (StopReason::AbortedBySink, "aborted_by_sink"),
        ];
        let spellings = cases.iter().map(|(_, s)| *s).collect::<Vec<_>>();
        assert_eq!(spellings, StopReason::SPELLINGS);
        for (stop, expected) in cases {
            assert_eq!(stop.as_str(), expected);
        }
        assert_eq!(StopReason::Other("length".to_owned()).as_str(), "length");
    }

    #[test]
    fn later_usage_overrides_field_by_field_and_never_upgrades_an_estimate() {
        let mut usage = TokenUsage {
            input_tokens: Some(100),
            output_tokens: None,
            exact: true,
        };
        usage.absorb(TokenUsage {
            input_tokens: None,
            output_tokens: Some(20),
            exact: false,
        });
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(20));
        assert!(
            !usage.exact,
            "one estimated report makes the whole turn estimated"
        );
    }

    #[test]
    fn a_recorder_stops_the_turn_rather_than_growing_past_its_bound() {
        let mut sink = RecordedEvents::with_capacity(2);
        let text = ModelEvent::TextDelta {
            text: "x".to_owned(),
        };
        assert_eq!(sink.event(text.clone()), SinkControl::Continue);
        assert_eq!(sink.event(text.clone()), SinkControl::Stop);
        assert_eq!(sink.event(text), SinkControl::Stop);
        assert_eq!(sink.events().len(), 2);
    }

    #[test]
    fn a_closure_is_a_sink() {
        let mut seen = 0;
        let mut sink = |_event: ModelEvent| {
            seen += 1;
            SinkControl::Continue
        };
        sink.event(ModelEvent::ToolCallReady { index: 0 });
        assert_eq!(seen, 1);
        assert_eq!(
            DiscardEvents.event(ModelEvent::ToolCallReady { index: 0 }),
            SinkControl::Continue
        );
    }

    /// An event *is* a piece of the conversation, and a sink is the most likely
    /// thing to log one. Bounding the turn but not the events it was assembled
    /// from would leave the same megabyte reachable one type earlier.
    #[test]
    fn debugging_an_event_previews_the_text_it_carries() {
        let delta = ModelEvent::TextDelta {
            text: "x".repeat(1024 * 1024),
        };
        let rendered = format!("{delta:?}");
        assert!(rendered.len() < 1024, "{} bytes", rendered.len());

        let arguments = ModelEvent::ToolCallArgumentsDelta {
            index: 0,
            fragment: "y".repeat(1024 * 1024),
        };
        assert!(format!("{arguments:?}").len() < 1024);
    }

    /// And a recorder holds thousands of them.
    #[test]
    fn debugging_a_recorder_summarizes_rather_than_printing_what_it_holds() {
        let mut sink = RecordedEvents::with_capacity(4_096);
        for _ in 0..2_000 {
            sink.event(ModelEvent::TextDelta {
                text: "z".repeat(1_024),
            });
        }
        let rendered = format!("{sink:?}");
        assert!(rendered.len() < 256, "{} bytes", rendered.len());
        assert!(rendered.contains("2000"), "{rendered}");
    }

    #[test]
    fn an_outcome_round_trips_through_serde() {
        let outcome = TurnOutcome {
            turn: AssistantTurn::default(),
            stop: StopReason::EndTurn,
            usage: Some(TokenUsage {
                input_tokens: Some(1),
                output_tokens: Some(2),
                exact: true,
            }),
            provider_request_id: Some("req_1".to_owned()),
            elapsed: Duration::from_millis(1_234),
            first_event_latency: Some(Duration::from_millis(12)),
            event_count: 7,
            diagnostics: AssemblyDiagnostics::default(),
        };
        let json = serde_json::to_string(&outcome).unwrap();
        assert_eq!(serde_json::from_str::<TurnOutcome>(&json).unwrap(), outcome);
    }
}
