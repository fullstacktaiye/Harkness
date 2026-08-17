//! A provider that replays a written-down stream, identically, every time.
//!
//! Everything above this crate — the native agent's loop ([#126]), prompt
//! construction ([#127]), the conversation surface ([#134]) — has to be built
//! against *something*. Against a real endpoint that something is slow, costs
//! money, needs a credential, and produces a different stream every run, which
//! makes the interesting cases (a tool call split across chunks, a duplicate
//! id, a disconnect mid-arguments) unreproducible exactly when they matter.
//!
//! A [`ScriptedProvider`] is the same [`ModelProvider`] contract backed by a
//! frozen JSON script. It reaches no network, reads no environment, sleeps on
//! no clock, and holds no credential, so CI exercises the whole streaming and
//! tool-call surface with none of those.
//!
//! # Determinism
//!
//! Two replays of one scenario produce the same events and the same
//! [`TurnOutcome`], byte for byte. That includes the timings: a script advances
//! a [`ManualTurnClock`] itself rather than
//! reading a real one, so `elapsed` and `first_event_latency` are properties of
//! the script rather than of the machine.
//!
//! The request is deliberately ignored. A scripted provider replays; it does
//! not answer. What it *does* answer for is
//! [`capabilities`](ModelProvider::capabilities), which every script declares
//! explicitly — including the one that declares nothing at all.
//!
//! # The script format
//!
//! A fixture is a versioned JSON object holding a scenario name, the
//! capabilities the provider will claim, and an ordered list of steps:
//!
//! ```json
//! {
//!   "v": 1,
//!   "id": "single_tool_call",
//!   "capabilities": { "context_window": 128000, "supports_tool_calls": true },
//!   "steps": [
//!     { "step": "advance", "millis": 30 },
//!     { "step": "emit", "event": { "kind": "turn_started" } },
//!     { "step": "emit",
//!       "event": { "kind": "tool_call_started", "index": 0, "id": "call_1", "name": "fs.read" } },
//!     { "step": "arguments", "index": 0, "text": "{\"path\":\"src/lib.rs\"}", "split_at": [8] },
//!     { "step": "emit", "event": { "kind": "tool_call_ready", "index": 0 } },
//!     { "step": "emit", "event": { "kind": "turn_completed", "stop": "tool_use" } }
//!   ]
//! }
//! ```
//!
//! Four steps, and between them they reach every shape the contract names:
//!
//! - `emit` sends one [`ModelEvent`] exactly as written, which is how a
//!   duplicate id, a missing id, and an event after the turn completed are all
//!   expressed — the event model already has room for each.
//! - `arguments` sends argument text chopped at byte offsets, including offsets
//!   inside a multi-byte character.
//! - `advance` moves the turn's clock, so latencies are the script's.
//! - `fail` ends the stream with any of the ten [`ProviderError`] kinds, and
//!   must be the last step because a failure ends the stream.
//!
//! Running out of steps is how a stream ends: with a completed turn that is the
//! turn's outcome, and without one it is a `disconnected` — or, if no event ever
//! arrived, an `empty_response`. `v` is probed before the strict body, so a
//! fixture from a newer build asks for an upgrade rather than reading as
//! corrupt, and the committed files are compared byte for byte against their
//! canonical encoding. A released script is replaced by a new version beside
//! it, never edited into a different meaning.
//!
//! [#126]: https://github.com/fullstacktaiye/harkness/issues/126
//! [#127]: https://github.com/fullstacktaiye/harkness/issues/127
//! [#134]: https://github.com/fullstacktaiye/harkness/issues/134

mod script;

use std::time::Duration;

use harkness_git::Cancellation;

use crate::{
    assemble::{ManualTurnClock, TurnAssembler, TurnDriver},
    contract::{
        ModelEvent, ModelEventSink, ModelId, ModelProvider, ModelRequest, ProviderCapabilities,
        ProviderError, ProviderId, SinkControl, TurnOutcome,
    },
};

