//! External-editor configuration and shell-free process launching.

use std::{
    env,
    ffi::{OsStr, OsString},
    io,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

const FILE_PLACEHOLDER: &str = "{file}";
const LINE_PLACEHOLDER: &str = "{line}";
const COLUMN_PLACEHOLDER: &str = "{column}";

/// One validated argv template used to launch an editor without a shell.
///
/// Each vector element becomes exactly one process argument. Placeholders are
/// expanded inside that argument, so a token such as `{file}:{line}:{column}`
/// remains one token even when the path contains spaces or non-UTF-8 bytes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EditorConfiguration {
    command: Vec<String>,
}

impl EditorConfiguration {
    /// Validates a custom editor argv template.
    pub fn new(command: Vec<String>) -> Result<Self, EditorError> {
        validate_template(&command, true)?;
        Ok(Self { command })
    }

    /// Returns the persisted argv template, beginning with the executable.
    #[must_use]
    pub fn command(&self) -> &[String] {
        &self.command
    }
}

impl<'de> Deserialize<'de> for EditorConfiguration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            command: Vec<String>,
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.command).map_err(serde::de::Error::custom)
    }
}

/// Built-in templates offered as conveniences rather than a closed editor set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditorPreset {
    Kate,
    VisualStudioCode,
    Zed,
}

impl EditorPreset {
    /// Presets in stable display order.
    pub const ALL: [Self; 3] = [Self::Kate, Self::VisualStudioCode, Self::Zed];

    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Kate => "kate",
            Self::VisualStudioCode => "code",
            Self::Zed => "zed",
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Kate => "Kate",
            Self::VisualStudioCode => "Visual Studio Code",
            Self::Zed => "Zed",
        }
    }

    /// Returns this preset's validated argv template.
    #[must_use]
    pub fn configuration(self) -> EditorConfiguration {
        let command = match self {
            Self::Kate => vec!["kate", "--line", "{line}", "--column", "{column}", "{file}"],
            Self::VisualStudioCode => vec!["code", "--goto", "{file}:{line}:{column}"],
            Self::Zed => vec!["zed", "{file}:{line}:{column}"],
        };
        EditorConfiguration::new(command.into_iter().map(str::to_owned).collect())
            .expect("built-in editor presets are valid")
    }
}

/// Selects the safe behavior used when no explicit editor is configured.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditorFallback {
    /// CLI behavior: `$VISUAL`, then `$EDITOR`, then the desktop default.
    Environment,
    /// GUI behavior: only the desktop default; terminal editors are skipped.
    Desktop,
}

/// A one-based source location passed to an editor template.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EditorPosition {
    /// One-based line number.
    pub line: u32,
    /// One-based column number.
    pub column: u32,
}

impl EditorPosition {
    #[must_use]
    pub const fn new(line: u32, column: u32) -> Self {
        Self {
            line: if line == 0 { 1 } else { line },
            column: if column == 0 { 1 } else { column },
        }
    }
}

/// Describes the detached editor process that was started.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditorLaunch {
    /// Configured executable spelling.
    pub command: String,
    /// Absolute platform-native path passed to the editor.
    pub file: PathBuf,
    /// Normalized one-based location passed to the editor.
    pub position: EditorPosition,
}

/// Typed failures from editor template validation and launch.
#[derive(Debug, Error)]
pub enum EditorError {
    #[error("invalid editor command template: {reason}")]
    InvalidTemplate { reason: String },

    #[error("editor path '{}' must stay within the selected project", path.display())]
    PathOutsideProject { path: PathBuf },

    #[error("failed to start configured editor command '{command}': {source}")]
    Launch {
        command: String,
        #[source]
        source: io::Error,
    },
}

impl EditorError {
    pub const KINDS: &'static [&'static str] = &[
        "invalid_editor_template",
        "editor_path_outside_project",
        "editor_launch",
    ];

    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::InvalidTemplate { .. } => "invalid_editor_template",
            Self::PathOutsideProject { .. } => "editor_path_outside_project",
            Self::Launch { .. } => "editor_launch",
        }
    }
}

