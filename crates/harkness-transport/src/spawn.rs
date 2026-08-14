//! How a peer process is described, and the hermeticity that description buys.
//!
//! `harkness-git`'s runner establishes what launching a child correctly costs:
//! no shell, its own process group, a pinned working directory, and an
//! environment that cannot carry a decision in from whoever started Harkness.
//! [`SpawnSpec`] generalizes that policy for protocol peers and tightens it in
//! one place — the environment is *allowlisted* rather than inherited and
//! scrubbed.
//!
//! The difference matters because of who the child is. Git is one known program
//! whose credential helpers are the reason its runner deliberately keeps a
//! denylist. An ACP agent or an MCP server is a program someone else wrote,
//! launched on a user's workspace, and the question "which of my environment
//! variables can it read" has exactly one safe default: none of them. A
//! credential reaches a peer only because a caller named it, which is a decision
//! policy above this layer gets to make and audit.

use std::{
    ffi::{OsStr, OsString},
    fmt,
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};

use crate::{
    error::TransportError,
    stderr::{DiscardedStderr, StderrSink},
};

/// How long a peer has to finish its handshake before the connection gives up.
pub const DEFAULT_STARTUP_DEADLINE: Duration = Duration::from_secs(30);

/// The largest single message accepted from a peer, in bytes.
///
/// Generous, because a legitimate `tools/list` from a server with many tools or
/// an ACP update carrying a file's contents is genuinely large, and small enough
/// that a peer cannot make this process hold a workspace in memory one line at a
/// time.
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// A peer program, and everything it is allowed to see.
///
/// Built rather than passed as a tuple so that every field with a security
/// meaning has one documented place to be set, and so that adding a field later
/// does not break every call site.
pub struct SpawnSpec {
    program: PathBuf,
    args: Vec<OsString>,
    env_allowlist: Vec<(OsString, OsString)>,
    working_dir: PathBuf,
    startup_deadline: Duration,
    max_message_bytes: usize,
    stderr_sink: Box<dyn StderrSink>,
}

impl fmt::Debug for SpawnSpec {
    /// Names the environment's keys and not its values. A spec is the one place
    /// a credential can legitimately be, and a `Debug` that printed it would put
    /// it wherever the caller logged the spec.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SpawnSpec")
            .field("program", &self.program)
            .field("args", &self.args)
            .field(
                "env_allowlist",
                &self
                    .env_allowlist
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .field("working_dir", &self.working_dir)
            .field("startup_deadline", &self.startup_deadline)
            .field("max_message_bytes", &self.max_message_bytes)
            .finish_non_exhaustive()
    }
}

