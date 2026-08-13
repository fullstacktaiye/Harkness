use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::Value;
use time::OffsetDateTime;

use super::error::{IntegrationDomainError, invalid_record};
use super::record::{RECORD, TrustRecord};
use super::state::{InvalidationReason, TrustScope, TrustScopeKind, TrustState};
use super::subject::{IdentityBasis, SubjectKind};

/// Newest durable integration-record schema understood by this build.
///
/// Deliberately independent of
/// [`RUNTIME_RECORD_SCHEMA_VERSION`](crate::domain::RUNTIME_RECORD_SCHEMA_VERSION):
/// adding a subject kind or an invalidation reason must not force a version
/// bump on every stored run, and a new run-record field must not invalidate
/// every stored trust grant.
pub const INTEGRATION_RECORD_SCHEMA_VERSION: u32 = 1;
/// Oldest durable integration-record schema understood by this build.
pub const MINIMUM_INTEGRATION_RECORD_SCHEMA_VERSION: u32 = 1;

/// Strict owned representation used to deserialize a [`TrustRecord`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TrustRecordWire {
    /// Integration-record schema version.
    pub schema_version: u32,
    /// Kind of subject the grant is about.
    pub subject_kind: SubjectKind,
    /// The exact identity that was trusted.
    pub identity: IdentityBasis,
    /// How far the grant reaches.
    pub scope: TrustScopeKind,
    /// Workspace root the grant is confined to; required by, and permitted
    /// only in, [`TrustScopeKind::Workspace`].
    ///
    /// JSON serialization fails when this platform path is not valid UTF-8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    /// Current state of the grant.
    pub state: TrustState,
    /// Why the grant was invalidated; required by, and permitted only in,
    /// [`TrustState::Invalidated`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalidation_reason: Option<InvalidationReason>,
    /// UTC RFC 3339 time the grant now held was made.
    #[serde(with = "time::serde::rfc3339")]
    pub granted_at: OffsetDateTime,
}

/// Borrowing representation used to serialize a [`TrustRecord`] without
/// cloning its identity basis.
#[derive(Debug, Serialize)]
pub struct TrustRecordWireRef<'a> {
    /// Integration-record schema version.
    pub schema_version: u32,
    /// Kind of subject the grant is about.
    pub subject_kind: SubjectKind,
    /// The exact identity that was trusted.
    pub identity: &'a IdentityBasis,
    /// How far the grant reaches.
    pub scope: TrustScopeKind,
    /// Workspace root the grant is confined to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<&'a Path>,
    /// Current state of the grant.
    pub state: TrustState,
    /// Why the grant was invalidated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalidation_reason: Option<InvalidationReason>,
    /// UTC RFC 3339 time the grant now held was made.
    #[serde(with = "time::serde::rfc3339")]
    pub granted_at: OffsetDateTime,
}

impl<'a> From<&'a TrustRecord> for TrustRecordWireRef<'a> {
    fn from(record: &'a TrustRecord) -> Self {
        Self {
            schema_version: INTEGRATION_RECORD_SCHEMA_VERSION,
            subject_kind: record.subject_kind(),
            identity: record.identity_basis(),
            scope: record.scope().kind(),
            workspace: record.scope().root(),
            state: record.state(),
            invalidation_reason: record.invalidation_reason(),
            granted_at: record.granted_at(),
        }
    }
}

impl From<&TrustRecord> for TrustRecordWire {
    fn from(record: &TrustRecord) -> Self {
        Self {
            schema_version: INTEGRATION_RECORD_SCHEMA_VERSION,
            subject_kind: record.subject_kind(),
            identity: record.identity_basis().clone(),
            scope: record.scope().kind(),
            workspace: record.scope().root().map(Path::to_path_buf),
            state: record.state(),
            invalidation_reason: record.invalidation_reason(),
            granted_at: record.granted_at(),
        }
    }
}

impl TryFrom<TrustRecordWire> for TrustRecord {
    type Error = IntegrationDomainError;

