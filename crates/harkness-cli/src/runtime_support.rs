//! Shared plumbing for the four runtime command families.
//!
//! The command bodies in [`run_commands`](crate::run_commands),
//! [`approval_commands`](crate::approval_commands),
//! [`tool_commands`](crate::tool_commands) and
//! [`agent_commands`](crate::agent_commands) are projections over
//! `harkness-runtime`. This module owns what all four share: how a coordinator
//! is opened, how the two runtime error namespaces are classified for the
//! published exit-code contract, the wire projections a run is rendered
//! through, and the supervision loop that answers approval requests.
//!
//! # Reads must not create a run store
//!
//! `Store::open` creates `runtime.db`, its WAL sidecars and every migration as
//! a side effect. `run list`, `run show` and `approvals list` only report, so
//! they go through [`open_existing_runtime`], which answers `None` when no
//! store exists — no database means nothing has been recorded, which is the
//! empty projection. Only the commands that record something use
//! [`open_runtime`].
//!
//! # Every read still sweeps
//!
//! Both constructors build a [`RunCoordinator`], whose construction takes this
//! process's lease and sweeps runs abandoned by a dead one. That is deliberate
//! even for a read: `interrupted` is written by that sweep and by nothing else,
//! so a listing taken without it would report a crashed process's runs as still
//! `running` for ever.

use std::collections::HashSet;
use std::io::{self, BufRead};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as URL_BASE64};
use harkness_git::Cancellation;
use harkness_runtime::agent::WorkspaceRef;
use harkness_runtime::approval::{
    ApprovalDecision, ApprovalId, ApprovalRequest, ApprovalScope, ApprovalState, DecidedVia,
};
use harkness_runtime::coordinator::{
    EventDelivery, EventReceiver, RunCoordinator, RunSnapshot, RuntimeError, TryReceiveError,
};
use harkness_runtime::domain::{
    Approval, ExecutionState, Failure, Run, RunId, Step, Task, ToolCall, ToolCallState,
};
use harkness_runtime::policy::PolicyEngine;
use harkness_runtime::store::{
    Artifact, MAX_RUN_PAGE_LIMIT, PassThrough, RunCursor, Store, StoreError, StoredEvent,
};
use harkness_runtime::tool::{RegistryError, ToolRegistry};
use harkness_runtime::tools::{register_mutating_tools, register_read_only_tools};
use harkness_runtime::trust::{TrustState, WorkspaceTrust};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::{
    CliError, EXIT_CANCELLED, EXIT_CONFLICT, EXIT_NOT_FOUND, EXIT_OPERATION_FAILED, EXIT_REFUSED,
    EXIT_USAGE, emit_progress, single_line, wire_path,
};

/// How often the supervision loop re-reads durable run state.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How long the supervision loop keeps draining events after the run is
/// terminal.
///
/// The worker closes its delivery *after* it has recorded the terminal state,
/// so a loop that stopped the instant it saw one would drop the last few
/// entries of the very timeline it was streaming. It is a grace rather than a
/// wait because the run has already finished: nothing is pending on it.
const DRAIN_GRACE: Duration = Duration::from_millis(500);

/// The line protocol `--interactive` reads from standard input.
const INTERACTIVE_HELP: &str = "answer approve, deny, or show-input";

/// Builds the full production registry: the five read-only tools and the four
/// mutating ones.
///
/// Both sets, always. A `tool list` that published only half the contract would
/// be a second dialect of what a run can actually call.
fn production_registry() -> Result<ToolRegistry, CliError> {
    let mut registry = ToolRegistry::new();
    register_read_only_tools(&mut registry).map_err(registry_failure)?;
    register_mutating_tools(&mut registry).map_err(registry_failure)?;
    Ok(registry)
}

/// The tool registry `tool list` and `tool describe` publish.
///
/// Needs no data directory and opens no store, so describing the contract has
/// no side effect at all.
pub(crate) fn contract_registry() -> Result<ToolRegistry, CliError> {
    production_registry()
}

fn registry_failure(error: RegistryError) -> CliError {
    CliError::RuntimeOutcome {
        kind: tool_kind(error.kind()),
        code: tool_exit_code(error.kind()),
        message: error.to_string(),
        details: json!({}),
    }
}

/// Opens the run store, creating it when absent, and takes this process's lease.
pub(crate) fn open_runtime(data_dir: &Path) -> Result<RunCoordinator, CliError> {
    let store = Arc::new(Store::open(data_dir).map_err(CliError::Store)?);
    coordinator_for(store)
}

/// Opens an existing run store without bringing one into existence.
pub(crate) fn open_existing_runtime(data_dir: &Path) -> Result<Option<RunCoordinator>, CliError> {
    let Some(store) = Store::open_existing(data_dir).map_err(CliError::Store)? else {
        return Ok(None);
    };
    coordinator_for(Arc::new(store)).map(Some)
}

fn coordinator_for(store: Arc<Store>) -> Result<RunCoordinator, CliError> {
    let registry = Arc::new(production_registry()?);
    // One coordinator, and therefore one scheduler, for the process — the
    // reason `check_coordinator` exists rather than a per-call constructor. A
    // CLI invocation drives at most one run, so building it here is also
    // building it once.
    let policy = PolicyEngine::load(store.data_dir(), store.data_dir());
    RunCoordinator::new(store, registry, policy).map_err(CliError::Runtime)
}

