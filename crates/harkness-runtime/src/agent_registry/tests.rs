//! Registry, trust, discovery and health tests.
//!
//! Everything that talks to a program talks to a shim: a small `/bin/sh` script
//! written into a temporary directory that answers `initialize` from a `case`
//! statement, or refuses to. That is deliberate rather than convenient — the
//! properties under test are "no untrusted candidate is executed", "a swapped
//! binary invalidates its grant", "a hung agent is killed", and "an agent sees
//! only the variables it was allowed" — and every one of them is a claim about a
//! real process, which a mocked transport could not check.
//!
//! The shims are Unix-only, because `Fixture::shim` writes a `#!/bin/sh` script.
//! The Windows leg of the matrix runs everything that is not one of them.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use harkness_git::Cancellation;
use harkness_test_fixtures::Fixture;
use time::OffsetDateTime;

use super::config::encode_registry;
use super::*;
use crate::integration::{InvalidationReason, Sha256Hash, SubjectKind, TrustState};
use crate::store::Store;

/// The frozen v1 registry committed beside this module.
const FROZEN_V1_REGISTRY: &str = include_str!("fixtures/agents-v1.json");

fn at(offset: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000 + offset).unwrap()
}

/// A registry service over an isolated data directory and its own database.
struct Harness {
    fixture: Fixture,
    store: Arc<Store>,
    service: AgentRegistryService,
}

impl Harness {
    fn new() -> Self {
        let fixture = Fixture::new();
        let store = Arc::new(Store::open(&fixture.data_dir).unwrap());
        let service = AgentRegistryService::new(fixture.data_dir.clone(), Arc::clone(&store));
        Self {
            fixture,
            store,
            service,
        }
    }

    fn agents_json(&self) -> PathBuf {
        self.fixture.data_dir.join(AGENTS_FILE)
    }

    /// Registers, trusts and enables one agent in one step.
    ///
    /// Every test that is about something *else* still has to pass all four
    /// gates, so the gates get their own tests and this gets everything past
    /// them.
    fn ready(&self, id: &str, command: &Path, env: &[&str]) -> AgentId {
        let id = AgentId::new(id).unwrap();
        let registration = AgentRegistration::new(
            id.clone(),
            "Shim Agent",
            command.to_path_buf(),
            AgentSource::User,
        )
        .unwrap()
        .with_env_allowlist(env.iter().copied())
        .unwrap();
        self.service.register(registration).unwrap();
        self.service
            .trust(TrustAgent::new(id.clone(), at(0)).and_enable())
            .unwrap();
        id
    }
}

fn registration(id: &str, command: &str) -> AgentRegistration {
    AgentRegistration::new(
        AgentId::new(id).unwrap(),
        "Gemini CLI",
        command,
        AgentSource::User,
    )
    .unwrap()
}

/// The registry the frozen fixture pins.
fn fixture_registry() -> AgentRegistryFile {
    let mut file = AgentRegistryFile::default();
    let mut released = AgentRegistration::new(
        AgentId::new("gemini-cli").unwrap(),
        "Gemini CLI",
        "/usr/bin/gemini",
        AgentSource::User,
    )
    .unwrap()
    .with_env_allowlist(["HOME", "PATH"])
    .unwrap();
    released.set_enabled(true);
    file.insert(released).unwrap();
    file.insert(
        AgentRegistration::new(
            AgentId::new("gemini-cli-dev").unwrap(),
            "Gemini CLI (development build)",
            "/home/user/src/gemini/target/debug/gemini",
            AgentSource::Development,
        )
        .unwrap()
        .with_args(["--experimental-acp"])
        .unwrap(),
    )
    .unwrap();
    file
}

// -- the durable format ------------------------------------------------------

/// The frozen fixture is compared against the encoder that writes the file, not
/// against a second spelling of it: a fixture pinning something nothing produces
/// pins nothing.
#[test]
fn the_frozen_v1_registry_is_exactly_what_this_build_writes() {
    assert_eq!(
        encode_registry(&fixture_registry()).unwrap(),
        FROZEN_V1_REGISTRY
    );
}

#[test]
fn the_frozen_v1_registry_reads_back_as_the_registry_it_was_written_from() {
    let harness = Harness::new();
    std::fs::create_dir_all(&harness.fixture.data_dir).unwrap();
    std::fs::write(harness.agents_json(), FROZEN_V1_REGISTRY).unwrap();

    let file = harness.service.registrations().unwrap();

    assert_eq!(file.agents().len(), 2);
    let released = file.get(&AgentId::new("gemini-cli").unwrap()).unwrap();
    assert_eq!(released.display_name(), "Gemini CLI");
    assert_eq!(released.command(), Path::new("/usr/bin/gemini"));
    assert!(released.is_enabled());
    assert_eq!(released.source(), AgentSource::User);
    assert_eq!(
        released.env_allowlist().collect::<Vec<_>>(),
        ["HOME", "PATH"]
    );

    let development = file.get(&AgentId::new("gemini-cli-dev").unwrap()).unwrap();
    assert_eq!(
        development.args().collect::<Vec<_>>(),
        ["--experimental-acp"]
    );
    assert!(!development.is_enabled());
    assert_eq!(development.source(), AgentSource::Development);
}

/// Rewrites the frozen v1 registry fixture.
///
/// Run deliberately, and only when a *new* version of the format is published:
/// `cargo test -p harkness-runtime -- --ignored regenerate_the_frozen_v1_registry`.
/// A released wire form is replaced by a new versioned fixture beside it, never
/// edited in place — every `agents.json` on disk was written under this one.
#[test]
#[ignore = "rewrites a committed fixture; run only when a new agents.json version is published"]
fn regenerate_the_frozen_v1_registry() {
    let destination =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/agent_registry/fixtures/agents-v1.json");
    std::fs::write(destination, encode_registry(&fixture_registry()).unwrap()).unwrap();
}

#[test]
fn a_newer_schema_version_asks_for_an_upgrade_rather_than_reporting_corruption() {
    let harness = Harness::new();
    std::fs::create_dir_all(&harness.fixture.data_dir).unwrap();
    std::fs::write(
        harness.agents_json(),
        r#"{"schema_version": 2, "agents": [], "future_field": true}"#,
    )
    .unwrap();

    let error = harness.service.registrations().unwrap_err();

    assert_eq!(error.kind(), "agents_file_version_too_new");
    assert!(error.to_string().contains("upgrade Harkness"), "{error}");
}

#[test]
fn a_schema_version_below_the_minimum_is_refused() {
    let harness = Harness::new();
    std::fs::create_dir_all(&harness.fixture.data_dir).unwrap();
    std::fs::write(
        harness.agents_json(),
        r#"{"schema_version": 0, "agents": []}"#,
    )
    .unwrap();

    assert_eq!(
        harness.service.registrations().unwrap_err().kind(),
        "agents_file_version_too_old"
    );
}

#[test]
fn an_unknown_field_at_the_current_version_is_refused_rather_than_dropped() {
    let harness = Harness::new();
    std::fs::create_dir_all(&harness.fixture.data_dir).unwrap();
    std::fs::write(
        harness.agents_json(),
        r#"{"schema_version": 1, "agents": [], "surprise": 1}"#,
    )
    .unwrap();

    assert_eq!(
        harness.service.registrations().unwrap_err().kind(),
        "agents_file_malformed"
    );
}

#[test]
fn two_registrations_sharing_an_identifier_are_refused() {
    let harness = Harness::new();
    std::fs::create_dir_all(&harness.fixture.data_dir).unwrap();
    std::fs::write(
        harness.agents_json(),
        r#"{"schema_version": 1, "agents": [
            {"id": "a", "display_name": "A", "command": "/bin/a"},
            {"id": "a", "display_name": "B", "command": "/bin/b"}
        ]}"#,
    )
    .unwrap();

    assert_eq!(
        harness.service.registrations().unwrap_err().kind(),
        "invalid_agent_registration"
    );
}

