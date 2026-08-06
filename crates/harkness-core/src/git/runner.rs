//! The system Git command runner.
//!
//! Every Git invocation Harkness makes goes through [`GitCommand`], so the
//! properties that make one correct are properties of all of them: a dedicated
//! process group that cancellation can kill whole, a scrubbed environment, no
//! terminal prompt, and both output streams drained concurrently.

use std::{
    collections::VecDeque,
    ffi::{OsStr, OsString},
    io::{BufReader, Read},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use crate::git::GitError;

/// Git repeats a progress phase on every update, so retaining the whole stream
/// would put megabytes of overwritten counters into a failure message. The tail
/// is what matters: Git prints its diagnosis last.
const RETAINED_GIT_OUTPUT_SEGMENTS: usize = 20;

/// Variables that redirect Git at another repository entirely.
///
/// `harkness-cli` is an agent tool, so it is invoked from Git hooks and from
/// shells that exported these. Left in place, every command would silently
/// operate on whichever repository the parent was pointing at.
const REDIRECTING_ENVIRONMENT: [&str; 4] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
];

/// How long a local read may run before its process group is killed.
const LOCAL_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a local write may run before its process group is killed.
const LOCAL_WRITE_TIMEOUT: Duration = Duration::from_secs(120);

/// How often the wait loop forwards progress and re-checks for cancellation.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Cooperative cancellation token for one Git operation.
///
/// Cancelling kills the command's whole process group, so transport and
/// credential helpers stop with it rather than outliving the caller that gave
/// up on them.
#[derive(Clone, Debug, Default)]
pub struct Cancellation(Arc<AtomicBool>);

impl Cancellation {
    /// Requests cancellation of every operation sharing this token.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// The name this token carried when cloning was the only Git operation.
pub type CloneCancellation = Cancellation;

/// What a Git invocation touches, which fixes its locking and its timeout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitAccess {
    /// Reads local state. Runs with `GIT_OPTIONAL_LOCKS=0`, so a status refresh
    /// never takes `index.lock` to write back a refreshed index, and is bounded
    /// by a 30 second timeout.
    LocalRead,
    /// Writes local state, bounded by a 120 second timeout.
    LocalWrite,
    /// Contacts a remote. Deliberately never timed out: a large clone or fetch
    /// is legitimately slow, and cancellation already bounds it on the only
    /// terms a user cares about.
    Network,
}

impl GitAccess {
    fn default_timeout(self) -> Option<Duration> {
        match self {
            Self::LocalRead => Some(LOCAL_READ_TIMEOUT),
            Self::LocalWrite => Some(LOCAL_WRITE_TIMEOUT),
            Self::Network => None,
        }
    }
}

/// What one finished Git invocation produced.
#[derive(Clone, Debug)]
pub struct GitOutput {
    /// The exit code Git reported, or `None` when a signal ended it.
    pub code: Option<i32>,
    /// Everything Git wrote to standard output, as raw bytes: paths in Git's
    /// output are byte strings and need not be UTF-8.
    pub stdout: Vec<u8>,
    /// The retained tail of Git's diagnostic output.
    pub stderr: String,
}

/// One invocation of the system Git executable.
///
/// Built rather than run directly so that the guarantees below hold for every
/// caller:
///
/// - the child leads its own process group, so cancellation and timeouts kill
///   transport and credential helpers along with Git itself;
/// - `GIT_TERMINAL_PROMPT=0`, so a front end with no terminal fails with Git's
///   diagnostic instead of hanging on `/dev/tty`;
/// - the variables that redirect Git at another repository are removed from the
///   inherited environment;
/// - both output streams are drained on their own threads, so a command that
///   fills the 64 KiB pipe buffer on one stream cannot deadlock against a
///   parent reading the other.
pub struct GitCommand {
    executable: PathBuf,
    working_directory: PathBuf,
    access: GitAccess,
    arguments: Vec<OsString>,
    accepted_exit_codes: Vec<i32>,
    timeout: Option<Duration>,
}

