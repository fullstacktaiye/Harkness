//! Schema generation, compilation, and the two validation gates.
//!
//! Schemas are generated from the `Input` and `Output` associated types by
//! `schemars`, compiled into validators once at registration, and then applied
//! to every call. Nothing here retrieves a schema from outside the process:
//! `jsonschema` is built without its `resolve-http` and `resolve-file` features,
//! so a `$ref` to a URL or a local file is refused at registration rather than
//! fetched. A schema `schemars` generates only ever refers to its own `$defs`
//! anyway; the point of the missing features is that nothing else could either.
//!
//! One reference does resolve without being retrieved: the JSON Schema draft
//! meta-schemas ship inside `jsonschema`, so a `$ref` to one is satisfied from its
//! built-in registry. That is local resolution, not retrieval, and the tests keep
//! the two apart deliberately.

use jsonschema::Validator;
use schemars::{JsonSchema, SchemaGenerator};
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::{
    MAX_REPORTED_VIOLATIONS, RegistryError, SchemaDirection, SchemaViolation, ToolError,
    ToolIdentity,
};

/// Generates the JSON Schema published for one side of an invocation.
pub(super) fn generate<T>() -> Value
where
    T: JsonSchema,
{
    SchemaGenerator::default()
        .into_root_schema_for::<T>()
        .to_value()
}

/// Compiles a generated schema into a reusable validator.
///
/// # Errors
///
/// Returns [`RegistryError::InvalidSchema`] when the schema is not a valid JSON
/// Schema this build can compile, including when it refers to a resource this
/// process is not allowed to retrieve. Doing this at registration is the point:
/// a malformed schema becomes a refusal to declare the tool rather than a
/// surprise on the first call.
pub(super) fn compile(
    tool: &ToolIdentity,
    direction: SchemaDirection,
    schema: &Value,
) -> Result<Validator, RegistryError> {
    jsonschema::validator_for(schema).map_err(|error| RegistryError::InvalidSchema {
        tool: tool.clone(),
        direction,
        reason: error.to_string(),
    })
}

/// Applies a compiled validator, collecting a bounded list of violations.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] or [`ToolError::InvalidOutput`] according
/// to `direction`, carrying up to [`MAX_REPORTED_VIOLATIONS`] located
/// violations.
pub(super) fn validate(
    validator: &Validator,
    tool: &ToolIdentity,
    direction: SchemaDirection,
    instance: &Value,
) -> Result<(), ToolError> {
    let mut errors = validator.iter_errors(instance);
    let violations = errors
        .by_ref()
        .take(MAX_REPORTED_VIOLATIONS)
        .map(|error| {
            SchemaViolation::new(
                error.instance_path().to_string(),
                error.schema_path().to_string(),
                error.to_string(),
            )
        })
        .collect::<Vec<_>>();

    if violations.is_empty() {
        return Ok(());
    }

    // The iterator is lazy and taken by reference, so counting the remainder
    // finishes the same single pass rather than validating a second time. This
    // only ever runs on the failure path.
    let omitted = errors.count();
    Err(refusal(tool, direction, violations, omitted))
}

/// Builds the refusal for one direction from located violations.
pub(super) fn refusal(
    tool: &ToolIdentity,
    direction: SchemaDirection,
    violations: Vec<SchemaViolation>,
    omitted: usize,
) -> ToolError {
    match direction {
        SchemaDirection::Input => ToolError::InvalidInput {
            tool: tool.clone(),
            violations,
            omitted,
        },
        SchemaDirection::Output => ToolError::InvalidOutput {
            tool: tool.clone(),
            violations,
            omitted,
        },
    }
}

