//! The frozen fixture format a scripted scenario is written in.

use std::{fmt, time::Duration};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    assemble::Utf8Accumulator,
    contract::{ModelEvent, ProviderCapabilities, ProviderError},
};

/// Newest script fixture version this build understands.
pub const SCRIPT_FIXTURE_VERSION: u32 = 1;

/// Oldest script fixture version this build still reads.
const MIN_SCRIPT_FIXTURE_VERSION: u32 = 1;

/// Largest script accepted from disk or another untrusted source.
pub const MAX_SCRIPT_BYTES: usize = 64 * 1024;

/// Longest script this build will replay.
pub const MAX_SCRIPT_STEPS: usize = 128;

/// Stable lowercase snake-case identity of one scenario.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(into = "String", try_from = "String")]
pub struct ScenarioName(String);

impl ScenarioName {
    /// Parses and validates a scenario name.
    ///
    /// # Errors
    ///
    /// Returns [`ScriptError::InvalidScenarioName`] for an empty, overlong, or
    /// non-snake-case spelling.
    pub fn new(value: impl Into<String>) -> Result<Self, ScriptError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 64
            && value.bytes().enumerate().all(|(index, byte)| match byte {
                b'a'..=b'z' => true,
                b'0'..=b'9' => index > 0,
                b'_' => index > 0 && index + 1 < value.len(),
                _ => false,
            })
            && !value.contains("__");
        if !valid {
            return Err(ScriptError::InvalidScenarioName { value });
        }
        Ok(Self(value))
    }

    /// Borrows the stable spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ScenarioName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl TryFrom<String> for ScenarioName {
    type Error = ScriptError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ScenarioName> for String {
    fn from(value: ScenarioName) -> Self {
        value.0
    }
}

/// Failure to load or replay a script.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ScriptError {
    /// Scenario name is outside the stable grammar.
    #[error("invalid scenario name {value:?}; expected at most 64 lowercase snake-case characters")]
    InvalidScenarioName {
        /// Rejected value.
        value: String,
    },
    /// The registry does not contain the requested scenario.
    #[error("unknown scripted scenario {name:?}")]
    UnknownScenario {
        /// Requested name.
        name: String,
    },
    /// The fixture exceeded the input bound.
    #[error("script fixture is {bytes} bytes; the limit is {MAX_SCRIPT_BYTES}")]
    FixtureTooLarge {
        /// Actual byte length.
        bytes: usize,
    },
    /// JSON could not be decoded as the current strict wire form.
    #[error("invalid script fixture: {0}")]
    InvalidFixture(#[from] serde_json::Error),
    /// The fixture came from a newer Harkness build.
    #[error(
        "script fixture version {found} is newer than supported version {supported}; upgrade Harkness"
    )]
    FixtureTooNew {
        /// Version in the fixture.
        found: u32,
        /// Newest version this build supports.
        supported: u32,
    },
    /// The fixture version is older than the supported format floor.
    #[error("script fixture version {found} is not supported")]
    UnsupportedFixtureVersion {
        /// Version in the fixture.
        found: u32,
    },
    /// Structurally valid JSON described a script that cannot be replayed.
    #[error("invalid script {scenario}: {reason}")]
    InvalidDefinition {
        /// Scenario being checked.
        scenario: ScenarioName,
        /// Stable refusal reason.
        reason: &'static str,
    },
    /// A fixture named a scenario whose recorded identity is not its own.
    #[error("script fixture for {expected} declares the identity {found}")]
    MisfiledFixture {
        /// The registry name the fixture was loaded under.
        expected: String,
        /// The identity the fixture declares.
        found: ScenarioName,
    },
}

