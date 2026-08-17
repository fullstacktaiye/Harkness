//! The loop every provider implementation runs.

use harkness_git::Cancellation;

use crate::contract::{ModelEvent, ModelEventSink, ProviderError, SinkControl, TurnOutcome};

use super::assembler::{Absorbed, TurnAssembler};

/// Delivers a turn's events to a sink while assembling them.
///
/// Written once, here, because the rules an implementation has to hold to are
/// the same for every endpoint and are easy to get subtly wrong:
///
/// - Cancellation is polled *before* each event is assembled or delivered, so
///   nothing reaches the sink after the poll that observed it.
/// - An event arriving after the turn completed is counted and dropped rather
///   than forwarded, because a sink that has seen the turn end has seen it end.
/// - A sink answering [`SinkControl::Stop`] ends the turn as a success with
///   [`StopReason::AbortedBySink`](crate::contract::StopReason::AbortedBySink)
///   and the partial turn preserved — Harkness stopped it, so it is not a
///   provider failure.
/// - A stream that ends without a completed turn is a
///   [`disconnected`](ProviderError::Disconnected) carrying what did arrive,
///   and one that produced nothing at all is an
///   [`empty_response`](ProviderError::EmptyResponse).
///
/// An adapter that wants a different assembler — different limits, or a clock
/// it controls — builds one and passes it to
/// [`with_assembler`](Self::with_assembler).
pub struct TurnDriver<'a> {
    assembler: TurnAssembler,
    sink: &'a mut dyn ModelEventSink,
    cancellation: &'a Cancellation,
    aborted: bool,
}

impl<'a> TurnDriver<'a> {
    /// Drives a turn into `sink` under `cancellation`.
    #[must_use]
    pub fn new(sink: &'a mut dyn ModelEventSink, cancellation: &'a Cancellation) -> Self {
        Self::with_assembler(sink, cancellation, TurnAssembler::new())
    }

    /// Drives a turn through a caller-built assembler.
    #[must_use]
    pub fn with_assembler(
        sink: &'a mut dyn ModelEventSink,
        cancellation: &'a Cancellation,
        assembler: TurnAssembler,
    ) -> Self {
        Self {
            assembler,
            sink,
            cancellation,
            aborted: false,
        }
    }

    /// Answers whether the turn has been asked to stop.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Cancelled`] once the token has been tripped.
    /// An implementation waiting between events calls this at the contract's
    /// [`CANCELLATION_POLL_INTERVAL`](crate::contract::CANCELLATION_POLL_INTERVAL).
    pub fn check_cancelled(&self) -> Result<(), ProviderError> {
        if self.cancellation.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        Ok(())
    }

    /// Assembles one event and hands it to the sink.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Cancelled`] when the token was tripped before
    /// this event, and [`ProviderError::MalformedResponse`] when the event
    /// cannot be part of the turn built so far.
    pub fn deliver(&mut self, event: ModelEvent) -> Result<SinkControl, ProviderError> {
        self.check_cancelled()?;
        if self.assembler.observe(&event)? == Absorbed::IgnoredAfterCompletion {
            return Ok(SinkControl::Continue);
        }
        let control = self.sink.event(event);
        if control == SinkControl::Stop {
            self.aborted = true;
        }
        Ok(control)
    }

    /// Whether the sink has asked for the turn to stop.
    #[must_use]
    pub const fn aborted(&self) -> bool {
        self.aborted
    }

    /// The assembler, for an implementation that wants to inspect progress.
    #[must_use]
    pub const fn assembler(&self) -> &TurnAssembler {
        &self.assembler
    }

    /// Ends the turn because the stream ended.
    ///
    /// A sink that stopped the turn *after* the provider had already completed
    /// it stopped nothing: the turn is reported exactly as the provider ended
    /// it. Reading `aborted` alone would file a fully delivered turn as
    /// [`AbortedBySink`](crate::contract::StopReason::AbortedBySink) — which a
    /// sink bounded by its own capacity reaches by answering `Stop` on the last
    /// event — and tell [#126] work was cut short when none was.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::EmptyResponse`] or
    /// [`ProviderError::Disconnected`] when the stream stopped short, unless
    /// the sink is what stopped it.
    ///
    /// [#126]: https://github.com/fullstacktaiye/harkness/issues/126
    pub fn finish(self) -> Result<TurnOutcome, ProviderError> {
        if self.aborted && !self.assembler.is_completed() {
            return Ok(self.assembler.abort_by_sink());
        }
        self.assembler.finish()
    }