/// An entry that says nothing about being enabled has said the safe thing.
#[test]
fn an_entry_that_omits_the_optional_fields_reads_as_disabled_with_no_arguments() {
    let harness = Harness::new();
    std::fs::create_dir_all(&harness.fixture.data_dir).unwrap();
    std::fs::write(
        harness.agents_json(),
        r#"{"schema_version": 1, "agents": [
            {"id": "minimal", "display_name": "Minimal", "command": "/usr/bin/minimal"}
        ]}"#,
    )
    .unwrap();

    let file = harness.service.registrations().unwrap();
    let entry = file.get(&AgentId::new("minimal").unwrap()).unwrap();

    assert!(!entry.is_enabled());
    assert_eq!(entry.args().len(), 0);
    assert_eq!(entry.env_allowlist().len(), 0);
    assert_eq!(entry.source(), AgentSource::User);
}

#[test]
fn a_missing_registry_reads_as_empty_and_creates_nothing() {
    let harness = Harness::new();

    assert_eq!(harness.service.registrations().unwrap().agents().len(), 0);
    assert!(harness.service.list().unwrap().is_empty());
    assert!(!harness.agents_json().exists());
    assert!(
        !harness.fixture.data_dir.join(AGENTS_LOCK_FILE).exists(),
        "a read must not create the lock inode either"
    );
}

#[test]
fn reading_the_registry_never_rewrites_it() {
    let harness = Harness::new();
    harness
        .service
        .register(registration("gemini-cli", "/usr/bin/gemini"))
        .unwrap();
    let before = std::fs::read(harness.agents_json()).unwrap();
    let stamp = std::fs::metadata(harness.agents_json())
        .unwrap()
        .modified()
        .unwrap();

    let _ = harness.service.registrations().unwrap();
    let _ = harness.service.list().unwrap();
    let _ = harness.service.get(&AgentId::new("gemini-cli").unwrap());

    assert_eq!(std::fs::read(harness.agents_json()).unwrap(), before);
    assert_eq!(
        std::fs::metadata(harness.agents_json())
            .unwrap()
            .modified()
            .unwrap(),
        stamp
    );
}

// -- registration validation -------------------------------------------------

#[test]
fn a_relative_command_is_refused_with_a_typed_error() {
    let error = AgentRegistration::new(
        AgentId::new("gemini-cli").unwrap(),
        "Gemini CLI",
        "gemini",
        AgentSource::User,
    )
    .unwrap_err();

    assert_eq!(error.kind(), "invalid_agent_registration");
    assert!(error.to_string().contains("absolute path"), "{error}");
}

#[test]
fn an_empty_or_control_bearing_display_name_is_refused() {
    for name in ["", " Gemini", "Gemini\n"] {
        assert!(
            AgentRegistration::new(
                AgentId::new("gemini-cli").unwrap(),
                name,
                "/usr/bin/gemini",
                AgentSource::User,
            )
            .is_err(),
            "{name:?} should not be a display name"
        );
    }
}

#[test]
fn an_environment_allowlist_entry_outside_the_grammar_is_refused() {
    let base = registration("gemini-cli", "/usr/bin/gemini");
    for names in [vec!["1PATH"], vec![""], vec!["A=B"], vec!["PATH", "PATH"]] {
        assert!(
            base.clone().with_env_allowlist(names.clone()).is_err(),
            "{names:?} should not be an environment allowlist"
        );
    }
    // Case is preserved rather than folded: on Unix `path` and `PATH` are two
    // variables, and folding one onto the other admits one nobody named.
    let lowercase = base.with_env_allowlist(["path"]).unwrap();
    assert_eq!(lowercase.env_allowlist().collect::<Vec<_>>(), ["path"]);
}

#[test]
fn an_argument_carrying_a_nul_byte_is_refused() {
    assert!(
        registration("gemini-cli", "/usr/bin/gemini")
            .with_args(["--flag\0"])
            .is_err()
    );
}

// -- registration lifecycle --------------------------------------------------

#[test]
fn registering_the_same_configuration_twice_rewrites_nothing() {
    let harness = Harness::new();
    let first = harness
        .service
        .register(registration("gemini-cli", "/usr/bin/gemini"))
        .unwrap();
    assert!(first.changed());
    let bytes = std::fs::read(harness.agents_json()).unwrap();

    let second = harness
        .service
        .register(registration("gemini-cli", "/usr/bin/gemini"))
        .unwrap();

    assert!(!second.changed());
    assert_eq!(std::fs::read(harness.agents_json()).unwrap(), bytes);
}

#[test]
fn registering_a_different_configuration_under_one_identifier_is_refused() {
    let harness = Harness::new();
    harness
        .service
        .register(registration("gemini-cli", "/usr/bin/gemini"))
        .unwrap();

    let error = harness
        .service
        .register(registration("gemini-cli", "/usr/local/bin/gemini"))
        .unwrap_err();

    assert_eq!(error.kind(), "agent_already_registered");
}

#[test]
fn updating_an_unregistered_agent_is_refused() {
    let harness = Harness::new();

    assert_eq!(
        harness
            .service
            .update(registration("gemini-cli", "/usr/bin/gemini"))
            .unwrap_err()
            .kind(),
        "unknown_agent"
    );
}

/// A replacement lands disabled, so the window between repointing a command and
/// noticing that the new program is a different one is not a window in which it
/// runs.
#[test]
fn an_update_lands_disabled() {
    let harness = Harness::new();
    let command = Path::new("/usr/bin/gemini");
    let id = AgentId::new("gemini-cli").unwrap();
    harness
        .service
        .register(registration("gemini-cli", command.to_str().unwrap()))
        .unwrap();

    let outcome = harness
        .service
        .update(registration("gemini-cli", "/usr/local/bin/gemini"))
        .unwrap();

    assert!(outcome.changed());
    assert!(!outcome.registration().is_enabled());
    assert_eq!(
        harness.service.get(&id).unwrap().registration().command(),
        Path::new("/usr/local/bin/gemini")
    );
}

#[test]
fn an_untrusted_agent_cannot_be_enabled() {
    let harness = Harness::new();
    let id = AgentId::new("gemini-cli").unwrap();
    harness
        .service
        .register(registration("gemini-cli", "/usr/bin/gemini"))
        .unwrap();

    let error = harness.service.set_enabled(&id, true).unwrap_err();

    assert_eq!(error.kind(), "agent_not_trusted");
    assert!(
        !harness
            .service
            .get(&id)
            .unwrap()
            .registration()
            .is_enabled()
    );
}

/// The registry is rewritten whole, so two writers that do not serialize lose
/// each other's entries. The exclusive lock is what stops that, and this is what
/// says so.
#[test]
fn concurrent_registrations_all_survive() {
    let harness = Harness::new();
    // The lock inode has to exist before the threads race for it, or each one
    // creates its own and they lock different files.
    harness
        .service
        .register(registration("first", "/usr/bin/first"))
        .unwrap();

    std::thread::scope(|scope| {
        for index in 0..8 {
            let service = &harness.service;
            scope.spawn(move || {
                let id = format!("agent-{index}");
                service
                    .register(registration(&id, &format!("/usr/bin/{id}")))
                    .unwrap();
            });
        }
    });

    let file = harness.service.registrations().unwrap();
    assert_eq!(file.agents().len(), 9);
    for index in 0..8 {
        assert!(
            file.get(&AgentId::new(format!("agent-{index}")).unwrap())
                .is_some(),
            "registration {index} was lost to a concurrent rewrite"
        );
    }
}

