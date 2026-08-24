//! Structured diagnostics and the redaction boundary.
//!
//! Two things live here because they are the same problem seen from either end.
//! A run that fails at two in the morning has to be reconstructable from what
//! Harkness wrote down, and everything Harkness writes down is durable — so the
//! more inspectable a run becomes, the more places a credential can come to
//! rest. This module owns the instrumentation *and* the rules that decide what
//! never reaches it.
//!
//! # Correlation: every span names its run
//!
//! The convention is deliberately blunt: **every span the runtime opens carries
//! `run_id` as a field of its own**, rather than relying on a parent span to
//! supply it. Work crosses threads — the coordinator's worker, the scheduler's
//! admission, the executor's supervision, a tool body on a thread of its own —
//! and a span opened on a thread that never entered the run's span would
//! otherwise lose the one field that makes the log searchable. Filtering the
//! diagnostic log to a single run is therefore a substring match on its
//! identifier and nothing more.
//!
//! The field names are fixed, snake_case, and machine-parseable:
//!
//! | Field | On | Value |
//! | --- | --- | --- |
//! | `run_id` | every runtime span | the run's UUID |
//! | `step_id` | step and tool-call spans | the step's UUID |
//! | `tool_call_id` | tool-call spans | the call's UUID |
//! | `tool_id` | tool-call spans | the resolved tool identifier |
//! | `tool_version` | tool-call spans | the exact version that ran |
//! | `approval_id` | approval spans | the durable request's UUID |
//!
//! Use [`run_span`], [`step_span`], [`tool_call_span`] and [`approval_span`]
//! rather than writing the macros out, so a field cannot be spelled two ways.
//!
//! Events inside a span carry the outcome as fields rather than as prose —
//! `decision = "ask"`, `state = "interrupted"`, `verdict = "granted"` — because
//! a log a machine can filter is the only kind anybody greps at two in the
//! morning.
//!
//! # Hot-path discipline
//!
//! Span creation is O(1) per tool call. Nothing here opens a span inside a
//! per-line output loop or inside the 20 ms supervision poll, because the tool
//! runtime's budget is under 10 ms per call excluding the tool's own work and a
//! span per line of output would spend all of it. A poll loop that has something
//! to say says it once, when the state changes.
//!
//! # The log
//!
//! [`init`] installs a JSON-lines subscriber writing to `<data_dir>/logs/`,
//! bounded at [`MAX_LOG_FILES`] files of [`MAX_LOG_FILE_BYTES`] each — 20 MiB
//! total, whatever happens. The directory is created lazily and privately
//! (`0700`, files `0600`), the level comes from `HARKNESS_LOG` and defaults to
//! `info`, and `HARKNESS_LOG_STDERR` or a front end's `--verbose` mirrors the
//! same lines to standard error. Initializing twice changes nothing, and a
//! directory that cannot be written degrades to the stderr mirror instead of
//! failing whatever asked for a log.
//!
//! # The redaction boundary
//!
//! [`StandardRedactor`] is the [`Redactor`](crate::store::Redactor) a store
//! installs by default, so redaction happens *once*, before persistence, rather
//! than at each of the many places a value is later shown. The rules are in
//! [`RedactionRule`]; what they cover, channel by channel, is this:
//!
//! | Channel | Rules applied | Notes |
//! | --- | --- | --- |
//! | run event payloads | all | string values only; object keys are field names |
//! | approval summary and decision reason | all | |
//! | tool result payloads (`tool_calls.output_json`) | all | |
//! | failure messages (run, step, tool call) | all | |
//! | task titles | all | |
//! | artifact label and media type | all | |
//! | artifact content | all but [`PrivateKeyBlock`](RedactionRule::PrivateKeyBlock) | byte-wise, a line at a time |
//! | the diagnostic log | all | applied to the formatted line, not the call site |
//! | agent observations | all | the same redactor, at the agent seam |
//! | **tool input (`tool_calls.input_json`)** | **none** | see below |
//! | **`workspace_snapshots.payload_json`** | **none** | digest-bound; see `store::redaction` |
//!
//! **A tool's input is deliberately not redacted, and that is a boundary rather
//! than an oversight.** The executor runs the bytes it reads back out of that
//! column, so rewriting them would not protect a secret, it would run a
//! different command than the one that was approved — and the approval's own
//! hash is taken over the input, so the record would no longer match the
//! decision made about it. A secret belongs in a declared environment variable,
//! which [`secret`] covers everywhere it can subsequently appear, and never in a
//! tool argument. `docs/observability.md` states this where tool authors read.
//!
//! # What this does not attempt
//!
//! No entropy scoring: a false positive silently rewrites the audit trail a user
//! is relying on, and nothing downstream can detect or undo it. No network, no
//! metrics export, no telemetry — the log is a local file. And the pre-existing
//! CLI Git error path still prints raw `git` stderr to the invoking terminal;
//! what this module guarantees is that such text is redacted when it is
//! *persisted into a run record*.

