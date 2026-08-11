//! Behaviour tests for the tool contract, driven through the public surface.
//!
//! The tools here are fixtures, not production tools: an echo, a tool that
//! panics, a tool whose output contradicts its own schema, and a tool whose
//! schema cannot be compiled at all. None of them touches the filesystem, Git,
//! the network, or the run store, which is the point — the contract has to be
//! exercisable with nothing else wired up.
//!
//! The panic-containment tests print `thread ... panicked at ... a static panic
//! payload` and similar. That output is expected: the panics are deliberate and
//! the tests assert on the error they were converted into. Silencing it would mean
//! installing a panic hook, and a hook is process-global — tests run in parallel,
//! so a suppressing hook installed by one test masks a genuine panic in another,
//! and two tests swapping it can leave the suppressing one in place for the rest of
//! the run. Noisy output is a better trade than a debugging hazard.

use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use harkness_git::Cancellation;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::json;
use serde_json::value::RawValue;

use crate::domain::{RunId, StepId, ToolCallId};

use super::{
    DiscardedProgress, ExecutionContext, InvocationError, ProgressEvent, ProgressUnit,
    RecordedProgress, RegistryError, RiskLevel, Tool, ToolError, ToolId, ToolIdentity,
    ToolMetadata, ToolRegistry, ToolVersion, UnsupportedArtifacts, erase, invoke, invoke_resolved,
};

const WORKSPACE: &str = if cfg!(windows) {
    r"C:\workspace"
} else {
    "/workspace"
};

fn context() -> ExecutionContext {
    ExecutionContext::detached(RunId::new(), StepId::new(), ToolCallId::new(), WORKSPACE).unwrap()
}

fn raw(json: &str) -> Box<RawValue> {
    RawValue::from_string(json.to_owned()).unwrap()
}

fn id(value: &str) -> ToolId {
    ToolId::new(value).unwrap()
}

fn version(value: &str) -> ToolVersion {
    ToolVersion::new(value).unwrap()
}

// ---------------------------------------------------------------------------
// Fixture tools
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EchoInput {
    /// Text to echo back.
    message: String,
    /// How many times to repeat it.
    #[serde(default = "one")]
    repeat: u8,
}

const fn one() -> u8 {
    1
}

#[derive(Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct EchoOutput {
    /// The echoed text.
    echoed: String,
}

/// Echoes its input, counting how often its body actually ran.
///
/// The counter is what turns "validation happens first" from a claim about the
/// code into an assertion: a refused input must leave it untouched.
struct Echo {
    version: &'static str,
    executions: Arc<AtomicUsize>,
}

impl Echo {
    fn new(version: &'static str) -> Self {
        Self {
            version,
            executions: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn executions(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.executions)
    }
}

impl Tool for Echo {
    type Input = EchoInput;
    type Output = EchoOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse("fixture.echo", self.version).unwrap(),
            "Echo",
            "Repeats the supplied message.",
            RiskLevel::Observe,
        )
        .with_capabilities([super::Capability::new("fixture.read").unwrap()])
    }

    fn execute(
        &self,
        input: EchoInput,
        context: &mut ExecutionContext,
    ) -> Result<EchoOutput, ToolError> {
        self.executions.fetch_add(1, Ordering::Release);
        context.report(ProgressEvent::stage("echoing"));
        for index in 0..input.repeat {
            context.check_cancelled()?;
            context.report(ProgressEvent::counted(
                u64::from(index) + 1,
                u64::from(input.repeat),
                ProgressUnit::Items,
            ));
        }
        Ok(EchoOutput {
            echoed: input.message.repeat(usize::from(input.repeat)),
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Empty {}

/// Panics with the payload type the test asks for.
struct Panics {
    payload: PanicPayload,
}

#[derive(Clone, Copy, Debug)]
enum PanicPayload {
    /// A `&'static str`, the payload of `panic!("literal")`.
    Str,
    /// A `String`, the payload of a formatted `panic!`.
    Owned,
    /// Neither, so the message cannot be recovered.
    Opaque,
}

impl Tool for Panics {
    type Input = Empty;
    type Output = EchoOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse("fixture.panics", "1.0.0").unwrap(),
            "Panics",
            "Panics on every call, to prove the boundary contains it.",
            RiskLevel::Execute,
        )
    }

    fn execute(
        &self,
        _input: Empty,
        _context: &mut ExecutionContext,
    ) -> Result<EchoOutput, ToolError> {
        match self.payload {
            PanicPayload::Str => panic!("a static panic payload"),
            PanicPayload::Owned => panic!("a formatted payload: {}", 42),
            PanicPayload::Opaque => std::panic::panic_any(7_u32),
        }
    }
}

/// A value that serializes as a string while declaring an object schema.
///
/// A concrete `Output` struct cannot contradict its own generated schema, so
/// reaching the output gate at all needs a type where `Serialize` and
/// `JsonSchema` disagree. That is exactly the mistake the gate exists to catch:
/// a hand-written schema, a `serde(with = ...)` shim, or a `Value` output whose
/// real shape drifted from what was published.
#[derive(Debug)]
struct MismatchedOutput;

impl Serialize for MismatchedOutput {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str("not an object at all")
    }
}

impl JsonSchema for MismatchedOutput {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("MismatchedOutput")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "object",
            "properties": { "ok": { "type": "boolean" } },
            "required": ["ok"],
            "additionalProperties": false,
        })
    }
}

/// Returns a value its own declared output schema refuses.
struct BadOutput;