/// `AgentRegistration` is `Clone` and `AgentRegistryFile::get` hands one out, so
/// an *enabled* value can be round-tripped back into the public API. Neither
/// write path may take it at its word: the gate is "a grant somebody made", and
/// a value carrying `enabled: true` is not one.
#[test]
fn an_enabled_registration_round_tripped_through_the_api_still_lands_disabled() {
    let harness = Harness::new();
    let id = AgentId::new("gemini-cli").unwrap();
    harness
        .service
        .register(registration("gemini-cli", "/usr/bin/gemini"))
        .unwrap();
    // Reach an enabled value the only way the API allows: enable it legitimately
    // — which needs a grant — and read it back.
    let executable = harness
        .fixture
        .shim("round-trip-agent", "#!/bin/sh\nexit 0\n");
    harness
        .service
        .update(
            AgentRegistration::new(id.clone(), "Gemini CLI", executable, AgentSource::User)
                .unwrap(),
        )
        .unwrap();
    harness
        .service
        .trust(TrustAgent::new(id.clone(), at(0)).and_enable())
        .unwrap();
    let enabled = harness.service.get(&id).unwrap().registration().clone();
    assert!(enabled.is_enabled());

    // Removing drops the grant; re-registering the enabled value must not put an
    // enabled agent back that nothing trusts.
    harness.service.remove(&id).unwrap();
    let outcome = harness.service.register(enabled.clone()).unwrap();

    assert!(!outcome.registration().is_enabled());
    assert!(
        !harness
            .service
            .get(&id)
            .unwrap()
            .registration()
            .is_enabled()
    );
    assert_eq!(
        harness
            .service
            .prepare_launch(&id, &LaunchContext::default())
            .unwrap_err()
            .kind(),
        "agent_disabled"
    );

    // And an update carrying the same value cannot re-enable it either.
    harness
        .service
        .update(enabled.clone().with_args(["--acp"]).unwrap())
        .unwrap();
    assert!(
        !harness
            .service
            .get(&id)
            .unwrap()
            .registration()
            .is_enabled()
    );
}

/// The file is one a user keeps in version control, so changing one entry must
/// not move every entry after it.
#[test]
fn an_update_keeps_the_registration_where_it_was_in_the_file() {
    let harness = Harness::new();
    for name in ["alpha", "beta", "gamma"] {
        harness
            .service
            .register(registration(name, &format!("/usr/bin/{name}")))
            .unwrap();
    }

    harness
        .service
        .update(
            registration("alpha", "/usr/bin/alpha")
                .with_args(["--acp"])
                .unwrap(),
        )
        .unwrap();

    let order = harness
        .service
        .registrations()
        .unwrap()
        .agents()
        .map(|agent| agent.id().to_string())
        .collect::<Vec<_>>();
    assert_eq!(order, ["alpha", "beta", "gamma"]);
}

#[test]
fn removing_an_agent_that_was_never_registered_changes_nothing() {
    let harness = Harness::new();

    let outcome = harness
        .service
        .remove(&AgentId::new("gemini-cli").unwrap())
        .unwrap();

    assert!(!outcome.changed());
}

// -- store round-trips -------------------------------------------------------

#[test]
fn agent_observations_round_trip_through_the_store() {
    let harness = Harness::new();
    let id = AgentId::new("gemini-cli").unwrap();
    let capabilities = AgentCapabilitySnapshot {
        load_session: true,
        session_resume: true,
        auth_methods: vec![AgentAuthMethod {
            id: "oauth".to_owned(),
            name: "Sign in".to_owned(),
            description: Some("Opens a browser".to_owned()),
        }],
        ..AgentCapabilitySnapshot::default()
    };
    let mut observations = AgentObservations::unobserved(at(1));
    observations.record_initialize(InitializeRecord::new(None, 1, capabilities.clone(), at(2)));
    observations.record_health(
        HealthRecord::failed(
            HealthStatus::Failed,
            "initialize_timeout",
            "no answer",
            Duration::from_millis(1_500),
            at(3),
        )
        .torn_down(AgentTeardown::Killed),
        at(3),
    );

    harness
        .store
        .put_agent_observations(&id, &observations)
        .unwrap();
    let loaded = harness.store.agent_observations(&id).unwrap().unwrap();

    assert_eq!(loaded, observations);
    assert_eq!(loaded.auth_status(), AuthStatus::Required);
    assert_eq!(loaded.compatibility(), CompatibilityStatus::Compatible);
    assert_eq!(
        loaded.last_initialize().unwrap().capabilities(),
        &capabilities
    );
    let health = loaded.last_health().unwrap();
    assert_eq!(health.failure_kind(), Some("initialize_timeout"));
    assert_eq!(health.teardown(), Some(AgentTeardown::Killed));
    assert_eq!(health.elapsed(), Duration::from_millis(1_500));
}

/// The tag and its payload are validated together, so a row claiming a refusal
/// while carrying no version — or the reverse — is refused rather than half-read.
#[test]
fn an_unsupported_protocol_version_round_trips_with_the_version_it_names() {
    let harness = Harness::new();
    let id = AgentId::new("gemini-cli").unwrap();
    let mut observations = AgentObservations::unobserved(at(1));
    observations
        .record_compatibility(CompatibilityStatus::UnsupportedProtocolVersion { advertised: 2 });

    harness
        .store
        .put_agent_observations(&id, &observations)
        .unwrap();

    assert_eq!(
        harness
            .store
            .agent_observations(&id)
            .unwrap()
            .unwrap()
            .compatibility(),
        CompatibilityStatus::UnsupportedProtocolVersion { advertised: 2 }
    );
    assert_eq!(
        CompatibilityStatus::from_stored("compatible", Some(2)),
        None,
        "a tag that carries no version must refuse one"
    );
    assert_eq!(
        CompatibilityStatus::from_stored("unsupported_protocol_version", None),
        None,
        "a tag that requires a version must refuse its absence"
    );
}

/// A trust row is addressed by its own identity, so writing one twice is a
/// duplicate rather than an upsert — an upsert here would let a fresh grant
/// overwrite the revocation that preceded it.
#[test]
fn a_repeated_trust_record_identity_is_refused_rather_than_upserted() {
    use crate::integration::{
        ConfigurationSource, ExecutableIdentity, IdentityBasis, TrustRecord, TrustRecordId,
        TrustScope,
    };

    let harness = Harness::new();
    let basis = IdentityBasis::new("Gemini CLI", ConfigurationSource::User)
        .unwrap()
        .launched_from(
            ExecutableIdentity::new("/usr/bin/gemini", Sha256Hash::of("bytes")).unwrap(),
        );
    let record = TrustRecord::grant(
        SubjectKind::AgentExecutable,
        basis,
        TrustScope::Global,
        at(0),
    )
    .unwrap();
    let id = TrustRecordId::new();

    harness
        .store
        .insert_trust_record(
            id,
            SubjectKind::AgentExecutable,
            "gemini-cli",
            &record,
            at(0),
        )
        .unwrap();
    let error = harness
        .store
        .insert_trust_record(
            id,
            SubjectKind::AgentExecutable,
            "gemini-cli",
            &record,
            at(1),
        )
        .unwrap_err();

    assert_eq!(error.kind(), "already_exists");
}

