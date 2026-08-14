//! External-editor configuration and shell-free process launching.

use std::{
    env,
    ffi::{OsStr, OsString},
    fs, io,
    num::NonZeroU32,
    path::{Component, Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{OnceLock, mpsc},
    thread,
    time::Duration,
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

/// Names the front end requesting an editor launch.
///
/// Besides selecting the unconfigured fallback, this controls process I/O:
/// command-line launches inherit the caller's terminal, while graphical
/// launches detach from standard streams and never consult terminal-editor
/// environment variables.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditorLaunchContext {
    /// CLI behavior: `$VISUAL`, then `$EDITOR`, then the desktop default.
    CommandLine,
    /// GUI behavior: only the desktop default; terminal editors are skipped.
    Graphical,
}

/// A one-based source location passed to an editor template.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EditorPosition {
    /// One-based line number.
    line: NonZeroU32,
    /// One-based column number.
    column: NonZeroU32,
}

impl EditorPosition {
    #[must_use]
    pub const fn new(line: NonZeroU32, column: NonZeroU32) -> Self {
        Self { line, column }
    }

    #[must_use]
    pub const fn line(self) -> u32 {
        self.line.get()
    }

    #[must_use]
    pub const fn column(self) -> u32 {
        self.column.get()
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
    #[error("invalid editor command template for '{command}': {reason}")]
    InvalidTemplate { command: String, reason: String },

    #[error("editor path '{}' must stay within the selected project", path.display())]
    PathOutsideProject { path: PathBuf },

    #[error("working-tree editor file '{}' is unavailable", path.display())]
    FileUnavailable { path: PathBuf },

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
        "editor_file_unavailable",
        "editor_launch",
    ];

    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::InvalidTemplate { .. } => "invalid_editor_template",
            Self::PathOutsideProject { .. } => "editor_path_outside_project",
            Self::FileUnavailable { .. } => "editor_file_unavailable",
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
    context: EditorLaunchContext,
) -> Result<EditorLaunch, EditorError> {
    let path = normalize_relative_path(path)?;
    let file = root.join(path);
    if context == EditorLaunchContext::Graphical {
        match fs::metadata(&file) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) | Err(_) => return Err(EditorError::FileUnavailable { path: file }),
        }
    }
    let plan = launch_plan(configured, context)?;
    let argv = expand(&plan.configuration, &file, position);
    let command_name = plan.configuration.command[0].clone();
    let reaper = editor_reaper().map_err(|source| EditorError::Launch {
        command: command_name.clone(),
        source,
    })?;
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]).current_dir(root);
    if plan.stdio == EditorStdio::Null {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    }
    let child = command.spawn().map_err(|source| EditorError::Launch {
        command: command_name.clone(),
        source,
    })?;
    enqueue_for_reaping(reaper, child);
    Ok(EditorLaunch {
        command: command_name,
        file,
        position,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EditorStdio {
    Inherit,
    Null,
}

struct EditorLaunchPlan {
    configuration: EditorConfiguration,
    stdio: EditorStdio,
}

fn launch_plan(
    configured: Option<&EditorConfiguration>,
    context: EditorLaunchContext,
) -> Result<EditorLaunchPlan, EditorError> {
    let configuration = configured
        .cloned()
        .map(Ok)
        .unwrap_or_else(|| fallback_configuration(context))?;
    let stdio = match context {
        EditorLaunchContext::CommandLine => EditorStdio::Inherit,
        EditorLaunchContext::Graphical => EditorStdio::Null,
    };
    Ok(EditorLaunchPlan {
        configuration,
        stdio,
    })
}

fn fallback_configuration(
    context: EditorLaunchContext,
) -> Result<EditorConfiguration, EditorError> {
    if context == EditorLaunchContext::CommandLine {
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
                    command: format!("${variable}"),
                    reason: format!("${variable} is not valid UTF-8"),
                })?;
            let mut command = shlex::split(&value).ok_or_else(|| EditorError::InvalidTemplate {
                command: format!("${variable}"),
                reason: format!("${variable} contains unmatched quoting"),
            })?;
            if !contains_file_placeholder(&command) {
                command.push(FILE_PLACEHOLDER.to_owned());
            }
            validate_template(&command, true).map_err(|error| match error {
                EditorError::InvalidTemplate { command, reason } => EditorError::InvalidTemplate {
                    command,
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

fn normalize_relative_path(path: &Path) -> Result<PathBuf, EditorError> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(component) => normalized.push(component),
            Component::CurDir => {}
            _ => {
                return Err(EditorError::PathOutsideProject {
                    path: path.to_path_buf(),
                });
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(EditorError::PathOutsideProject {
            path: path.to_path_buf(),
        });
    }
    Ok(normalized)
}

fn validate_template(command: &[String], require_file: bool) -> Result<(), EditorError> {
    if command.first().is_none_or(String::is_empty) {
        return Err(invalid_template(command, "the executable is empty"));
    }
    if require_file && !contains_file_placeholder(command) {
        return Err(invalid_template(command, "the command must contain {file}"));
    }
    for token in command {
        let mut remainder = token.as_str();
        loop {
            let opening = remainder.find('{');
            let closing = remainder.find('}');
            if closing.is_some_and(|closing| opening.is_none_or(|opening| closing < opening)) {
                return Err(invalid_template(
                    command,
                    format!("unmatched closing brace in '{token}'"),
                ));
            }
            let Some(start) = opening else {
                break;
            };
            remainder = &remainder[start..];
            let Some(end) = remainder.find('}') else {
                return Err(invalid_template(
                    command,
                    format!("unclosed placeholder in '{token}'"),
                ));
            };
            let placeholder = &remainder[..=end];
            if !matches!(
                placeholder,
                FILE_PLACEHOLDER | LINE_PLACEHOLDER | COLUMN_PLACEHOLDER
            ) {
                return Err(invalid_template(
                    command,
                    format!("unknown placeholder '{placeholder}'"),
                ));
            }
            remainder = &remainder[end + 1..];
        }
    }
    Ok(())
}

fn invalid_template(command: &[String], reason: impl Into<String>) -> EditorError {
    EditorError::InvalidTemplate {
        command: command
            .first()
            .filter(|command| !command.is_empty())
            .cloned()
            .unwrap_or_else(|| "<empty>".to_owned()),
        reason: reason.into(),
    }
}

static EDITOR_REAPER: OnceLock<mpsc::Sender<Child>> = OnceLock::new();

fn editor_reaper() -> io::Result<&'static mpsc::Sender<Child>> {
    if let Some(reaper) = EDITOR_REAPER.get() {
        return Ok(reaper);
    }
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("harkness-editor-reaper".to_owned())
        .spawn(move || reap_children(receiver))?;
    let _ = EDITOR_REAPER.set(sender);
    EDITOR_REAPER
        .get()
        .ok_or_else(|| io::Error::other("the editor reaper could not be installed"))
}

fn reap_children(receiver: mpsc::Receiver<Child>) {
    let mut children = Vec::<Child>::new();
    loop {
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(child) => children.push(child),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) if children.is_empty() => return,
            Err(mpsc::RecvTimeoutError::Disconnected) => {}
        }
        children.retain_mut(|child| matches!(child.try_wait(), Ok(None)));
    }
}

fn enqueue_for_reaping(reaper: &mpsc::Sender<Child>, child: Child) {
    if let Err(error) = reaper.send(child) {
        let mut child = error.0;
        let _ = thread::Builder::new()
            .name("harkness-editor-reaper-fallback".to_owned())
            .spawn(move || {
                let _ = child.wait();
            });
    }
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
    use super::{EditorConfiguration, EditorLaunchContext, EditorPosition, expand, open};
    use harkness_test_fixtures::{Fixture, wait_for_file};
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;
    use std::{ffi::OsString, fs, num::NonZeroU32, path::Path};

    fn position(line: u32, column: u32) -> EditorPosition {
        EditorPosition::new(
            NonZeroU32::new(line).unwrap(),
            NonZeroU32::new(column).unwrap(),
        )
    }

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
            position(12, 3),
        );
        assert_eq!(
            expanded,
            ["code", "--goto", "name with spaces.rs:12:3"].map(OsString::from)
        );
    }

    #[test]
    fn graphical_fallback_uses_only_the_desktop_opener() {
        let configuration = super::fallback_configuration(EditorLaunchContext::Graphical).unwrap();
        #[cfg(target_os = "macos")]
        assert_eq!(configuration.command()[0], "/usr/bin/open");
        #[cfg(target_os = "windows")]
        assert_eq!(configuration.command()[0], "explorer.exe");
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        assert_eq!(configuration.command()[0], "xdg-open");
    }

    #[test]
    fn launch_context_owns_the_stdio_policy() {
        let configuration = super::EditorPreset::VisualStudioCode.configuration();
        let command_line =
            super::launch_plan(Some(&configuration), EditorLaunchContext::CommandLine).unwrap();
        let graphical =
            super::launch_plan(Some(&configuration), EditorLaunchContext::Graphical).unwrap();

        assert_eq!(command_line.stdio, super::EditorStdio::Inherit);
        assert_eq!(graphical.stdio, super::EditorStdio::Null);
    }

    #[test]
    fn template_validation_names_the_executable_and_rejects_bad_shapes() {
        let cases = [
            vec!["code".to_owned()],
            vec![
                "code".to_owned(),
                "{unknown}".to_owned(),
                "{file}".to_owned(),
            ],
            vec!["code".to_owned(), "{file".to_owned()],
            vec!["code".to_owned(), "{file}}".to_owned()],
        ];
        for command in cases {
            assert!(matches!(
                EditorConfiguration::new(command),
                Err(super::EditorError::InvalidTemplate { command, .. }) if command == "code"
            ));
        }
    }

    #[test]
    fn dot_components_are_normalized_but_escape_components_are_refused() {
        assert_eq!(
            super::normalize_relative_path(Path::new("./src/./main.rs")).unwrap(),
            Path::new("src/main.rs")
        );
        for path in ["", ".", "../main.rs", "src/../../main.rs"] {
            assert!(matches!(
                super::normalize_relative_path(Path::new(path)),
                Err(super::EditorError::PathOutsideProject { .. })
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_file_placeholder_preserves_non_utf8_bytes_when_concatenated() {
        use std::os::unix::ffi::OsStringExt;

        let configuration =
            EditorConfiguration::new(vec!["zed".to_owned(), "{file}:{line}:{column}".to_owned()])
                .unwrap();
        let file = OsString::from_vec(b"bad-\xff.rs".to_vec());
        let expanded = expand(&configuration, Path::new(&file), position(9, 1));
        assert_eq!(expanded[1].as_bytes(), b"bad-\xff.rs:9:1");
    }

    #[cfg(windows)]
    #[test]
    fn a_file_placeholder_preserves_non_unicode_wide_units_when_concatenated() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};

        let configuration =
            EditorConfiguration::new(vec!["zed".to_owned(), "{file}:{line}:{column}".to_owned()])
                .unwrap();
        let file = OsString::from_wide(&[b'b' as u16, b'a' as u16, b'd' as u16, 0xD800]);
        let expanded = expand(&configuration, Path::new(&file), position(9, 2));
        assert_eq!(
            expanded[1].encode_wide().collect::<Vec<_>>(),
            [
                b'b' as u16,
                b'a' as u16,
                b'd' as u16,
                0xD800,
                b':' as u16,
                b'9' as u16,
                b':' as u16,
                b'2' as u16
            ]
        );
    }

    #[cfg(unix)]
    fn recording_editor(fixture: &Fixture, log: &Path) -> EditorConfiguration {
        // Publish the log only after its complete contents are durable to the
        // test process. `wait_for_file` observes existence, and a direct shell
        // redirection creates the destination before `printf` fills it; on a
        // sufficiently busy runner that made the assertion race an empty file.
        let executable = fixture.shim(
            "record-editor",
            "#!/bin/sh\nprintf '%s' \"$2\" > \"$1.tmp\" && mv \"$1.tmp\" \"$1\"\n",
        );
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
            position(17, 4),
            Some(&configuration),
            EditorLaunchContext::Graphical,
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
    fn a_running_editor_holds_no_catalog_lock_and_never_blocks_the_caller() {
        use crate::ProjectService;

        let fixture = Fixture::new();
        let root = fixture.directory("repository");
        fs::write(root.join("changed.rs"), "content\n").unwrap();
        let ready = fixture.root.path().join("ready");
        let release = fixture.root.path().join("release");
        let executable = fixture.shim(
            "blocking-editor",
            "#!/bin/sh\nprintf ready > \"$1\"\nwhile [ ! -f \"$2\" ]; do sleep 0.01; done\n",
        );
        let configuration = EditorConfiguration::new(vec![
            executable.to_string_lossy().into_owned(),
            ready.to_string_lossy().into_owned(),
            release.to_string_lossy().into_owned(),
            "{file}".to_owned(),
        ])
        .unwrap();
        let mut service = ProjectService::load_from_data_dir(&fixture.data_dir).unwrap();
        let project = service.import_local(&root).unwrap();
        service
            .set_editor_configuration(Some(configuration))
            .unwrap();

        service
            .open_in_editor(
                project.id,
                Path::new("changed.rs"),
                position(1, 1),
                EditorLaunchContext::Graphical,
            )
            .unwrap();
        wait_for_file(&ready);
        service.set_editor_configuration(None).unwrap();
        fs::write(release, b"release").unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_short_lived_editor_is_reaped_in_the_background() {
        use std::{
            io, thread,
            time::{Duration, Instant},
        };

        let fixture = Fixture::new();
        let root = fixture.directory("repository");
        fs::write(root.join("changed.rs"), "content\n").unwrap();
        let pid_log = fixture.root.path().join("editor-pid");
        let executable = fixture.shim("short-editor", "#!/bin/sh\nprintf '%s' \"$$\" > \"$1\"\n");
        let configuration = EditorConfiguration::new(vec![
            executable.to_string_lossy().into_owned(),
            pid_log.to_string_lossy().into_owned(),
            "{file}".to_owned(),
        ])
        .unwrap();
        open(
            &root,
            Path::new("changed.rs"),
            position(1, 1),
            Some(&configuration),
            EditorLaunchContext::Graphical,
        )
        .unwrap();
        wait_for_file(&pid_log);
        let process_id = String::from_utf8(fs::read(pid_log).unwrap())
            .unwrap()
            .parse::<libc::pid_t>()
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let exists = unsafe { libc::kill(process_id, 0) } == 0;
            if !exists && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                break;
            }
            assert!(Instant::now() < deadline, "editor child was not reaped");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn graphical_launch_refuses_a_missing_working_tree_file() {
        let fixture = Fixture::new();
        let root = fixture.directory("repository");
        let configuration = super::EditorPreset::VisualStudioCode.configuration();

        assert!(matches!(
            open(
                &root,
                Path::new("missing.rs"),
                position(1, 1),
                Some(&configuration),
                EditorLaunchContext::Graphical,
            ),
            Err(super::EditorError::FileUnavailable { .. })
        ));
    }

    #[cfg(target_os = "linux")]
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
            position(8, 2),
            Some(&configuration),
            EditorLaunchContext::Graphical,
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
            "version": 3,
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
                position(3, 1),
                EditorLaunchContext::Graphical,
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
        assert_eq!(configured["version"], 3);

        service.set_editor_configuration(None).unwrap();
        let cleared: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture.data_dir.join("projects.json")).unwrap())
                .unwrap();
        assert!(cleared.get("editor").is_none());
        assert_eq!(cleared["version"], 1);
    }
}
