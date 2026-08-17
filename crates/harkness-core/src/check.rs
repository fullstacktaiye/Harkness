//! Per-project commands whose recorded results may be shown beside a change.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Most checks one project may retain in its catalog entry.
pub const MAX_PROJECT_CHECKS: usize = 32;
/// Most argv elements one configured check may contain.
pub const MAX_CHECK_ARGUMENTS: usize = 128;
/// Most explicit environment overrides one configured check may contain.
pub const MAX_CHECK_ENVIRONMENT_ENTRIES: usize = 128;
/// Longest configured identifier, label, argument, path, or environment value.
pub const MAX_CHECK_TEXT_BYTES: usize = 4 * 1024;
/// Largest serialized check definition accepted by the catalog.
///
/// Runtime tool inputs have a 64 KiB durable-column limit. Keeping the catalog
/// definition below 60 KiB leaves room for the `check.run` input field names,
/// nullable fields, and parser spelling added by the runtime projection.
pub const MAX_CHECK_SERIALIZED_BYTES: usize = 60 * 1024;

/// How Harkness should interpret a check's machine-readable output.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckParser {
    /// Do not infer file or line associations from output.
    #[default]
    Plain,
    /// Parse Cargo/rustc newline-delimited JSON diagnostics.
    CargoJson,
}

/// One explicitly configured project check.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckConfiguration {
    /// Stable lowercase identifier used by front ends.
    pub id: String,
    /// Human-readable name.
    pub label: String,
    /// Executable followed by argv. No element is interpreted by a shell.
    pub command: Vec<String>,
    /// Optional workspace-relative working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Exact environment overrides.
    ///
    /// A name still has to be one the executing tool declares. `check.run`
    /// declares none of its own, so today only the baseline an arbitrary child
    /// may inherit — `PATH`, `HOME`, `LANG`, `LC_ALL`, `TERM` — can be set here,
    /// and configuring anything else is refused before the check is launched
    /// rather than failing when the child is spawned. This type deliberately
    /// does not encode that list: the allowlist belongs to the tool contract in
    /// `harkness-runtime`, which sits above this crate.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Output parser selected for this command.
    #[serde(default)]
    pub parser: CheckParser,
    /// Optional child timeout in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
}

impl CheckConfiguration {
    /// Refuses configurations that could not be represented safely by a front end.
    pub fn validate_all(checks: &[Self]) -> Result<(), &'static str> {
        if checks.len() > MAX_PROJECT_CHECKS {
            return Err("a project may configure at most 32 checks");
        }
        let mut ids = HashSet::new();
        for check in checks {
            check.validate()?;
            if !ids.insert(check.id.as_str()) {
                return Err("project check identifiers must be unique");
            }
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), &'static str> {
        let valid_id = !self.id.is_empty()
            && self.id.len() <= 64
            && self.id.bytes().enumerate().all(|(index, byte)| match byte {
                b'a'..=b'z' => true,
                b'0'..=b'9' => index > 0,
                b'-' | b'_' => index > 0 && index + 1 < self.id.len(),
                _ => false,
            });
        if !valid_id {
            return Err(
                "a project check id must be lowercase ASCII with optional inner '-' or '_'",
            );
        }
        if self.label.trim().is_empty() || self.label.len() > MAX_CHECK_TEXT_BYTES {
            return Err("a project check label must be non-empty and at most 4096 bytes");
        }
        if self.command.is_empty() || self.command.len() > MAX_CHECK_ARGUMENTS {
            return Err("a project check command must contain 1 to 128 argv elements");
        }
        if self
            .command
            .iter()
            .any(|part| part.is_empty() || part.len() > MAX_CHECK_TEXT_BYTES)
        {
            return Err("project check argv elements must be non-empty and at most 4096 bytes");
        }
        if self
            .cwd
            .as_ref()
            .is_some_and(|cwd| cwd.is_empty() || cwd.len() > MAX_CHECK_TEXT_BYTES)
        {
            return Err("a project check cwd must be non-empty and at most 4096 bytes");
        }
        if self.env.len() > MAX_CHECK_ENVIRONMENT_ENTRIES {
            return Err("a project check environment may contain at most 128 entries");
        }
        if self.env.iter().any(|(name, value)| {
            name.is_empty()
                || name.len() > MAX_CHECK_TEXT_BYTES
                || value.len() > MAX_CHECK_TEXT_BYTES
        }) {
            return Err("project check environment names and values must fit the 4096-byte bound");
        }
        let serialized = serde_json::to_vec(self)
            .map_err(|_| "a project check definition must be JSON-serializable")?;
        if serialized.len() > MAX_CHECK_SERIALIZED_BYTES {
            return Err("a project check definition must serialize to at most 61440 bytes");
        }
        Ok(())
    }
}