/// The three constants that bound a capability snapshot bound one *column*
/// together, so the worst case a peer can produce has to fit the store's inline
/// threshold. If it does not, an agent takes away its own health record by doing
/// nothing worse than advertising verbosely.
#[test]
fn the_largest_snapshot_a_peer_can_produce_fits_one_column() {
    let harness = Harness::new();
    let id = AgentId::new("verbose").unwrap();
    let auth_methods = (0..MAX_AGENT_AUTH_METHODS)
        .map(|index| AgentAuthMethod {
            id: format!("{index:0>width$}", width = MAX_AUTH_METHOD_TEXT_LENGTH),
            name: "n".repeat(MAX_AUTH_METHOD_TEXT_LENGTH),
            description: Some("d".repeat(MAX_AUTH_METHOD_DESCRIPTION_LENGTH)),
        })
        .collect();
    let mut observations = AgentObservations::unobserved(at(1));
    observations.record_initialize(InitializeRecord::new(
        None,
        1,
        AgentCapabilitySnapshot {
            auth_methods,
            auth_methods_truncated: true,
            ..AgentCapabilitySnapshot::default()
        },
        at(2),
    ));
    observations.record_health(
        HealthRecord::failed(
            HealthStatus::Failed,
            "initialize_timeout",
            "x".repeat(MAX_HEALTH_DETAIL_LENGTH),
            Duration::from_millis(1),
            at(2),
        ),
        at(2),
    );

    harness
        .store
        .put_agent_observations(&id, &observations)
        .expect("the worst case a peer can advertise must still be recordable");
    assert_eq!(
        harness.store.agent_observations(&id).unwrap().unwrap(),
        observations
    );
}

/// Every bound the encoder applies is re-applied on load, so a row nothing here
/// wrote cannot enter the process carrying more than one this build produced.
#[test]
fn a_stored_snapshot_beyond_the_bounds_is_refused_on_load() {
    let oversized = serde_json::json!({
        "schema_version": 1,
        "protocol_version": 1,
        "capabilities": {
            "auth_methods": (0..MAX_AGENT_AUTH_METHODS + 1)
                .map(|index| serde_json::json!({"id": index.to_string(), "name": "n"}))
                .collect::<Vec<_>>(),
        },
        "recorded_at": "2023-11-14T22:13:20Z",
    });
    let error = super::state::decode_initialize(&oversized).unwrap_err();
    assert_eq!(error.kind(), "invalid_agent_registration");
    assert!(
        error.to_string().contains("authentication methods"),
        "{error}"
    );

    let long_detail = serde_json::json!({
        "schema_version": 1,
        "status": "failed",
        "detail": "x".repeat(MAX_HEALTH_DETAIL_LENGTH + 1),
        "elapsed_ms": 1,
        "checked_at": "2023-11-14T22:13:20Z",
    });
    assert_eq!(
        super::state::decode_health(&long_detail)
            .unwrap_err()
            .kind(),
        "invalid_agent_registration"
    );
}

#[test]
fn every_authentication_status_spelling_round_trips() {
    for status in AuthStatus::ALL {
        assert_eq!(AuthStatus::from_stored(status.as_str()), Some(*status));
    }
    for status in HealthStatus::ALL {
        assert_eq!(HealthStatus::from_stored(status.as_str()), Some(*status));
    }
    for rung in AgentTeardown::ALL {
        assert_eq!(AgentTeardown::from_stored(rung.as_str()), Some(*rung));
    }
}

// -- discovery ---------------------------------------------------------------

/// The whole point of the probe, asserted directly: the candidates are
/// executables that record every invocation, and there are none.
#[cfg(unix)]
#[test]
fn discovery_lists_candidates_without_executing_any_of_them() {
    let fixture = Fixture::new();
    let receipts = fixture.root.path().join("invocations.txt");
    let script = format!(
        "#!/bin/sh\nprintf 'ran\\n' >> {}\n",
        receipts.to_str().unwrap()
    );
    let first = fixture.shim("gemini", &script);
    fixture.shim("opencode", &script);
    let elsewhere = fixture.directory("other-bin");
    std::fs::copy(&first, elsewhere.join("codex-acp")).unwrap();
    let mut permissions = std::fs::metadata(elsewhere.join("codex-acp"))
        .unwrap()
        .permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(elsewhere.join("codex-acp"), permissions).unwrap();

    let path = std::env::join_paths([fixture.root.path(), elsewhere.as_path()]).unwrap();
    let report = Discovery::default()
        .on_path(path)
        .run(&Cancellation::default());

    let found = report
        .candidates()
        .map(|candidate| candidate.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(found, ["gemini", "opencode", "codex-acp"]);
    assert_eq!(
        report.candidates().next().unwrap().resolved_path,
        fixture.root.path().join("gemini")
    );
    assert!(report.is_complete());
    assert!(
        !receipts.exists(),
        "discovery executed a candidate; it must only enumerate"
    );
}

#[test]
fn discovery_reports_a_truncated_probe_rather_than_a_short_list() {
    let fixture = Fixture::new();
    let path = std::env::join_paths([
        fixture.root.path().to_path_buf(),
        fixture.root.path().to_path_buf(),
        fixture.root.path().to_path_buf(),
    ])
    .unwrap();

    let report = Discovery::default()
        .on_path(path)
        .across_at_most(2)
        .run(&Cancellation::default());

    assert_eq!(report.directories_searched(), 2);
    assert_eq!(
        report.truncation(),
        Some(DiscoveryTruncation::DirectoryBudget)
    );
    assert!(!report.is_complete());
}

#[test]
fn an_already_cancelled_probe_reports_the_cancellation() {
    let fixture = Fixture::new();
    let cancel = Cancellation::default();
    cancel.cancel();

    let report = Discovery::default()
        .looking_for(["a", "b", "c", "d", "e", "f", "g", "h", "i"])
        .on_path(fixture.root.path().to_path_buf())
        .run(&cancel);

    assert_eq!(report.truncation(), Some(DiscoveryTruncation::Cancelled));
}

/// A candidate name is joined onto a search-path directory, so anything but one
/// ordinary file name would make the probe report a path outside the directory
/// it claims to have searched — `..` most obviously, and a bare root or prefix
/// most quietly, because joining an absolute path discards the directory.
#[cfg(unix)]
#[test]
fn a_candidate_name_that_is_not_one_file_name_is_dropped() {
    let fixture = Fixture::new();
    let outside = fixture.directory("outside");
    let inside = fixture.directory("outside/bin");
    fixture.shim("outside/reachable", "#!/bin/sh\nexit 0\n");
    let unreachable = inside.join("../reachable");
    assert!(unreachable.exists(), "the escape target really is there");

    let report = Discovery::default()
        .looking_for(["../reachable", "..", ".", "/", "gemini"])
        .on_path(inside.clone())
        .run(&Cancellation::default());

    assert_eq!(
        report.candidates().count(),
        0,
        "only `gemini` survived the filter, and nothing is installed under it"
    );
    assert!(report.is_complete());
    assert!(outside.exists());

    // The filter itself: only the plain name is kept.
    let kept = Discovery::default()
        .looking_for(["../reachable", "..", ".", "/", "gemini"])
        .on_path(String::new())
        .run(&Cancellation::default());
    assert_eq!(kept.directories_searched(), 0);
}

/// A trailing separator is an ordinary way to write a `PATH`, and the empty
/// entry it produces must not be what reports a complete probe as truncated.
#[test]
fn a_trailing_path_separator_does_not_report_a_complete_probe_as_truncated() {
    let fixture = Fixture::new();
    let mut entries = std::env::join_paths([fixture.root.path(), fixture.root.path()])
        .unwrap()
        .into_string()
        .expect("a temporary path is valid UTF-8");
    entries.push(':');

    let report = Discovery::default()
        .on_path(entries)
        .across_at_most(2)
        .run(&Cancellation::default());

    assert_eq!(report.directories_searched(), 2);
    assert_eq!(
        report.truncation(),
        None,
        "both real directories were searched, so nothing was truncated"
    );
}

// -- repository suggestions --------------------------------------------------

#[test]
fn a_repository_suggestion_is_untrusted_and_cannot_reach_enabled() {
    let harness = Harness::new();
    let workspace = harness.fixture.directory("workspace");
    std::fs::create_dir_all(workspace.join(".harkness")).unwrap();
    std::fs::write(
        workspace.join(REPOSITORY_AGENTS_PATH),
        r#"{"schema_version": 1, "agents": [
            {"id": "project-agent", "display_name": "Project Agent",
             "command": "/usr/bin/project-agent", "enabled": true, "source": "user"}
        ]}"#,
    )
    .unwrap();

    let suggestions = harness.service.repository_suggestions(&workspace).unwrap();

    assert_eq!(suggestions.len(), 1);
    let suggestion = &suggestions[0];
    assert!(
        suggestion.requested_enable(),
        "the repository asked, which is worth showing"
    );
    assert!(
        !suggestion.registration().is_enabled(),
        "and asking is all it may do"
    );
    assert_eq!(
        suggestion.registration().source(),
        AgentSource::Discovered,
        "the file said `user`, and a repository does not get to claim the user typed it"
    );
    assert_eq!(suggestion.origin(), workspace.join(REPOSITORY_AGENTS_PATH));
    assert!(suggestion.is_new_to(&harness.service.registrations().unwrap()));

    // Adopting it is the user's act, and lands disabled and untrusted like every
    // other registration.
    let id = suggestion.registration().id().clone();
    let adopted = harness
        .service
        .register(suggestion.registration().clone())
        .unwrap();
    assert!(!adopted.registration().is_enabled());
    assert_eq!(
        harness.service.set_enabled(&id, true).unwrap_err().kind(),
        "agent_not_trusted"
    );
    assert_eq!(
        harness
            .service
            .prepare_launch(&id, &LaunchContext::default())
            .unwrap_err()
            .kind(),
        "agent_disabled"
    );
}