/// Encodes an opaque run continuation exactly as `git log`'s cursor is encoded.
///
/// [`RunCursor`] is serde-opaque and versioned by the store, so the CLI wraps
/// its JSON rather than inventing coordinates of its own. A timeline continues
/// by [`EventSeq`](harkness_runtime::store::EventSeq) instead — a plain number,
/// because the store documents that position as the pagination key itself.
pub(crate) fn encode_run_cursor(cursor: &RunCursor) -> Result<String, CliError> {
    serde_json::to_vec(cursor)
        .map(|bytes| URL_BASE64.encode(bytes))
        .map_err(|error| CliError::WireProjection(error.to_string()))
}

/// Decodes a token produced by [`encode_run_cursor`].
pub(crate) fn decode_run_cursor(token: &str) -> Result<RunCursor, CliError> {
    let bytes = URL_BASE64.decode(token).map_err(|_| {
        CliError::Usage("--cursor is not a valid Harkness run cursor token".to_owned())
    })?;
    serde_json::from_slice(&bytes).map_err(|_| {
        CliError::Usage("--cursor is not a valid Harkness run cursor token".to_owned())
    })
}

/// Parses `--limit` for a run page against the store's own published bound.
///
/// A limit past the cap is refused here, as a usage error, rather than being
/// clamped or handed to the store: clamping silently returns a different page
/// than the one that was asked for.
pub(crate) fn parse_run_limit(value: &str) -> Result<usize, String> {
    parse_limit(value, MAX_RUN_PAGE_LIMIT)
}

/// Parses `--limit` for a timeline page against the store's own published bound.
pub(crate) fn parse_event_limit(value: &str) -> Result<usize, String> {
    parse_limit(value, harkness_runtime::store::MAX_EVENT_PAGE_LIMIT)
}

fn parse_limit(value: &str, maximum: usize) -> Result<usize, String> {
    let limit = value
        .parse::<usize>()
        .map_err(|_| format!("--limit must be a whole number between 1 and {maximum}"))?;
    if limit == 0 || limit > maximum {
        return Err(format!("--limit must be between 1 and {maximum}"));
    }
    Ok(limit)
}

/// RFC 3339 UTC, the one spelling every runtime record is stored with.
pub(crate) fn timestamp(at: OffsetDateTime) -> String {
    at.format(&Rfc3339)
        .expect("a UTC OffsetDateTime is always RFC 3339 representable")
}

// ---------------------------------------------------------------------------
// Wire projections
// ---------------------------------------------------------------------------

/// The CLI's own projection of a run record.
///
/// Deliberately hand-written rather than `RunWireRef`: that type is the durable
/// storage form and carries `schema_version`, which is a fact about the
/// database rather than about the run. `retry_of` and
/// `workspace_may_be_modified` are always present here — the wire form omits
/// them when absent, and a caller reading a retry's provenance should not have
/// to tell "not a retry" from "an older producer".
pub(crate) fn run_value(run: &Run) -> Value {
    json!({
        "id": run.id(),
        "task_id": run.task_id(),
        "state": run.state().as_str(),
        "revision": run.revision(),
        "created_at": timestamp(run.created_at()),
        "updated_at": timestamp(run.updated_at()),
        "started_at": run.started_at().map(timestamp),
        "finished_at": run.finished_at().map(timestamp),
        "failure": run.failure().map(failure_value),
        "approvals": run.approvals().iter().map(audit_value).collect::<Vec<_>>(),
        "retry_of": run.retry_of(),
        "workspace_may_be_modified": run.workspace_may_be_modified(),
    })
}

pub(crate) fn task_value(task: &Task) -> Value {
    let (root, path_is_lossy) = wire_path(task.workspace_root());
    json!({
        "id": task.id(),
        "title": task.title(),
        "workspace_root": root,
        "path_is_lossy": path_is_lossy,
        "project_id": task.project_id().map(|id| id.to_string()),
        "created_at": timestamp(task.created_at()),
    })
}

pub(crate) fn step_value(step: &Step) -> Value {
    json!({
        "id": step.id(),
        "run_id": step.run_id(),
        "ordinal": step.ordinal(),
        "title": step.title(),
        "state": step.state().as_str(),
        "created_at": timestamp(step.created_at()),
        "updated_at": timestamp(step.updated_at()),
        "started_at": step.started_at().map(timestamp),
        "finished_at": step.finished_at().map(timestamp),
        "failure": step.failure().map(failure_value),
    })
}

pub(crate) fn tool_call_value(call: &ToolCall) -> Value {
    json!({
        "id": call.id(),
        "run_id": call.run_id(),
        "step_id": call.step_id(),
        "tool_id": call.tool_id(),
        "tool_version": call.tool_version(),
        "state": call.state().as_str(),
        // Already redacted: every caller value passes through the store's
        // `Redactor` before it becomes durable, so this projection reads what
        // was persisted rather than redacting a second time.
        "input": call.input(),
        "output": call.output(),
        "failure": call.failure().map(failure_value),
        "policy_decision": call.policy_decision(),
        "approvals": call.approvals().iter().map(audit_value).collect::<Vec<_>>(),
        "created_at": timestamp(call.created_at()),
        "updated_at": timestamp(call.updated_at()),
        "started_at": call.started_at().map(timestamp),
        "finished_at": call.finished_at().map(timestamp),
    })
}