impl SpawnSpec {
    /// Describes `program`, run in `working_dir` with an empty environment.
    ///
    /// Both paths are absolute by requirement rather than by convention: a
    /// relative program is resolved against a `PATH` this spec does not carry,
    /// and a relative working directory against whatever directory the process
    /// happens to be in — which for `harkness-cli` invoked from a Git hook is
    /// not a place a decision may come from. Callers identify the executable
    /// itself before they get here; that is a trust question, decided against an
    /// `ExecutableIdentity` in `harkness-runtime`.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>, working_dir: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env_allowlist: Vec::new(),
            working_dir: working_dir.into(),
            startup_deadline: DEFAULT_STARTUP_DEADLINE,
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            stderr_sink: Box::new(DiscardedStderr),
        }
    }

    /// Appends one argument.
    #[must_use]
    pub fn arg(mut self, argument: impl AsRef<OsStr>) -> Self {
        self.args.push(argument.as_ref().to_os_string());
        self
    }

    /// Appends several arguments.
    #[must_use]
    pub fn args(mut self, arguments: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Self {
        self.args.extend(
            arguments
                .into_iter()
                .map(|argument| argument.as_ref().to_os_string()),
        );
        self
    }

    /// Admits exactly one environment variable, with exactly this value.
    ///
    /// Nothing is inherited, so this is the only way a peer sees anything at
    /// all. A name is never a pattern: there is no wildcard here and none may be
    /// added, for the same reason `AllowlistedEnv` in `harkness-runtime` has
    /// none — a prefix is a promise about names nobody has read.
    #[must_use]
    pub fn env(mut self, name: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.env_allowlist
            .push((name.as_ref().to_os_string(), value.as_ref().to_os_string()));
        self
    }

    /// Admits several environment variables.
    #[must_use]
    pub fn envs(
        mut self,
        variables: impl IntoIterator<Item = (impl AsRef<OsStr>, impl AsRef<OsStr>)>,
    ) -> Self {
        for (name, value) in variables {
            self.env_allowlist
                .push((name.as_ref().to_os_string(), value.as_ref().to_os_string()));
        }
        self
    }

    /// Replaces the window the peer has to complete its handshake.
    #[must_use]
    pub fn startup_deadline(mut self, deadline: Duration) -> Self {
        self.startup_deadline = deadline;
        self
    }

    /// Replaces the maximum size of one inbound message.
    #[must_use]
    pub fn max_message_bytes(mut self, limit: usize) -> Self {
        self.max_message_bytes = limit;
        self
    }

    /// Replaces the destination of the peer's standard error.
    #[must_use]
    pub fn stderr_sink(mut self, sink: impl StderrSink + 'static) -> Self {
        self.stderr_sink = Box::new(sink);
        self
    }

    /// The window the peer has to complete its handshake.
    #[must_use]
    pub fn declared_startup_deadline(&self) -> Duration {
        self.startup_deadline
    }

    /// The maximum size of one inbound message.
    #[must_use]
    pub fn declared_max_message_bytes(&self) -> usize {
        self.max_message_bytes
    }

    /// Splits the description into the parts a connection owns separately.
    pub(crate) fn into_parts(self) -> (Command, Box<dyn StderrSink>, Limits) {
        let limits = Limits {
            startup_deadline: self.startup_deadline,
            max_message_bytes: self.max_message_bytes,
            program: self.program.clone(),
        };
        (self.build_command(), self.stderr_sink, limits)
    }

    /// Refuses a description that cannot produce a hermetic invocation.
    ///
    /// Every check here is a property the spawn would otherwise depend on
    /// something outside this crate for, and each one fails *before* a process
    /// exists so there is never a half-started child to clean up.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::InvalidSpawnSpec`] naming the field at fault.
    pub(crate) fn validate(&self) -> Result<(), TransportError> {
        let refuse = |detail: String| Err(TransportError::InvalidSpawnSpec { detail });

        if !self.program.is_absolute() {
            return refuse(format!(
                "the program '{}' is not an absolute path, so it would be resolved \
                 against a PATH this connection does not carry",
                self.program.display()
            ));
        }
        if !self.working_dir.is_absolute() {
            return refuse(format!(
                "the working directory '{}' is not an absolute path",
                self.working_dir.display()
            ));
        }
        if self.max_message_bytes == 0 {
            return refuse(
                "a maximum message size of zero refuses every message the peer could send"
                    .to_owned(),
            );
        }
        if self.startup_deadline.is_zero() {
            return refuse(
                "a startup deadline of zero expires before the peer is launched".to_owned(),
            );
        }
        for (name, value) in &self.env_allowlist {
            check_environment_component(name, "name")?;
            check_environment_component(value, "value")?;
            if name.is_empty() || name.as_encoded_bytes().contains(&b'=') {
                return refuse(format!(
                    "'{}' is not a usable environment variable name",
                    name.to_string_lossy()
                ));
            }
        }
        Ok(())
    }

    /// Builds the hermetic invocation.
    ///
    /// `env_clear` before `envs` is the whole allowlist: it is what makes the
    /// list exhaustive rather than additive, and it is why a canary variable in
    /// this process's environment is absent from the child's.
    fn build_command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .current_dir(&self.working_dir)
            .env_clear()
            .envs(self.env_allowlist.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // The peer starts helpers of its own — an MCP server that shells out, an
        // agent that runs a language server — and a teardown that reached only
        // the program Harkness named would leave them holding the workspace.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        command
    }
}