impl Tool for BadOutput {
    type Input = Empty;
    type Output = MismatchedOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse("fixture.bad_output", "1.0.0").unwrap(),
            "Bad output",
            "Returns a value that contradicts its declared output schema.",
            RiskLevel::Observe,
        )
    }

    fn execute(
        &self,
        _input: Empty,
        _context: &mut ExecutionContext,
    ) -> Result<MismatchedOutput, ToolError> {
        Ok(MismatchedOutput)
    }
}

/// A type whose declared schema is not a schema any validator can compile.
#[derive(Debug, Serialize)]
struct UncompilableSchema;

impl JsonSchema for UncompilableSchema {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("UncompilableSchema")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "not_a_json_schema_type" })
    }
}

/// Declares an input schema that cannot be compiled.
struct BrokenSchema;

impl Tool for BrokenSchema {
    type Input = Empty;
    type Output = UncompilableSchema;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse("fixture.broken_schema", "1.0.0").unwrap(),
            "Broken schema",
            "Declares an output schema no validator can compile.",
            RiskLevel::Observe,
        )
    }

    fn execute(
        &self,
        _input: Empty,
        _context: &mut ExecutionContext,
    ) -> Result<UncompilableSchema, ToolError> {
        Ok(UncompilableSchema)
    }
}

/// Reports the failure a tool author would report for a real problem.
struct Failing;

impl Tool for Failing {
    type Input = Empty;
    type Output = EchoOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse("fixture.failing", "1.0.0").unwrap(),
            "Failing",
            "Always reports a tool-authored failure.",
            RiskLevel::Observe,
        )
    }

    fn execute(
        &self,
        _input: Empty,
        _context: &mut ExecutionContext,
    ) -> Result<EchoOutput, ToolError> {
        Err(ToolError::execution_failed("the fixture refused"))
    }
}

/// One of many identities, so a lookup benchmark has something to scan past.
struct Indexed {
    index: usize,
}