    /// Ends the turn because the source failed, attaching what had arrived.
    ///
    /// Only a [`Disconnected`](ProviderError::Disconnected) carries a partial
    /// turn, and only when it does not already have one: every other kind is
    /// returned exactly as the implementation built it, so a driver cannot
    /// quietly rewrite a failure into one that says something else.
    #[must_use]
    pub fn fail(self, error: ProviderError) -> ProviderError {
        match error {
            ProviderError::Disconnected {
                detail,
                partial: None,
            } => ProviderError::Disconnected {
                detail,
                partial: Some(Box::new(self.assembler.into_partial())),
            },
            other => other,
        }
    }
}

impl std::fmt::Debug for TurnDriver<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TurnDriver")
            .field("assembler", &self.assembler)
            .field("aborted", &self.aborted)
            .field("cancelled", &self.cancellation.is_cancelled())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use harkness_git::Cancellation;

    use super::TurnDriver;
    use crate::{
        assemble::{ManualTurnClock, ToolCallDefect, TurnAssembler},
        contract::{ModelEvent, ProviderToolCallId, RecordedEvents, SinkControl, StopReason},
    };

    fn text(fragment: &str) -> ModelEvent {
        ModelEvent::TextDelta {
            text: fragment.to_owned(),
        }
    }

    #[test]
    fn a_completed_turn_reports_what_the_provider_said() {
        let mut sink = RecordedEvents::default();
        let cancellation = Cancellation::default();
        let mut driver = TurnDriver::new(&mut sink, &cancellation);
        driver
            .deliver(ModelEvent::TurnStarted {
                provider_request_id: Some("req_7".to_owned()),
            })
            .unwrap();
        driver.deliver(text("Hello")).unwrap();
        driver
            .deliver(ModelEvent::TurnCompleted {
                stop: StopReason::EndTurn,
            })
            .unwrap();
        let outcome = driver.finish().unwrap();

        assert_eq!(outcome.turn.text, "Hello");
        assert_eq!(outcome.stop, StopReason::EndTurn);
        assert_eq!(outcome.provider_request_id.as_deref(), Some("req_7"));
        assert_eq!(outcome.event_count, 3);
        assert_eq!(sink.events().len(), 3);
    }

    #[test]
    fn a_sink_that_stops_ends_the_turn_as_a_success_with_the_partial_text() {
        let mut delivered = 0;
        let mut sink = |_event: ModelEvent| {
            delivered += 1;
            if delivered == 2 {
                SinkControl::Stop
            } else {
                SinkControl::Continue
            }
        };
        let cancellation = Cancellation::default();
        let mut driver = TurnDriver::new(&mut sink, &cancellation);
        assert_eq!(driver.deliver(text("one ")).unwrap(), SinkControl::Continue);
        assert_eq!(driver.deliver(text("two")).unwrap(), SinkControl::Stop);
        assert!(driver.aborted());
        let outcome = driver.finish().unwrap();

        assert_eq!(outcome.stop, StopReason::AbortedBySink);
        assert_eq!(outcome.turn.text, "one two");
        assert_eq!(
            outcome.turn.stop, None,
            "the provider never said why; only the call did"
        );
    }

    /// A sink that stops on the last event stopped nothing. `RecordedEvents`
    /// reaches this by construction — it answers `Stop` on the event that fills
    /// it — so a turn whose length happens to equal a recorder's capacity must
    /// not be filed as one somebody cut short.
    #[test]
    fn a_sink_stopping_on_the_final_event_does_not_make_a_completed_turn_aborted() {
        let cancellation = Cancellation::default();
        let mut sink = RecordedEvents::with_capacity(2);
        let mut driver = TurnDriver::new(&mut sink, &cancellation);
        assert_eq!(driver.deliver(text("done")).unwrap(), SinkControl::Continue);
        assert_eq!(
            driver
                .deliver(ModelEvent::TurnCompleted {
                    stop: StopReason::EndTurn
                })
                .unwrap(),
            SinkControl::Stop
        );
        assert!(driver.aborted());

        let outcome = driver.finish().unwrap();
        assert_eq!(
            outcome.stop,
            StopReason::EndTurn,
            "the provider completed the turn; the sink only stopped listening"
        );
        assert_eq!(outcome.turn.text, "done");
    }

    /// The deterministic half of the cancellation contract: the poll happens
    /// before assembly and before delivery, so the sink's last event is the one
    /// before the cancel and nothing else is offered to it.
    #[test]
    fn nothing_reaches_the_sink_after_the_poll_that_observes_cancellation() {
        let cancellation = Cancellation::default();
        let mut sink = RecordedEvents::default();
        {
            let mut driver = TurnDriver::new(&mut sink, &cancellation);
            driver.deliver(text("before")).unwrap();
            cancellation.cancel();
            let error = driver.deliver(text("after")).unwrap_err();
            assert_eq!(error.kind(), "cancelled");
            let second = driver.deliver(text("also after")).unwrap_err();
            assert_eq!(second.kind(), "cancelled");
        }
        assert_eq!(sink.events(), &[text("before")]);
    }

    #[test]
    fn a_stream_that_ends_without_completing_attaches_what_arrived() {
        let cancellation = Cancellation::default();
        let mut sink = RecordedEvents::default();
        let mut driver = TurnDriver::new(&mut sink, &cancellation);
        driver.deliver(text("half a th")).unwrap();
        driver
            .deliver(ModelEvent::ToolCallStarted {
                index: 0,
                id: Some(ProviderToolCallId::new("call_1").unwrap()),
                name: Some("fs.read".to_owned()),
            })
            .unwrap();
        driver
            .deliver(ModelEvent::ToolCallArgumentsDelta {
                index: 0,
                fragment: "{\"path\":".to_owned(),
            })
            .unwrap();
        let error = driver.finish().unwrap_err();

        assert_eq!(error.kind(), "disconnected");
        let partial = error
            .partial_turn()
            .expect("a disconnect carries what it had");
        assert_eq!(partial.text, "half a th");
        assert_eq!(partial.tool_calls.len(), 1);
        assert_eq!(
            partial.tool_calls[0].defect(),
            Some(&ToolCallDefect::Truncated),
            "the call the stream was cut under is invalid, not merely unparsable"
        );
    }

    #[test]
    fn a_stream_that_produced_nothing_is_an_empty_response_rather_than_a_disconnect() {
        let cancellation = Cancellation::default();
        let mut sink = RecordedEvents::default();
        let error = TurnDriver::new(&mut sink, &cancellation)
            .finish()
            .unwrap_err();
        assert_eq!(error.kind(), "empty_response");
        assert!(error.partial_turn().is_none(), "nothing arrived to attach");
    }

    #[test]
    fn an_injected_failure_keeps_its_own_kind_and_gains_a_partial_only_when_disconnected() {
        let cancellation = Cancellation::default();
        let mut sink = RecordedEvents::default();
        let mut driver = TurnDriver::new(&mut sink, &cancellation);
        driver.deliver(text("some")).unwrap();
        let rate_limited = driver.fail(crate::contract::ProviderError::rate_limited(
            None,
            "slow down",
        ));
        assert_eq!(rate_limited.kind(), "rate_limited");
        assert!(rate_limited.partial_turn().is_none());

        let mut sink = RecordedEvents::default();
        let mut driver = TurnDriver::new(&mut sink, &cancellation);
        driver.deliver(text("some")).unwrap();
        let disconnected = driver.fail(crate::contract::ProviderError::disconnected(
            "peer went away",
        ));
        assert_eq!(
            disconnected.partial_turn().map(|turn| turn.text.as_str()),
            Some("some")
        );
    }

    #[test]
    fn an_event_after_the_turn_completed_is_counted_and_never_forwarded() {
        let cancellation = Cancellation::default();
        let mut sink = RecordedEvents::default();
        let outcome = {
            let mut driver = TurnDriver::new(&mut sink, &cancellation);
            driver
                .deliver(ModelEvent::TurnCompleted {
                    stop: StopReason::EndTurn,
                })
                .unwrap();
            assert_eq!(
                driver.deliver(text("afterwards")).unwrap(),
                SinkControl::Continue
            );
            driver.finish().unwrap()
        };
        assert_eq!(
            outcome.turn.text, "",
            "an ignored event contributes nothing"
        );
        assert_eq!(outcome.diagnostics.ignored_after_completion, 1);
        assert_eq!(outcome.event_count, 2, "it still happened");
        assert_eq!(sink.events().len(), 1);
    }

    /// Timings come from the assembler's clock, so a scripted turn can assert
    /// them without a sleep and without measuring the machine it ran on.
    #[test]
    fn latencies_are_read_from_the_assemblers_clock() {
        let clock = ManualTurnClock::new();
        let cancellation = Cancellation::default();
        let mut sink = RecordedEvents::default();
        let mut driver = TurnDriver::with_assembler(
            &mut sink,
            &cancellation,
            TurnAssembler::with_clock(Box::new(clock.clone())),
        );
        clock.advance(std::time::Duration::from_millis(30));
        driver.deliver(text("first")).unwrap();
        clock.advance(std::time::Duration::from_millis(70));
        driver
            .deliver(ModelEvent::TurnCompleted {
                stop: StopReason::EndTurn,
            })
            .unwrap();
        let outcome = driver.finish().unwrap();

        assert_eq!(
            outcome.first_event_latency,
            Some(std::time::Duration::from_millis(30))
        );
        assert_eq!(outcome.elapsed, std::time::Duration::from_millis(100));
    }
}