/// The bounds a running connection enforces, kept after the spec is consumed.
pub(crate) struct Limits {
    pub(crate) startup_deadline: Duration,
    pub(crate) max_message_bytes: usize,
    pub(crate) program: PathBuf,
}

/// Refuses an environment component the operating system cannot carry.
///
/// A NUL byte is not a value `execve` can pass: the standard library would
/// refuse it at spawn time with an error that names neither the variable nor the
/// reason, so it is named here instead.
fn check_environment_component(value: &OsStr, role: &str) -> Result<(), TransportError> {
    if value.as_encoded_bytes().contains(&0) {
        return Err(TransportError::InvalidSpawnSpec {
            detail: format!("an environment variable {role} contains a NUL byte"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{DEFAULT_MAX_MESSAGE_BYTES, SpawnSpec};
    use crate::error::TransportError;

    /// An absolute program path *on the platform running the test*.
    ///
    /// `Path::is_absolute` answers for the current platform, and that is the
    /// right question here: unlike a durable trust record, which outlives the
    /// machine that wrote it and must recognize both conventions, a `SpawnSpec`
    /// describes a program this machine is about to launch. `/usr/bin/agent` is
    /// rooted but not absolute on Windows, so the tests have to name a path the
    /// platform would actually accept or they assert the opposite of what they
    /// read.
    #[cfg(unix)]
    const PROGRAM: &str = "/usr/bin/agent";
    #[cfg(unix)]
    const WORKSPACE: &str = "/workspace";
    #[cfg(windows)]
    const PROGRAM: &str = r"C:\Program Files\agent\agent.exe";
    #[cfg(windows)]
    const WORKSPACE: &str = r"C:\workspace";

    fn detail(spec: &SpawnSpec) -> String {
        match spec.validate() {
            Err(TransportError::InvalidSpawnSpec { detail }) => detail,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn an_absolute_program_and_directory_validate() {
        let spec = SpawnSpec::new(PROGRAM, WORKSPACE)
            .arg("--stdio")
            .env("PATH", "/usr/bin:/bin");

        spec.validate().unwrap();
        assert_eq!(spec.declared_max_message_bytes(), DEFAULT_MAX_MESSAGE_BYTES);
    }

    #[test]
    fn a_relative_program_is_refused_by_name() {
        assert!(detail(&SpawnSpec::new("agent", WORKSPACE)).contains("PATH"));
    }

    #[test]
    fn a_relative_working_directory_is_refused_by_name() {
        assert!(detail(&SpawnSpec::new(PROGRAM, "workspace")).contains("working directory"));
    }

    #[test]
    fn a_bound_that_refuses_every_message_is_refused() {
        assert!(detail(&SpawnSpec::new(PROGRAM, WORKSPACE).max_message_bytes(0)).contains("zero"));
    }

    #[test]
    fn a_startup_deadline_that_has_already_passed_is_refused() {
        assert!(
            detail(&SpawnSpec::new(PROGRAM, WORKSPACE).startup_deadline(Duration::ZERO))
                .contains("zero")
        );
    }

    #[test]
    fn an_environment_name_the_system_cannot_carry_is_refused() {
        assert!(
            detail(&SpawnSpec::new(PROGRAM, WORKSPACE).env("A=B", "c"))
                .contains("usable environment variable name")
        );
        assert!(
            detail(&SpawnSpec::new(PROGRAM, WORKSPACE).env("", "c"))
                .contains("usable environment variable name")
        );
    }

    /// A spec is where a credential legitimately lives, so its `Debug` names the
    /// keys and never the values.
    #[test]
    fn debug_output_names_environment_keys_and_no_values() {
        let rendered = format!(
            "{:?}",
            SpawnSpec::new(PROGRAM, WORKSPACE).env("GITHUB_TOKEN", "ghp_secret")
        );

        assert!(rendered.contains("GITHUB_TOKEN"));
        assert!(!rendered.contains("ghp_secret"));
    }
}