pub(crate) fn artifact_value(artifact: &Artifact) -> Value {
    json!({
        "id": artifact.id(),
        "run_id": artifact.run_id(),
        "step_id": artifact.step_id(),
        "tool_call_id": artifact.tool_call_id(),
        "name": artifact.name(),
        "media_type": artifact.media_type(),
        "byte_size": artifact.byte_size(),
        "sha256": artifact.sha256(),
        "created_at": timestamp(artifact.created_at()),
        // Probed at read time rather than trusted from the row: content can go
        // missing or be resized after finalization, and a metadata read
        // refines the answer without writing.
        "availability": artifact.availability().as_str(),
    })
}

pub(crate) fn approval_value(request: &ApprovalRequest, input: Option<&Value>) -> Value {
    let decision = request.decision().map(|decision| {
        json!({
            "verdict": decision.verdict().as_str(),
            "scope": decision.scope().map(ApprovalScope::as_str),
            "decided_at": timestamp(decision.decided_at()),
            "decided_via": decision.decided_via().as_str(),
            "reason": decision.reason(),
        })
    });
    let (canonical_root, root_is_lossy) = wire_path(request.workspace().canonical_root());
    json!({
        "id": request.id(),
        "run_id": request.run_id(),
        "tool_call_id": request.tool_call_id(),
        "tool_id": request.tool().id.as_str(),
        "tool_version": request.tool().version.to_string(),
        "capabilities": request
            .capabilities()
            .iter()
            .map(|capability| capability.as_str().to_owned())
            .collect::<Vec<_>>(),
        "input_hash": request.input_hash().to_hex(),
        "input_summary": request.input_summary(),
        // The recorded call's own input, already redacted at the persistence
        // boundary. Absent when the call could not be read, never silently
        // replaced by an empty object.
        "input": input,
        "workspace": {
            "project_id": request.workspace().project_id().map(|id| id.to_string()),
            "canonical_root": canonical_root,
            "path_is_lossy": root_is_lossy,
        },
        "risk": request.risk().as_str(),
        "requested_scope": request.requested_scope().as_str(),
        "effective_scope": request.effective_scope().as_str(),
        "was_downgraded": request.was_downgraded(),
        "state": request.state().as_str(),
        "created_at": timestamp(request.created_at()),
        "expires_at": request.expires_at().map(timestamp),
        "resolved_at": request.resolved_at().map(timestamp),
        "decision": decision,
    })
}

pub(crate) fn event_value(stored: &StoredEvent) -> Value {
    json!({
        "run_id": stored.run_id,
        "seq": stored.seq.get(),
        "kind": stored.event.kind(),
        "at": timestamp(stored.event.at()),
        "step_id": stored.event.step_id(),
        "tool_call_id": stored.event.tool_call_id(),
        "artifact_id": stored.event.artifact_id(),
        "payload": stored.event.payload(),
    })
}

fn failure_value(failure: &Failure) -> Value {
    json!({ "kind": failure.kind(), "message": failure.message() })
}

fn audit_value(approval: &Approval) -> Value {
    json!({
        "decided_by": approval.decided_by(),
        "decision": approval.decision(),
        "decided_at": timestamp(approval.decided_at()),
    })
}

/// One tab-separated timeline row, with every untrusted field collapsed.
pub(crate) fn event_line(stored: &StoredEvent) -> String {
    format!(
        "{}\t{}\t{}",
        stored.seq.get(),
        stored.event.kind(),
        timestamp(stored.event.at())
    )
}

/// One tab-separated run row.
pub(crate) fn run_line(run: &Run, title: Option<&str>) -> String {
    format!(
        "{}\t{}\t{}\t{}",
        run.id(),
        run.state().as_str(),
        timestamp(run.created_at()),
        single_line(title.unwrap_or(""))
    )
}

// ---------------------------------------------------------------------------
// Error classification
// ---------------------------------------------------------------------------

/// Every kind the coordinator namespace publishes, with its exit code.
///
/// The table is `RuntimeError::KINDS` followed by `StoreError::KINDS`, in that
/// order, because `RuntimeError::Store` delegates its discriminant rather than
/// spelling one of its own —
/// `runtime_error_kinds_are_classified_for_the_exit_code_contract` is what
/// refuses a kind added upstream without an exit code stated here.
pub(crate) const RUNTIME_KIND_EXIT_CODES: &[(&str, u8)] = &[
    // A run that cannot be attributed to a catalogued workspace is refused
    // rather than scheduled against a path: the identity is what every grant
    // and every trust decision is bound to.
    ("workspace_identity_required", EXIT_REFUSED),
    ("workspace_mismatch", EXIT_OPERATION_FAILED),
    ("workspace_unavailable", EXIT_NOT_FOUND),
    ("worker_spawn_failed", EXIT_OPERATION_FAILED),
    // Both "not active" kinds are refusals rather than not-founds: the record
    // exists, and what is missing is this process's claim on it.
    ("run_not_active", EXIT_REFUSED),
    ("approval_not_active", EXIT_REFUSED),
    ("lease_unavailable", EXIT_CONFLICT),
    ("run_still_active", EXIT_REFUSED),
    ("run_not_retryable", EXIT_REFUSED),
    ("data_directory_unavailable", EXIT_OPERATION_FAILED),
    ("store_open", EXIT_OPERATION_FAILED),
    ("store_busy", EXIT_CONFLICT),
    ("incomplete_checkpoint", EXIT_OPERATION_FAILED),
    ("schema_too_new", EXIT_OPERATION_FAILED),
    ("migration_failed", EXIT_OPERATION_FAILED),
    ("not_found", EXIT_NOT_FOUND),
    ("already_exists", EXIT_CONFLICT),
    ("missing_parent", EXIT_OPERATION_FAILED),
    ("duplicate_step_ordinal", EXIT_CONFLICT),
    ("payload_too_large", EXIT_REFUSED),
    ("artifact_io", EXIT_OPERATION_FAILED),
    ("forbidden_artifact_path", EXIT_REFUSED),
    ("non_utf8_path", EXIT_REFUSED),
    ("invalid_transition", EXIT_CONFLICT),
    // Deciding an already-decided approval lands here, which is why it is a
    // refusal: the question had an answer before this one arrived.
    ("approval_refused", EXIT_REFUSED),
    ("approval_binding_mismatch", EXIT_REFUSED),
    ("invalid_record", EXIT_OPERATION_FAILED),
    ("column_encoding", EXIT_OPERATION_FAILED),
    ("invalid_page_limit", EXIT_REFUSED),
    ("query_failed", EXIT_OPERATION_FAILED),
];