#[test]
fn a_repository_with_no_agent_configuration_suggests_nothing() {
    let harness = Harness::new();
    let workspace = harness.fixture.directory("workspace");

    assert!(
        harness
            .service
            .repository_suggestions(&workspace)
            .unwrap()
            .is_empty()
    );
}

// -- shims -------------------------------------------------------------------

/// An agent that answers `initialize` from a `case` statement and nothing else.
///
/// Only shell builtins, so it needs no environment at all — which is what lets
/// the environment-allowlist test start from a genuinely empty one.
#[cfg(unix)]
fn healthy_shim(fixture: &Fixture, name: &str) -> PathBuf {
    fixture.shim(
        name,
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      rest=${line#*\"id\":}
      id=${rest%%,*}
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true,"promptCapabilities":{"image":true},"sessionCapabilities":{"resume":{}}},"agentInfo":{"name":"shim-agent","version":"1.2.3"}}}\n' "$id"
      ;;
  esac
done
"#,
    )
}

#[cfg(unix)]
fn auth_required_shim(fixture: &Fixture, name: &str) -> PathBuf {
    fixture.shim(
        name,
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      rest=${line#*\"id\":}
      id=${rest%%,*}
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"authMethods":[{"id":"oauth","name":"Sign in with the browser"}],"agentInfo":{"name":"shim-agent","version":"2.0.0"}}}\n' "$id"
      ;;
  esac
done
"#,
    )
}