pub use script::{
    MAX_SCRIPT_BYTES, MAX_SCRIPT_STEPS, SCRIPT_FIXTURE_VERSION, ScenarioName, Script, ScriptError,
    ScriptFailure, ScriptStep,
};

/// Stable identity every scripted provider reports.
pub const SCRIPTED_PROVIDER_ID: &str = "scripted";

/// Every built-in scenario, in registry order, with the fixture it is compiled
/// from.
///
/// The first fifteen are the set [#111] requires; the rest carry the failure
/// kinds and stream shapes those fifteen do not reach, so the ten-kind
/// injection matrix and the documented edge cases are all replayable.
///
/// [#111]: https://github.com/fullstacktaiye/harkness/issues/111
const BUILTIN_SCRIPTS: &[(&str, &str)] = &[
    (
        "text_only_turn",
        include_str!("fixtures/text-only-turn-v1.json"),
    ),
    (
        "single_tool_call",
        include_str!("fixtures/single-tool-call-v1.json"),
    ),
    (
        "multi_tool_call_interleaved",
        include_str!("fixtures/multi-tool-call-interleaved-v1.json"),
    ),
    (
        "split_arguments_tool_call",
        include_str!("fixtures/split-arguments-tool-call-v1.json"),
    ),
    (
        "duplicate_tool_call_id",
        include_str!("fixtures/duplicate-tool-call-id-v1.json"),
    ),
    (
        "missing_tool_call_id",
        include_str!("fixtures/missing-tool-call-id-v1.json"),
    ),
    (
        "malformed_tool_arguments",
        include_str!("fixtures/malformed-tool-arguments-v1.json"),
    ),
    (
        "context_overflow_rejection",
        include_str!("fixtures/context-overflow-rejection-v1.json"),
    ),
    (
        "rate_limited_with_retry_after",
        include_str!("fixtures/rate-limited-with-retry-after-v1.json"),
    ),
    (
        "authentication_failure",
        include_str!("fixtures/authentication-failure-v1.json"),
    ),
    (
        "disconnect_mid_arguments",
        include_str!("fixtures/disconnect-mid-arguments-v1.json"),
    ),
    (
        "empty_response",
        include_str!("fixtures/empty-response-v1.json"),
    ),
    ("refusal", include_str!("fixtures/refusal-v1.json")),
    (
        "usage_estimated_only",
        include_str!("fixtures/usage-estimated-only-v1.json"),
    ),
    (
        "capabilities_unknown",
        include_str!("fixtures/capabilities-unknown-v1.json"),
    ),
    (
        "endpoint_unreachable",
        include_str!("fixtures/endpoint-unreachable-v1.json"),
    ),
    (
        "provider_timeout",
        include_str!("fixtures/provider-timeout-v1.json"),
    ),
    (
        "unsupported_capability",
        include_str!("fixtures/unsupported-capability-v1.json"),
    ),
    (
        "malformed_stream_unknown_index",
        include_str!("fixtures/malformed-stream-unknown-index-v1.json"),
    ),
    (
        "stream_ends_without_completion",
        include_str!("fixtures/stream-ends-without-completion-v1.json"),
    ),
    (
        "event_after_turn_completed",
        include_str!("fixtures/event-after-turn-completed-v1.json"),
    ),
    (
        "argumentless_tool_call",
        include_str!("fixtures/argumentless-tool-call-v1.json"),
    ),
];

/// A [`ModelProvider`] that replays a [`Script`].
#[derive(Clone, Debug)]
pub struct ScriptedProvider {
    script: Script,
}