    fn try_from(wire: TrustRecordWire) -> Result<Self, Self::Error> {
        validate_schema_version(RECORD, wire.schema_version)?;
        let scope = match (wire.scope, wire.workspace) {
            (TrustScopeKind::Global, None) => TrustScope::Global,
            (TrustScopeKind::Workspace, Some(workspace)) => TrustScope::Workspace { workspace },
            (TrustScopeKind::Global, Some(_)) => {
                return Err(invalid_record(
                    RECORD,
                    "a global grant cannot name a workspace",
                ));
            }
            (TrustScopeKind::Workspace, None) => {
                return Err(invalid_record(
                    RECORD,
                    "a workspace-scoped grant requires the workspace it is confined to",
                ));
            }
        };
        Self::from_parts(
            wire.subject_kind,
            wire.identity,
            scope,
            wire.state,
            wire.invalidation_reason,
            wire.granted_at,
        )
    }
}

/// Refuses an integration-record schema version this build cannot read.
///
/// Exposed so a persistence layer can probe a row's version before it decodes
/// anything else: a future record may spell a field in a way this build cannot
/// parse, and the caller should learn that it needs an upgrade rather than that
/// some column looked corrupt.
///
/// # Errors
///
/// Returns [`IntegrationDomainError::SchemaVersionTooOld`] or
/// [`IntegrationDomainError::SchemaVersionTooNew`].
pub fn validate_integration_schema_version(
    record: &'static str,
    found: u32,
) -> Result<(), IntegrationDomainError> {
    validate_schema_version(record, found)
}

fn validate_schema_version(record: &'static str, found: u32) -> Result<(), IntegrationDomainError> {
    if found < MINIMUM_INTEGRATION_RECORD_SCHEMA_VERSION {
        return Err(IntegrationDomainError::SchemaVersionTooOld {
            record,
            found,
            minimum: MINIMUM_INTEGRATION_RECORD_SCHEMA_VERSION,
        });
    }
    if found > INTEGRATION_RECORD_SCHEMA_VERSION {
        return Err(IntegrationDomainError::SchemaVersionTooNew {
            record,
            found,
            maximum: INTEGRATION_RECORD_SCHEMA_VERSION,
        });
    }
    Ok(())
}

#[derive(Deserialize)]
struct SchemaVersionProbe {
    schema_version: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustRecordWireStrict {
    schema_version: u32,
    subject_kind: SubjectKind,
    identity: IdentityBasis,
    scope: TrustScopeKind,
    #[serde(default)]
    workspace: Option<PathBuf>,
    state: TrustState,
    #[serde(default)]
    invalidation_reason: Option<InvalidationReason>,
    #[serde(with = "time::serde::rfc3339")]
    granted_at: OffsetDateTime,
}

impl From<TrustRecordWireStrict> for TrustRecordWire {
    fn from(wire: TrustRecordWireStrict) -> Self {
        Self {
            schema_version: wire.schema_version,
            subject_kind: wire.subject_kind,
            identity: wire.identity,
            scope: wire.scope,
            workspace: wire.workspace,
            state: wire.state,
            invalidation_reason: wire.invalidation_reason,
            granted_at: wire.granted_at,
        }
    }
}

impl<'de> Deserialize<'de> for TrustRecordWire {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let probe = SchemaVersionProbe::deserialize(&value).map_err(de::Error::custom)?;
        validate_schema_version(RECORD, probe.schema_version).map_err(de::Error::custom)?;
        TrustRecordWireStrict::deserialize(value)
            .map(Into::into)
            .map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use serde::{Serialize, de::DeserializeOwned};

    use super::{
        INTEGRATION_RECORD_SCHEMA_VERSION, MINIMUM_INTEGRATION_RECORD_SCHEMA_VERSION,
        TrustRecordWire, TrustRecordWireRef, validate_integration_schema_version,
    };
    use crate::integration::record::tests::{agent_record, at, hash};
    use crate::integration::{
        ConfigurationSource, EndpointIdentity, IdentityBasis, IntegrationDomainError,
        InvalidationReason, SubjectKind, TrustRecord, TrustScope, TrustState,
    };