impl ScriptError {
    /// Every stable discriminant this namespace can emit.
    pub const KINDS: &'static [&'static str] = &[
        "invalid_scenario_name",
        "unknown_scenario",
        "fixture_too_large",
        "invalid_fixture",
        "fixture_too_new",
        "unsupported_fixture_version",
        "invalid_definition",
        "misfiled_fixture",
    ];

    /// Stable machine-readable discriminant.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::InvalidScenarioName { .. } => "invalid_scenario_name",
            Self::UnknownScenario { .. } => "unknown_scenario",
            Self::FixtureTooLarge { .. } => "fixture_too_large",
            Self::InvalidFixture(_) => "invalid_fixture",
            Self::FixtureTooNew { .. } => "fixture_too_new",
            Self::UnsupportedFixtureVersion { .. } => "unsupported_fixture_version",
            Self::InvalidDefinition { .. } => "invalid_definition",
            Self::MisfiledFixture { .. } => "misfiled_fixture",
        }
    }
}

/// A failure a script injects at a chosen point in a stream.
///
/// One variant per [`ProviderError`] kind, so the ten are injectable by
/// construction and a fixture naming one this build does not define fails to
/// parse rather than replaying something else.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScriptFailure {
    /// Inject [`ProviderError::EndpointUnreachable`].
    EndpointUnreachable {
        /// What the failure reports.
        detail: String,
    },
    /// Inject [`ProviderError::AuthenticationFailed`].
    AuthenticationFailed {
        /// What the failure reports.
        detail: String,
    },
    /// Inject [`ProviderError::RateLimited`].
    RateLimited {
        /// The window the endpoint asks for, when the fixture names one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_millis: Option<u64>,
        /// What the failure reports.
        detail: String,
    },
    /// Inject [`ProviderError::ContextOverflow`].
    ContextOverflow {
        /// What the failure reports.
        detail: String,
    },
    /// Inject [`ProviderError::ProviderTimeout`].
    ProviderTimeout {
        /// What the failure reports.
        detail: String,
    },
    /// Inject [`ProviderError::Disconnected`]. The partial turn is attached by
    /// the driver, exactly as it would be for a real stream.
    Disconnected {
        /// What the failure reports.
        detail: String,
    },
    /// Inject [`ProviderError::MalformedResponse`].
    MalformedResponse {
        /// What the failure reports.
        detail: String,
    },
    /// Inject [`ProviderError::UnsupportedCapability`].
    UnsupportedCapability {
        /// The capability the request asked for.
        capability: String,
    },
    /// Inject [`ProviderError::EmptyResponse`].
    EmptyResponse,
    /// Inject [`ProviderError::Cancelled`].
    ///
    /// Present for completeness of the injection matrix. A real cancellation is
    /// observed from the token rather than scripted, which is what the
    /// cancellation tests exercise.
    Cancelled,
}

impl ScriptFailure {
    /// Builds the error this failure stands for.
    #[must_use]
    pub fn to_error(&self) -> ProviderError {
        match self {
            Self::EndpointUnreachable { detail } => ProviderError::endpoint_unreachable(detail),
            Self::AuthenticationFailed { detail } => ProviderError::authentication_failed(detail),
            Self::RateLimited {
                retry_after_millis,
                detail,
            } => ProviderError::rate_limited(
                retry_after_millis.map(Duration::from_millis),
                detail.clone(),
            ),
            Self::ContextOverflow { detail } => ProviderError::context_overflow(detail),
            Self::ProviderTimeout { detail } => ProviderError::provider_timeout(detail),
            Self::Disconnected { detail } => ProviderError::disconnected(detail),
            Self::MalformedResponse { detail } => ProviderError::malformed_response(detail),
            Self::UnsupportedCapability { capability } => {
                ProviderError::unsupported_capability(capability)
            }
            Self::EmptyResponse => ProviderError::EmptyResponse,
            Self::Cancelled => ProviderError::Cancelled,
        }
    }
}