impl Tool for Indexed {
    type Input = Empty;
    type Output = EchoOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new(
            ToolIdentity::parse(&format!("bench.tool_{}", self.index), "1.0.0").unwrap(),
            "Indexed",
            "One of many registered identities.",
            RiskLevel::Observe,
        )
    }

    fn execute(
        &self,
        _input: Empty,
        _context: &mut ExecutionContext,
    ) -> Result<EchoOutput, ToolError> {
        Ok(EchoOutput {
            echoed: String::new(),
        })
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

#[test]
fn duplicate_tool_id_and_version_registration_is_rejected() {
    let mut registry = ToolRegistry::new();
    registry.register(Echo::new("1.0.0")).unwrap();

    let error = registry.register(Echo::new("1.0.0")).unwrap_err();
    assert_eq!(error.kind(), "duplicate_registration");
    assert!(
        error.to_string().contains("fixture.echo@1.0.0"),
        "the refusal must name the identity that is taken: {error}"
    );
    assert_eq!(
        registry.len(),
        1,
        "a refused registration must not be stored"
    );

    // The same identifier at a different version is how a tool evolves.
    registry.register(Echo::new("1.1.0")).unwrap();
    registry.register(Echo::new("2.0.0-rc.1")).unwrap();
    assert_eq!(registry.len(), 3);
    assert_eq!(
        registry.versions(&id("fixture.echo")),
        [&version("1.0.0"), &version("1.1.0"), &version("2.0.0-rc.1")]
    );
}

#[test]
fn registration_refuses_metadata_a_person_could_not_read() {
    struct Blank;

    impl Tool for Blank {
        type Input = Empty;
        type Output = EchoOutput;

        fn metadata(&self) -> ToolMetadata {
            ToolMetadata::new(
                ToolIdentity::parse("fixture.blank", "1.0.0").unwrap(),
                "   ",
                "described",
                RiskLevel::Observe,
            )
        }

        fn execute(
            &self,
            _input: Empty,
            _context: &mut ExecutionContext,
        ) -> Result<EchoOutput, ToolError> {
            unreachable!("registration never succeeds")
        }
    }

    let mut registry = ToolRegistry::new();
    let error = registry.register(Blank).unwrap_err();
    assert_eq!(error.kind(), "invalid_metadata");
    assert!(registry.is_empty());
}

#[test]
fn a_schema_that_cannot_be_compiled_fails_registration_not_the_first_call() {
    let mut registry = ToolRegistry::new();
    let error = registry.register(BrokenSchema).unwrap_err();
    assert_eq!(error.kind(), "invalid_schema");
    assert!(
        error.to_string().contains("output schema"),
        "the refusal must name the side that is broken: {error}"
    );
    assert!(
        registry.is_empty(),
        "an uncompilable tool must not be reachable"
    );
}

#[test]
fn registered_descriptors_carry_generated_schemas_for_input_and_output() {
    let mut registry = ToolRegistry::new();
    registry.register(Echo::new("1.0.0")).unwrap();

    let descriptor = registry
        .get(&id("fixture.echo"), None)
        .unwrap()
        .descriptor();

    let input = descriptor.input_schema();
    assert_eq!(input["type"], json!("object"));
    assert_eq!(input["required"], json!(["message"]));
    assert_eq!(
        input["properties"]["message"]["type"],
        json!("string"),
        "the generated schema must describe the tool's own Input type: {input}"
    );
    assert_eq!(
        input["additionalProperties"],
        json!(false),
        "deny_unknown_fields must close the published schema: {input}"
    );

    let output = descriptor.output_schema();
    assert_eq!(output["required"], json!(["echoed"]));
    assert_eq!(output["properties"]["echoed"]["type"], json!("string"));

    // The descriptor also publishes what only the author could state.
    assert_eq!(descriptor.title(), "Echo");
    assert_eq!(descriptor.risk(), RiskLevel::Observe);
    assert_eq!(descriptor.capabilities().len(), 1);
    assert_eq!(descriptor.capabilities()[0].as_str(), "fixture.read");
}

#[test]
fn descriptor_enumeration_is_sorted_and_stable() {
    let mut registry = ToolRegistry::new();
    // Registered in an order that is neither alphabetical nor version-ordered.
    registry.register(Echo::new("1.10.0")).unwrap();
    registry
        .register(Panics {
            payload: PanicPayload::Str,
        })
        .unwrap();
    registry.register(Echo::new("1.9.0")).unwrap();
    registry.register(BadOutput).unwrap();
    registry.register(Echo::new("2.0.0")).unwrap();

    let enumerate = |registry: &ToolRegistry| {
        registry
            .descriptors()
            .map(|descriptor| descriptor.identity().to_string())
            .collect::<Vec<_>>()
    };

    let expected = [
        "fixture.bad_output@1.0.0",
        "fixture.echo@1.9.0",
        "fixture.echo@1.10.0",
        "fixture.echo@2.0.0",
        "fixture.panics@1.0.0",
    ];
    assert_eq!(enumerate(&registry), expected);

    // Stable, not merely sorted once: repeated enumeration of the same registry
    // is what `harkness contract` relies on to produce a diff-stable projection.
    for _ in 0..5 {
        assert_eq!(enumerate(&registry), expected);
    }
    assert_eq!(
        registry.ids().map(ToString::to_string).collect::<Vec<_>>(),
        ["fixture.bad_output", "fixture.echo", "fixture.panics"]
    );

    // And the serialized projection is byte-identical across runs.
    let project = |registry: &ToolRegistry| {
        serde_json::to_string(&registry.descriptors().collect::<Vec<_>>()).unwrap()
    };
    assert_eq!(project(&registry), project(&registry));
}

// ---------------------------------------------------------------------------
// Lookup
// ---------------------------------------------------------------------------

#[test]
fn lookup_resolves_an_exact_version_and_the_latest_of_an_id() {
    let mut registry = ToolRegistry::new();
    for spelling in ["1.0.0", "1.9.0", "1.10.0", "2.0.0-rc.1"] {
        registry.register(Echo::new(spelling)).unwrap();
    }

    for spelling in ["1.0.0", "1.9.0", "1.10.0", "2.0.0-rc.1"] {
        let resolved = registry
            .resolve(&id("fixture.echo"), Some(&version(spelling)))
            .unwrap();
        assert_eq!(resolved.descriptor().version(), &version(spelling));
    }

    // Latest is by precedence among *stable* versions, so `1.10.0` beats `1.9.0`
    // rather than losing to it on string order, and the registered `2.0.0-rc.1`
    // does not hijack callers that named no version.
    assert_eq!(
        registry.latest_version(&id("fixture.echo")),
        Some(&version("1.10.0"))
    );
    assert_eq!(
        registry
            .resolve(&id("fixture.echo"), None)
            .unwrap()
            .descriptor()
            .version(),
        &version("1.10.0")
    );

    registry.register(Echo::new("2.0.0")).unwrap();
    assert_eq!(
        registry.latest_version(&id("fixture.echo")),
        Some(&version("2.0.0")),
        "a released version becomes the new default"
    );
}

#[test]
fn a_pre_release_does_not_become_the_default_for_unversioned_callers() {
    let mut registry = ToolRegistry::new();
    registry.register(Echo::new("1.10.0")).unwrap();
    assert_eq!(
        registry.latest_version(&id("fixture.echo")),
        Some(&version("1.10.0"))
    );

    // Publishing a release candidate must not silently redirect production. Raw
    // semver precedence puts 2.0.0-rc.1 above 1.10.0, so an unfiltered
    // "highest version" would move every unpinned caller onto the candidate.
    registry.register(Echo::new("2.0.0-rc.1")).unwrap();
    assert_eq!(
        registry.latest_version(&id("fixture.echo")),
        Some(&version("1.10.0")),
        "a pre-release must not take over unversioned resolution"
    );

    // It is still reachable, by asking for it.
    assert_eq!(
        registry
            .resolve(&id("fixture.echo"), Some(&version("2.0.0-rc.1")))
            .unwrap()
            .descriptor()
            .version(),
        &version("2.0.0-rc.1")
    );

    // With nothing stable registered, a pre-release is better than refusing.
    let mut only_prerelease = ToolRegistry::new();
    only_prerelease
        .register(Echo::new("0.1.0-alpha.1"))
        .unwrap();
    only_prerelease
        .register(Echo::new("0.1.0-alpha.2"))
        .unwrap();
    assert_eq!(
        only_prerelease.latest_version(&id("fixture.echo")),
        Some(&version("0.1.0-alpha.2")),
        "the highest pre-release wins when there is no stable version"
    );
}

#[test]
fn a_tool_dispatched_after_cancellation_never_starts() {
    // Cooperative cancellation inside the body is not enough: a tool that does
    // its work in one non-polling call would otherwise complete a push or a
    // delete after the user cancelled. The pipeline gates on the token itself.
    struct NeverPolls {
        executions: Arc<AtomicUsize>,
    }

    impl Tool for NeverPolls {
        type Input = Empty;
        type Output = EchoOutput;

        fn metadata(&self) -> ToolMetadata {
            ToolMetadata::new(
                ToolIdentity::parse("fixture.never_polls", "1.0.0").unwrap(),
                "Never polls",
                "Does its work in one call without checking cancellation.",
                RiskLevel::RemoteWrite,
            )
        }

        fn execute(
            &self,
            _input: Empty,
            _context: &mut ExecutionContext,
        ) -> Result<EchoOutput, ToolError> {
            self.executions.fetch_add(1, Ordering::Release);
            Ok(EchoOutput {
                echoed: "pushed".to_owned(),
            })
        }
    }

    let executions = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry
        .register(NeverPolls {
            executions: Arc::clone(&executions),
        })
        .unwrap();

    let cancellation = Cancellation::default();
    let mut context = ExecutionContext::new(
        RunId::new(),
        StepId::new(),
        ToolCallId::new(),
        WORKSPACE,
        cancellation.clone(),
        Box::new(DiscardedProgress),
        Box::new(UnsupportedArtifacts),
    )
    .unwrap();
    cancellation.cancel();

    let error = invoke(
        &registry,
        &id("fixture.never_polls"),
        None,
        &raw("{}"),
        &mut context,
    )
    .unwrap_err();

    assert_eq!(error.kind(), "cancelled");
    assert_eq!(
        executions.load(Ordering::Acquire),
        0,
        "a tool that does not poll still ran after cancellation"
    );
}

#[test]
fn an_unknown_lookup_is_a_typed_not_found_and_never_a_panic() {
    let mut registry = ToolRegistry::new();
    registry.register(Echo::new("1.0.0")).unwrap();

    let unknown_id = registry.resolve(&id("fixture.absent"), None).unwrap_err();
    assert_eq!(unknown_id.kind(), "unknown_tool");
    assert!(unknown_id.to_string().contains("fixture.absent"));
    assert!(registry.get(&id("fixture.absent"), None).is_none());

    let unknown_version = registry
        .resolve(&id("fixture.echo"), Some(&version("9.9.9")))
        .unwrap_err();
    assert_eq!(unknown_version.kind(), "unknown_tool_version");
    assert!(
        unknown_version.to_string().contains("available: 1.0.0"),
        "a stale pin should be told what does exist: {unknown_version}"
    );
    assert!(
        registry
            .get(&id("fixture.echo"), Some(&version("9.9.9")))
            .is_none()
    );

    assert!(
        ToolRegistry::new()
            .resolve(&id("fixture.echo"), None)
            .is_err(),
        "an empty registry resolves nothing"
    );
}

// ---------------------------------------------------------------------------
// Invocation
// ---------------------------------------------------------------------------

#[test]
fn a_tool_invokes_directly_without_agent_policy_or_store() {
    // No agent, no policy engine, no database, no filesystem: a registry, a
    // JSON string, and a context.
    let mut registry = ToolRegistry::new();
    registry.register(Echo::new("1.0.0")).unwrap();
    let mut context = context();

    let outcome = invoke(
        &registry,
        &id("fixture.echo"),
        None,
        &raw(r#"{"message":"ab","repeat":3}"#),
        &mut context,
    )
    .unwrap();

    assert_eq!(outcome.output().get(), r#"{"echoed":"ababab"}"#);
    // The resolved identity travels with the result, because a caller that asked
    // for "the latest" has to record which version actually ran.
    assert_eq!(outcome.tool().to_string(), "fixture.echo@1.0.0");

    let (identity, output) = outcome.into_parts();
    assert_eq!(identity.version, version("1.0.0"));
    assert_eq!(
        serde_json::from_str::<EchoOutput>(output.get()).unwrap(),
        EchoOutput {
            echoed: "ababab".to_owned()
        }
    );
}

#[test]
fn a_defaulted_field_may_be_omitted_from_the_input() {
    let mut registry = ToolRegistry::new();
    registry.register(Echo::new("1.0.0")).unwrap();
    let mut context = context();

    let outcome = invoke(
        &registry,
        &id("fixture.echo"),
        Some(&version("1.0.0")),
        &raw(r#"{"message":"once"}"#),
        &mut context,
    )
    .unwrap();
    assert_eq!(outcome.output().get(), r#"{"echoed":"once"}"#);
}

#[test]
fn schema_invalid_input_is_refused_before_the_tool_body_runs() {
    let echo = Echo::new("1.0.0");
    let executions = echo.executions();
    let mut registry = ToolRegistry::new();
    registry.register(echo).unwrap();
    let mut context = context();

    let cases = [
        // Wrong type.
        (r#"{"message":42}"#, "/message"),
        // Missing required field.
        (r"{}", ""),
        // Unknown field, refused because the Input type denies unknown fields.
        (r#"{"message":"a","surprise":true}"#, ""),
        // Wrong top-level type entirely.
        (r#"["message"]"#, ""),
        // Out of range for the declared integer type.
        (r#"{"message":"a","repeat":4096}"#, "/repeat"),
    ];

    for (input, pointer) in cases {
        let error = invoke(
            &registry,
            &id("fixture.echo"),
            None,
            &raw(input),
            &mut context,
        )
        .unwrap_err();

        assert_eq!(error.kind(), "invalid_input", "accepted {input}");
        let Some(ToolError::InvalidInput {
            tool, violations, ..
        }) = error.tool_error()
        else {
            panic!("expected an input refusal for {input}, got {error:?}");
        };
        assert_eq!(tool.to_string(), "fixture.echo@1.0.0");
        assert_eq!(
            error.tool().map(ToString::to_string),
            Some("fixture.echo@1.0.0".to_owned()),
            "the refused call must name the version it resolved"
        );
        assert!(
            !violations.is_empty(),
            "no violation was located for {input}"
        );
        assert_eq!(
            violations[0].pointer(),
            pointer,
            "unexpected pointer for {input}: {violations:?}"
        );
        assert!(
            error.as_failure().kind() == "invalid_input",
            "the refusal must be recordable against the call"
        );
    }

    // The whole guarantee: nothing ran, so a caller may retry a correction
    // without wondering whether the first attempt had a side effect.
    assert_eq!(
        executions.load(Ordering::Acquire),
        0,
        "the tool body ran despite a refused input"
    );
    assert!(
        ToolError::InvalidInput {
            tool: ToolIdentity::parse("fixture.echo", "1.0.0").unwrap(),
            violations: Vec::new(),
            omitted: 0,
        }
        .happened_before_execution()
    );

    // And a corrected input on the same registry still works.
    let outcome = invoke(
        &registry,
        &id("fixture.echo"),
        None,
        &raw(r#"{"message":"ok"}"#),
        &mut context,
    )
    .unwrap();
    assert_eq!(outcome.output().get(), r#"{"echoed":"ok"}"#);
    assert_eq!(executions.load(Ordering::Acquire), 1);
}

#[test]
fn schema_invalid_output_is_refused_before_delivery() {
    let mut registry = ToolRegistry::new();
    registry.register(BadOutput).unwrap();
    let mut context = context();

    let error = invoke(
        &registry,
        &id("fixture.bad_output"),
        None,
        &raw("{}"),
        &mut context,
    )
    .unwrap_err();

    assert_eq!(error.kind(), "invalid_output");
    let Some(ToolError::InvalidOutput {
        tool, violations, ..
    }) = error.tool_error()
    else {
        panic!("expected an output refusal, got {error:?}");
    };
    assert_eq!(tool.to_string(), "fixture.bad_output@1.0.0");
    assert_eq!(
        violations[0].pointer(),
        "",
        "a wrong top-level output type is located at the root: {violations:?}"
    );
    assert!(
        error.to_string().contains("output does not satisfy"),
        "{error}"
    );
    // The refusal quotes what was wrong, so the tool author can see it, but it
    // arrives as a failure. There is no code path on which a caller receives the
    // refused value as a result: `invoke` returns `Err`, so the schema-violating
    // output never becomes a `ToolOutcome`.
    assert!(
        error
            .as_failure()
            .message()
            .contains("not an object at all"),
        "the diagnostic should quote the offending value: {error}"
    );
    assert_eq!(error.as_failure().kind(), "invalid_output");
}

#[test]
fn a_panicking_tool_becomes_a_structured_error_and_the_registry_survives() {
    let mut registry = ToolRegistry::new();
    registry
        .register(Panics {
            payload: PanicPayload::Str,
        })
        .unwrap();
    registry.register(Echo::new("1.0.0")).unwrap();
    let mut context = context();

    let error = invoke(
        &registry,
        &id("fixture.panics"),
        None,
        &raw("{}"),
        &mut context,
    )
    .unwrap_err();

    assert_eq!(error.kind(), "tool_panicked");
    let Some(ToolError::ToolPanicked { tool, payload }) = error.tool_error() else {
        panic!("expected a contained panic, got {error:?}");
    };
    assert_eq!(tool.to_string(), "fixture.panics@1.0.0");
    assert_eq!(payload.as_deref(), Some("a static panic payload"));

    // The calling thread and the registry are both still usable, which is the
    // property that keeps one buggy tool from orphaning a run record.
    let outcome = invoke(
        &registry,
        &id("fixture.echo"),
        None,
        &raw(r#"{"message":"still here"}"#),
        &mut context,
    )
    .unwrap();
    assert_eq!(outcome.output().get(), r#"{"echoed":"still here"}"#);

    // A second panic on the same registry is contained the same way.
    let again = invoke(
        &registry,
        &id("fixture.panics"),
        None,
        &raw("{}"),
        &mut context,
    )
    .unwrap_err();
    assert_eq!(again.kind(), "tool_panicked");
}

#[test]
fn a_panic_payload_is_recovered_when_it_is_a_string_and_omitted_otherwise() {
    let expectations = [
        (PanicPayload::Str, Some("a static panic payload".to_owned())),
        (
            PanicPayload::Owned,
            Some("a formatted payload: 42".to_owned()),
        ),
        (PanicPayload::Opaque, None),
    ];

    for (payload, expected) in expectations {
        let tool = erase(Panics { payload }).unwrap();
        let mut context = context();

        let error = tool.execute_json(&raw("{}"), &mut context).unwrap_err();

        let ToolError::ToolPanicked {
            payload: recovered, ..
        } = &error
        else {
            panic!("expected a contained panic for {payload:?}, got {error:?}");
        };
        assert_eq!(recovered, &expected, "for {payload:?}");
        // Either way the tool is named, so the report is attributable.
        assert!(
            error
                .to_string()
                .starts_with("fixture.panics@1.0.0 panicked")
        );
    }
}

#[test]
fn a_failed_unpinned_call_still_reports_the_version_that_ran() {
    // The row a coordinator writes for a failed call needs `tool_version`, and
    // the caller named no version. Re-resolving to find it is a second lookup
    // that could disagree with the first — for instance if a newer version were
    // registered in between — so the failure carries the resolved identity.
    let mut registry = ToolRegistry::new();
    registry.register(Failing).unwrap();
    let mut context = context();

    let error = invoke(
        &registry,
        &id("fixture.failing"),
        None,
        &raw("{}"),
        &mut context,
    )
    .unwrap_err();

    // `execution_failed` is authored by the tool, which does not know its own
    // identity, so this is the variant that would otherwise have nothing to
    // record against.
    assert_eq!(error.kind(), "execution_failed");
    assert_eq!(
        error.tool(),
        Some(&ToolIdentity::parse("fixture.failing", "1.0.0").unwrap())
    );

    // Every failure path that reached a tool answers, not just the ones whose
    // ToolError variant happens to name it.
    let cancelled = {
        let cancellation = Cancellation::default();
        let mut cancelled_context = ExecutionContext::new(
            RunId::new(),
            StepId::new(),
            ToolCallId::new(),
            WORKSPACE,
            cancellation.clone(),
            Box::new(DiscardedProgress),
            Box::new(UnsupportedArtifacts),
        )
        .unwrap();
        cancellation.cancel();
        invoke(
            &registry,
            &id("fixture.failing"),
            None,
            &raw("{}"),
            &mut cancelled_context,
        )
        .unwrap_err()
    };
    assert_eq!(cancelled.kind(), "cancelled");
    assert_eq!(
        cancelled.tool().map(|tool| tool.version.to_string()),
        Some("1.0.0".to_owned())
    );

    // A resolution failure is the one case with no version, because resolving is
    // what failed.
    let unresolved = invoke(
        &registry,
        &id("fixture.absent"),
        None,
        &raw("{}"),
        &mut context,
    )
    .unwrap_err();
    assert_eq!(unresolved.tool(), None);
}

#[test]
fn a_tool_can_return_an_artifact_reference_in_its_output() {
    // `ArtifactRef` exists to be returned inside a tool's `Output`, and an
    // `Output` must implement `JsonSchema`. This test is the guard on that: without
    // the derive on `ArtifactRef` the tool below does not compile, which would make
    // the module's only documented route for returning stored content unusable.
    #[derive(Serialize, JsonSchema)]
    struct Stored {
        log: super::ArtifactRef,
    }

    struct WritesAnArtifact;

    impl Tool for WritesAnArtifact {
        type Input = Empty;
        type Output = Stored;

        fn metadata(&self) -> ToolMetadata {
            ToolMetadata::new(
                ToolIdentity::parse("fixture.stores", "1.0.0").unwrap(),
                "Stores",
                "Writes an artifact and returns a reference to it.",
                RiskLevel::WorkspaceWrite,
            )
        }

        fn execute(
            &self,
            _input: Empty,
            context: &mut ExecutionContext,
        ) -> Result<Stored, ToolError> {
            let log = context.write_artifact("build.log", "text/plain", b"output")?;
            Ok(Stored { log })
        }
    }

    struct Storing;

    impl super::ArtifactWriter for Storing {
        fn open(
            &mut self,
            _name: &str,
            media_type: &str,
        ) -> Result<Box<dyn super::ArtifactStream>, ToolError> {
            Ok(Box::new(StoringStream {
                media_type: media_type.to_owned(),
                byte_len: 0,
            }))
        }
    }

    struct StoringStream {
        media_type: String,
        byte_len: u64,
    }

    impl std::io::Write for StoringStream {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.byte_len += buffer.len() as u64;
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl super::ArtifactStream for StoringStream {
        fn finish(self: Box<Self>) -> Result<super::ArtifactRef, ToolError> {
            Ok(super::ArtifactRef {
                id: "artifact-1".to_owned(),
                media_type: self.media_type,
                byte_len: self.byte_len,
            })
        }
    }

    let mut registry = ToolRegistry::new();
    registry.register(WritesAnArtifact).unwrap();

    // The generated output schema describes the reference, so a consumer of the
    // published contract knows what it is receiving.
    let schema = registry
        .get(&id("fixture.stores"), None)
        .unwrap()
        .descriptor()
        .output_schema();
    assert_eq!(
        schema["properties"]["log"]["$ref"],
        json!("#/$defs/ArtifactRef"),
        "the reference should be published as a definition: {schema}"
    );
    assert_eq!(
        schema["$defs"]["ArtifactRef"]["required"],
        json!(["id", "media_type", "byte_len"])
    );

    let mut context = ExecutionContext::new(
        RunId::new(),
        StepId::new(),
        ToolCallId::new(),
        WORKSPACE,
        Cancellation::default(),
        Box::new(DiscardedProgress),
        Box::new(Storing),
    )
    .unwrap();

    // And the round trip passes the output validation gate, which is what would
    // fail if the schema and the type disagreed.
    //
    // Note the key order: the delivered JSON is sorted, not in field-declaration
    // order, because the output gate re-serializes through `serde_json::Value` and
    // its object map is a `BTreeMap`. That canonicalization is a property worth
    // relying on rather than an accident — it is what lets a hash taken over a
    // tool's result be stable across builds and across tools that declare the same
    // fields in a different order.
    let outcome = invoke(
        &registry,
        &id("fixture.stores"),
        None,
        &raw("{}"),
        &mut context,
    )
    .unwrap();
    assert_eq!(
        outcome.output().get(),
        r#"{"log":{"byte_len":6,"id":"artifact-1","media_type":"text/plain"}}"#
    );
}

#[test]
fn delivered_output_has_canonical_key_order() {
    // The property the artifact test observes, stated on its own so it is a
    // contract rather than an incidental detail of one assertion. #92 will hash
    // over recorded input and output, and a hash is only stable if the bytes are.
    #[derive(Serialize, JsonSchema)]
    struct Unsorted {
        zebra: u8,
        apple: u8,
        mango: u8,
    }

    struct Declares;

    impl Tool for Declares {
        type Input = Empty;
        type Output = Unsorted;

        fn metadata(&self) -> ToolMetadata {
            ToolMetadata::new(
                ToolIdentity::parse("fixture.unsorted", "1.0.0").unwrap(),
                "Unsorted",
                "Declares its output fields out of alphabetical order.",
                RiskLevel::Observe,
            )
        }

        fn execute(
            &self,
            _input: Empty,
            _context: &mut ExecutionContext,
        ) -> Result<Unsorted, ToolError> {
            Ok(Unsorted {
                zebra: 1,
                apple: 2,
                mango: 3,
            })
        }
    }

    let mut registry = ToolRegistry::new();
    registry.register(Declares).unwrap();
    let mut context = context();

    let outcome = invoke(
        &registry,
        &id("fixture.unsorted"),
        None,
        &raw("{}"),
        &mut context,
    )
    .unwrap();
    assert_eq!(
        outcome.output().get(),
        r#"{"apple":2,"mango":3,"zebra":1}"#,
        "the pipeline should deliver canonical key order"
    );
}

#[test]
fn a_body_raised_schema_error_cannot_claim_the_body_never_ran() {
    // `ToolError` is `#[non_exhaustive]` at the enum level, which does not seal its
    // variants, so a tool can construct `InvalidInput` itself and return it after
    // having already done work. Left alone that error would tell a coordinator
    // nothing ran and licence a retry that repeats the earlier side effect.
    struct ValidatesItself {
        writes: Arc<AtomicUsize>,
    }

    impl Tool for ValidatesItself {
        type Input = Empty;
        type Output = EchoOutput;

        fn metadata(&self) -> ToolMetadata {
            ToolMetadata::new(
                ToolIdentity::parse("fixture.self_validating", "1.0.0").unwrap(),
                "Self validating",
                "Writes, then reports an input violation of its own.",
                RiskLevel::WorkspaceWrite,
            )
        }

        fn execute(
            &self,
            _input: Empty,
            _context: &mut ExecutionContext,
        ) -> Result<EchoOutput, ToolError> {
            self.writes.fetch_add(1, Ordering::Release);
            Err(ToolError::InvalidInput {
                tool: ToolIdentity::parse("fixture.self_validating", "1.0.0").unwrap(),
                violations: vec![super::SchemaViolation::new(
                    "/nested",
                    "",
                    "the tool's own opinion",
                )],
                omitted: 0,
            })
        }
    }

    let writes = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry
        .register(ValidatesItself {
            writes: Arc::clone(&writes),
        })
        .unwrap();
    let mut context = context();

    let error = invoke(
        &registry,
        &id("fixture.self_validating"),
        None,
        &raw("{}"),
        &mut context,
    )
    .unwrap_err();

    assert_eq!(writes.load(Ordering::Acquire), 1, "the body did run");
    assert_eq!(
        error.kind(),
        "execution_failed",
        "a body-raised schema error must be re-attributed"
    );
    let Some(tool_error) = error.tool_error() else {
        panic!("expected a tool failure, got {error:?}");
    };
    assert!(
        !tool_error.happened_before_execution(),
        "the retry guarantee must not survive a body that already wrote"
    );
    // The tool's complaint is still readable, it just cannot pose as a
    // pre-execution refusal.
    assert!(
        error.to_string().contains("the tool's own opinion"),
        "the detail should be preserved: {error}"
    );
    assert!(
        error.to_string().contains("had already validated"),
        "the re-attribution should say what happened: {error}"
    );
}

#[test]
fn a_tool_authored_failure_reaches_the_caller_unchanged() {
    let mut registry = ToolRegistry::new();
    registry.register(Failing).unwrap();
    let mut context = context();

    let error = invoke(
        &registry,
        &id("fixture.failing"),
        None,
        &raw("{}"),
        &mut context,
    )
    .unwrap_err();

    assert_eq!(error.kind(), "execution_failed");
    assert!(error.to_string().contains("the fixture refused"), "{error}");
    let failure = error.as_failure();
    assert_eq!(failure.kind(), "execution_failed");
    assert!(failure.message().contains("the fixture refused"));
}

#[test]
fn cancellation_reaches_the_tool_through_the_shared_token() {
    let mut registry = ToolRegistry::new();
    registry.register(Echo::new("1.0.0")).unwrap();

    let cancellation = Cancellation::default();
    let mut context = ExecutionContext::new(
        RunId::new(),
        StepId::new(),
        ToolCallId::new(),
        WORKSPACE,
        cancellation.clone(),
        Box::new(DiscardedProgress),
        Box::new(UnsupportedArtifacts),
    )
    .unwrap();
    cancellation.cancel();

    let error = invoke(
        &registry,
        &id("fixture.echo"),
        None,
        &raw(r#"{"message":"a","repeat":2}"#),
        &mut context,
    )
    .unwrap_err();
    assert_eq!(error.kind(), "cancelled");
    assert!(
        !error.as_failure().message().is_empty(),
        "a cancellation still records why the call ended"
    );
}

#[test]
fn a_running_tool_reports_progress_through_its_context() {
    let mut registry = ToolRegistry::new();
    registry.register(Echo::new("1.0.0")).unwrap();

    let recorder = RecordedProgress::new();
    let mut context = ExecutionContext::new(
        RunId::new(),
        StepId::new(),
        ToolCallId::new(),
        WORKSPACE,
        Cancellation::default(),
        Box::new(recorder.clone()),
        Box::new(UnsupportedArtifacts),
    )
    .unwrap();

    invoke(
        &registry,
        &id("fixture.echo"),
        None,
        &raw(r#"{"message":"a","repeat":2}"#),
        &mut context,
    )
    .unwrap();

    assert_eq!(
        recorder.events(),
        [
            ProgressEvent::stage("echoing"),
            ProgressEvent::counted(1, 2, ProgressUnit::Items),
            ProgressEvent::counted(2, 2, ProgressUnit::Items),
        ]
    );
}

#[test]
fn an_unresolvable_invocation_reports_a_resolution_failure_not_a_tool_failure() {
    let registry = ToolRegistry::new();
    let mut context = context();

    let error = invoke(
        &registry,
        &id("fixture.echo"),
        None,
        &raw("{}"),
        &mut context,
    )
    .unwrap_err();

    assert!(
        matches!(
            error,
            InvocationError::Resolution(RegistryError::UnknownTool { .. })
        ),
        "{error:?}"
    );
    assert_eq!(error.kind(), "unknown_tool");
    // The split is the point: a `ToolError` always describes an invocation that
    // reached a real tool.
    assert!(InvocationError::kinds().contains(&"unknown_tool"));
}

#[test]
fn a_caller_that_resolved_first_invokes_the_tool_it_already_inspected() {
    // The shape a policy engine wants: resolve, read the descriptor, decide, then
    // run *that* tool — with no second lookup that could disagree with the first.
    let mut registry = ToolRegistry::new();
    registry.register(Echo::new("1.0.0")).unwrap();
    registry.register(Echo::new("2.0.0")).unwrap();
    let mut context = context();

    let tool = registry.resolve(&id("fixture.echo"), None).unwrap();
    assert_eq!(tool.descriptor().risk(), RiskLevel::Observe);
    assert_eq!(tool.descriptor().version(), &version("2.0.0"));

    let outcome = invoke_resolved(tool, &raw(r#"{"message":"pinned"}"#), &mut context).unwrap();
    assert_eq!(outcome.tool().version, version("2.0.0"));
    assert_eq!(outcome.output().get(), r#"{"echoed":"pinned"}"#);

    // Both entry points agree on the version and the result.
    let through_invoke = invoke(
        &registry,
        &id("fixture.echo"),
        None,
        &raw(r#"{"message":"pinned"}"#),
        &mut context,
    )
    .unwrap();
    assert_eq!(through_invoke, outcome);
}

#[test]
fn an_erased_tool_can_be_registered_and_shared_without_its_own_type() {
    let echo = erase(Echo::new("1.0.0")).unwrap();
    let mut registry = ToolRegistry::new();
    registry.register_erased(Arc::clone(&echo)).unwrap();

    assert_eq!(
        registry.register_erased(echo).unwrap_err().kind(),
        "duplicate_registration"
    );
    assert_eq!(registry.len(), 1);
    assert!(format!("{registry:?}").contains("fixture.echo@1.0.0"));
}

#[test]
fn a_registry_and_a_context_can_both_cross_a_thread_boundary() {
    // The coordinator runs tools on a worker thread and reports back to the Qt
    // thread, so a registry has to be shareable and a context has to be sendable.
    // Asserting it here means a future sink or writer that is not `Send` fails in
    // this module rather than at the call site that needs it.
    const fn assert_send<T: Send>() {}
    const fn assert_send_sync<T: Send + Sync>() {}

    assert_send::<ExecutionContext>();
    assert_send_sync::<ToolRegistry>();
    assert_send_sync::<Arc<dyn super::ErasedTool>>();

    let mut registry = ToolRegistry::new();
    registry.register(Echo::new("1.0.0")).unwrap();
    let shared = Arc::new(registry);

    let worker = {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            let mut context = context();
            invoke(
                &shared,
                &id("fixture.echo"),
                None,
                &raw(r#"{"message":"threaded"}"#),
                &mut context,
            )
            .map(|outcome| outcome.output().get().to_owned())
        })
    };

    assert_eq!(
        worker.join().unwrap().unwrap(),
        r#"{"echoed":"threaded"}"#.to_owned()
    );
}

// ---------------------------------------------------------------------------
// Latency
// ---------------------------------------------------------------------------

/// Latency targets are meaningful only in a release build, so debug and CI runs
/// skip them; run with `--release ... -- --ignored` to record numbers.
#[test]
#[ignore = "latency target; meaningful only in a release build"]
fn registry_lookup_meets_the_latency_target() {
    const TOOLS: usize = 1_000;
    const LOOKUPS: usize = 10_000;

    let mut registry = ToolRegistry::new();
    for index in 0..TOOLS {
        registry.register(Indexed { index }).unwrap();
    }
    assert_eq!(registry.len(), TOOLS);

    // The last-registered identity, so a linear scan would be worst case.
    let last = id(&format!("bench.tool_{}", TOOLS - 1));
    let pinned = version("1.0.0");

    let started = std::time::Instant::now();
    for _ in 0..LOOKUPS {
        registry.resolve(&last, Some(&pinned)).unwrap();
    }
    let exact = started.elapsed() / u32::try_from(LOOKUPS).unwrap();

    let started = std::time::Instant::now();
    for _ in 0..LOOKUPS {
        registry.resolve(&last, None).unwrap();
    }
    let latest = started.elapsed() / u32::try_from(LOOKUPS).unwrap();

    println!("exact lookup: {exact:?} per call over {TOOLS} tools");
    println!("latest lookup: {latest:?} per call over {TOOLS} tools");
    assert!(
        exact < std::time::Duration::from_millis(1),
        "exact lookup took {exact:?}"
    );
    assert!(
        latest < std::time::Duration::from_millis(1),
        "latest lookup took {latest:?}"
    );
}