mod log;
mod redactor;
mod rules;
pub mod secret;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::Span;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::approval::ApprovalId;
use crate::domain::{RunId, StepId, ToolCallId};
use crate::tool::ToolIdentity;

pub use log::{
    LOG_FILE_NAME, LOGS_DIRECTORY, MAX_LOG_FILE_BYTES, MAX_LOG_FILES, MAX_LOG_LINE_BYTES,
    log_directory, log_file,
};
pub use redactor::{MAX_FILTERED_LINE_BYTES, StandardRedactor, stream_rules};
pub use rules::RedactionRule;
pub use secret::{
    Declared, MIN_DECLARED_SECRET_BYTES, SecretRegistry, declare_environment_secrets,
    is_sensitive_environment_name,
};

/// Environment variable holding `tracing` filter directives.
pub const LOG_FILTER_ENV: &str = "HARKNESS_LOG";

/// Environment variable that mirrors the log to standard error when set.
///
/// Any value other than empty or `0` counts, because the shapes people actually
/// type are `1`, `true` and `yes`, and refusing all but one of them would make
/// the switch look broken.
pub const LOG_STDERR_ENV: &str = "HARKNESS_LOG_STDERR";

/// Filter directives used when `HARKNESS_LOG` says nothing.
pub const DEFAULT_FILTER: &str = "info";

/// How a front end wants the diagnostic log set up.
#[derive(Clone, Debug, Default)]
pub struct Options {
    mirror_to_stderr: bool,
    default_filter: Option<String>,
}

impl Options {
    /// Mirrors every recorded line to standard error as well as to the file.
    ///
    /// The mirror is the same JSON-lines rendering rather than a friendlier one,
    /// so what a `--verbose` run shows is exactly what was written down — and so
    /// `harkness --json`'s promise that standard error carries one JSON object
    /// per line still holds.
    #[must_use]
    pub const fn mirror_to_stderr(mut self, mirror: bool) -> Self {
        self.mirror_to_stderr = mirror;
        self
    }

    /// Filter directives to use when [`LOG_FILTER_ENV`] is unset.
    #[must_use]
    pub fn with_default_filter(mut self, directives: impl Into<String>) -> Self {
        self.default_filter = Some(directives.into());
        self
    }
}

/// What [`init`] managed to arrange.
///
/// Returned rather than swallowed so a front end can say what happened when a
/// user asks where their logs are. Nothing here is an error a caller must
/// handle: every variant is a working process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InitOutcome {
    /// Lines will be written to `path`, and mirrored to stderr if asked.
    ///
    /// The file is created by the first line rather than by this call, so a
    /// command that records nothing leaves a data directory it only read
    /// exactly as it found it. Should the directory turn out to be unwritable
    /// when that first line arrives, the log falls back to standard error for
    /// the life of the process and nothing else changes.
    Logging {
        /// The file lines are destined for.
        path: PathBuf,
        /// Whether the same lines also reach standard error.
        mirrored: bool,
    },
    /// No data directory resolved; lines reach standard error and nowhere else.
    StderrOnly {
        /// Why the file is unavailable, for a front end to relay.
        reason: String,
    },
    /// A subscriber was already installed. This call changed nothing.
    AlreadyInitialized,
}