/// One instruction in a script.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "step", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScriptStep {
    /// Emit one event exactly as written.
    Emit {
        /// The event.
        event: ModelEvent,
    },
    /// Emit argument text for one call, chopped at the given byte offsets.
    ///
    /// Offsets are *byte* offsets and may land inside a multi-byte character:
    /// a fragment is only emitted once the character it started is complete, so
    /// what reaches a sink is always valid UTF-8 and what assembles is always
    /// the same call. Several of these steps for one index concatenate, which is
    /// how interleaved calls are written.
    Arguments {
        /// The call being described.
        index: u32,
        /// The text to send.
        text: String,
        /// Where to chop it. Strictly increasing and within the text.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        split_at: Vec<usize>,
    },
    /// Move the turn's clock forward, so latencies are deterministic.
    Advance {
        /// Milliseconds to advance.
        millis: u64,
    },
    /// End the stream with a failure. Must be the last step.
    Fail {
        /// The failure to inject.
        failure: ScriptFailure,
    },
}

/// A deterministic scenario: what a provider does, written down.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Script {
    version: u32,
    id: ScenarioName,
    capabilities: ProviderCapabilities,
    steps: Vec<ScriptStep>,
}

#[derive(Deserialize)]
struct VersionProbe {
    v: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptWire {
    v: u32,
    id: ScenarioName,
    capabilities: ProviderCapabilities,
    steps: Vec<ScriptStep>,
}

#[derive(Serialize)]
struct ScriptWireRef<'a> {
    v: u32,
    id: &'a ScenarioName,
    capabilities: &'a ProviderCapabilities,
    steps: &'a [ScriptStep],
}

impl Script {
    /// Parses a versioned strict JSON fixture after probing its version.
    ///
    /// # Errors
    ///
    /// Returns [`ScriptError::FixtureTooNew`] *before* the strict body is
    /// decoded when `v` came from a newer build, so a future fixture asks for
    /// an upgrade rather than reading as corrupt. A same-version unknown field
    /// is [`ScriptError::InvalidFixture`], and a script that cannot be replayed
    /// is [`ScriptError::InvalidDefinition`].
    pub fn from_json(bytes: &str) -> Result<Self, ScriptError> {
        if bytes.len() > MAX_SCRIPT_BYTES {
            return Err(ScriptError::FixtureTooLarge { bytes: bytes.len() });
        }
        let probe: VersionProbe = serde_json::from_str(bytes)?;
        if probe.v > SCRIPT_FIXTURE_VERSION {
            return Err(ScriptError::FixtureTooNew {
                found: probe.v,
                supported: SCRIPT_FIXTURE_VERSION,
            });
        }
        if probe.v < MIN_SCRIPT_FIXTURE_VERSION {
            return Err(ScriptError::UnsupportedFixtureVersion { found: probe.v });
        }
        let wire: ScriptWire = serde_json::from_str(bytes)?;
        let script = Self {
            version: wire.v,
            id: wire.id,
            capabilities: wire.capabilities,
            steps: wire.steps,
        };
        script.validate()?;
        Ok(script)
    }