impl ScriptedProvider {
    /// Loads one built-in scenario by name.
    ///
    /// # Errors
    ///
    /// Returns [`ScriptError::UnknownScenario`] for a name the registry does
    /// not hold. A compiled-in fixture that fails to parse is a build this
    /// build cannot trust, so that surfaces as the parse error itself.
    pub fn scenario(name: &str) -> Result<Self, ScriptError> {
        let (_, fixture) = BUILTIN_SCRIPTS
            .iter()
            .find(|(scenario, _)| *scenario == name)
            .ok_or_else(|| ScriptError::UnknownScenario {
                name: name.to_owned(),
            })?;
        let script = Script::from_json(fixture)?;
        if script.id().as_str() != name {
            return Err(ScriptError::MisfiledFixture {
                expected: name.to_owned(),
                found: script.id().clone(),
            });
        }
        Ok(Self::from_script(script))
    }

    /// Loads a script from JSON, for a caller with a scenario of its own.
    ///
    /// # Errors
    ///
    /// Returns whatever [`Script::from_json`] refuses the fixture with.
    pub fn from_json(bytes: &str) -> Result<Self, ScriptError> {
        Ok(Self::from_script(Script::from_json(bytes)?))
    }

    /// Wraps an already-parsed script.
    #[must_use]
    pub const fn from_script(script: Script) -> Self {
        Self { script }
    }

    /// Every built-in scenario name, in registry order.
    #[must_use]
    pub fn scenario_names() -> Vec<&'static str> {
        BUILTIN_SCRIPTS.iter().map(|(name, _)| *name).collect()
    }

    /// The script being replayed.
    #[must_use]
    pub const fn script(&self) -> &Script {
        &self.script
    }
}

impl ModelProvider for ScriptedProvider {
    fn id(&self) -> ProviderId {
        ProviderId::new(SCRIPTED_PROVIDER_ID)
            .expect("the scripted provider's identity is a valid provider id")
    }

    fn capabilities(&self, _model: &ModelId) -> ProviderCapabilities {
        self.script.capabilities()
    }