/// Deserializes a schema-validated value into the tool's own input type.
///
/// The schema gate has already run, so reaching a failure here means serde
/// enforces something the published schema does not express — a `try_from`
/// conversion, a custom `Deserialize`, an integer range. Those are genuine
/// input errors, and `serde_path_to_error` is what turns them into a located
/// violation instead of an unanchored sentence.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] with a JSON Pointer to the field serde
/// rejected.
pub(super) fn deserialize_input<T>(tool: &ToolIdentity, instance: Value) -> Result<T, ToolError>
where
    T: DeserializeOwned,
{
    serde_path_to_error::deserialize(instance).map_err(|error| {
        let pointer = json_pointer(error.path());
        let violation = SchemaViolation::new(pointer, "", error.inner().to_string());
        refusal(tool, SchemaDirection::Input, vec![violation], 0)
    })
}

/// Renders a serde path as an RFC 6901 JSON Pointer.
///
/// Serde's own rendering is dot-separated (`items.0.name`), which is ambiguous
/// for a key containing a dot. A pointer is the spelling the schema gate
/// already reports, so both gates locate a field the same way.
fn json_pointer(path: &serde_path_to_error::Path) -> String {
    use serde_path_to_error::Segment;

    let mut pointer = String::new();
    for segment in path.iter() {
        pointer.push('/');
        match segment {
            Segment::Seq { index } => pointer.push_str(&index.to_string()),
            Segment::Map { key } => pointer.push_str(&escape_token(key)),
            Segment::Enum { variant } => pointer.push_str(&escape_token(variant)),
            // Serde reports an unknown segment when the failing element cannot
            // be named. Recording that honestly beats pointing at the root.
            _ => pointer.push('-'),
        }
    }
    pointer
}