    fn mcp_tool_record() -> TrustRecord {
        let mut record = TrustRecord::grant(
            SubjectKind::McpToolSchema,
            IdentityBasis::new("files.read", ConfigurationSource::Repository)
                .unwrap()
                .speaking("2026-07-28")
                .unwrap()
                .fingerprinted(hash("files.read schema"))
                .declaring(["resources", "tools"])
                .unwrap(),
            TrustScope::workspace("/workspace"),
            at(0),
        )
        .unwrap();
        record
            .invalidate(InvalidationReason::ToolSchemaFingerprintChanged)
            .unwrap();
        record
    }

    fn forge_repository_record() -> TrustRecord {
        let mut record = TrustRecord::grant(
            SubjectKind::ForgeRepository,
            IdentityBasis::new("octocat/hello-world", ConfigurationSource::User)
                .unwrap()
                .reached_at(
                    EndpointIdentity::new("github.com", Some("octocat/hello-world".to_owned()))
                        .unwrap(),
                ),
            TrustScope::Global,
            at(120),
        )
        .unwrap();
        record.revoke().unwrap();
        record
    }

    /// The fewest fields a valid record can carry: a global grant with no
    /// invalidation reason and only the one identity field a recipe is
    /// recognized by.
    fn recipe_record() -> TrustRecord {
        TrustRecord::grant(
            SubjectKind::Recipe,
            IdentityBasis::new("release", ConfigurationSource::Builtin)
                .unwrap()
                .hashing(hash("release recipe")),
            TrustScope::Global,
            at(240),
        )
        .unwrap()
    }

    fn assert_fixture(wire: impl Serialize, fixture: &str) {
        let actual = format!("{}\n", serde_json::to_string_pretty(&wire).unwrap());
        assert_eq!(actual, fixture);
    }

    fn assert_owned_fixture<T>(fixture: &str)
    where
        T: DeserializeOwned + Serialize,
    {
        let wire = serde_json::from_str::<T>(fixture).unwrap();
        assert_fixture(wire, fixture);
    }

    #[test]
    fn frozen_v1_json_fixtures_cover_every_state_and_optional_field() {
        let fixtures = [
            (
                agent_record(),
                include_str!("fixtures/trust-record-agent-v1.json"),
            ),
            (
                mcp_tool_record(),
                include_str!("fixtures/trust-record-mcp-tool-v1.json"),
            ),
            (
                forge_repository_record(),
                include_str!("fixtures/trust-record-forge-repository-v1.json"),
            ),
            (
                recipe_record(),
                include_str!("fixtures/trust-record-recipe-v1.json"),
            ),
        ];

        for (record, fixture) in fixtures {
            assert_fixture(TrustRecordWireRef::from(&record), fixture);
            assert_owned_fixture::<TrustRecordWire>(fixture);

            // The owned and borrowing forms are byte-compatible, and a record
            // survives the whole round trip unchanged.
            let owned = TrustRecordWire::from(&record);
            assert_fixture(&owned, fixture);
            assert_eq!(TrustRecord::try_from(owned).unwrap(), record);
        }
    }

    #[test]
    fn a_minimal_record_omits_every_absent_optional_field() {
        let fixture = include_str!("fixtures/trust-record-recipe-v1.json");
        assert!(!fixture.contains("workspace"));
        assert!(!fixture.contains("invalidation_reason"));
        assert!(!fixture.contains("subject_version"));
        assert!(!fixture.contains("capabilities"));
    }

    #[test]
    fn a_future_schema_is_reported_before_future_fields_are_parsed() {
        let fixture = include_str!("fixtures/trust-record-future-schema.json");
        let message = serde_json::from_str::<TrustRecordWire>(fixture)
            .unwrap_err()
            .to_string();
        assert!(message.contains("is newer than the maximum supported version"));
        assert!(message.contains("upgrade Harkness"));
        assert!(!message.contains("unknown field"));
    }