/// Every kind one tool invocation publishes, with its exit code.
///
/// The table is `RegistryError::KINDS` followed by `ToolError::KINDS`, which is
/// exactly `InvocationError::kinds()`. `not_found` appears in this namespace
/// and in the coordinator's, deliberately mapped to the same code in both: a
/// caller reading one discriminant must not have to know which table it came
/// from to learn what the process exited with.
pub(crate) const TOOL_KIND_EXIT_CODES: &[(&str, u8)] = &[
    ("invalid_tool_id", EXIT_USAGE),
    ("invalid_tool_version", EXIT_USAGE),
    ("invalid_capability", EXIT_OPERATION_FAILED),
    ("invalid_metadata", EXIT_OPERATION_FAILED),
    ("invalid_schema", EXIT_OPERATION_FAILED),
    ("duplicate_registration", EXIT_OPERATION_FAILED),
    ("unknown_tool", EXIT_NOT_FOUND),
    ("unknown_tool_version", EXIT_NOT_FOUND),
    ("invalid_input", EXIT_USAGE),
    ("invalid_output", EXIT_OPERATION_FAILED),
    ("execution_failed", EXIT_OPERATION_FAILED),
    ("process_failed", EXIT_OPERATION_FAILED),
    ("timed_out", EXIT_OPERATION_FAILED),
    ("cancelled", EXIT_CANCELLED),
    ("denied", EXIT_REFUSED),
    ("forbidden_path", EXIT_REFUSED),
    ("not_found", EXIT_NOT_FOUND),
    ("output_budget_exhausted", EXIT_OPERATION_FAILED),
    ("outside_allowed_roots", EXIT_REFUSED),
    ("symlink_escapes", EXIT_REFUSED),
    ("root_unavailable", EXIT_NOT_FOUND),
    ("candidate_unavailable", EXIT_NOT_FOUND),
    ("stale_patch", EXIT_CONFLICT),
    ("patch_conflict", EXIT_CONFLICT),
    ("interrupted", EXIT_OPERATION_FAILED),
    ("tool_panicked", EXIT_OPERATION_FAILED),
];

/// The exit code a coordinator or store failure reports.
pub(crate) fn runtime_exit_code(error: &RuntimeError) -> u8 {
    kind_exit_code(RUNTIME_KIND_EXIT_CODES, error.kind())
}

/// The exit code a store failure reports, for the paths that carry one directly.
pub(crate) fn store_exit_code(error: &StoreError) -> u8 {
    kind_exit_code(RUNTIME_KIND_EXIT_CODES, error.kind())
}

/// The exit code an invocation kind reports.
pub(crate) fn tool_exit_code(kind: &str) -> u8 {
    kind_exit_code(TOOL_KIND_EXIT_CODES, kind)
}

/// The `'static` spelling of an invocation kind, for a `CliError` discriminant.
///
/// A recorded failure carries its kind as stored text. Looking it up in the
/// published table rather than leaking the row's own bytes is what keeps the
/// error envelope's `kind` inside the contract `harkness contract` announces.
pub(crate) fn tool_kind(kind: &str) -> &'static str {
    TOOL_KIND_EXIT_CODES
        .iter()
        .find(|(published, _)| *published == kind)
        .map_or("tool_call_failed", |(published, _)| *published)
}

fn kind_exit_code(table: &[(&str, u8)], kind: &str) -> u8 {
    table
        .iter()
        .find(|(published, _)| *published == kind)
        .map_or(EXIT_OPERATION_FAILED, |(_, code)| *code)
}

/// Details for a coordinator failure, naming the record it is about.
pub(crate) fn runtime_details(error: &RuntimeError) -> Value {
    match error {
        RuntimeError::RunNotActive { run } => json!({ "run_id": run.to_string() }),
        RuntimeError::RunStillActive { run } => json!({ "run_id": run.to_string() }),
        RuntimeError::RunNotRetryable { run, state } => {
            json!({ "run_id": run.to_string(), "state": state.as_str() })
        }
        RuntimeError::ApprovalNotActive { approval } => {
            json!({ "approval_id": approval.to_string() })
        }
        RuntimeError::Store(error) => store_details(error),
        _ => json!({}),
    }
}

/// Details for a store failure.
///
/// `NotFound` is the one that matters to a script: the runtime spells every
/// missing record `not_found`, so the record kind and the identifier are what
/// tell "no such run" from "no such approval".
pub(crate) fn store_details(error: &StoreError) -> Value {
    match error {
        StoreError::NotFound { record, id } => json!({ "record": record, "id": id }),
        _ => json!({}),
    }
}