impl InitOutcome {
    /// One line a front end can print when a user asks where the log went.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Logging { path, mirrored } => {
                let mirror = if *mirrored {
                    ", mirrored to stderr"
                } else {
                    ""
                };
                format!(
                    "diagnostics are being written to {}{mirror}",
                    path.display()
                )
            }
            Self::StderrOnly { reason } => {
                format!("diagnostics are going to stderr only: {reason}")
            }
            Self::AlreadyInitialized => {
                "diagnostics were already initialized by this process".to_owned()
            }
        }
    }
}

/// Installs the diagnostic subscriber for this process, once.
///
/// `data_dir` is the Harkness data directory the log belongs under; `None` means
/// none resolved, which is a reason to log to stderr rather than a reason to
/// fail. Calling this twice is safe and changes nothing — the second call
/// reports [`InitOutcome::AlreadyInitialized`] and leaves the first
/// installation in place, which is what makes it safe for a library entry point
/// as well as a `main`.
///
/// This never returns an error and never panics. A log that could take down the
/// work it describes would be worse than no log at all.
pub fn init(data_dir: Option<&Path>, options: Options) -> InitOutcome {
    static STATE: OnceLock<InitOutcome> = OnceLock::new();
    let mut installed = false;
    let outcome = STATE.get_or_init(|| {
        installed = true;
        install(data_dir, &options)
    });
    if installed {
        outcome.clone()
    } else {
        InitOutcome::AlreadyInitialized
    }
}

/// Whether the environment asks for the stderr mirror.
fn stderr_requested() -> bool {
    std::env::var_os(LOG_STDERR_ENV)
        .map(|value| !value.is_empty() && value != "0")
        .unwrap_or(false)
}

fn filter(options: &Options) -> EnvFilter {
    let configured = std::env::var(LOG_FILTER_ENV)
        .ok()
        .filter(|directives| !directives.trim().is_empty())
        .or_else(|| options.default_filter.clone())
        .unwrap_or_else(|| DEFAULT_FILTER.to_owned());
    // A directive nobody can parse is a typo in an environment variable, not a
    // reason to run without diagnostics — so the default takes over and the
    // process keeps going.
    EnvFilter::try_new(&configured).unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER))
}

fn install(data_dir: Option<&Path>, options: &Options) -> InitOutcome {
    let redactor = StandardRedactor::standard();
    let mirrored = options.mirror_to_stderr || stderr_requested();

    let (writer, failure) = match data_dir {
        Some(data_dir) => (
            Some((log::log_file(data_dir), log::RotatingLog::new(data_dir))),
            None,
        ),
        None => (None, Some("no Harkness data directory resolved".to_owned())),
    };

    // The stderr layer is installed when it was asked for, and also when the
    // file is unavailable: degrading to a visible log is the point of the
    // fallback, and a silent one would leave a user with no way to tell that
    // anything went wrong with their logging.
    let stderr_layer = (mirrored || failure.is_some()).then(|| {
        tracing_subscriber::fmt::layer()
            .json()
            .with_ansi(false)
            .with_timer(Utc)
            .with_writer(log::StderrWriter::new(redactor.clone()))
    });
    let (path, file_layer) = match writer {
        Some((path, opened)) => (
            Some(path),
            Some(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_ansi(false)
                    .with_timer(Utc)
                    .with_writer(log::LogWriter::new(opened, redactor)),
            ),
        ),
        None => (None, None),
    };

    let installed = tracing_subscriber::registry()
        .with(filter(options))
        .with(file_layer)
        .with(stderr_layer)
        .try_init();
    if installed.is_err() {
        // Something outside Harkness already owns the global subscriber — a host
        // application embedding the runtime, most likely. Its choices win.
        return InitOutcome::AlreadyInitialized;
    }
    // `path` is `Some` exactly when a data directory resolved, which is exactly
    // when `failure` is `None`, so the two are one decision rather than four
    // combinations a reader has to rule out.
    match path {
        Some(path) => InitOutcome::Logging { path, mirrored },
        None => InitOutcome::StderrOnly {
            reason: failure.unwrap_or_else(|| "no diagnostic file was requested".to_owned()),
        },
    }
}

