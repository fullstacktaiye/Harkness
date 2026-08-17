//! Controlled argv-only process execution and the thin test runner built on it.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use harkness_context::{CaptureRequest, FilesystemProbe, SnapshotWireRef, WorkspaceSnapshot};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::tool::{
    ArtifactRef, Capability, Capture, CapturedStream, ExecutionContext, RequestEffects, RiskLevel,
    Tool, ToolError, ToolIdentity, ToolMetadata, ToolProcess,
};
use crate::trust::{
    AllowlistedEnv, CommandSpec, EnvironmentName, PathAccess, PathBoundary, RequestFlags,
};

/// Timeout used when a process request omits one.
pub const DEFAULT_PROCESS_TIMEOUT_SECONDS: u64 = 120;
/// Longest child-process timeout a request can select.
pub const MAX_PROCESS_TIMEOUT_SECONDS: u64 = 600;
/// Most text from either stream carried inline beside its captured artifact.
pub const MAX_INLINE_TAIL_BYTES: usize = 4 * 1024;
/// Most bytes from each check output stream admitted to artifact storage.
pub const MAX_CHECK_ARTIFACT_BYTES: u64 = 8 * 1024 * 1024;
/// Extra call time reserved for spawn, termination, draining, and persistence.
const PROCESS_CALL_TIMEOUT_SECONDS: u64 = MAX_PROCESS_TIMEOUT_SECONDS + 10;

/// Input to `process.exec@1.0.0`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProcessExecInput {
    /// Executable followed by its arguments. No element is interpreted by a shell.
    #[schemars(length(min = 1))]
    pub argv: Vec<String>,
    /// Workspace-relative working directory. Defaults to the workspace root.
    pub cwd: Option<String>,
    /// Exact environment overrides, limited to the descriptor allowlist.
    pub env: Option<BTreeMap<String, String>>,
    /// Child timeout. Defaults to 120 seconds and is clamped to 1..=600.
    pub timeout_seconds: Option<u64>,
}

/// A bounded inline view of one artifact-backed process stream.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BoundedText {
    /// Lossy UTF-8 excerpt from the end of the stream.
    pub text: String,
    /// Total bytes emitted before any excerpting.
    pub byte_len: u64,
    /// Whether the inline text omits any stream bytes.
    pub truncated: bool,
}

/// Result of `process.exec@1.0.0`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessExecOutput {
    /// Direct child's exit code, absent when a signal ended it.
    pub exit_code: Option<i32>,
    /// Terminating signal on platforms that expose it.
    pub signal: Option<i32>,
    /// Whether the child reached the request's enforced timeout.
    pub timed_out: bool,
    /// Timeout actually enforced after defaulting and clamping.
    pub timeout_seconds: u64,
    /// Wall-clock process duration in milliseconds.
    pub duration_ms: u64,
    /// Bounded tail of standard output.
    pub stdout_tail: BoundedText,
    /// Bounded tail of standard error.
    pub stderr_tail: BoundedText,
    /// Artifact containing captured standard-output bytes.
    pub stdout_artifact: ArtifactRef,
    /// Artifact containing captured standard-error bytes.
    pub stderr_artifact: ArtifactRef,
}

/// The production `process.exec@1.0.0` tool.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessExec;

impl Tool for ProcessExec {
    type Input = ProcessExecInput;
    type Output = ProcessExecOutput;

    fn metadata(&self) -> ToolMetadata {
        process_metadata(
            "process.exec",
            "Execute a process",
            "Runs one argv-only child with a contained cwd, allowlisted environment, bounded output artifacts, and a mandatory timeout.",
            &[],
        )
    }

    fn request_effects(
        &self,
        input: &Self::Input,
        boundary: &PathBoundary,
    ) -> Result<RequestEffects, ToolError> {
        process_effects(input.cwd.as_deref(), boundary)
    }

    fn execute(
        &self,
        input: Self::Input,
        context: &mut ExecutionContext,
    ) -> Result<Self::Output, ToolError> {
        run_process(
            input.argv,
            input.cwd,
            input.env,
            input.timeout_seconds,
            &[],
            None,
            "process",
            context,
        )
        .map(|output| output.process)
    }
}

/// Input to `test.run@1.0.0`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TestRunInput {
    /// Explicit test executable and arguments. Harkness never guesses a command.
    #[schemars(length(min = 1))]
    pub command: Vec<String>,
    /// Workspace-relative working directory. Defaults to the workspace root.
    pub cwd: Option<String>,
    /// Exact environment overrides, limited to the descriptor allowlist.
    pub env: Option<BTreeMap<String, String>>,
    /// Child timeout. Defaults to 120 seconds and is clamped to 1..=600.
    pub timeout_seconds: Option<u64>,
}

