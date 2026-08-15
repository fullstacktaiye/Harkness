//! Controlled argv-only process execution and the thin test runner built on it.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::time::Duration;

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
/// Most text from either stream carried inline beside its full artifact.
pub const MAX_INLINE_TAIL_BYTES: usize = 4 * 1024;
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

/// A bounded inline view of one complete artifact-backed stream.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
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
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
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
    /// Artifact containing the full standard-output byte stream.
    pub stdout_artifact: ArtifactRef,
    /// Artifact containing the full standard-error byte stream.
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
            "process",
            context,
        )
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
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
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
            "test",
            context,
        )?;
        Ok(TestRunOutput {
            passed: process.exit_code == Some(0) && !process.timed_out,
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
    artifact_prefix: &str,
    context: &mut ExecutionContext,
) -> Result<ProcessExecOutput, ToolError> {
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
    let output = ToolProcess::new(spec)
        .capture_stdout(Capture::artifact(format!("{artifact_prefix}-stdout.log")))
        .capture_stderr(Capture::artifact(format!("{artifact_prefix}-stderr.log")))
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
    Ok(ProcessExecOutput {
        exit_code: output.code(),
        signal: output.signal(),
        timed_out: output.timed_out(),
        timeout_seconds,
        duration_ms: u64::try_from(output.duration().as_millis()).unwrap_or(u64::MAX),
        stdout_tail: bounded_tail(output.stdout()),
        stderr_tail: bounded_tail(output.stderr()),
        stdout_artifact,
        stderr_artifact,
    })
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
    use super::effective_timeout;

    #[test]
    fn timeout_default_and_hard_cap_are_stable() {
        assert_eq!(effective_timeout(None), 120);
        assert_eq!(effective_timeout(Some(0)), 1);
        assert_eq!(effective_timeout(Some(601)), 600);
    }
}