    fn stream(
        &self,
        _request: &ModelRequest,
        sink: &mut dyn ModelEventSink,
        cancellation: &Cancellation,
    ) -> Result<TurnOutcome, ProviderError> {
        let clock = ManualTurnClock::new();
        let mut driver = TurnDriver::with_assembler(
            sink,
            cancellation,
            TurnAssembler::with_clock(Box::new(clock.clone())),
        );

        // Polled here rather than only inside `deliver`, because a step is not
        // always an event: a scenario that injects a failure and emits nothing
        // would otherwise answer an already-cancelled token with its scripted
        // error, and a retry loop reading `rate_limited` off a run somebody
        // stopped would retry it.
        driver.check_cancelled()?;
        for step in self.script.steps() {
            driver.check_cancelled()?;
            match step {
                ScriptStep::Advance { millis } => clock.advance(Duration::from_millis(*millis)),
                ScriptStep::Emit { event } => {
                    if driver.deliver(event.clone())? == SinkControl::Stop {
                        return driver.finish();
                    }
                }
                ScriptStep::Arguments {
                    index,
                    text,
                    split_at,
                } => {
                    for fragment in script::fragments(text, split_at)? {
                        let event = ModelEvent::ToolCallArgumentsDelta {
                            index: *index,
                            fragment,
                        };
                        if driver.deliver(event)? == SinkControl::Stop {
                            return driver.finish();
                        }
                    }
                }
                ScriptStep::Fail { failure } => {
                    // Through the driver, so a scripted disconnect carries the
                    // partial turn exactly as a real one does.
                    return Err(driver.fail(failure.to_error()));
                }
            }
        }
        driver.finish()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use harkness_git::Cancellation;
    use serde_json::json;

    use super::{BUILTIN_SCRIPTS, SCRIPTED_PROVIDER_ID, Script, ScriptedProvider};
    use crate::{
        assemble::{IdProvenance, ToolCallDefect},
        contract::{
            ModelEvent, ModelId, ModelMessage, ModelProvider, ModelRequest, ProviderError,
            RecordedEvents, Role, SinkControl, StopReason,
        },
    };

    /// The set [#111] names. Extra scenarios are welcome; a missing one is not.
    ///
    /// [#111]: https://github.com/fullstacktaiye/harkness/issues/111
    const REQUIRED_SCENARIOS: &[&str] = &[
        "text_only_turn",
        "single_tool_call",
        "multi_tool_call_interleaved",
        "split_arguments_tool_call",
        "duplicate_tool_call_id",
        "missing_tool_call_id",
        "malformed_tool_arguments",
        "context_overflow_rejection",
        "rate_limited_with_retry_after",
        "authentication_failure",
        "disconnect_mid_arguments",
        "empty_response",
        "refusal",
        "usage_estimated_only",
        "capabilities_unknown",
    ];

    fn request() -> ModelRequest {
        ModelRequest::new(
            ModelId::new("scripted-model").unwrap(),
            vec![ModelMessage::text(Role::User, "Do the thing.")],
        )
    }

    fn run(
        name: &str,
    ) -> (
        Vec<ModelEvent>,
        Result<crate::contract::TurnOutcome, ProviderError>,
    ) {
        let provider = ScriptedProvider::scenario(name).expect("a built-in scenario");
        let mut sink = RecordedEvents::default();
        let cancellation = Cancellation::default();
        let outcome = provider.stream(&request(), &mut sink, &cancellation);
        (sink.into_events(), outcome)
    }

    fn outcome_of(name: &str) -> crate::contract::TurnOutcome {
        run(name)
            .1
            .unwrap_or_else(|error| panic!("{name}: {error}"))
    }

    fn error_of(name: &str) -> ProviderError {
        match run(name).1 {
            Ok(outcome) => panic!("{name} succeeded with {outcome:?}"),
            Err(error) => error,
        }
    }

    /// The whole transcript of one replay, as bytes, which is what "identical"
    /// is asserted on.
    fn transcript(name: &str) -> String {
        let (events, result) = run(name);
        let result = match result {
            Ok(outcome) => json!({"outcome": outcome}),
            Err(error) => json!({
                "error": {
                    "kind": error.kind(),
                    "message": error.to_string(),
                    "partial": error.partial_turn(),
                }
            }),
        };
        serde_json::to_string_pretty(&json!({"events": events, "result": result}))
            .expect("a transcript encodes")
    }

    #[test]
    fn every_required_scenario_is_registered_and_listed() {
        let names = ScriptedProvider::scenario_names();
        for required in REQUIRED_SCENARIOS {
            assert!(
                names.contains(required),
                "{required} is missing from the registry"
            );
            assert!(ScriptedProvider::scenario(required).is_ok());
        }
        assert_eq!(
            names.len(),
            BUILTIN_SCRIPTS.len(),
            "the listing and the registry are one list"
        );
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "a name is registered once");
        assert_eq!(
            ScriptedProvider::scenario("no_such_scenario")
                .unwrap_err()
                .kind(),
            "unknown_scenario"
        );
    }

    #[test]
    fn every_fixture_parses_and_declares_its_own_registry_name() {
        for (name, fixture) in BUILTIN_SCRIPTS {
            let script = Script::from_json(fixture)
                .unwrap_or_else(|error| panic!("{name} does not parse: {error}"));
            assert_eq!(script.id().as_str(), *name);
            assert_eq!(script.version(), 1);
        }
    }

    /// The committed fixture is the canonical encoding of what it describes, so
    /// a hand edit that reformats or reorders shows up here rather than as a
    /// difference nobody notices. The `.gitattributes` entry for this directory
    /// is what keeps it true on Windows.
    #[test]
    fn every_fixture_is_stored_in_its_canonical_form() {
        for (name, fixture) in BUILTIN_SCRIPTS {
            let script = Script::from_json(fixture).unwrap();
            assert_eq!(
                script.to_json_pretty().unwrap(),
                *fixture,
                "{name} is not stored canonically; run the regenerator"
            );
        }
    }