/// Result of `test.run@1.0.0`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TestRunOutput {
    /// True exactly when the process exited with code zero without timing out.
    pub passed: bool,
    /// The same controlled-process result returned by `process.exec`.
    #[serde(flatten)]
    pub process: ProcessExecOutput,
}

/// The production `test.run@1.0.0` tool.
#[derive(Clone, Copy, Debug, Default)]
pub struct TestRun;

impl Tool for TestRun {
    type Input = TestRunInput;
    type Output = TestRunOutput;

    fn metadata(&self) -> ToolMetadata {
        process_metadata(
            "test.run",
            "Run an explicit test command",
            "Runs only the supplied test argv through the controlled process supervisor and reports whether it exited zero.",
            &[],
        )
    }

    fn request_effects(
        &self,
        input: &Self::Input,
        boundary: &PathBoundary,
    ) -> Result<RequestEffects, ToolError> {
        process_effects(input.cwd.as_deref(), boundary)
    }

    fn execute(
        &self,
        input: Self::Input,
        context: &mut ExecutionContext,
    ) -> Result<Self::Output, ToolError> {
        let process = run_process(
            input.command,
            input.cwd,
            input.env,
            input.timeout_seconds,
            &[],
            None,
            "test",
            context,
        )?
        .process;
        Ok(TestRunOutput {
            passed: process.exit_code == Some(0) && !process.timed_out,
            process,
        })
    }
}

/// Machine-readable output convention for `check.run@1.0.0`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckOutputParser {
    /// Keep the result at run level; make no file or line inference.
    #[default]
    Plain,
    /// Parse Cargo/rustc newline-delimited JSON diagnostics.
    CargoJson,
}

/// Input to `check.run@1.0.0`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CheckRunInput {
    /// Stable per-project check identifier.
    pub check_id: String,
    /// Human-readable configured label.
    pub label: String,
    /// Explicit executable and argv. No shell is involved.
    #[schemars(length(min = 1))]
    pub command: Vec<String>,
    /// Workspace-relative working directory.
    pub cwd: Option<String>,
    /// Exact environment overrides admitted by the descriptor.
    pub env: Option<BTreeMap<String, String>>,
    /// Child timeout in seconds.
    pub timeout_seconds: Option<u64>,
    /// How stored output may be associated with files and lines.
    #[serde(default)]
    pub parser: CheckOutputParser,
}

/// Compact identity of the workspace a check actually ran against.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckWorkspaceState {
    /// Composite workspace digest. Never `HEAD` alone.
    pub digest: String,
    /// Checked-out commit, when one exists.
    pub head: Option<String>,
    /// Checked-out branch, when attached.
    pub branch: Option<String>,
    /// Full strict snapshot used to compute freshness later.
    pub snapshot_artifact: ArtifactRef,
}

/// Redaction-safe machine encoding of a strict snapshot wire record.
///
/// The snapshot's own JSON is carried as one opaque base64 string rather than as
/// the object it is. Artifact value redaction is deliberately free to rewrite
/// string values, and this record has to survive byte-exact because the digest a
/// check's freshness is bound to is recomputed from it.
///
/// Version 2 encodes those bytes as base64. Version 1 encoded them as a JSON
/// array of decimal integers, which cost up to four characters per byte: a
/// workspace with many untracked files produced a multi-megabyte wire record
/// stored as tens of megabytes, and read back into memory in that form on every
/// freshness check. Base64 is ~1.33x and is just as rewritable by a redactor,
/// which was the only property the integer encoding was chosen for.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckSnapshotArtifact {
    pub(crate) schema_version: u32,
    /// Base64 of the snapshot wire record's JSON, at schema version 2.
    pub(crate) snapshot_json_base64: String,
}

pub(crate) const CHECK_SNAPSHOT_ARTIFACT_SCHEMA_VERSION: u32 = 2;

/// Result of `check.run@1.0.0`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckRunOutput {
    /// Stable configured identity copied from the request.
    pub check_id: String,
    /// Configured display label copied from the request.
    pub label: String,
    /// True exactly when the process exited zero without timing out.
    pub passed: bool,
    /// Parser a read-only projection may apply to the stored artifacts.
    pub parser: CheckOutputParser,
    /// Exact workspace identity captured immediately before spawn.
    pub workspace_state: CheckWorkspaceState,
    /// Per-stream byte cap applied to each check output artifact.
    pub artifact_byte_limit: u64,
    /// Whether standard output exceeded the artifact byte cap.
    pub stdout_artifact_truncated: bool,
    /// Whether standard error exceeded the artifact byte cap.
    pub stderr_artifact_truncated: bool,
    /// Controlled child-process result and bounded inline tails.
    #[serde(flatten)]
    pub process: ProcessExecOutput,
}