/// Sensible commands for a workspace that has not supplied its own list.
///
/// Cargo is inferred only from the manifest at the project root. Every other
/// project calmly defaults to no checks rather than being treated as Rust.
#[must_use]
pub fn default_checks(root: &Path) -> Vec<CheckConfiguration> {
    if !root.join("Cargo.toml").is_file() {
        return Vec::new();
    }
    [
        (
            "test",
            "Tests",
            vec!["cargo", "test", "--workspace", "--message-format=json"],
            CheckParser::CargoJson,
        ),
        (
            "fmt",
            "Formatting",
            vec!["cargo", "fmt", "--check"],
            CheckParser::Plain,
        ),
        (
            "clippy",
            "Clippy",
            vec![
                "cargo",
                "clippy",
                "--workspace",
                "--all-targets",
                "--message-format=json",
            ],
            CheckParser::CargoJson,
        ),
    ]
    .into_iter()
    .map(|(id, label, command, parser)| CheckConfiguration {
        id: id.to_owned(),
        label: label.to_owned(),
        command: command.into_iter().map(str::to_owned).collect(),
        cwd: None,
        env: BTreeMap::new(),
        parser,
        timeout_seconds: None,
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::{
        CheckConfiguration, CheckParser, MAX_CHECK_ENVIRONMENT_ENTRIES, MAX_CHECK_SERIALIZED_BYTES,
        MAX_CHECK_TEXT_BYTES, default_checks,
    };

    fn configured_check() -> CheckConfiguration {
        CheckConfiguration {
            id: "test".to_owned(),
            label: "Tests".to_owned(),
            command: vec!["true".to_owned()],
            cwd: None,
            env: Default::default(),
            parser: CheckParser::Plain,
            timeout_seconds: None,
        }
    }

    #[test]
    fn cargo_defaults_are_explicit_argv_only_and_machine_readable_where_possible() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("Cargo.toml"), "[workspace]\n").unwrap();

        let checks = default_checks(root.path());

        assert_eq!(
            checks
                .iter()
                .map(|check| check.id.as_str())
                .collect::<Vec<_>>(),
            ["test", "fmt", "clippy"]
        );
        assert_eq!(checks[0].command[0], "cargo");
        assert!(
            checks[0]
                .command
                .contains(&"--message-format=json".to_owned())
        );
        assert_eq!(checks[0].parser, CheckParser::CargoJson);
        assert_eq!(checks[1].parser, CheckParser::Plain);
    }

    #[test]
    fn non_cargo_projects_default_to_no_checks() {
        let root = tempdir().unwrap();
        assert!(default_checks(root.path()).is_empty());
    }

    #[test]
    fn duplicate_ids_are_refused() {
        let mut check = default_checks(Path::new("."))
            .into_iter()
            .next()
            .unwrap_or_else(configured_check);
        check.command = vec!["true".to_owned()];
        assert_eq!(
            CheckConfiguration::validate_all(&[check.clone(), check]),
            Err("project check identifiers must be unique")
        );
    }

    #[test]
    fn environment_entry_count_is_bounded_before_serialization() {
        let mut check = configured_check();
        for index in 0..=MAX_CHECK_ENVIRONMENT_ENTRIES {
            check.env.insert(format!("CHECK_{index}"), String::new());
        }

        assert!(serde_json::to_vec(&check).unwrap().len() < MAX_CHECK_SERIALIZED_BYTES);
        assert_eq!(
            CheckConfiguration::validate_all(&[check]),
            Err("a project check environment may contain at most 128 entries")
        );
    }

    #[test]
    fn aggregate_serialized_definition_stays_below_the_runtime_input_limit() {
        let mut boundary = configured_check();
        boundary.command = vec!["x".repeat(MAX_CHECK_TEXT_BYTES); 14];
        let base_bytes = serde_json::to_vec(&boundary).unwrap().len();
        // Appending one JSON string adds its bytes plus a comma and two quotes.
        let final_argument_bytes = MAX_CHECK_SERIALIZED_BYTES - base_bytes - 3;
        assert!((1..=MAX_CHECK_TEXT_BYTES).contains(&final_argument_bytes));
        boundary.command.push("x".repeat(final_argument_bytes));
        assert_eq!(
            serde_json::to_vec(&boundary).unwrap().len(),
            MAX_CHECK_SERIALIZED_BYTES
        );
        assert_eq!(
            CheckConfiguration::validate_all(&[boundary.clone()]),
            Ok(())
        );

        boundary.command.last_mut().unwrap().push('x');
        assert_eq!(
            serde_json::to_vec(&boundary).unwrap().len(),
            MAX_CHECK_SERIALIZED_BYTES + 1
        );
        assert_eq!(
            CheckConfiguration::validate_all(&[boundary]),
            Err("a project check definition must serialize to at most 61440 bytes")
        );
    }
}