#[cfg(unix)]
fn future_version_shim(fixture: &Fixture, name: &str) -> PathBuf {
    fixture.shim(
        name,
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      rest=${line#*\"id\":}
      id=${rest%%,*}
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":2,"agentInfo":{"name":"shim-agent","version":"4.0.0"}}}\n' "$id"
      ;;
  esac
done
"#,
    )
}

/// An agent that never answers and ignores `SIGTERM`.
///
/// `sleep` is an external program, so this shim is the one that genuinely needs
/// `PATH` — which makes it double as evidence that the allowlist is what admits
/// one rather than an inherited environment.
#[cfg(unix)]
fn stubborn_shim(fixture: &Fixture, name: &str) -> PathBuf {
    fixture.shim(
        name,
        "#!/bin/sh\ntrap '' TERM\nwhile true; do sleep 0.05; done\n",
    )
}

/// Runs a health check, retrying the one failure the test binary causes itself.
///
/// These tests write executables and fork concurrently, so an `exec` can fail
/// `ETXTBSY` while another thread still holds a write descriptor on a shim it is
/// creating. It is an artifact of the fixtures rather than of the registry,
/// which is why it is answered here: a production `invalid_executable` naming
/// the operating system's reason is the diagnosis a user needs, and retrying it
/// inside the service would hide a genuinely unusable agent binary.
#[cfg(unix)]
fn check(
    service: &AgentRegistryService,
    options: &HealthCheck,
) -> Result<HealthOutcome, AgentRegistryError> {
    for _ in 0..50 {
        let outcome = service.health_check(options, &Cancellation::default());
        let busy = matches!(
            &outcome,
            Err(AgentRegistryError::InvalidExecutable { reason, .. })
                if reason.contains("Text file busy") || reason.contains("ETXTBSY")
        );
        if !busy {
            return outcome;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the shim's executable stayed busy");
}

// -- health checks -----------------------------------------------------------

#[cfg(unix)]
#[test]
fn a_healthy_agent_records_its_version_its_protocol_and_its_capabilities() {
    let harness = Harness::new();
    let shim = healthy_shim(&harness.fixture, "healthy-agent");
    let id = harness.ready("healthy", &shim, &[]);

    let outcome = check(&harness.service, &HealthCheck::new(id.clone())).unwrap();

    assert_eq!(outcome.status(), HealthStatus::Healthy);
    let initialize = outcome.initialize().unwrap();
    assert_eq!(initialize.protocol_version(), 1);
    assert_eq!(initialize.agent_name(), Some("shim-agent"));
    assert_eq!(initialize.agent_version(), Some("1.2.3"));
    assert!(initialize.capabilities().load_session);
    assert!(initialize.capabilities().prompt_image);
    assert!(initialize.capabilities().session_resume);
    assert!(!initialize.capabilities().session_close);
    assert!(!initialize.capabilities().requires_authentication());

    // And it is durable, not merely returned.
    let state = harness.service.get(&id).unwrap();
    assert_eq!(state.state().auth_status(), AuthStatus::NotRequired);
    assert_eq!(
        state.state().compatibility(),
        CompatibilityStatus::Compatible
    );
    assert!(
        state
            .state()
            .last_initialize()
            .unwrap()
            .capabilities()
            .load_session
    );
    assert_eq!(
        state.state().last_health().unwrap().status(),
        HealthStatus::Healthy
    );
    assert!(state.is_ready());

    // A healthy, trusted, enabled agent is launchable, and the launch carries
    // the digest policy binds to.
    let launch = harness
        .service
        .prepare_launch(&id, &LaunchContext::default())
        .unwrap();
    assert_eq!(launch.command(), shim.as_path());
    assert_eq!(
        launch.integration_identity().agent_executable_sha256(),
        Some(launch.executable_sha256())
    );
}

#[cfg(unix)]
#[test]
fn an_agent_that_selects_protocol_version_two_is_recorded_incompatible_and_sessions_refuse() {
    let harness = Harness::new();
    let shim = future_version_shim(&harness.fixture, "future-agent");
    let id = harness.ready("future", &shim, &[]);

    let error = check(&harness.service, &HealthCheck::new(id.clone())).unwrap_err();

    assert_eq!(error.kind(), "unsupported_protocol_version");
    let state = harness.service.get(&id).unwrap();
    assert_eq!(
        state.state().compatibility(),
        CompatibilityStatus::UnsupportedProtocolVersion { advertised: 2 }
    );
    assert_eq!(
        state.state().last_health().unwrap().status(),
        HealthStatus::Incompatible
    );

    let refusal = harness
        .service
        .prepare_launch(&id, &LaunchContext::default())
        .unwrap_err();
    assert_eq!(refusal.kind(), "agent_incompatible");
    assert!(refusal.to_string().contains('2'), "{refusal}");
}

#[cfg(unix)]
#[test]
fn an_agent_that_hangs_is_force_terminated_and_records_an_initialize_timeout() {
    let harness = Harness::new();
    let shim = stubborn_shim(&harness.fixture, "stubborn-agent");
    let id = harness.ready("stubborn", &shim, &["PATH"]);
    let options = HealthCheck::new(id.clone())
        .within(Duration::from_millis(300))
        .tearing_down_within(Duration::from_millis(200));

    let started = std::time::Instant::now();
    let error = check(&harness.service, &options).unwrap_err();
    let elapsed = started.elapsed();

    assert_eq!(error.kind(), "initialize_timeout");
    assert!(
        elapsed < Duration::from_secs(20),
        "the deadline did not bound the check: {elapsed:?}"
    );
    let health = harness.service.get(&id).unwrap();
    let record = health.state().last_health().unwrap();
    assert_eq!(record.status(), HealthStatus::Failed);
    assert_eq!(record.failure_kind(), Some("initialize_timeout"));
    assert_eq!(
        record.teardown(),
        Some(AgentTeardown::Killed),
        "an agent that ignores SIGTERM has to be killed, and the record says so"
    );
    assert!(record.teardown().unwrap().was_forced());
}

#[cfg(unix)]
#[test]
fn an_agent_advertising_authentication_refuses_to_launch_until_a_sign_in_is_recorded() {
    let harness = Harness::new();
    let shim = auth_required_shim(&harness.fixture, "auth-agent");
    let id = harness.ready("auth", &shim, &[]);

    let outcome = check(&harness.service, &HealthCheck::new(id.clone())).unwrap();

    assert_eq!(outcome.status(), HealthStatus::AuthenticationRequired);
    let capabilities = outcome.initialize().unwrap().capabilities();
    assert!(capabilities.requires_authentication());
    assert_eq!(capabilities.auth_methods.len(), 1);
    assert_eq!(capabilities.auth_methods[0].id, "oauth");
    assert!(!capabilities.auth_methods_truncated);

    let state = harness.service.get(&id).unwrap();
    assert_eq!(state.state().auth_status(), AuthStatus::Required);

    let refusal = harness
        .service
        .prepare_launch(&id, &LaunchContext::default())
        .unwrap_err();
    assert_eq!(refusal.kind(), "agent_authentication_required");

    // And there is a way out of that state. ACP v1 has the agent authenticate
    // itself, so a signed-in agent advertises exactly what an unsigned one does
    // and no handshake can tell them apart — without this, running a health
    // check would permanently take away an agent that was launchable before it.
    let state = harness
        .service
        .record_authentication(&id, AuthStatus::Authenticated)
        .unwrap();
    assert_eq!(state.auth_status(), AuthStatus::Authenticated);
    assert!(
        harness
            .service
            .prepare_launch(&id, &LaunchContext::default())
            .is_ok()
    );

    // A later check does not undo it: the agent still offers the method it was
    // authenticated through, and still advertising a way in is not asking again.
    check(&harness.service, &HealthCheck::new(id.clone())).unwrap();
    assert_eq!(
        harness.service.get(&id).unwrap().state().auth_status(),
        AuthStatus::Authenticated
    );
    assert!(
        harness
            .service
            .prepare_launch(&id, &LaunchContext::default())
            .is_ok()
    );
}

#[cfg(unix)]
#[test]
fn a_program_that_cannot_be_run_is_recorded_as_an_invalid_executable() {
    let harness = Harness::new();
    // A file with the executable bit and no interpreter: it exists, it passes
    // every check that does not run it, and `exec` fails.
    let broken = harness.fixture.shim("broken-agent", "\u{0}not a program\n");
    let id = harness.ready("broken", &broken, &[]);

    let error = harness
        .service
        .health_check(&HealthCheck::new(id.clone()), &Cancellation::default())
        .unwrap_err();

    assert_eq!(error.kind(), "invalid_executable");
    let record = harness.service.get(&id).unwrap();
    let health = record.state().last_health().unwrap();
    assert_eq!(health.status(), HealthStatus::Failed);
    assert_eq!(health.failure_kind(), Some("invalid_executable"));
    assert!(
        health.detail().is_some_and(|detail| !detail.is_empty()),
        "the operating system's reason is what makes this actionable"
    );
}

#[test]
fn a_missing_executable_is_refused_before_anything_is_spawned() {
    let harness = Harness::new();
    let missing = harness.fixture.root.path().join("not-installed");
    let id = AgentId::new("missing").unwrap();
    harness
        .service
        .register(
            AgentRegistration::new(
                id.clone(),
                "Missing Agent",
                missing.clone(),
                AgentSource::User,
            )
            .unwrap(),
        )
        .unwrap();

    // Trusting it fails for the same reason: there is nothing to bind a grant
    // to, and a grant with no digest would be valid against every observation.
    let error = harness
        .service
        .trust(TrustAgent::new(id.clone(), at(0)))
        .unwrap_err();

    assert_eq!(error.kind(), "executable_not_found");
    assert!(
        harness.service.get(&id).is_ok(),
        "the registration is retained so the user can fix the path"
    );
}

#[cfg(unix)]
#[test]
fn a_disabled_agent_refuses_every_launch_path_and_spawns_nothing() {
    let harness = Harness::new();
    let shim = healthy_shim(&harness.fixture, "disabled-agent");
    let id = harness.ready("disabled", &shim, &[]);
    harness.service.set_enabled(&id, false).unwrap();

    assert_eq!(
        harness
            .service
            .health_check(&HealthCheck::new(id.clone()), &Cancellation::default())
            .unwrap_err()
            .kind(),
        "agent_disabled"
    );
    assert_eq!(
        harness
            .service
            .prepare_launch(&id, &LaunchContext::default())
            .unwrap_err()
            .kind(),
        "agent_disabled"
    );
    assert!(
        harness
            .service
            .get(&id)
            .unwrap()
            .state()
            .last_health()
            .is_none(),
        "a refusal before anything ran has nothing about the agent to record"
    );
}

/// The allowlist is exhaustive: the child starts from an empty environment and
/// sees exactly the names that were written down.
#[cfg(unix)]
#[test]
fn the_agent_sees_only_the_variables_the_registration_admits() {
    // SAFETY: single-threaded setup before any child is spawned, and both names
    // are unique to this test.
    unsafe {
        std::env::set_var("HARKNESS_AGENT_ALLOWED", "yes");
        std::env::set_var("HARKNESS_AGENT_DENIED", "leak");
    }

    let harness = Harness::new();
    let shim = harness.fixture.shim(
        "env-agent",
        r#"#!/bin/sh
export -p > ./environment.txt
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      rest=${line#*\"id\":}
      id=${rest%%,*}
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1}}\n' "$id"
      ;;
  esac
done
"#,
    );
    let id = harness.ready("env", &shim, &["HARKNESS_AGENT_ALLOWED"]);
    let workspace = harness.fixture.directory("env-workspace");

    let outcome = check(
        &harness.service,
        &HealthCheck::new(id).in_directory(workspace.clone()),
    )
    .unwrap();
    assert_eq!(outcome.status(), HealthStatus::Healthy);

    let observed = std::fs::read_to_string(workspace.join("environment.txt")).unwrap();
    assert!(
        observed.contains("HARKNESS_AGENT_ALLOWED"),
        "an admitted variable must reach the agent: {observed}"
    );
    assert!(
        !observed.contains("HARKNESS_AGENT_DENIED"),
        "a variable nobody admitted must not: {observed}"
    );

    unsafe {
        std::env::remove_var("HARKNESS_AGENT_ALLOWED");
        std::env::remove_var("HARKNESS_AGENT_DENIED");
    }
}

// -- trust lifecycle ---------------------------------------------------------

#[cfg(unix)]
#[test]
fn trusting_an_agent_binds_the_executable_digest() {
    let harness = Harness::new();
    let shim = healthy_shim(&harness.fixture, "bound-agent");
    let id = harness.ready("bound", &shim, &[]);

    let state = harness.service.get(&id).unwrap();
    let expected = Sha256Hash::of(std::fs::read(&shim).unwrap());

    assert_eq!(state.state().trust().state(), TrustState::Trusted);
    assert_eq!(state.state().executable_sha256(), Some(expected));
    assert_eq!(state.state().trust().granted_at(), Some(at(0)));

    let history = harness.service.trust_history(&id).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].subject_kind(), SubjectKind::AgentExecutable);
    assert_eq!(history[0].subject_ref(), id.as_str());
}