/// State-bound project check executed through the ordinary runtime pipeline.
#[derive(Clone, Copy, Debug, Default)]
pub struct CheckRun;

impl Tool for CheckRun {
    type Input = CheckRunInput;
    type Output = CheckRunOutput;

    fn metadata(&self) -> ToolMetadata {
        process_metadata(
            "check.run",
            "Run a project check",
            "Captures the composite workspace identity, executes one configured argv-only check, and stores capped output artifacts with total-byte and truncation metadata.",
            &[],
        )
    }

    fn request_effects(
        &self,
        input: &Self::Input,
        boundary: &PathBoundary,
    ) -> Result<RequestEffects, ToolError> {
        process_effects(input.cwd.as_deref(), boundary)
    }

    fn execute(
        &self,
        input: Self::Input,
        context: &mut ExecutionContext,
    ) -> Result<Self::Output, ToolError> {
        let metadata = context.workspace_metadata().ok_or_else(|| {
            ToolError::execution_failed("check.run requires catalog workspace metadata")
        })?;
        let project_id = metadata.project_id();
        let root = context.workspace_root().to_path_buf();
        let git = harkness_git::GitService::new(&root, &root);
        let snapshot = WorkspaceSnapshot::capture(
            &CaptureRequest::new(project_id),
            &git,
            &FilesystemProbe::new(&root),
            context.cancellation(),
        )
        .map_err(ToolError::execution_failed)?;
        let snapshot_json = serde_json::to_vec(&SnapshotWireRef::from(&snapshot))
            .map_err(ToolError::execution_failed)?;
        let encoded = serde_json::to_value(CheckSnapshotArtifact {
            schema_version: CHECK_SNAPSHOT_ARTIFACT_SCHEMA_VERSION,
            snapshot_json_base64: BASE64.encode(&snapshot_json),
        })
        .map_err(ToolError::execution_failed)?;
        let snapshot_artifact = context.write_json_artifact(
            "check-workspace-state.json",
            "application/vnd.harkness.workspace-snapshot-bytes+json",
            &encoded,
        )?;
        let workspace_state = CheckWorkspaceState {
            digest: snapshot.digest().to_string(),
            head: snapshot.head().map(str::to_owned),
            branch: snapshot.branch().map(str::to_owned),
            snapshot_artifact,
        };
        let controlled = run_process(
            input.command,
            input.cwd,
            input.env,
            input.timeout_seconds,
            &[],
            Some(MAX_CHECK_ARTIFACT_BYTES),
            "check",
            context,
        )?;
        let process = controlled.process;
        Ok(CheckRunOutput {
            check_id: input.check_id,
            label: input.label,
            passed: process.exit_code == Some(0) && !process.timed_out,
            parser: input.parser,
            workspace_state,
            artifact_byte_limit: MAX_CHECK_ARTIFACT_BYTES,
            stdout_artifact_truncated: controlled.stdout_artifact_truncated,
            stderr_artifact_truncated: controlled.stderr_artifact_truncated,
            process,
        })
    }
}

fn process_effects(
    cwd: Option<&str>,
    boundary: &PathBoundary,
) -> Result<RequestEffects, ToolError> {
    let effects = RequestEffects::default().with_flags(RequestFlags::default().executing());
    match cwd {
        Some(cwd) => Ok(effects.with_path(boundary.contain(cwd)?, PathAccess::Read)),
        None => Ok(effects),
    }
}

fn process_metadata(
    id: &str,
    title: &str,
    description: &str,
    environment: &[EnvironmentName],
) -> ToolMetadata {
    ToolMetadata::new(
        ToolIdentity::parse(id, "1.0.0").expect("a built-in tool identity"),
        title,
        description,
        RiskLevel::Execute,
    )
    .with_capabilities([Capability::new("process.spawn").expect("a built-in capability")])
    .with_environment(environment.iter().cloned())
    .within(Duration::from_secs(PROCESS_CALL_TIMEOUT_SECONDS))
    .spawning_processes()
}