    /// Produces the canonical pretty JSON the committed fixtures hold.
    ///
    /// # Errors
    ///
    /// Returns a JSON encoding error, although a script holds only
    /// representable values.
    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        let mut encoded = serde_json::to_string_pretty(&ScriptWireRef {
            v: self.version,
            id: &self.id,
            capabilities: &self.capabilities,
            steps: &self.steps,
        })?;
        encoded.push('\n');
        Ok(encoded)
    }

    /// Fixture version this script was written under.
    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    /// Stable scenario identity.
    #[must_use]
    pub const fn id(&self) -> &ScenarioName {
        &self.id
    }

    /// What the scripted provider claims for every model.
    #[must_use]
    pub const fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities
    }

    /// The instructions, in order.
    #[must_use]
    pub fn steps(&self) -> &[ScriptStep] {
        &self.steps
    }

    fn validate(&self) -> Result<(), ScriptError> {
        if self.steps.len() > MAX_SCRIPT_STEPS {
            return Err(self.invalid("a script exceeds the 128-step bound"));
        }
        // A script with no steps is the empty-response scenario, so emptiness is
        // meaningful rather than malformed.
        for (position, step) in self.steps.iter().enumerate() {
            match step {
                ScriptStep::Fail { .. } if position + 1 != self.steps.len() => {
                    return Err(
                        self.invalid("a failure ends the stream, so it must be the last step")
                    );
                }
                ScriptStep::Arguments { text, split_at, .. } => {
                    let mut previous = 0;
                    for offset in split_at {
                        if *offset > text.len() {
                            return Err(self.invalid("a split offset is past the end of its text"));
                        }
                        if *offset < previous {
                            return Err(self.invalid("split offsets must not decrease"));
                        }
                        previous = *offset;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn invalid(&self, reason: &'static str) -> ScriptError {
        ScriptError::InvalidDefinition {
            scenario: self.id.clone(),
            reason,
        }
    }
}

/// Chops `text` at `split_at` and returns the fragments that are whole text.
///
/// A chop inside a character contributes nothing and its bytes travel with the
/// next fragment, which is why the returned list can be shorter than the number
/// of offsets — and why every scenario replays byte-identically whatever the
/// offsets are.
///
/// # Errors
///
/// Returns [`ProviderError::MalformedResponse`] only if the accumulator is left
/// holding an incomplete character, which a complete `text` cannot do.
pub(crate) fn fragments(text: &str, split_at: &[usize]) -> Result<Vec<String>, ProviderError> {
    let bytes = text.as_bytes();
    let mut accumulator = Utf8Accumulator::new();
    let mut fragments = Vec::new();
    let mut previous = 0;
    for offset in split_at.iter().copied().chain(std::iter::once(bytes.len())) {
        let offset = offset.min(bytes.len()).max(previous);
        let released = accumulator.push(&bytes[previous..offset])?;
        if !released.is_empty() {
            fragments.push(released);
        }
        previous = offset;
    }
    accumulator.finish()?;
    Ok(fragments)
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_SCRIPT_BYTES, SCRIPT_FIXTURE_VERSION, ScenarioName, Script, ScriptError, ScriptFailure,
        fragments,
    };
    use crate::contract::ProviderError;

    const MINIMAL: &str = r#"{
  "v": 1,
  "id": "fixture",
  "capabilities": {},
  "steps": []
}"#;

    #[test]
    fn a_name_accepts_the_snake_case_grammar_and_nothing_else() {
        assert!(ScenarioName::new("text_only_turn").is_ok());
        for refused in ["", "Text", "text-only", "_leading", "trailing_", "a__b"] {
            assert!(ScenarioName::new(refused).is_err(), "accepted {refused}");
        }
    }

    #[test]
    fn a_fixture_probes_its_version_before_the_strict_body() {
        let future = MINIMAL.replace("\"v\": 1", "\"v\": 2");
        let error = Script::from_json(&future).unwrap_err();
        assert_eq!(error.kind(), "fixture_too_new");
        assert!(error.to_string().contains("upgrade Harkness"), "{error}");

        let ancient = MINIMAL.replace("\"v\": 1", "\"v\": 0");
        assert_eq!(
            Script::from_json(&ancient).unwrap_err().kind(),
            "unsupported_fixture_version"
        );

        // The probe reads only `v`, so a future fixture asks for an upgrade even
        // when its body is a shape this build could never decode.
        let future_body = "{\"v\": 9, \"id\": \"fixture\", \"quantum\": true}";
        assert_eq!(
            Script::from_json(future_body).unwrap_err().kind(),
            "fixture_too_new"
        );
    }

    #[test]
    fn a_same_version_unknown_field_is_a_malformed_current_fixture() {
        let unknown = MINIMAL.replace("\"steps\": []", "\"steps\": [], \"mood\": \"chatty\"");
        let error = Script::from_json(&unknown).unwrap_err();
        assert_eq!(error.kind(), "invalid_fixture");
        assert!(error.to_string().contains("mood"), "{error}");
    }

    #[test]
    fn an_oversized_fixture_is_refused_before_it_is_parsed() {
        let padded = format!("{}{}", " ".repeat(MAX_SCRIPT_BYTES + 1), MINIMAL);
        assert_eq!(
            Script::from_json(&padded).unwrap_err().kind(),
            "fixture_too_large"
        );
    }

    #[test]
    fn a_failure_that_is_not_the_last_step_is_refused() {
        let script = MINIMAL.replace(
            "\"steps\": []",
            "\"steps\": [\
             {\"step\": \"fail\", \"failure\": {\"kind\": \"cancelled\"}}, \
             {\"step\": \"advance\", \"millis\": 1}]",
        );
        let error = Script::from_json(&script).unwrap_err();
        assert_eq!(error.kind(), "invalid_definition");
        assert!(
            error.to_string().contains("must be the last step"),
            "{error}"
        );
    }

    #[test]
    fn a_split_offset_outside_its_text_is_refused() {
        let script = MINIMAL.replace(
            "\"steps\": []",
            "\"steps\": [{\"step\": \"arguments\", \"index\": 0, \"text\": \"{}\", \
             \"split_at\": [99]}]",
        );
        assert!(
            Script::from_json(&script)
                .unwrap_err()
                .to_string()
                .contains("past the end")
        );
    }

    #[test]
    fn a_script_round_trips_through_its_canonical_encoding() {
        let script = Script::from_json(MINIMAL).unwrap();
        assert_eq!(script.version(), SCRIPT_FIXTURE_VERSION);
        let encoded = script.to_json_pretty().unwrap();
        assert_eq!(Script::from_json(&encoded).unwrap(), script);
        assert!(encoded.ends_with("\n"));
    }

    /// The injection matrix has to be able to reach every published kind, and
    /// this is what makes that true by construction rather than by inspection.
    #[test]
    fn every_provider_error_kind_is_injectable() {
        let failures = [
            ScriptFailure::EndpointUnreachable {
                detail: "fixture".to_owned(),
            },
            ScriptFailure::AuthenticationFailed {
                detail: "fixture".to_owned(),
            },
            ScriptFailure::RateLimited {
                retry_after_millis: Some(1_000),
                detail: "fixture".to_owned(),
            },
            ScriptFailure::ContextOverflow {
                detail: "fixture".to_owned(),
            },
            ScriptFailure::ProviderTimeout {
                detail: "fixture".to_owned(),
            },
            ScriptFailure::Disconnected {
                detail: "fixture".to_owned(),
            },
            ScriptFailure::MalformedResponse {
                detail: "fixture".to_owned(),
            },
            ScriptFailure::UnsupportedCapability {
                capability: "tool calls".to_owned(),
            },
            ScriptFailure::EmptyResponse,
            ScriptFailure::Cancelled,
        ];
        let kinds = failures
            .iter()
            .map(|failure| failure.to_error().kind())
            .collect::<Vec<_>>();
        assert_eq!(kinds, ProviderError::KINDS);
    }

    #[test]
    fn a_fixture_naming_a_failure_this_build_does_not_define_fails_to_parse() {
        let script = MINIMAL.replace(
            "\"steps\": []",
            "\"steps\": [{\"step\": \"fail\", \"failure\": {\"kind\": \"quota_exhausted\"}}]",
        );
        assert_eq!(
            Script::from_json(&script).unwrap_err().kind(),
            "invalid_fixture"
        );
    }

    #[test]
    fn every_script_error_kind_is_declared() {
        assert_eq!(ScriptError::KINDS.len(), 8);
        assert_eq!(
            ScenarioName::new("Bad").unwrap_err().kind(),
            "invalid_scenario_name"
        );
    }

    #[test]
    fn fragments_never_carry_half_a_character() {
        let text = "{\"q\":\"café ☕\"}";
        let bytes = text.as_bytes();
        for offset in 0..=bytes.len() {
            let pieces = fragments(text, &[offset]).unwrap();
            assert_eq!(pieces.concat(), text, "split at {offset}");
            assert!(!pieces.iter().any(String::is_empty));
        }
        assert_eq!(fragments(text, &[]).unwrap(), vec![text.to_owned()]);
    }
}