impl GitCommand {
    /// Prepares a Git invocation in `working_directory`.
    ///
    /// The working directory is selected with [`Command::current_dir`] rather
    /// than `git -C`, so it applies before Git reads any configuration and
    /// cannot be overridden by an argument a caller appends later.
    #[must_use]
    pub fn new(
        executable: impl Into<PathBuf>,
        working_directory: impl Into<PathBuf>,
        access: GitAccess,
    ) -> Self {
        Self {
            executable: executable.into(),
            working_directory: working_directory.into(),
            access,
            arguments: Vec::new(),
            accepted_exit_codes: Vec::new(),
            timeout: access.default_timeout(),
        }
    }

    /// Appends one argument.
    #[must_use]
    pub fn arg(mut self, argument: impl AsRef<OsStr>) -> Self {
        self.arguments.push(argument.as_ref().to_os_string());
        self
    }

    /// Appends several arguments.
    #[must_use]
    pub fn args(mut self, arguments: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Self {
        self.arguments.extend(
            arguments
                .into_iter()
                .map(|argument| argument.as_ref().to_os_string()),
        );
        self
    }

    /// Treats `code` as success rather than failure.
    ///
    /// Several Git verbs answer a question through their exit status:
    /// `git diff --quiet` exits 1 for "there are differences" and
    /// `git merge-base --is-ancestor` exits 1 for false. Without this they
    /// would be unusable, because every non-zero status is otherwise a failure.
    #[must_use]
    pub fn accept_exit_code(mut self, code: i32) -> Self {
        self.accepted_exit_codes.push(code);
        self
    }

    /// Replaces the timeout implied by the command's [`GitAccess`].
    ///
    /// Setting one on a [`GitAccess::Network`] command opts that command out of
    /// the deliberate no-timeout rule; nothing in Harkness does.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Runs Git to completion, discarding its progress reporting.
    pub fn run(self, cancellation: &Cancellation) -> Result<GitOutput, GitError> {
        self.run_with_progress(cancellation, |_| {})
    }

    /// Runs Git to completion, forwarding each diagnostic segment as it
    /// arrives.
    ///
    /// This blocks until Git exits, is cancelled, or times out, so a front end
    /// with an event loop must call it on a worker thread.
    pub fn run_with_progress(
        self,
        cancellation: &Cancellation,
        mut on_progress: impl FnMut(String),
    ) -> Result<GitOutput, GitError> {
        let described = self.describe();
        let mut command = Command::new(&self.executable);
        command
            .args(&self.arguments)
            .current_dir(&self.working_directory)
            // Git may open /dev/tty even when stdin is closed. A GUI cannot
            // answer that prompt, so fail with Git's diagnostic instead of
            // hanging.
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in REDIRECTING_ENVIRONMENT {
            command.env_remove(name);
        }
        if self.access == GitAccess::LocalRead {
            command.env("GIT_OPTIONAL_LOCKS", "0");
        }
        // Network verbs start transport and credential helpers. Keeping the
        // whole tree in a dedicated group lets cancellation stop every process
        // before a caller cleans up after it.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        let mut child = command
            .spawn()
            .map_err(|source| GitError::Launch { source })?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (sender, receiver) = mpsc::channel();
        // Two readers, always. Piping both streams and draining only one
        // deadlocks as soon as Git fills the pipe buffer of the other, which
        // `git status --porcelain=v2` does on a large repository.
        let stdout_reader = thread::spawn(move || read_to_end(stdout));
        let stderr_reader = thread::spawn(move || read_git_output(stderr, &sender));

        let deadline = self.timeout.map(|timeout| Instant::now() + timeout);
        loop {
            while let Ok(message) = receiver.try_recv() {
                on_progress(message);
            }
            if cancellation.is_cancelled() {
                terminate(&mut child, stdout_reader, stderr_reader);
                return Err(GitError::Cancelled);
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                terminate(&mut child, stdout_reader, stderr_reader);
                return Err(GitError::TimedOut {
                    command: described,
                    timeout: self.timeout.unwrap_or_default(),
                });
            }
            if let Some(status) = child
                .try_wait()
                .map_err(|source| GitError::Launch { source })?
            {
                let stdout = stdout_reader.join().unwrap_or_default();
                let stderr = stderr_reader
                    .join()
                    .unwrap_or_else(|_| "Git output reader failed".to_owned());
                while let Ok(message) = receiver.try_recv() {
                    on_progress(message);
                }
                let code = status.code();
                let accepted = status.success()
                    || code.is_some_and(|code| self.accepted_exit_codes.contains(&code));
                return if accepted {
                    Ok(GitOutput {
                        code,
                        stdout,
                        stderr,
                    })
                } else {
                    Err(GitError::Failed {
                        command: described,
                        stderr,
                    })
                };
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Names the invocation for a diagnostic, without its executable path.
    fn describe(&self) -> String {
        self.arguments
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Kills the command's process group and waits for its readers to drain.
///
/// The readers finish because every writer of both pipes belonged to the group
/// that was just killed.
fn terminate(
    child: &mut Child,
    stdout_reader: thread::JoinHandle<Vec<u8>>,
    stderr_reader: thread::JoinHandle<String>,
) {
    terminate_process_group(child);
    let _ = child.wait();
    let _ = stdout_reader.join();
    let _ = stderr_reader.join();
}

#[cfg(unix)]
fn terminate_process_group(child: &mut Child) {
    // `process_group(0)` makes the child's PID its process-group ID. A negative
    // target atomically signals Git and all helpers that still belong to it.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn terminate_process_group(child: &mut Child) {
    let _ = child.kill();
}

/// Drains Git's standard output.
///
/// A read failure yields whatever arrived before it: the exit status and Git's
/// diagnostics decide the outcome of a command, never this.
fn read_to_end(stdout: impl Read) -> Vec<u8> {
    let mut reader = BufReader::new(stdout);
    let mut captured = Vec::new();
    let _ = reader.read_to_end(&mut captured);
    captured
}

/// Forwards Git's standard-error segments and returns the retained tail.
///
/// Git separates the updates within a progress phase with carriage returns and
/// only emits a newline when the phase ends, so reading lines would report
/// nothing for the whole of the slowest phase and then deliver every
/// overwritten counter at once. Both separators end a segment here.
fn read_git_output(stderr: impl Read, sender: &mpsc::Sender<String>) -> String {
    let mut reader = BufReader::new(stderr);
    let mut retained: VecDeque<String> = VecDeque::new();
    let mut segment = Vec::new();
    let mut buffer = [0u8; 4096];

    let end_segment = |segment: &mut Vec<u8>, retained: &mut VecDeque<String>| {
        if segment.is_empty() {
            return;
        }
        let message = String::from_utf8_lossy(segment).trim().to_owned();
        segment.clear();
        if message.is_empty() {
            return;
        }
        if retained.len() == RETAINED_GIT_OUTPUT_SEGMENTS {
            retained.pop_front();
        }
        retained.push_back(message.clone());
        let _ = sender.send(message);
    };

    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                for &byte in &buffer[..read] {
                    if byte == b'\n' || byte == b'\r' {
                        end_segment(&mut segment, &mut retained);
                    } else {
                        segment.push(byte);
                    }
                }
            }
            Err(error) => {
                segment.extend_from_slice(format!("failed to read Git output: {error}").as_bytes());
                break;
            }
        }
    }
    end_segment(&mut segment, &mut retained);
    Vec::from(retained).join("\n")
}

#[cfg(test)]
mod tests {
    use std::{io, sync::mpsc};

    #[cfg(unix)]
    use std::time::{Duration, Instant};

    #[cfg(unix)]
    use super::{Cancellation, GitAccess, GitCommand};
    #[cfg(unix)]
    use crate::{git::GitError, testing::Fixture};

    /// Git overwrites a progress phase with carriage returns and only emits a
    /// newline when the phase ends, so line-oriented reads report nothing for
    /// the whole of the slowest phase.
    #[test]
    fn carriage_returns_end_a_progress_segment() {
        let (sender, receiver) = mpsc::channel();
        let output = "Cloning into 'x'...\nReceiving objects:  50% (1/2)\rReceiving objects: 100% (2/2), done.\n";

        let retained = super::read_git_output(io::Cursor::new(output), &sender);
        drop(sender);

        assert_eq!(
            receiver.iter().collect::<Vec<_>>(),
            [
                "Cloning into 'x'...",
                "Receiving objects:  50% (1/2)",
                "Receiving objects: 100% (2/2), done.",
            ]
        );
        assert!(retained.ends_with("Receiving objects: 100% (2/2), done."));
    }

    #[test]
    fn retained_git_output_keeps_only_the_diagnostic_tail() {
        let (sender, receiver) = mpsc::channel();
        let mut output = (0..500).fold(String::new(), |mut output, index| {
            output.push_str(&format!("Receiving objects: {index}%\r"));
            output
        });
        output.push_str("fatal: repository not found\n");

        let retained = super::read_git_output(io::Cursor::new(output), &sender);
        drop(sender);

        assert_eq!(receiver.iter().count(), 501, "every update is forwarded");
        assert_eq!(
            retained.lines().count(),
            super::RETAINED_GIT_OUTPUT_SEGMENTS
        );
        assert!(retained.ends_with("fatal: repository not found"));
    }

    /// Both pipes are drained concurrently. Were only one reader running, this
    /// command would block forever once it filled the 64 KiB buffer of the
    /// other stream, so a hang here is the expected failure mode.
    #[cfg(unix)]
    #[test]
    fn a_command_flooding_both_streams_completes() {
        const FLOOD: usize = 200 * 1024;

        let fixture = Fixture::new();
        let working_directory = fixture.directory("flooding");
        let flooding_git = fixture.shim(
            "flooding-git",
            &format!(
                "#!/bin/sh\n\
                 yes harkness-stdout | head -c {FLOOD}\n\
                 yes harkness-stderr | head -c {FLOOD} >&2\n"
            ),
        );

        let output = GitCommand::new(flooding_git, working_directory, GitAccess::LocalRead)
            .arg("status")
            .run(&Cancellation::default())
            .unwrap();

        assert_eq!(output.code, Some(0));
        assert_eq!(output.stdout.len(), FLOOD);
        assert!(
            output
                .stderr
                .lines()
                .all(|line| line.contains("harkness-stderr"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_accepted_exit_code_is_not_a_failure() {
        let fixture = Fixture::new();
        let working_directory = fixture.directory("accepted-exit");
        let refusing_git = fixture.shim("refusing-git", "#!/bin/sh\nexit 1\n");

        let accepted = GitCommand::new(&refusing_git, &working_directory, GitAccess::LocalRead)
            .args(["diff", "--quiet"])
            .accept_exit_code(1)
            .run(&Cancellation::default())
            .unwrap();
        assert_eq!(accepted.code, Some(1));

        let rejected = GitCommand::new(&refusing_git, &working_directory, GitAccess::LocalRead)
            .args(["diff", "--quiet"])
            .run(&Cancellation::default())
            .unwrap_err();
        assert!(matches!(rejected, GitError::Failed { .. }));
    }

    /// A timeout has to kill the group rather than the leader, or a background
    /// helper outlives the command that started it. The activity file is the
    /// only evidence available once the group is gone.
    #[cfg(unix)]
    #[test]
    fn a_timed_out_command_leaves_no_process_in_its_group() {
        let fixture = Fixture::new();
        let working_directory = fixture.directory("timing-out");
        let activity = fixture.root.path().join("timeout-helper-activity");
        let sleeping_git = fixture.shim(
            "sleeping-git",
            &format!(
                "#!/bin/sh\n\
                 (while true; do printf x >> '{}'; sleep 0.01; done) 2>/dev/null &\n\
                 echo ready >&2\n\
                 wait\n",
                activity.display()
            ),
        );

        let started = Instant::now();
        let error = GitCommand::new(sleeping_git, working_directory, GitAccess::LocalRead)
            .arg("status")
            .timeout(Duration::from_millis(300))
            .run(&Cancellation::default())
            .unwrap_err();

        assert!(matches!(error, GitError::TimedOut { .. }));
        assert!(started.elapsed() < Duration::from_secs(10));
        let activity_at_timeout = std::fs::read(&activity).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            std::fs::read(&activity).unwrap(),
            activity_at_timeout,
            "a helper survived the timeout"
        );
    }
}