#[allow(clippy::too_many_arguments)]
fn run_process(
    argv: Vec<String>,
    cwd: Option<String>,
    overrides: Option<BTreeMap<String, String>>,
    timeout_seconds: Option<u64>,
    declared_environment: &[EnvironmentName],
    artifact_byte_limit: Option<u64>,
    artifact_prefix: &str,
    context: &mut ExecutionContext,
) -> Result<ControlledProcessOutput, ToolError> {
    let Some((program, arguments)) = argv.split_first() else {
        // The generated schema rejects this before the body in normal dispatch.
        // Keep the body total for a typed caller invoking it directly.
        return Err(ToolError::execution_failed(
            "argv must contain an executable",
        ));
    };
    let cwd = context.resolve(cwd.as_deref().unwrap_or("."))?;
    let mut environment = AllowlistedEnv::build(declared_environment);
    if let Some(overrides) = overrides.as_ref() {
        environment
            .apply_overrides(declared_environment, overrides)
            .map_err(ToolError::execution_failed)?;
    }
    let spec = CommandSpec::new(
        program,
        arguments.iter().map(OsString::from).collect(),
        cwd,
        environment,
    )
    .map_err(ToolError::execution_failed)?;
    let timeout_seconds = effective_timeout(timeout_seconds);
    let stdout_capture =
        artifact_capture(format!("{artifact_prefix}-stdout.log"), artifact_byte_limit);
    let stderr_capture =
        artifact_capture(format!("{artifact_prefix}-stderr.log"), artifact_byte_limit);
    let output = ToolProcess::new(spec)
        .capture_stdout(stdout_capture)
        .capture_stderr(stderr_capture)
        .within(Duration::from_secs(timeout_seconds))
        .run(context)?;

    let stdout_artifact = output
        .stdout()
        .artifact()
        .cloned()
        .expect("stdout was configured as an artifact");
    let stderr_artifact = output
        .stderr()
        .artifact()
        .cloned()
        .expect("stderr was configured as an artifact");
    Ok(ControlledProcessOutput {
        stdout_artifact_truncated: output.stdout().artifact_is_truncated(),
        stderr_artifact_truncated: output.stderr().artifact_is_truncated(),
        process: ProcessExecOutput {
            exit_code: output.code(),
            signal: output.signal(),
            timed_out: output.timed_out(),
            timeout_seconds,
            duration_ms: u64::try_from(output.duration().as_millis()).unwrap_or(u64::MAX),
            stdout_tail: bounded_tail(output.stdout()),
            stderr_tail: bounded_tail(output.stderr()),
            stdout_artifact,
            stderr_artifact,
        },
    })
}

struct ControlledProcessOutput {
    process: ProcessExecOutput,
    stdout_artifact_truncated: bool,
    stderr_artifact_truncated: bool,
}

fn artifact_capture(name: String, byte_limit: Option<u64>) -> Capture {
    match byte_limit {
        Some(max_bytes) => Capture::bounded_artifact(name, max_bytes),
        None => Capture::artifact(name),
    }
}

fn effective_timeout(requested: Option<u64>) -> u64 {
    requested
        .unwrap_or(DEFAULT_PROCESS_TIMEOUT_SECONDS)
        .clamp(1, MAX_PROCESS_TIMEOUT_SECONDS)
}

fn bounded_tail(stream: &CapturedStream) -> BoundedText {
    let tail = stream.tail();
    let mut start = tail.len().saturating_sub(MAX_INLINE_TAIL_BYTES);
    while start < tail.len() && !tail.is_char_boundary(start) {
        start += 1;
    }
    BoundedText {
        text: tail[start..].to_owned(),
        byte_len: stream.byte_len(),
        truncated: stream.is_truncated() || start > 0,
    }
}

#[cfg(test)]
mod tests {
    use super::{Capture, MAX_CHECK_ARTIFACT_BYTES, artifact_capture, effective_timeout};

    #[test]
    fn timeout_default_and_hard_cap_are_stable() {
        assert_eq!(effective_timeout(None), 120);
        assert_eq!(effective_timeout(Some(0)), 1);
        assert_eq!(effective_timeout(Some(601)), 600);
    }

    #[test]
    fn check_capture_uses_the_named_artifact_cap() {
        assert_eq!(
            artifact_capture(
                "check-stdout.log".to_owned(),
                Some(MAX_CHECK_ARTIFACT_BYTES)
            ),
            Capture::BoundedArtifact {
                name: "check-stdout.log".to_owned(),
                media_type: "text/plain".to_owned(),
                max_bytes: MAX_CHECK_ARTIFACT_BYTES,
            }
        );
    }
}