    #[test]
    fn two_replays_of_every_scenario_are_byte_identical() {
        for (name, _) in BUILTIN_SCRIPTS {
            assert_eq!(transcript(name), transcript(name), "{name} is not stable");
        }
    }

    /// A compact snapshot of every scenario: what it emits, and how it ends.
    /// Adding a scenario means adding a row, which is the point — a scenario
    /// nobody described the shape of is one nobody is testing.
    #[test]
    fn every_scenario_produces_the_stream_it_describes() {
        let expected: &[(&str, &[&str], &str)] = &[
            (
                "text_only_turn",
                &[
                    "turn_started",
                    "text_delta",
                    "text_delta",
                    "usage",
                    "turn_completed",
                ],
                "ok:end_turn",
            ),
            (
                "single_tool_call",
                &[
                    "turn_started",
                    "text_delta",
                    "tool_call_started",
                    "tool_call_arguments_delta",
                    "tool_call_arguments_delta",
                    "tool_call_ready",
                    "usage",
                    "turn_completed",
                ],
                "ok:tool_use",
            ),
            (
                "multi_tool_call_interleaved",
                &[
                    "turn_started",
                    "tool_call_started",
                    "tool_call_started",
                    "tool_call_arguments_delta",
                    "tool_call_arguments_delta",
                    "tool_call_arguments_delta",
                    "tool_call_arguments_delta",
                    "tool_call_ready",
                    "tool_call_ready",
                    "usage",
                    "turn_completed",
                ],
                "ok:tool_use",
            ),
            (
                "split_arguments_tool_call",
                &[
                    "turn_started",
                    "tool_call_started",
                    "tool_call_arguments_delta",
                    "tool_call_arguments_delta",
                    "tool_call_arguments_delta",
                    "tool_call_arguments_delta",
                    "tool_call_arguments_delta",
                    "tool_call_ready",
                    "turn_completed",
                ],
                "ok:tool_use",
            ),
            (
                "duplicate_tool_call_id",
                &[
                    "turn_started",
                    "tool_call_started",
                    "tool_call_arguments_delta",
                    "tool_call_ready",
                    "tool_call_started",
                    "tool_call_arguments_delta",
                    "tool_call_ready",
                    "turn_completed",
                ],
                "ok:tool_use",
            ),
            (
                "missing_tool_call_id",
                &[
                    "turn_started",
                    "tool_call_started",
                    "tool_call_arguments_delta",
                    "tool_call_ready",
                    "turn_completed",
                ],
                "ok:tool_use",
            ),
            (
                "malformed_tool_arguments",
                &[
                    "turn_started",
                    "tool_call_started",
                    "tool_call_arguments_delta",
                    "tool_call_ready",
                    "turn_completed",
                ],
                "ok:tool_use",
            ),
            ("context_overflow_rejection", &[], "err:context_overflow"),
            ("rate_limited_with_retry_after", &[], "err:rate_limited"),
            ("authentication_failure", &[], "err:authentication_failed"),
            (
                "disconnect_mid_arguments",
                &[
                    "turn_started",
                    "text_delta",
                    "tool_call_started",
                    "tool_call_arguments_delta",
                ],
                "err:disconnected",
            ),
            ("empty_response", &[], "err:empty_response"),
            (
                "refusal",
                &["turn_started", "text_delta", "turn_completed"],
                "ok:refusal",
            ),
            (
                "usage_estimated_only",
                &["turn_started", "text_delta", "usage", "turn_completed"],
                "ok:end_turn",
            ),
            (
                "capabilities_unknown",
                &["turn_started", "text_delta", "turn_completed"],
                "ok:end_turn",
            ),
            ("endpoint_unreachable", &[], "err:endpoint_unreachable"),
            (
                "provider_timeout",
                &["turn_started", "text_delta"],
                "err:provider_timeout",
            ),
            ("unsupported_capability", &[], "err:unsupported_capability"),
            (
                "malformed_stream_unknown_index",
                &["turn_started"],
                "err:malformed_response",
            ),
            (
                "stream_ends_without_completion",
                &["turn_started", "text_delta"],
                "err:disconnected",
            ),
            (
                "event_after_turn_completed",
                &["turn_started", "text_delta", "turn_completed"],
                "ok:end_turn",
            ),
            (
                "argumentless_tool_call",
                &[
                    "turn_started",
                    "tool_call_started",
                    "tool_call_ready",
                    "turn_completed",
                ],
                "ok:tool_use",
            ),
        ];

        assert_eq!(
            expected.len(),
            BUILTIN_SCRIPTS.len(),
            "every registered scenario is described here"
        );
        for (name, kinds, ending) in expected {
            let (events, result) = run(name);
            let observed = events.iter().map(ModelEvent::kind).collect::<Vec<_>>();
            assert_eq!(&observed, kinds, "{name} emitted a different stream");
            let observed_ending = match &result {
                Ok(outcome) => format!("ok:{}", outcome.stop.as_str()),
                Err(error) => format!("err:{}", error.kind()),
            };
            assert_eq!(&observed_ending, ending, "{name} ended differently");
        }
    }