/// Opens one repository-relative path and immediately returns after spawning.
pub(crate) fn open(
    root: &Path,
    path: &Path,
    position: EditorPosition,
    configured: Option<&EditorConfiguration>,
    fallback: EditorFallback,
) -> Result<EditorLaunch, EditorError> {
    validate_relative_path(path)?;
    let file = root.join(path);
    let configuration = configured
        .cloned()
        .map(Ok)
        .unwrap_or_else(|| fallback_configuration(fallback))?;
    let argv = expand(&configuration, &file, position);
    let command_name = configuration.command[0].clone();
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.spawn().map_err(|source| EditorError::Launch {
        command: command_name.clone(),
        source,
    })?;
    Ok(EditorLaunch {
        command: command_name,
        file,
        position,
    })
}

fn fallback_configuration(fallback: EditorFallback) -> Result<EditorConfiguration, EditorError> {
    if fallback == EditorFallback::Environment {
        for variable in ["VISUAL", "EDITOR"] {
            let Some(value) = env::var_os(variable) else {
                continue;
            };
            if value.is_empty() {
                continue;
            }
            let value = value
                .into_string()
                .map_err(|_| EditorError::InvalidTemplate {
                    reason: format!("${variable} is not valid UTF-8"),
                })?;
            let mut command = shlex::split(&value).ok_or_else(|| EditorError::InvalidTemplate {
                reason: format!("${variable} contains unmatched quoting"),
            })?;
            if !contains_file_placeholder(&command) {
                command.push(FILE_PLACEHOLDER.to_owned());
            }
            validate_template(&command, true).map_err(|error| match error {
                EditorError::InvalidTemplate { reason } => EditorError::InvalidTemplate {
                    reason: format!("${variable}: {reason}"),
                },
                other => other,
            })?;
            return Ok(EditorConfiguration { command });
        }
    }
    desktop_configuration()
}

#[cfg(target_os = "macos")]
fn desktop_configuration() -> Result<EditorConfiguration, EditorError> {
    EditorConfiguration::new(vec![
        "/usr/bin/open".to_owned(),
        FILE_PLACEHOLDER.to_owned(),
    ])
}