#[cfg(unix)]
#[test]
fn a_replaced_binary_invalidates_trust_disables_the_agent_and_refuses_until_re_trusted() {
    let harness = Harness::new();
    let shim = healthy_shim(&harness.fixture, "swapped-agent");
    let id = harness.ready("swapped", &shim, &[]);
    let original = harness
        .service
        .get(&id)
        .unwrap()
        .state()
        .executable_sha256()
        .unwrap();
    check(&harness.service, &HealthCheck::new(id.clone())).unwrap();

    // The same path, a different program.
    let replaced = healthy_shim(&harness.fixture, "swapped-agent-v2");
    std::fs::copy(&replaced, &shim).unwrap();
    std::fs::write(
        &shim,
        format!("{}\n# changed\n", std::fs::read_to_string(&shim).unwrap()),
    )
    .unwrap();
    let now = Sha256Hash::of(std::fs::read(&shim).unwrap());
    assert_ne!(original, now);

    let error = harness
        .service
        .prepare_launch(&id, &LaunchContext::default())
        .unwrap_err();

    assert_eq!(error.kind(), "executable_hash_mismatch");
    assert!(error.to_string().contains(&original.to_hex()), "{error}");
    assert!(error.to_string().contains(&now.to_hex()), "{error}");

    let state = harness.service.get(&id).unwrap();
    assert_eq!(state.state().trust().state(), TrustState::Invalidated);
    assert_eq!(
        state.state().trust().invalidation_reason(),
        Some(InvalidationReason::ExecutableHashChanged)
    );
    assert!(
        !state.registration().is_enabled(),
        "drift disables the agent rather than merely refusing this once"
    );

    // Every launch path refuses, and enabling it again is refused too.
    assert_eq!(
        harness
            .service
            .health_check(&HealthCheck::new(id.clone()), &Cancellation::default())
            .unwrap_err()
            .kind(),
        "agent_disabled"
    );
    assert_eq!(
        harness.service.set_enabled(&id, true).unwrap_err().kind(),
        "agent_not_trusted"
    );

    // Re-trusting continues the same record against the identity that is there.
    harness
        .service
        .trust(TrustAgent::new(id.clone(), at(10)).and_enable())
        .unwrap();
    let regranted = harness.service.get(&id).unwrap();
    assert_eq!(regranted.state().trust().state(), TrustState::Trusted);
    assert_eq!(regranted.state().executable_sha256(), Some(now));
    assert_eq!(regranted.state().trust().granted_at(), Some(at(10)));
    assert_eq!(
        harness.service.trust_history(&id).unwrap().len(),
        1,
        "a re-grant moves the record it re-affirms rather than adding a second"
    );
    assert!(
        harness
            .service
            .prepare_launch(&id, &LaunchContext::default())
            .is_ok()
    );
}

#[cfg(unix)]
#[test]
fn a_revoked_grant_is_terminal_and_a_later_decision_is_a_new_record() {
    let harness = Harness::new();
    let shim = healthy_shim(&harness.fixture, "revoked-agent");
    let id = harness.ready("revoked", &shim, &[]);

    let outcome = harness.service.revoke_trust(&id).unwrap();

    assert_eq!(outcome.trust().state(), TrustState::Revoked);
    assert!(!outcome.registration().is_enabled());
    assert_eq!(
        harness
            .service
            .prepare_launch(&id, &LaunchContext::default())
            .unwrap_err()
            .kind(),
        "agent_disabled"
    );

    harness
        .service
        .trust(TrustAgent::new(id.clone(), at(20)))
        .unwrap();
    let history = harness.service.trust_history(&id).unwrap();
    assert_eq!(
        history.len(),
        2,
        "the refusal stays in the audit trail beside the later grant"
    );
    assert_eq!(history[0].record().state(), TrustState::Revoked);
    assert_eq!(history[1].record().state(), TrustState::Trusted);
    assert_eq!(
        harness.service.get(&id).unwrap().state().trust().state(),
        TrustState::Trusted,
        "the newer decision is the one in force, whatever grant time it carries"
    );
}

/// Revoking twice is not an error. "Make sure this is not trusted" is a call a
/// surface makes without first asking whether it already is.
#[cfg(unix)]
#[test]
fn revoking_an_already_revoked_grant_reports_the_state_rather_than_refusing() {
    let harness = Harness::new();
    let shim = healthy_shim(&harness.fixture, "twice-revoked-agent");
    let id = harness.ready("twice", &shim, &[]);

    harness.service.revoke_trust(&id).unwrap();
    let second = harness.service.revoke_trust(&id).unwrap();

    assert_eq!(second.trust().state(), TrustState::Revoked);
    assert!(!second.registration().is_enabled());
    assert_eq!(
        harness.service.trust_history(&id).unwrap().len(),
        1,
        "a repeat is a no-op, not a second record"
    );

    // And an agent nobody ever trusted is the same no-op.
    harness
        .service
        .register(registration("never", "/usr/bin/never"))
        .unwrap();
    let never = harness
        .service
        .revoke_trust(&AgentId::new("never").unwrap())
        .unwrap();
    assert_eq!(never.trust().state(), TrustState::Untrusted);
}

/// The grant time is the user's decision and the row order is Harkness's
/// bookkeeping. Conflating them lets a caller naming an old `granted_at` file a
/// fresh decision behind the revocation it replaces.
#[cfg(unix)]
#[test]
fn a_grant_time_older_than_the_record_it_replaces_is_still_the_latest_record() {
    let harness = Harness::new();
    let shim = healthy_shim(&harness.fixture, "backdated-agent");
    let id = harness.ready("backdated", &shim, &[]);
    harness.service.revoke_trust(&id).unwrap();

    // A grant time *before* the revoked record's, which is what a caller reading
    // a clock that disagrees with this machine's would produce.
    harness
        .service
        .trust(TrustAgent::new(id.clone(), at(-500)).and_enable())
        .unwrap();

    let state = harness.service.get(&id).unwrap();
    assert_eq!(state.state().trust().state(), TrustState::Trusted);
    assert!(state.registration().is_enabled());
    assert!(
        harness
            .service
            .prepare_launch(&id, &LaunchContext::default())
            .is_ok(),
        "an enabled agent whose latest record read `revoked` would be the worst \
         of both answers"
    );
}