    /// Ten kinds, ten scenarios. `cancelled` is reached the way a real one is —
    /// by tripping the token — rather than by a scripted step, because that is
    /// the only route that also proves the poll happens where it should.
    #[test]
    fn every_provider_error_kind_is_reachable_from_a_scenario() {
        let matrix = [
            ("endpoint_unreachable", "endpoint_unreachable"),
            ("authentication_failure", "authentication_failed"),
            ("rate_limited_with_retry_after", "rate_limited"),
            ("context_overflow_rejection", "context_overflow"),
            ("provider_timeout", "provider_timeout"),
            ("disconnect_mid_arguments", "disconnected"),
            ("malformed_stream_unknown_index", "malformed_response"),
            ("unsupported_capability", "unsupported_capability"),
            ("empty_response", "empty_response"),
        ];
        let mut reached = matrix
            .iter()
            .map(|(scenario, kind)| {
                assert_eq!(error_of(scenario).kind(), *kind, "{scenario}");
                *kind
            })
            .collect::<Vec<_>>();

        let provider = ScriptedProvider::scenario("single_tool_call").unwrap();
        let cancellation = Cancellation::default();
        cancellation.cancel();
        let mut sink = RecordedEvents::default();
        let cancelled = provider
            .stream(&request(), &mut sink, &cancellation)
            .unwrap_err();
        assert_eq!(cancelled.kind(), "cancelled");
        assert!(
            sink.events().is_empty(),
            "an already-cancelled turn emits nothing"
        );
        reached.push(cancelled.kind());

        reached.sort_unstable();
        let mut published = ProviderError::KINDS.to_vec();
        published.sort_unstable();
        assert_eq!(reached, published, "every published kind is injectable");
    }

    /// Every scenario, not only the ones that emit events. A script's steps are
    /// not all events, so a failure-only scenario polled nowhere would answer an
    /// already-cancelled token with its scripted error — and a retry loop
    /// reading `rate_limited` off a run somebody stopped would retry it.
    #[test]
    fn an_already_cancelled_token_stops_every_scenario_before_it_starts() {
        for (name, _) in BUILTIN_SCRIPTS {
            let provider = ScriptedProvider::scenario(name).unwrap();
            let cancellation = Cancellation::default();
            cancellation.cancel();
            let mut sink = RecordedEvents::default();

            let error = provider
                .stream(&request(), &mut sink, &cancellation)
                .unwrap_err();
            assert_eq!(error.kind(), "cancelled", "{name} answered {error}");
            assert!(sink.events().is_empty(), "{name} emitted an event");
        }
    }