/// Escapes one reference token: `~` becomes `~0` and `/` becomes `~1`.
fn escape_token(token: &str) -> String {
    if token.contains(['~', '/']) {
        token.replace('~', "~0").replace('/', "~1")
    } else {
        token.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use schemars::JsonSchema;
    use serde::Deserialize;
    use serde_json::json;

    use super::{compile, deserialize_input, generate, validate};
    use crate::tool::{SchemaDirection, ToolIdentity};

    #[derive(Debug, Deserialize, JsonSchema, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Fixture {
        name: String,
        depth: u8,
        #[serde(default)]
        nested: Vec<Inner>,
        #[serde(default)]
        labels: BTreeMap<String, String>,
    }

    #[derive(Debug, Deserialize, JsonSchema, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Inner {
        label: String,
    }

    fn identity() -> ToolIdentity {
        ToolIdentity::parse("fixture.tool", "1.0.0").unwrap()
    }

    #[test]
    fn a_generated_schema_declares_its_draft_required_fields_and_definitions() {
        let schema = generate::<Fixture>();
        assert_eq!(
            schema["$schema"],
            json!("https://json-schema.org/draft/2020-12/schema")
        );
        assert_eq!(schema["type"], json!("object"));
        assert_eq!(schema["required"], json!(["name", "depth"]));
        assert_eq!(schema["additionalProperties"], json!(false));
        assert!(
            schema["$defs"]["Inner"].is_object(),
            "the nested type was not published: {schema}"
        );
    }

    #[test]
    fn deny_unknown_fields_is_what_closes_the_schema() {
        #[derive(Deserialize, JsonSchema)]
        struct Open {
            #[allow(dead_code)]
            name: String,
        }

        // Documented, not enforced: a tool whose input type does not deny
        // unknown fields publishes an open schema, and serde will discard the
        // extra key rather than refuse it.
        assert!(generate::<Open>().get("additionalProperties").is_none());
        assert_eq!(generate::<Fixture>()["additionalProperties"], json!(false));
    }

    #[test]
    fn validation_locates_each_violation_with_a_json_pointer() {
        let schema = generate::<Fixture>();
        let validator = compile(&identity(), SchemaDirection::Input, &schema).unwrap();

        let error = validate(
            &validator,
            &identity(),
            SchemaDirection::Input,
            &json!({"name": "a", "depth": "deep"}),
        )
        .unwrap_err();
        let crate::tool::ToolError::InvalidInput { violations, .. } = &error else {
            panic!("expected an input refusal, got {error:?}");
        };
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].pointer(), "/depth");
        assert!(
            violations[0].schema_pointer().contains("depth"),
            "{:?}",
            violations[0]
        );

        // A nested violation points into the array element, not at the array.
        let nested = validate(
            &validator,
            &identity(),
            SchemaDirection::Input,
            &json!({"name": "a", "depth": 1, "nested": [{"label": 7}]}),
        )
        .unwrap_err();
        let crate::tool::ToolError::InvalidInput { violations, .. } = &nested else {
            panic!("expected an input refusal, got {nested:?}");
        };
        assert_eq!(violations[0].pointer(), "/nested/0/label");
    }

    #[test]
    fn a_missing_required_field_and_an_unknown_field_are_both_refused() {
        let schema = generate::<Fixture>();
        let validator = compile(&identity(), SchemaDirection::Input, &schema).unwrap();

        for instance in [
            json!({"name": "a"}),
            json!({"name": "a", "depth": 1, "surprise": true}),
        ] {
            assert!(
                validate(&validator, &identity(), SchemaDirection::Input, &instance).is_err(),
                "accepted {instance}"
            );
        }
        assert!(
            validate(
                &validator,
                &identity(),
                SchemaDirection::Input,
                &json!({"name": "a", "depth": 1}),
            )
            .is_ok()
        );
    }

    #[test]
    fn a_large_violation_set_reports_its_real_remainder() {
        // The reported list is capped, but the count of what was dropped is the
        // true remainder rather than the difference between the list and the cap.
        #[derive(Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        struct Wide {
            #[allow(dead_code)]
            values: Vec<u8>,
        }

        let schema = generate::<Wide>();
        let validator = compile(&identity(), SchemaDirection::Input, &schema).unwrap();

        // Thirty elements of the wrong type is thirty violations.
        let instance = json!({"values": vec!["not a number"; 30]});
        let error =
            validate(&validator, &identity(), SchemaDirection::Input, &instance).unwrap_err();
        let crate::tool::ToolError::InvalidInput {
            violations,
            omitted,
            ..
        } = &error
        else {
            panic!("expected an input refusal, got {error:?}");
        };

        assert_eq!(violations.len(), crate::tool::MAX_REPORTED_VIOLATIONS);
        assert_eq!(*omitted, 20, "the remainder must be exact, not the cap + 1");
        assert!(error.to_string().contains("(and 20 more)"), "{error}");
    }

    #[test]
    fn a_wrong_top_level_type_reports_the_empty_root_pointer() {
        let schema = generate::<Fixture>();
        let validator = compile(&identity(), SchemaDirection::Input, &schema).unwrap();
        let error =
            validate(&validator, &identity(), SchemaDirection::Input, &json!(42)).unwrap_err();
        let crate::tool::ToolError::InvalidInput { violations, .. } = &error else {
            panic!("expected an input refusal, got {error:?}");
        };
        assert_eq!(violations[0].pointer(), "");
    }

    #[test]
    fn the_direction_decides_which_refusal_is_reported() {
        let schema = generate::<Fixture>();
        let validator = compile(&identity(), SchemaDirection::Output, &schema).unwrap();
        let error =
            validate(&validator, &identity(), SchemaDirection::Output, &json!({})).unwrap_err();
        assert_eq!(error.kind(), "invalid_output");
    }

    #[test]
    fn a_malformed_schema_is_refused_at_compile_time() {
        let error = compile(
            &identity(),
            SchemaDirection::Input,
            &json!({"type": "not_a_type"}),
        )
        .unwrap_err();
        assert_eq!(error.kind(), "invalid_schema");
        assert!(error.to_string().contains("input schema"), "{error}");
    }

    #[test]
    fn an_external_reference_is_refused_for_want_of_a_retriever() {
        // `jsonschema` is built without `resolve-file` or `resolve-http`, so a
        // `$ref` outside the document cannot be followed.
        //
        // Two things make this test actually pin that. The file reference points at
        // a file that *exists* and holds a *valid* schema, and the HTTP reference
        // uses a host that is not one of the draft meta-schemas `jsonschema`
        // bundles — otherwise the reference would resolve from the built-in
        // registry and prove nothing. And each assertion names the missing feature
        // rather than merely checking that compilation failed: an unreachable host
        // or a malformed file fails either way, so a weaker test would stay green
        // if retrieval came back. It could come back without anyone editing this
        // crate — through Cargo feature unification, if some other workspace member
        // ever depends on `jsonschema` with default features.
        let directory = tempfile::tempdir().unwrap();
        let schema_path = directory.path().join("referenced.json");
        std::fs::write(&schema_path, br#"{"type": "string"}"#).unwrap();

        // A `file:` URI needs forward slashes and, on Windows, a slash before the
        // drive letter. Interpolating a native path directly would produce
        // `file://C:\...` there, which fails to parse as a URI — and the assertion
        // below would then pass for the wrong reason on one CI platform while
        // testing what it claims to on the other two.
        let native = schema_path.display().to_string();
        let file_uri = if cfg!(windows) {
            format!("file:///{}", native.replace('\\', "/"))
        } else {
            format!("file://{native}")
        };

        let cases = [
            (json!({"$ref": file_uri}), "`resolve-file` feature"),
            (
                json!({"$ref": "https://example.com/schema.json"}),
                "`resolve-http` feature",
            ),
        ];

        for (reference, required) in cases {
            let error = compile(&identity(), SchemaDirection::Input, &reference).unwrap_err();
            assert_eq!(error.kind(), "invalid_schema", "retrieved {reference}");
            assert!(
                error.to_string().contains(required),
                "{reference} failed for some other reason than a missing retriever: {error}"
            );
        }
    }

    #[test]
    fn a_bundled_meta_schema_reference_resolves_without_retrieval() {
        // The counterpart to the test above, and the reason it cannot simply use
        // any URL: the draft meta-schemas ship inside `jsonschema`, so a `$ref` to
        // one compiles from the built-in registry with nothing fetched. Recording
        // that here keeps the distinction between "resolved locally" and
        // "retrieved" explicit rather than looking like an inconsistency.
        let reference = json!({"$ref": "https://json-schema.org/draft/2020-12/schema"});
        assert!(compile(&identity(), SchemaDirection::Input, &reference).is_ok());
    }

    #[test]
    fn serde_failures_past_the_schema_gate_still_carry_a_pointer() {
        // `depth` is a `u8`, a bound the generated schema states as a maximum;
        // a custom `Deserialize` is the general case, and this is the cheapest
        // way to reach the serde gate with a valid-looking document.
        #[derive(Debug, Deserialize, JsonSchema)]
        struct Strict {
            #[allow(dead_code)]
            depth: u8,
        }

        let error = deserialize_input::<Strict>(&identity(), json!({"depth": 4096})).unwrap_err();
        let crate::tool::ToolError::InvalidInput { violations, .. } = &error else {
            panic!("expected an input refusal, got {error:?}");
        };
        assert_eq!(violations[0].pointer(), "/depth");
    }

    #[test]
    fn a_map_key_containing_a_pointer_character_is_escaped() {
        #[derive(Debug, Deserialize, JsonSchema)]
        struct Keyed {
            #[allow(dead_code)]
            labels: BTreeMap<String, u8>,
        }

        let error = deserialize_input::<Keyed>(&identity(), json!({"labels": {"a/b~c": 4096}}))
            .unwrap_err();
        let crate::tool::ToolError::InvalidInput { violations, .. } = &error else {
            panic!("expected an input refusal, got {error:?}");
        };
        assert_eq!(violations[0].pointer(), "/labels/a~1b~0c");
    }

    #[test]
    fn a_schema_valid_document_deserializes_into_the_tool_type() {
        let fixture = deserialize_input::<Fixture>(
            &identity(),
            json!({"name": "a", "depth": 2, "nested": [{"label": "x"}]}),
        )
        .unwrap();
        assert_eq!(
            fixture,
            Fixture {
                name: "a".to_owned(),
                depth: 2,
                nested: vec![super::tests::Inner {
                    label: "x".to_owned()
                }],
                labels: BTreeMap::new(),
            }
        );
    }
}