#[cfg(target_os = "windows")]
fn desktop_configuration() -> Result<EditorConfiguration, EditorError> {
    EditorConfiguration::new(vec!["explorer.exe".to_owned(), FILE_PLACEHOLDER.to_owned()])
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn desktop_configuration() -> Result<EditorConfiguration, EditorError> {
    EditorConfiguration::new(vec!["xdg-open".to_owned(), FILE_PLACEHOLDER.to_owned()])
}

fn validate_relative_path(path: &Path) -> Result<(), EditorError> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(EditorError::PathOutsideProject {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn validate_template(command: &[String], require_file: bool) -> Result<(), EditorError> {
    if command.first().is_none_or(String::is_empty) {
        return Err(EditorError::InvalidTemplate {
            reason: "the executable is empty".to_owned(),
        });
    }
    if require_file && !contains_file_placeholder(command) {
        return Err(EditorError::InvalidTemplate {
            reason: "the command must contain {file}".to_owned(),
        });
    }
    for token in command {
        let mut remainder = token.as_str();
        loop {
            let opening = remainder.find('{');
            let closing = remainder.find('}');
            if closing.is_some_and(|closing| opening.is_none_or(|opening| closing < opening)) {
                return Err(EditorError::InvalidTemplate {
                    reason: format!("unmatched closing brace in '{token}'"),
                });
            }
            let Some(start) = opening else {
                break;
            };
            remainder = &remainder[start..];
            let Some(end) = remainder.find('}') else {
                return Err(EditorError::InvalidTemplate {
                    reason: format!("unclosed placeholder in '{token}'"),
                });
            };
            let placeholder = &remainder[..=end];
            if !matches!(
                placeholder,
                FILE_PLACEHOLDER | LINE_PLACEHOLDER | COLUMN_PLACEHOLDER
            ) {
                return Err(EditorError::InvalidTemplate {
                    reason: format!("unknown placeholder '{placeholder}'"),
                });
            }
            remainder = &remainder[end + 1..];
        }
    }
    Ok(())
}

fn contains_file_placeholder(command: &[String]) -> bool {
    command.iter().any(|token| token.contains(FILE_PLACEHOLDER))
}

fn expand(
    configuration: &EditorConfiguration,
    file: &Path,
    position: EditorPosition,
) -> Vec<OsString> {
    configuration
        .command
        .iter()
        .map(|token| expand_token(token, file.as_os_str(), position))
        .collect()
}

fn expand_token(token: &str, file: &OsStr, position: EditorPosition) -> OsString {
    let mut result = OsString::new();
    let mut remainder = token;
    while let Some((offset, placeholder, replacement)) = next_placeholder(remainder, file, position)
    {
        result.push(&remainder[..offset]);
        result.push(replacement);
        remainder = &remainder[offset + placeholder.len()..];
    }
    result.push(remainder);
    result
}

fn next_placeholder<'a>(
    value: &'a str,
    file: &'a OsStr,
    position: EditorPosition,
) -> Option<(usize, &'static str, OsString)> {
    [
        (FILE_PLACEHOLDER, OsString::from(file)),
        (LINE_PLACEHOLDER, OsString::from(position.line.to_string())),
        (
            COLUMN_PLACEHOLDER,
            OsString::from(position.column.to_string()),
        ),
    ]
    .into_iter()
    .filter_map(|(placeholder, replacement)| {
        value
            .find(placeholder)
            .map(|offset| (offset, placeholder, replacement))
    })
    .min_by_key(|(offset, _, _)| *offset)
}

#[cfg(test)]
mod tests {
    use super::{EditorConfiguration, EditorFallback, EditorPosition, expand, open};
    use harkness_test_fixtures::{Fixture, wait_for_file};
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;
    use std::{ffi::OsString, fs, path::Path};

    #[test]
    fn a_file_placeholder_stays_inside_one_platform_argument() {
        let configuration = EditorConfiguration::new(vec![
            "code".to_owned(),
            "--goto".to_owned(),
            "{file}:{line}:{column}".to_owned(),
        ])
        .unwrap();
        let expanded = expand(
            &configuration,
            Path::new("name with spaces.rs"),
            EditorPosition::new(12, 3),
        );
        assert_eq!(
            expanded,
            ["code", "--goto", "name with spaces.rs:12:3"].map(OsString::from)
        );
    }

    #[test]
    fn graphical_fallback_uses_only_the_desktop_opener() {
        let configuration = super::fallback_configuration(EditorFallback::Desktop).unwrap();
        #[cfg(target_os = "macos")]
        assert_eq!(configuration.command()[0], "/usr/bin/open");
        #[cfg(target_os = "windows")]
        assert_eq!(configuration.command()[0], "explorer.exe");
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        assert_eq!(configuration.command()[0], "xdg-open");
    }

    #[cfg(unix)]
    #[test]
    fn a_file_placeholder_preserves_non_utf8_bytes_when_concatenated() {
        use std::os::unix::ffi::OsStringExt;

        let configuration =
            EditorConfiguration::new(vec!["zed".to_owned(), "{file}:{line}:{column}".to_owned()])
                .unwrap();
        let file = OsString::from_vec(b"bad-\xff.rs".to_vec());
        let expanded = expand(&configuration, Path::new(&file), EditorPosition::new(9, 1));
        assert_eq!(expanded[1].as_bytes(), b"bad-\xff.rs:9:1");
    }

    #[cfg(unix)]
    fn recording_editor(fixture: &Fixture, log: &Path) -> EditorConfiguration {
        let executable = fixture.shim("record-editor", "#!/bin/sh\nprintf '%s' \"$2\" > \"$1\"\n");
        EditorConfiguration::new(vec![
            executable.to_string_lossy().into_owned(),
            log.to_string_lossy().into_owned(),
            "{file}:{line}:{column}".to_owned(),
        ])
        .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn shell_metacharacters_are_passed_literally_and_execute_nothing() {
        let fixture = Fixture::new();
        let root = fixture.directory("repository");
        let path = Path::new("review; touch HARKNESS_EDITOR_PWNED; #.rs");
        fs::write(root.join(path), "content\n").unwrap();
        let log = fixture.root.path().join("argv");
        let configuration = recording_editor(&fixture, &log);

        open(
            &root,
            path,
            EditorPosition::new(17, 4),
            Some(&configuration),
            EditorFallback::Desktop,
        )
        .unwrap();

        wait_for_file(&log);
        assert_eq!(
            fs::read(&log).unwrap(),
            root.join(path)
                .as_os_str()
                .as_bytes()
                .iter()
                .copied()
                .chain(b":17:4".iter().copied())
                .collect::<Vec<_>>()
        );
        assert!(!root.join("HARKNESS_EDITOR_PWNED").exists());
    }

    #[cfg(unix)]
    #[test]
    fn launching_preserves_a_non_utf8_repository_path() {
        use std::os::unix::ffi::OsStringExt;

        let fixture = Fixture::new();
        let root = fixture.directory("repository");
        let path = OsString::from_vec(b"not-utf8-\xff.rs".to_vec());
        fs::write(root.join(&path), "content\n").unwrap();
        let log = fixture.root.path().join("argv");
        let configuration = recording_editor(&fixture, &log);

        open(
            &root,
            Path::new(&path),
            EditorPosition::new(8, 2),
            Some(&configuration),
            EditorFallback::Desktop,
        )
        .unwrap();

        wait_for_file(&log);
        let mut expected = root.join(&path).as_os_str().as_bytes().to_vec();
        expected.extend_from_slice(b":8:2");
        assert_eq!(fs::read(log).unwrap(), expected);
    }

    #[cfg(unix)]
    #[test]
    fn a_catalogued_worktree_opens_against_its_own_root() {
        use crate::{ProjectId, ProjectService};

        let fixture = Fixture::new();
        let parent = fixture.directory("parent");
        let worktree = fixture.directory("worktree");
        fs::write(worktree.join("changed.rs"), "content\n").unwrap();
        fs::create_dir_all(&fixture.data_dir).unwrap();
        let log = fixture.root.path().join("argv");
        let configuration = recording_editor(&fixture, &log);
        let parent_id = "00000000-0000-4000-8000-000000000001";
        let worktree_id = "00000000-0000-4000-8000-000000000002";
        let catalog = serde_json::json!({
            "version": 2,
            "projects": [
                {
                    "id": parent_id,
                    "display_name": "parent",
                    "root": parent,
                    "source": "local",
                    "last_opened": "2026-08-11 00:00:00.000000000 +00:00:00"
                },
                {
                    "id": worktree_id,
                    "display_name": "worktree",
                    "root": worktree,
                    "source": "worktree",
                    "parent": parent_id,
                    "worktree_branch": "agent/test",
                    "last_opened": "2026-08-11 00:00:00.000000000 +00:00:00"
                }
            ],
            "editor": configuration,
        });
        fs::write(
            fixture.data_dir.join("projects.json"),
            serde_json::to_vec_pretty(&catalog).unwrap(),
        )
        .unwrap();

        let service = ProjectService::load_from_data_dir(&fixture.data_dir).unwrap();
        service
            .open_in_editor(
                worktree_id.parse::<ProjectId>().unwrap(),
                Path::new("changed.rs"),
                EditorPosition::new(3, 1),
                EditorFallback::Desktop,
            )
            .unwrap();

        wait_for_file(&log);
        assert_eq!(
            fs::read(log).unwrap(),
            format!("{}:3:1", worktree.join("changed.rs").display()).as_bytes()
        );
    }

    #[test]
    fn clearing_editor_configuration_omits_the_additive_catalog_field() {
        use crate::ProjectService;

        let fixture = Fixture::new();
        let mut service = ProjectService::load_from_data_dir(&fixture.data_dir).unwrap();
        service
            .set_editor_configuration(Some(super::EditorPreset::VisualStudioCode.configuration()))
            .unwrap();
        let configured: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture.data_dir.join("projects.json")).unwrap())
                .unwrap();
        assert_eq!(configured["editor"]["command"][0], "code");

        service.set_editor_configuration(None).unwrap();
        let cleared: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture.data_dir.join("projects.json")).unwrap())
                .unwrap();
        assert!(cleared.get("editor").is_none());
    }
}