    /// The other half of the cancellation criterion: a token tripped *during* a
    /// stream stops it at the next poll, with nothing delivered afterwards.
    #[test]
    fn cancelling_mid_stream_stops_at_the_next_poll_and_delivers_nothing_after_it() {
        let provider = ScriptedProvider::scenario("disconnect_mid_arguments").unwrap();
        let cancellation = Cancellation::default();
        let mut delivered = Vec::new();
        let mut sink = |event: ModelEvent| {
            delivered.push(event);
            if delivered.len() == 2 {
                cancellation.cancel();
            }
            SinkControl::Continue
        };

        let error = provider
            .stream(&request(), &mut sink, &cancellation)
            .unwrap_err();
        assert_eq!(error.kind(), "cancelled");
        assert_eq!(
            delivered.len(),
            2,
            "the event that tripped the token is the last one delivered"
        );
    }

    /// Harkness stopping its own turn is not a provider failure: the stream
    /// stops where the sink said, and what had arrived is kept.
    #[test]
    fn a_sink_that_stops_a_scripted_turn_gets_the_partial_turn_back() {
        let provider = ScriptedProvider::scenario("single_tool_call").unwrap();
        let cancellation = Cancellation::default();
        let mut delivered = 0;
        let mut sink = |_event| {
            delivered += 1;
            if delivered == 3 {
                SinkControl::Stop
            } else {
                SinkControl::Continue
            }
        };

        let outcome = provider
            .stream(&request(), &mut sink, &cancellation)
            .expect("a stopped turn is a success");
        assert_eq!(outcome.stop, StopReason::AbortedBySink);
        assert_eq!(outcome.event_count, 3);
        assert_eq!(outcome.turn.text, "Reading the file first.");
        assert_eq!(
            outcome.turn.tool_calls[0].defect(),
            Some(&ToolCallDefect::Truncated),
            "the call the stop landed inside never received its arguments"
        );
        assert_eq!(outcome.turn.stop, None, "the provider never said why");
    }

    #[test]
    fn a_split_call_assembles_to_the_same_arguments_an_unsplit_one_would() {
        let outcome = outcome_of("split_arguments_tool_call");
        let call = &outcome.turn.tool_calls[0];
        assert_eq!(
            call.arguments(),
            Some(&json!({"query": "café ☕", "limit": 5}))
        );
        assert_eq!(call.name(), Some("workspace.search"));
    }

    #[test]
    fn a_duplicate_id_is_kept_beside_the_call_it_repeats() {
        let outcome = outcome_of("duplicate_tool_call_id");
        assert_eq!(outcome.turn.tool_calls.len(), 2);
        assert!(outcome.turn.tool_calls[0].duplicate_of().is_none());
        assert_eq!(
            outcome.turn.tool_calls[1].duplicate_of(),
            Some(outcome.turn.tool_calls[0].id())
        );
        assert_eq!(outcome.diagnostics.duplicate_ids, 1);
    }

    #[test]
    fn a_call_with_no_id_is_synthesized_rather_than_dropped() {
        let outcome = outcome_of("missing_tool_call_id");
        let call = &outcome.turn.tool_calls[0];
        assert_eq!(call.id_provenance(), IdProvenance::Synthesized);
        assert_eq!(call.id().as_str(), "harkness-synth-1");
        assert_eq!(outcome.diagnostics.synthesized_ids, 1);
    }

    #[test]
    fn malformed_arguments_are_surfaced_as_an_invalid_call() {
        let outcome = outcome_of("malformed_tool_arguments");
        let call = &outcome.turn.tool_calls[0];
        assert!(!call.is_ready());
        assert!(matches!(
            call.defect(),
            Some(ToolCallDefect::UnparsableArguments { .. })
        ));
        assert_eq!(outcome.turn.tool_calls.len(), 1, "nothing was dropped");
    }