    #[test]
    fn a_same_version_unknown_field_is_refused_rather_than_dropped() {
        let fixture = include_str!("fixtures/trust-record-unknown-field.json");
        let message = serde_json::from_str::<TrustRecordWire>(fixture)
            .unwrap_err()
            .to_string();
        assert!(message.contains("unknown field"));
        assert!(message.contains("audited_by"));
    }

    #[test]
    fn old_and_programmatically_constructed_future_versions_are_typed() {
        let mut old = TrustRecordWire::from(&agent_record());
        old.schema_version = 0;
        assert_eq!(
            TrustRecord::try_from(old).unwrap_err(),
            IntegrationDomainError::SchemaVersionTooOld {
                record: "trust_record",
                found: 0,
                minimum: MINIMUM_INTEGRATION_RECORD_SCHEMA_VERSION,
            }
        );

        let mut future = TrustRecordWire::from(&agent_record());
        future.schema_version = INTEGRATION_RECORD_SCHEMA_VERSION + 1;
        assert_eq!(
            TrustRecord::try_from(future).unwrap_err(),
            IntegrationDomainError::SchemaVersionTooNew {
                record: "trust_record",
                found: INTEGRATION_RECORD_SCHEMA_VERSION + 1,
                maximum: INTEGRATION_RECORD_SCHEMA_VERSION,
            }
        );

        assert!(
            validate_integration_schema_version("trust_record", INTEGRATION_RECORD_SCHEMA_VERSION)
                .is_ok()
        );
    }

    #[test]
    fn a_scope_that_contradicts_its_workspace_is_refused() {
        let mut global_with_workspace = TrustRecordWire::from(&recipe_record());
        global_with_workspace.workspace = Some("/workspace".into());
        let error = TrustRecord::try_from(global_with_workspace).unwrap_err();
        assert_eq!(error.kind(), "invalid_integration_record");
        assert!(
            error
                .to_string()
                .contains("a global grant cannot name a workspace")
        );

        let mut workspace_without_root = TrustRecordWire::from(&agent_record());
        workspace_without_root.workspace = None;
        let error = TrustRecord::try_from(workspace_without_root).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("a workspace-scoped grant requires the workspace")
        );
    }

    #[test]
    fn an_invalidation_reason_is_required_by_invalidated_and_permitted_nowhere_else() {
        let mut missing = TrustRecordWire::from(&mcp_tool_record());
        missing.invalidation_reason = None;
        let error = TrustRecord::try_from(missing).unwrap_err();
        assert_eq!(error.kind(), "invalid_integration_record");

        let mut spurious = TrustRecordWire::from(&agent_record());
        spurious.invalidation_reason = Some(InvalidationReason::CapabilityExpansion);
        let error = TrustRecord::try_from(spurious).unwrap_err();
        assert_eq!(error.kind(), "invalid_integration_record");

        let mut revoked_with_reason = TrustRecordWire::from(&forge_repository_record());
        revoked_with_reason.invalidation_reason = Some(InvalidationReason::EndpointHostChanged);
        assert!(TrustRecord::try_from(revoked_with_reason).is_err());
    }

    #[test]
    fn an_untrusted_wire_record_is_refused_because_absence_is_what_untrusted_means() {
        let mut wire = TrustRecordWire::from(&agent_record());
        wire.state = TrustState::Untrusted;
        let error = TrustRecord::try_from(wire).unwrap_err();
        assert_eq!(error.kind(), "invalid_integration_record");
        assert!(
            error
                .to_string()
                .contains("an untrusted subject is the absence of a record")
        );
    }