/// RFC 3339 in UTC, from the same clock every persisted timestamp uses.
///
/// `tracing-subscriber` can format time itself only with a feature that pulls a
/// second time library into a workspace that already has one. Spelling it out
/// here keeps one clock and one rendering, so a log line and the run event it
/// describes sort against each other.
#[derive(Clone, Copy, Debug, Default)]
struct Utc;

impl FormatTime for Utc {
    fn format_time(&self, writer: &mut Writer<'_>) -> fmt::Result {
        let now = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .map_err(|_| fmt::Error)?;
        writer.write_str(&now)
    }
}

/// The span every piece of one run's work belongs to.
#[must_use]
pub fn run_span(run: RunId) -> Span {
    tracing::info_span!("run", run_id = %run)
}

/// The span one planned step of a run belongs to.
#[must_use]
pub fn step_span(run: RunId, step: StepId) -> Span {
    tracing::info_span!("step", run_id = %run, step_id = %step)
}

/// The span one recorded tool call belongs to.
///
/// The identity is the *resolved* one — the version that actually ran, not the
/// one the request named — so a log line can be compared against
/// `tool_calls.tool_version` without a second lookup.
#[must_use]
pub fn tool_call_span(run: RunId, step: StepId, call: ToolCallId, tool: &ToolIdentity) -> Span {
    tracing::info_span!(
        "tool_call",
        run_id = %run,
        step_id = %step,
        tool_call_id = %call,
        tool_id = %tool.id,
        tool_version = %tool.version,
    )
}

/// The span a call spends parked on a human decision.
///
/// Deliberately a span rather than two events: how long a run waited for a
/// person is one of the few durations worth reading straight off a log, and a
/// pair of events makes a reader compute it.
#[must_use]
pub fn approval_span(run: RunId, call: ToolCallId, approval: ApprovalId) -> Span {
    tracing::info_span!(
        "approval",
        run_id = %run,
        tool_call_id = %call,
        approval_id = %approval,
    )
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_FILTER, InitOutcome, Options, RedactionRule, stream_rules};

    #[test]
    fn an_outcome_describes_itself_without_naming_anything_secret() {
        let logging = InitOutcome::Logging {
            path: std::path::PathBuf::from("/data/logs/harkness.log"),
            mirrored: true,
        };
        assert!(logging.describe().contains("/data/logs/harkness.log"));
        assert!(logging.describe().contains("mirrored"));

        let degraded = InitOutcome::StderrOnly {
            reason: "read-only filesystem".to_owned(),
        };
        assert!(degraded.describe().contains("read-only filesystem"));
        assert_eq!(
            InitOutcome::AlreadyInitialized.describe(),
            "diagnostics were already initialized by this process"
        );
    }

    #[test]
    fn options_are_additive_and_default_to_the_quiet_arrangement() {
        let options = Options::default();
        assert!(!options.mirror_to_stderr);
        assert!(options.default_filter.is_none());

        let configured = Options::default()
            .mirror_to_stderr(true)
            .with_default_filter("harkness_runtime=debug");
        assert!(configured.mirror_to_stderr);
        assert_eq!(
            configured.default_filter.as_deref(),
            Some("harkness_runtime=debug")
        );
        assert_eq!(DEFAULT_FILTER, "info");
    }

    #[test]
    fn the_documented_stream_gap_is_exactly_one_rule() {
        let covered = stream_rules();
        assert!(!covered.contains(&RedactionRule::PrivateKeyBlock));
        assert!(covered.contains(&RedactionRule::DeclaredSecret));
        assert!(covered.contains(&RedactionRule::UrlUserinfo));
        assert!(covered.contains(&RedactionRule::Authorization));
        assert!(covered.contains(&RedactionRule::CredentialParameter));
        assert!(covered.contains(&RedactionRule::CredentialToken));
    }
}