    #[test]
    fn a_disconnect_mid_arguments_reports_what_had_arrived() {
        let error = error_of("disconnect_mid_arguments");
        let partial = error
            .partial_turn()
            .expect("a disconnect carries a partial");
        assert_eq!(partial.text, "Looking that up.");
        assert_eq!(partial.tool_calls.len(), 1);
        assert_eq!(
            partial.tool_calls[0].defect(),
            Some(&ToolCallDefect::Truncated)
        );
        assert_eq!(error.retry_hint(), crate::contract::RetryHint::Immediate);
    }

    #[test]
    fn a_rate_limit_carries_the_window_the_endpoint_asked_for() {
        let error = error_of("rate_limited_with_retry_after");
        assert_eq!(
            error.retry_hint(),
            crate::contract::RetryHint::After(Duration::from_millis(2_000))
        );
    }

    #[test]
    fn an_estimated_usage_report_stays_estimated() {
        let outcome = outcome_of("usage_estimated_only");
        let usage = outcome.usage.expect("the scenario reports usage");
        assert!(!usage.exact);
        assert_eq!(usage.output_tokens, Some(24));
    }

    #[test]
    fn an_unknown_capability_set_is_answered_for_every_model() {
        let provider = ScriptedProvider::scenario("capabilities_unknown").unwrap();
        let capabilities = provider.capabilities(&ModelId::new("anything-at-all").unwrap());
        assert!(capabilities.is_unknown());
        assert_eq!(capabilities.context_window_or(4_096), 4_096);

        let declared = ScriptedProvider::scenario("single_tool_call")
            .unwrap()
            .capabilities(&ModelId::new("scripted-model").unwrap());
        assert_eq!(declared.context_window, Some(128_000));
        assert!(declared.supports_tool_calls);
    }

    #[test]
    fn an_event_after_the_turn_completed_is_counted_and_not_assembled() {
        let outcome = outcome_of("event_after_turn_completed");
        assert_eq!(outcome.diagnostics.ignored_after_completion, 1);
        assert_eq!(outcome.turn.text, "All done.");
        assert_eq!(outcome.event_count, 4, "it happened, and is counted");
    }

    #[test]
    fn a_multi_tool_turn_reports_everything_a_record_needs() {
        let outcome = outcome_of("multi_tool_call_interleaved");
        assert_eq!(outcome.stop, StopReason::ToolUse);
        assert_eq!(outcome.event_count, 11);
        assert_eq!(
            outcome.first_event_latency,
            Some(Duration::from_millis(40)),
            "read from the script's own clock, not the machine's"
        );
        assert_eq!(outcome.elapsed, Duration::from_millis(115));
        assert_eq!(outcome.provider_request_id.as_deref(), Some("scripted-3"));
        let usage = outcome.usage.expect("usage is reported");
        assert!(usage.exact);

        let json = serde_json::to_string(&outcome).unwrap();
        assert_eq!(
            serde_json::from_str::<crate::contract::TurnOutcome>(&json).unwrap(),
            outcome
        );
    }

    #[test]
    fn a_scripted_provider_reports_its_own_identity() {
        let provider = ScriptedProvider::scenario("text_only_turn").unwrap();
        assert_eq!(provider.id().as_str(), SCRIPTED_PROVIDER_ID);
    }

    /// Rewrites the committed fixtures from their parsed form, which is how a
    /// hand-written one is canonicalized. It changes formatting only: a script
    /// this build cannot parse is not rewritten, it fails.
    #[test]
    #[ignore = "rewrites the frozen v1 fixtures; run only after editing one by hand"]
    fn regenerate_the_frozen_v1_fixtures() {
        let directory =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/scripted/fixtures");
        for (name, fixture) in BUILTIN_SCRIPTS {
            let script = Script::from_json(fixture)
                .unwrap_or_else(|error| panic!("{name} does not parse: {error}"));
            let path = directory.join(format!("{}-v1.json", name.replace('_', "-")));
            std::fs::write(&path, script.to_json_pretty().unwrap())
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        }
    }
}