// ---------------------------------------------------------------------------
// Run supervision
// ---------------------------------------------------------------------------

/// What a supervised run is allowed to do about an approval request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ApprovalMode {
    /// Deny every request nobody has already granted.
    ///
    /// The mandate is that absence of an answer is refusal, so this is the
    /// default and there is no flag that turns it into consent.
    Noninteractive,
    /// Ask on standard error and read the answer from standard input.
    Interactive,
}

/// What supervising one run observed.
pub(crate) struct RunOutcome {
    /// Durable view of the run once it reached a terminal state.
    pub(crate) snapshot: RunSnapshot,
    /// Whether this process denied a request because it could not ask anybody.
    pub(crate) denied_noninteractively: bool,
}

/// Streams one run's events, answers its approvals, and waits for it to finish.
///
/// The loop is poll-based rather than driven purely by the subscription, and
/// deliberately so. A subscriber that falls behind is *disconnected* with a
/// `Lagged` marker — which is the right behavior for a live timeline and the
/// wrong one for a supervisor, since an interactive prompt blocks on a human
/// for as long as it takes. The durable run record is the authority on when the
/// run finished; the subscription is only how the timeline is streamed.
pub(crate) fn supervise_run(
    coordinator: &RunCoordinator,
    run_id: RunId,
    receiver: &EventReceiver,
    mode: ApprovalMode,
    json_output: bool,
    cancellation: &Cancellation,
) -> Result<RunOutcome, CliError> {
    let mut answered: HashSet<ApprovalId> = HashSet::new();
    let mut denied_noninteractively = false;
    let mut cancel_requested = false;
    let mut lagged = false;
    // The reader is created only for a mode that can ask. A noninteractive
    // invocation must not read standard input at all: a hook that piped a
    // document into a sibling command has no answer in it.
    let asker = match mode {
        ApprovalMode::Noninteractive => Asker::Deny,
        ApprovalMode::Interactive => Asker::Ask(AnswerReader::spawn()),
    };
    loop {
        lagged |= drain_events(receiver, json_output);
        if cancellation.is_cancelled() && !cancel_requested {
            // Asked once, then waited for, exactly as a configured check waits.
            // Returning the moment cancellation was requested hands back a run
            // whose child processes are still being torn down.
            match coordinator.cancel_run(run_id) {
                Ok(()) | Err(RuntimeError::RunNotActive { .. }) => {}
                Err(error) => return Err(CliError::Runtime(error)),
            }
            cancel_requested = true;
        }
        let run = coordinator
            .store()
            .load_run(run_id)
            .map_err(CliError::Store)?;
        if run.state().is_terminal() {
            break;
        }
        for request in coordinator
            .store()
            .run_approvals(run_id)
            .map_err(CliError::Store)?
        {
            if request.state() != ApprovalState::Pending || !answered.insert(request.id()) {
                continue;
            }
            let denied = decide(coordinator, &request, &asker, json_output, cancellation)?;
            denied_noninteractively |= denied;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    // The worker records the terminal state before it closes the delivery, so
    // the last entries of the timeline arrive after the state that ended it.
    let deadline = Instant::now() + DRAIN_GRACE;
    while !lagged && Instant::now() < deadline {
        match receiver.try_recv() {
            Ok(delivery) => lagged |= emit_delivery(&delivery, json_output),
            Err(TryReceiveError::Disconnected) => break,
            Err(TryReceiveError::Empty) => std::thread::sleep(POLL_INTERVAL),
        }
    }
    let snapshot = coordinator
        .run_snapshot(run_id)
        .map_err(CliError::Runtime)?;
    Ok(RunOutcome {
        snapshot,
        denied_noninteractively,
    })
}

/// Drains every event currently ready, returning whether the stream was lost.
fn drain_events(receiver: &EventReceiver, json_output: bool) -> bool {
    loop {
        match receiver.try_recv() {
            Ok(delivery) => {
                if emit_delivery(&delivery, json_output) {
                    return true;
                }
            }
            Err(TryReceiveError::Empty | TryReceiveError::Disconnected) => return false,
        }
    }
}

/// Emits one delivery as a progress line, returning whether the stream ended.
fn emit_delivery(delivery: &EventDelivery, json_output: bool) -> bool {
    match delivery {
        EventDelivery::Event(stored) => {
            crate::emit_event_progress(json_output, &event_line(stored), event_value(stored));
            false
        }
        // `EventDelivery` is `#[non_exhaustive]`, so a delivery this build does
        // not know is reported as a gap rather than silently ignored: a
        // consumer told nothing would read a truncated stream as a complete
        // one, which is the same mistake as swallowing a lag.
        EventDelivery::Lagged { last_available, .. } => {
            // Reported rather than swallowed. The run is unaffected — `run show`
            // reproduces the whole timeline from the log — but a consumer that
            // was told nothing would read a truncated stream as a complete one.
            emit_progress(
                json_output,
                &format!(
                    "the live timeline fell behind at sequence {last_available}; \
                     read the full log with harkness run show"
                ),
            );
            true
        }
        _ => {
            emit_progress(
                json_output,
                "the live timeline reported a delivery this build does not recognize; \
                 read the full log with harkness run show",
            );
            true
        }
    }
}

/// What this invocation can do when a run parks on a question.
///
/// The reader lives in the variant that can use it, so "interactive but with
/// nothing to read from" is not a state the code has to handle.
enum Asker {
    Deny,
    Ask(AnswerReader),
}

/// Answers one pending request, returning whether it was a noninteractive deny.
fn decide(
    coordinator: &RunCoordinator,
    request: &ApprovalRequest,
    asker: &Asker,
    json_output: bool,
    cancellation: &Cancellation,
) -> Result<bool, CliError> {
    let answer = match asker {
        Asker::Deny => Answer::Deny {
            reason: "noninteractive execution cannot answer an approval request".to_owned(),
        },
        Asker::Ask(answers) => prompt(coordinator, request, json_output, answers, cancellation),
    };
    // Cancellation is the run's to resolve. The next turn of the supervision
    // loop asks the coordinator to stop the run, which resolves this request as
    // `Cancelled` with no decision attached.
    if matches!(answer, Answer::Abandoned) {
        return Ok(false);
    }
    let now = OffsetDateTime::now_utc();
    let decision = match &answer {
        Answer::Approve => ApprovalDecision::grant(
            request.id(),
            // Never wider than the record asked for. A decision may narrow to
            // the one call in front of a human and may never widen, and the
            // stored request already carries the ceiling its risk imposes.
            request.effective_scope(),
            DecidedVia::Cli,
            now,
        )
        .because("approved on the Harkness command line"),
        Answer::Deny { reason } => {
            ApprovalDecision::deny(request.id(), DecidedVia::Cli, now).because(reason.clone())
        }
        Answer::Abandoned => unreachable!("an abandoned prompt returns before a decision is built"),
    };
    match coordinator.decide_approval(decision) {
        Ok(()) => {}
        // The run stopped between reading the request and answering it. The
        // request is resolved by whatever stopped it, and the waiter has
        // already been released, so there is nothing left to answer.
        Err(RuntimeError::ApprovalNotActive { .. }) => {}
        Err(error) => return Err(CliError::Runtime(error)),
    }
    Ok(matches!(asker, Asker::Deny))
}

enum Answer {
    Approve,
    Deny {
        reason: String,
    },
    /// Ctrl-C arrived while the question was still open.
    ///
    /// Not a denial. A run somebody stopped resolves its own approvals as
    /// `Cancelled` — with no decision attached, because no human answered — and
    /// synthesizing one here would make the audit claim a refusal that was
    /// never made.
    Abandoned,
}

/// Standard input, read on its own thread so a prompt does not block Ctrl-C.
///
/// A blocking `read_line` cannot be interrupted from the supervision loop: the
/// signal handler only sets a flag, and `signal(2)` restarts the read. Without
/// this, Ctrl-C at an approval prompt would do nothing until somebody typed an
/// answer — on the one screen where a user is most likely to press it.
///
/// One thread for the process, created on the first prompt. It is deliberately
/// never joined: it may be parked in a read that will never return, and the
/// process is exiting anyway.
struct AnswerReader {
    lines: std::sync::mpsc::Receiver<io::Result<String>>,
}

impl AnswerReader {
    fn spawn() -> Self {
        let (sender, lines) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("harkness-approval-stdin".to_owned())
            .spawn(move || {
                let stdin = io::stdin();
                loop {
                    let mut line = String::new();
                    let read = stdin.lock().read_line(&mut line);
                    let ended = !matches!(read, Ok(count) if count > 0);
                    if sender.send(read.map(|_| line)).is_err() || ended {
                        return;
                    }
                }
            })
            // A CLI that cannot spawn a reader still has to answer the
            // question, and the only safe answer is the one absence already
            // means. The receiver disconnects immediately, which is read as
            // end of input below.
            .ok();
        Self { lines }
    }

    /// Waits for one line, polling cancellation at the workspace's cadence.
    fn next_answer(&self, cancellation: &Cancellation) -> Option<io::Result<String>> {
        loop {
            if cancellation.is_cancelled() {
                return None;
            }
            match self.lines.recv_timeout(POLL_INTERVAL) {
                Ok(line) => return Some(line),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Some(Ok(String::new()));
                }
            }
        }
    }
}

/// Asks on standard error and reads one line from standard input.
///
/// The question goes to standard error — as a progress envelope under `--json`
/// — so a machine reading standard output still sees exactly one result
/// object. Closing standard input is a denial rather than a hang: absence of an
/// answer is never consent.
fn prompt(
    coordinator: &RunCoordinator,
    request: &ApprovalRequest,
    json_output: bool,
    answers: &AnswerReader,
    cancellation: &Cancellation,
) -> Answer {
    let summary = format!(
        "approval {} requested: {} {} ({} risk, scope {}) — {}",
        request.id(),
        request.tool().id.as_str(),
        request.tool().version,
        request.risk().as_str(),
        request.effective_scope().as_str(),
        single_line(request.input_summary()),
    );
    emit_progress(json_output, &summary);
    emit_progress(json_output, INTERACTIVE_HELP);
    loop {
        let line = match answers.next_answer(cancellation) {
            Some(Ok(line)) if line.is_empty() => {
                // End of input. A caller that closed the stream is not
                // answering, and the mandate says an unanswered request is
                // refused.
                return Answer::Deny {
                    reason: "standard input closed before the approval was answered".to_owned(),
                };
            }
            Some(Ok(line)) => line,
            Some(Err(error)) => {
                return Answer::Deny {
                    reason: format!("the approval answer could not be read: {error}"),
                };
            }
            None => return Answer::Abandoned,
        };
        match line.trim() {
            "approve" => return Answer::Approve,
            "deny" => {
                return Answer::Deny {
                    reason: "denied on the Harkness command line".to_owned(),
                };
            }
            "show-input" => {
                let input = recorded_input(coordinator, request);
                let rendered = serde_json::to_string(&input)
                    .unwrap_or_else(|_| "the recorded input could not be rendered".to_owned());
                emit_progress(json_output, &rendered);
                emit_progress(json_output, INTERACTIVE_HELP);
            }
            other => {
                emit_progress(
                    json_output,
                    &format!(
                        "{:?} is not an answer; {INTERACTIVE_HELP}",
                        single_line(other)
                    ),
                );
            }
        }
    }
}

/// The recorded input of the call an approval is about.
///
/// Read from the persisted tool call rather than from the request, because the
/// request binds a *hash* of the input rather than the value. What comes back
/// has already been through the store's redactor.
pub(crate) fn recorded_input(coordinator: &RunCoordinator, request: &ApprovalRequest) -> Value {
    coordinator
        .store()
        .load_tool_call(request.tool_call_id())
        .map_or(Value::Null, |call| call.input().clone())
}

// ---------------------------------------------------------------------------
// Terminal outcomes
// ---------------------------------------------------------------------------

/// The error a run that did not succeed reports, or `None` when it did.
///
/// The verdict decides the exit status for the same reason a project check's
/// does: a caller must be able to act on the process status without parsing
/// standard output.
pub(crate) fn run_verdict(
    run: &Run,
    denied_noninteractively: bool,
    data: &Value,
) -> Option<CliError> {
    let (kind, code, verdict) = match run.state() {
        ExecutionState::Succeeded => return None,
        ExecutionState::Cancelled => ("run_cancelled", EXIT_CANCELLED, "was cancelled"),
        ExecutionState::Interrupted => {
            ("run_interrupted", EXIT_OPERATION_FAILED, "was interrupted")
        }
        ExecutionState::Failed if denied_noninteractively => (
            "approval_required_noninteractive",
            EXIT_REFUSED,
            "stopped because an approval could not be answered without a terminal",
        ),
        ExecutionState::Failed => ("run_failed", EXIT_OPERATION_FAILED, "failed"),
        // The supervision loop exits only on a terminal state, so these are
        // unreachable. Named rather than caught by a wildcard so a new
        // lifecycle state has to be classified here on purpose.
        ExecutionState::Queued | ExecutionState::Running | ExecutionState::WaitingForApproval => (
            "run_failed",
            EXIT_OPERATION_FAILED,
            "did not reach a verdict",
        ),
    };
    Some(CliError::RuntimeOutcome {
        kind,
        code,
        message: format!("run {} {verdict}", run.id()),
        details: data.clone(),
    })
}

/// The error one recorded tool call reports, or `None` when it succeeded.
///
/// A failed call reports the kind the runtime recorded, so a caller reading
/// `invalid_input` learns the same thing whether the call came from an agent or
/// from `tool invoke`. A denial reports which gate denied it, because "policy
/// refused" and "a human refused" are different facts about the same call.
pub(crate) fn tool_call_verdict(
    call: &ToolCall,
    denied_noninteractively: bool,
    data: &Value,
) -> Option<CliError> {
    let failure = call.failure();
    let recorded = failure.map(Failure::kind).unwrap_or_default();
    let (kind, code) = match call.state() {
        ToolCallState::Succeeded => return None,
        ToolCallState::Failed => (tool_kind(recorded), tool_exit_code(recorded)),
        ToolCallState::Denied if denied_noninteractively => {
            ("approval_required_noninteractive", EXIT_REFUSED)
        }
        ToolCallState::Denied => match recorded {
            "policy" => ("policy_denied", EXIT_REFUSED),
            "approval_denied" => ("approval_denied", EXIT_REFUSED),
            _ => ("tool_call_denied", EXIT_REFUSED),
        },
        ToolCallState::Cancelled => ("tool_call_cancelled", EXIT_CANCELLED),
        ToolCallState::Interrupted => ("tool_call_interrupted", EXIT_OPERATION_FAILED),
        // A supervised call has reached a terminal state by the time this runs.
        ToolCallState::Pending | ToolCallState::AwaitingApproval | ToolCallState::Running => {
            ("tool_call_failed", EXIT_OPERATION_FAILED)
        }
    };
    Some(CliError::RuntimeOutcome {
        kind,
        code,
        message: failure.map_or_else(
            || format!("tool call {} is {}", call.id(), call.state().as_str()),
            |failure| single_line(failure.message()),
        ),
        details: data.clone(),
    })
}

/// Records the requested trust decision, then refuses an untrusted workspace.
///
/// Trust is a precondition rather than an authorization: a trusted workspace
/// still passes policy and answers approvals on every call. Refusing here
/// simply stops a run being recorded at all for a checkout nobody has vouched
/// for, which is what `check run` already does and for the same reason — an
/// untrusted workspace denies everything above `Observe`, so the run would fail
/// after a task, a run and a step had been persisted.
pub(crate) fn apply_workspace_trust(
    coordinator: &RunCoordinator,
    project: &harkness_core::Project,
    trust_workspace: bool,
    intent: &str,
) -> Result<(), CliError> {
    let store = coordinator.store();
    if trust_workspace {
        let trust = WorkspaceTrust::decide(
            project.id,
            &project.root,
            TrustState::Trusted,
            OffsetDateTime::now_utc(),
        )
        .map_err(|error| CliError::WireProjection(error.to_string()))?;
        store.put_workspace_trust(&trust).map_err(CliError::Store)?;
    }
    if store
        .resolve_workspace_trust(project.id, &project.root)
        .map_err(CliError::Store)?
        != TrustState::Trusted
    {
        let (root, path_is_lossy) = wire_path(&project.root);
        return Err(CliError::Refused {
            kind: crate::RefusalKind::ConfirmationRequired,
            message: format!(
                "the selected workspace is untrusted; review the project root and retry with \
                 --trust-workspace before {intent}"
            ),
            details: json!({
                "project_id": project.id.to_string(),
                "root": root,
                "path_is_lossy": path_is_lossy,
            }),
        });
    }
    Ok(())
}

/// The workspace reference a run is started against.
///
/// The coordinator validates this against a reference it builds with the
/// store's *own* redactor and refuses the run when either field differs, so the
/// two have to agree. A store this crate opened has the default redactor, which
/// is [`PassThrough`], and there is no route from here to a store configured
/// with another one: `Store::redacting` is never called in this binary. Should
/// that ever change, this is where it would have to change too — the failure
/// would be every run refused before it started, not a wrong redaction.
pub(crate) fn workspace_ref(_store: &Store, task: &Task) -> WorkspaceRef {
    WorkspaceRef::from_task(task, &PassThrough)
}

/// Whether a recorded failure kind is one the tool namespace publishes.
///
/// Used only by the contract test, which is what keeps
/// [`tool_kind`]'s fallback from quietly swallowing a spelling the runtime
/// really does emit.
#[cfg(test)]
pub(crate) fn publishes_tool_kind(kind: &str) -> bool {
    TOOL_KIND_EXIT_CODES
        .iter()
        .any(|(published, _)| *published == kind)
}

#[cfg(test)]
mod tests {
    use harkness_runtime::coordinator::RuntimeError;
    use harkness_runtime::store::{RunCursor, StoreError};
    use harkness_runtime::tool::InvocationError;

    use super::{
        RUNTIME_KIND_EXIT_CODES, TOOL_KIND_EXIT_CODES, decode_run_cursor, encode_run_cursor,
        publishes_tool_kind,
    };

    /// `RuntimeError` and `StoreError` are both `#[non_exhaustive]`, so a
    /// variant added upstream cannot fail to compile here. This is what refuses
    /// that silence: the concatenated table has to name every kind, in order,
    /// before `harkness contract` can publish it.
    #[test]
    fn runtime_error_kinds_are_classified_for_the_exit_code_contract() {
        let declared = RUNTIME_KIND_EXIT_CODES
            .iter()
            .map(|(kind, _)| *kind)
            .collect::<Vec<_>>();
        let expected = RuntimeError::KINDS
            .iter()
            .chain(StoreError::KINDS)
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(
            declared, expected,
            "RUNTIME_KIND_EXIT_CODES must classify every coordinator and store kind, in order"
        );
    }

    #[test]
    fn invocation_error_kinds_are_classified_for_the_exit_code_contract() {
        let declared = TOOL_KIND_EXIT_CODES
            .iter()
            .map(|(kind, _)| *kind)
            .collect::<Vec<_>>();
        assert_eq!(
            declared,
            InvocationError::kinds(),
            "TOOL_KIND_EXIT_CODES must classify every invocation kind, in order"
        );
    }

    /// The two tables are published side by side and both are read by a caller
    /// holding one discriminant. `not_found` is in both on purpose; what must
    /// not happen is the two disagreeing about what it means.
    #[test]
    fn kinds_published_by_both_runtime_namespaces_report_one_exit_code() {
        for (kind, code) in RUNTIME_KIND_EXIT_CODES {
            if let Some((_, other)) = TOOL_KIND_EXIT_CODES
                .iter()
                .find(|(published, _)| published == kind)
            {
                assert_eq!(
                    code, other,
                    "{kind} is published by both runtime namespaces with two exit codes"
                );
            }
        }
    }

    /// The kinds a *recorded* failure can carry that this build maps onto the
    /// published table. Anything outside it falls back to `tool_call_failed`,
    /// which is correct but loses detail, so the two spellings the coordinator
    /// records itself are asserted to be handled explicitly elsewhere.
    #[test]
    fn every_published_tool_kind_survives_the_lookup() {
        for (kind, _) in TOOL_KIND_EXIT_CODES {
            assert!(publishes_tool_kind(kind));
            assert_eq!(super::tool_kind(kind), *kind);
        }
        assert_eq!(
            super::tool_kind("a spelling this build does not publish"),
            "tool_call_failed"
        );
    }

    #[test]
    fn a_run_cursor_round_trips_through_its_opaque_token() {
        let listing = serde_json::json!({
            "v": 1,
            "created_at": "2026-08-10T12:34:56.000000000Z",
            "id": "00000000-0000-4000-8000-000000000001",
        });
        let cursor = serde_json::from_value::<RunCursor>(listing).unwrap();
        let token = encode_run_cursor(&cursor).unwrap();

        assert_eq!(decode_run_cursor(&token).unwrap(), cursor);
        assert!(decode_run_cursor("not a token").is_err());
    }

    #[test]
    fn a_page_limit_outside_the_stores_bound_is_a_usage_error() {
        assert!(super::parse_run_limit("0").is_err());
        assert!(super::parse_run_limit("501").is_err());
        assert_eq!(super::parse_run_limit("500").unwrap(), 500);
        assert!(super::parse_event_limit("1001").is_err());
        assert_eq!(super::parse_event_limit("1000").unwrap(), 1000);
    }
}