#[cfg(unix)]
#[test]
fn two_builds_of_one_agent_keep_independent_trust_health_and_capabilities() {
    let harness = Harness::new();
    let release = healthy_shim(&harness.fixture, "coexist-release");
    let development = future_version_shim(&harness.fixture, "coexist-development");
    let released = harness.ready("coexist", &release, &[]);
    let dev = harness.ready("coexist-dev", &development, &[]);

    check(&harness.service, &HealthCheck::new(released.clone())).unwrap();
    check(&harness.service, &HealthCheck::new(dev.clone())).unwrap_err();

    let released_state = harness.service.get(&released).unwrap();
    let dev_state = harness.service.get(&dev).unwrap();

    assert_eq!(
        released_state.state().compatibility(),
        CompatibilityStatus::Compatible
    );
    assert_eq!(
        dev_state.state().compatibility(),
        CompatibilityStatus::UnsupportedProtocolVersion { advertised: 2 }
    );
    assert_ne!(
        released_state.state().executable_sha256(),
        dev_state.state().executable_sha256()
    );
    assert!(released_state.state().last_initialize().is_some());
    assert!(dev_state.state().last_initialize().is_none());

    // Revoking one leaves the other alone.
    harness.service.revoke_trust(&dev).unwrap();
    assert!(harness.service.get(&released).unwrap().is_ready());
    assert!(!harness.service.get(&dev).unwrap().is_ready());
}

#[cfg(unix)]
#[test]
fn removing_a_registration_forgets_its_grants_and_its_observations() {
    let harness = Harness::new();
    let shim = healthy_shim(&harness.fixture, "removed-agent");
    let id = harness.ready("removed", &shim, &[]);
    check(&harness.service, &HealthCheck::new(id.clone())).unwrap();
    assert!(harness.store.agent_observations(&id).unwrap().is_some());

    let outcome = harness.service.remove(&id).unwrap();

    assert!(outcome.changed());
    assert_eq!(outcome.removed().unwrap().id(), &id);
    assert!(harness.store.agent_observations(&id).unwrap().is_none());
    assert!(harness.service.trust_history(&id).unwrap().is_empty());
    assert_eq!(
        harness.service.get(&id).unwrap_err().kind(),
        "unknown_agent"
    );

    // Re-registering the same identifier arrives untrusted, not inheriting the
    // decision somebody made about a program that is no longer there.
    harness
        .service
        .register(
            AgentRegistration::new(id.clone(), "Shim Agent", shim, AgentSource::User).unwrap(),
        )
        .unwrap();
    assert_eq!(
        harness.service.get(&id).unwrap().state().trust().state(),
        TrustState::Untrusted
    );
}

// -- workspace-scoped grants -------------------------------------------------

#[cfg(unix)]
#[test]
fn a_workspace_scoped_grant_does_not_reach_another_workspace() {
    let harness = Harness::new();
    let shim = healthy_shim(&harness.fixture, "scoped-agent");
    let project = harness.fixture.directory("project");
    let elsewhere = harness.fixture.directory("elsewhere");
    let id = AgentId::new("scoped").unwrap();
    harness
        .service
        .register(
            AgentRegistration::new(id.clone(), "Scoped Agent", shim, AgentSource::User).unwrap(),
        )
        .unwrap();
    harness
        .service
        .trust(
            TrustAgent::new(id.clone(), at(0))
                .in_workspace(project.clone())
                .and_enable(),
        )
        .unwrap();

    assert!(
        harness
            .service
            .prepare_launch(&id, &LaunchContext::default().in_workspace(&project))
            .is_ok()
    );

    let error = harness
        .service
        .prepare_launch(&id, &LaunchContext::default().in_workspace(elsewhere))
        .unwrap_err();

    assert_eq!(error.kind(), "agent_not_trusted");
    assert!(error.to_string().contains("different workspace"), "{error}");

    // And the grant it does not reach is *untouched*. Being used in the wrong
    // place is not drift: nothing about the subject changed, so treating it as
    // drift would destroy a decision that is still perfectly good where it was
    // made — and would do it whenever a caller forgot to name the workspace.
    let state = harness.service.get(&id).unwrap();
    assert_eq!(state.state().trust().state(), TrustState::Trusted);
    assert_eq!(state.state().trust().invalidation_reason(), None);
    assert!(
        state.registration().is_enabled(),
        "a refusal must not switch the agent off"
    );
    assert!(
        harness
            .service
            .prepare_launch(&id, &LaunchContext::default().in_workspace(project))
            .is_ok(),
        "the workspace the grant was given for still launches"
    );
}

/// A caller that forgets to name the workspace gets a refusal, not a wrecked
/// grant. This is the same property as above and the likeliest way to hit it.
#[cfg(unix)]
#[test]
fn a_launch_that_names_no_workspace_cannot_reach_a_workspace_scoped_grant() {
    let harness = Harness::new();
    let shim = healthy_shim(&harness.fixture, "unscoped-agent");
    let project = harness.fixture.directory("unscoped-project");
    let id = AgentId::new("unscoped").unwrap();
    harness
        .service
        .register(
            AgentRegistration::new(id.clone(), "Scoped Agent", shim, AgentSource::User).unwrap(),
        )
        .unwrap();
    harness
        .service
        .trust(
            TrustAgent::new(id.clone(), at(0))
                .in_workspace(project.clone())
                .and_enable(),
        )
        .unwrap();

    let error = harness
        .service
        .prepare_launch(&id, &LaunchContext::default())
        .unwrap_err();

    assert_eq!(error.kind(), "agent_not_trusted");
    assert_eq!(
        harness.service.get(&id).unwrap().state().trust().state(),
        TrustState::Trusted
    );
    assert!(
        harness
            .service
            .prepare_launch(&id, &LaunchContext::default().in_workspace(project))
            .is_ok()
    );
}

// -- events ------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn a_health_check_inside_a_run_lands_on_that_run_s_timeline() {
    use crate::domain::{Run, Task};
    use crate::store::EventKind;

    let harness = Harness::new();
    let shim = healthy_shim(&harness.fixture, "timeline-agent");
    let id = harness.ready("timeline", &shim, &[]);
    let task = Task::new("Check the agent", "/workspace", None, at(0));
    harness.store.insert_task(&task).unwrap();
    let run = Run::new(task.id(), at(1));
    harness.store.insert_run(&run).unwrap();

    check(
        &harness.service,
        &HealthCheck::new(id.clone()).with_context(LaunchContext::default().during_run(run.id())),
    )
    .unwrap();

    let events = harness.store.events(run.id(), None, 10).unwrap();
    let health = events
        .iter()
        .find(|stored| stored.event.kind() == &EventKind::ExternalAgentHealthChecked)
        .expect("the check is on the timeline");
    let payload = health.event.payload();
    assert_eq!(payload["agent_id"], id.as_str());
    assert_eq!(payload["status"], "healthy");
    assert_eq!(payload["protocol_version"], 1);
}

#[cfg(unix)]
#[test]
fn drift_found_during_a_launch_is_recorded_on_the_run_that_found_it() {
    use crate::domain::{Run, Task};
    use crate::store::EventKind;

    let harness = Harness::new();
    let shim = healthy_shim(&harness.fixture, "drift-agent");
    let id = harness.ready("drift", &shim, &[]);
    let task = Task::new("Launch the agent", "/workspace", None, at(0));
    harness.store.insert_task(&task).unwrap();
    let run = Run::new(task.id(), at(1));
    harness.store.insert_run(&run).unwrap();
    std::fs::write(&shim, "#!/bin/sh\nexit 0\n").unwrap();

    let error = harness
        .service
        .prepare_launch(&id, &LaunchContext::default().during_run(run.id()))
        .unwrap_err();

    assert_eq!(error.kind(), "executable_hash_mismatch");
    let events = harness.store.events(run.id(), None, 10).unwrap();
    let drift = events
        .iter()
        .find(|stored| stored.event.kind() == &EventKind::ExternalAgentTrustInvalidated)
        .expect("the drift is on the timeline");
    let payload = drift.event.payload();
    assert_eq!(payload["reason"], "executable_hash_changed");
    assert_eq!(payload["detected_at"], "launch");
}