    /// A hand-edited row that lost the one field its kind is known by must
    /// fail to load, not enter the process as a record that verifies anything.
    #[test]
    fn a_row_stripped_of_its_identity_evidence_is_refused_on_load() {
        let fixture = include_str!("fixtures/trust-record-recipe-v1.json");
        let mut value = serde_json::from_str::<serde_json::Value>(fixture).unwrap();
        value["identity"]
            .as_object_mut()
            .unwrap()
            .remove("content_hash");

        // The strict body still parses: the field is genuinely optional.
        let wire = serde_json::from_value::<TrustRecordWire>(value).unwrap();
        let error = TrustRecord::try_from(wire).unwrap_err();
        assert_eq!(error.kind(), "missing_identity_evidence");
        assert_eq!(
            error.to_string(),
            "a recipe grant requires a recipe content hash"
        );
    }

    #[test]
    fn a_row_whose_workspace_root_is_relative_is_refused_on_load() {
        let fixture = include_str!("fixtures/trust-record-agent-v1.json");
        let mut value = serde_json::from_str::<serde_json::Value>(fixture).unwrap();
        value["workspace"] = serde_json::json!("../workspace");

        let wire = serde_json::from_value::<TrustRecordWire>(value).unwrap();
        let error = TrustRecord::try_from(wire).unwrap_err();
        assert_eq!(error.kind(), "invalid_integration_record");
        assert!(
            error
                .to_string()
                .contains("must start from a filesystem root")
        );
    }

    #[test]
    fn a_non_utc_grant_time_is_refused_on_load() {
        let mut wire = TrustRecordWire::from(&agent_record());
        wire.granted_at = at(0).to_offset(time::UtcOffset::from_hms(1, 0, 0).unwrap());
        let error = TrustRecord::try_from(wire).unwrap_err();
        assert_eq!(error.kind(), "invalid_integration_timestamp");
    }

    #[test]
    fn no_record_shape_serializes_a_field_named_like_credential_material() {
        for record in [
            agent_record(),
            mcp_tool_record(),
            forge_repository_record(),
            recipe_record(),
        ] {
            let json = serde_json::to_string(&TrustRecordWireRef::from(&record)).unwrap();
            for forbidden in [
                "token",
                "secret",
                "password",
                "credential",
                "bearer",
                "cookie",
                "private_key",
            ] {
                assert!(
                    !json.contains(forbidden),
                    "{} mentions {forbidden}: {json}",
                    record.subject_kind()
                );
            }
        }
    }

    #[test]
    fn an_identity_basis_that_fails_its_own_grammar_fails_the_record_parse() {
        let fixture = include_str!("fixtures/trust-record-recipe-v1.json");
        let mut value = serde_json::from_str::<serde_json::Value>(fixture).unwrap();
        value["identity"]["display_name"] = serde_json::json!(" padded ");
        let message = serde_json::from_value::<TrustRecordWire>(value)
            .unwrap_err()
            .to_string();
        assert!(message.contains("it cannot begin or end with whitespace"));
    }

    #[test]
    #[ignore = "rewrites the frozen v1 fixtures; run only when publishing a new integration schema version"]
    fn regenerate_the_frozen_v1_fixtures() {
        let directory =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/integration/fixtures");
        std::fs::create_dir_all(&directory).unwrap();

        for (name, record) in [
            ("trust-record-agent-v1.json", agent_record()),
            ("trust-record-mcp-tool-v1.json", mcp_tool_record()),
            (
                "trust-record-forge-repository-v1.json",
                forge_repository_record(),
            ),
            ("trust-record-recipe-v1.json", recipe_record()),
        ] {
            let json = serde_json::to_string_pretty(&TrustRecordWireRef::from(&record)).unwrap();
            std::fs::write(directory.join(name), format!("{json}\n")).unwrap();
        }

        // `trust-record-future-schema.json` and
        // `trust-record-unknown-field.json` are deliberately not regenerated.
        // Neither is a wire form this build can produce — one carries a version
        // it cannot write and the other a field it does not define — so they
        // are hand-maintained beside the frozen set they probe.
    }
}
