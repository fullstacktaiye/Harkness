//! Statement-level persistence for durable approval requests.
//!
//! Reads rebuild the record through the approval module's own `from_stored`
//! constructors and re-derive nothing they can check, exactly as the four core
//! record types do: a hand-edited row fails to load rather than entering the
//! process as an impossible approval. The row's `schema_version` is probed
//! before any other column is decoded, so a request written by a future build
//! reads as an upgrade request instead of a corrupt column.

use std::path::PathBuf;

use rusqlite::{Connection, Row, named_params};
use serde_json::json;
use time::OffsetDateTime;

use crate::approval::{
    ApprovalDecision, ApprovalGrant, ApprovalRequest, ApprovalScope, ApprovalState,
    ApprovalVerdict, DecidedVia, InputHash, PendingApproval, WorkspaceBinding,
};
use crate::domain::{ApprovalId, RUNTIME_RECORD_SCHEMA_VERSION, RunId};
use crate::tool::{Capability, RiskLevel, ToolIdentity};

use super::column::{
    decode_id, decode_optional_timestamp, decode_timestamp, encode_optional_timestamp, encode_path,
    encode_text, encode_timestamp,
};
use super::error::{Containment, StoreError, insert_failed, query_failed};
use super::repository::{missing_row, optional_text, row_failed, schema_version, text};
use super::{EventKind, Redactor, RunEvent};

const APPROVAL: &str = "approval";

const APPROVAL_COLUMNS: &str = "schema_version, id, run_id, tool_call_id, tool_id, tool_version, \
     capabilities_json, input_hash, input_summary, project_id, canonical_root, risk, \
     requested_scope, effective_scope, state, created_at, expires_at, resolved_at, decided_via, \
     decision_verdict, decision_scope, decision_reason";

/// Rewrites the caller text an approval makes durable.
///
/// An approval's input summary and a decider's reason are the two caller values
/// this table persists, and both are shown to a human and replayed into the
/// timeline. Scrubbing them on the way in rather than at each display site is
/// the bargain the event log already makes: one write path, not an audit of
/// every renderer. Redaction happens before any transaction opens, and the
/// record that comes back is the record that was stored — a caller never holds a
/// summary that differs from its own row.
pub(super) fn redact(redactor: &dyn Redactor, request: ApprovalRequest) -> ApprovalRequest {
    let summary = redactor.redact_text(request.input_summary()).into_owned();
    request.with_redacted_summary(summary)
}

/// Rewrites a decision's reason before it is recorded.
pub(super) fn redact_decision(
    redactor: &dyn Redactor,
    decision: ApprovalDecision,
) -> ApprovalDecision {
    let reason = decision
        .reason()
        .map(|reason| redactor.redact_text(reason).into_owned());
    decision.with_redacted_reason(reason)
}

/// The timeline entry announcing a question, derived from the record itself.
///
/// Built here rather than accepted from the caller, because the payload is a
/// security boundary and not a formatting choice: an event carries the summary
/// and the binding facts a human needs to recognize a request, and never the raw
/// input. The input stays in `tool_calls.input_json`, where a surface expands it
/// on demand and one oversized or secret-bearing value is not replicated into
/// the hot event stream. A caller free to supply its own payload would be one
/// `with_payload` away from putting the input there.
pub(super) fn requested_event(request: &ApprovalRequest) -> Result<RunEvent, StoreError> {
    Ok(
        RunEvent::new(EventKind::ApprovalRequested, request.created_at())
            .for_tool_call(request.tool_call_id())
            .with_payload(json!({
                "approval_id": request.id().to_string(),
                "tool": request.tool().to_string(),
                "risk": request.risk().as_str(),
                "requested_scope": request.requested_scope().as_str(),
                "effective_scope": request.effective_scope().as_str(),
                "summary": request.input_summary(),
                "expires_at": encode_optional_timestamp(
                    APPROVAL,
                    "expires_at",
                    request.expires_at(),
                )?,
            })),
    )
}

/// The timeline entry recording a human's answer.
///
/// Derived from the decision rather than from the record it is about to change,
/// so the whole event can be redacted, encoded, and — in the impossible case —
/// spilled before the transaction opens. Both are pure functions of the same
/// inputs, so describing the outcome in advance and applying it under the write
/// lock cannot disagree: if the record moved underneath, the change is refused
/// and this event is rolled back with it.
pub(super) fn decided_event(request: &ApprovalRequest, decision: &ApprovalDecision) -> RunEvent {
    let state = match decision.verdict() {
        ApprovalVerdict::Granted => ApprovalState::Granted,
        ApprovalVerdict::Denied => ApprovalState::Denied,
    };
    RunEvent::new(EventKind::ApprovalDecided, decision.decided_at())
        .for_tool_call(request.tool_call_id())
        .with_payload(json!({
            "approval_id": request.id().to_string(),
            "state": state.as_str(),
            "verdict": decision.verdict().as_str(),
            "scope": decision.scope().map(ApprovalScope::as_str),
            "decided_via": decision.decided_via().as_str(),
            "reason": decision.reason(),
        }))
}

/// The timeline entry recording a question that ended without an answer.
///
/// Emitted for expiry, cancellation, and supersession alike, so a timeline never
/// shows a request that was asked and then simply stopped being mentioned. It
/// carries no verdict, because nobody gave one.
pub(super) fn unanswered_event(
    request: &ApprovalRequest,
    to: ApprovalState,
    at: OffsetDateTime,
) -> RunEvent {
    RunEvent::new(EventKind::ApprovalDecided, at)
        .for_tool_call(request.tool_call_id())
        .with_payload(json!({
            "approval_id": request.id().to_string(),
            "state": to.as_str(),
        }))
}

pub(super) fn insert(connection: &Connection, request: &ApprovalRequest) -> Result<(), StoreError> {
    let workspace = request.workspace();
    connection
        .execute(
            &format!(
                "INSERT INTO approvals ({APPROVAL_COLUMNS}) VALUES (:schema_version, :id, \
                 :run_id, :tool_call_id, :tool_id, :tool_version, :capabilities_json, \
                 :input_hash, :input_summary, :project_id, :canonical_root, :risk, \
                 :requested_scope, :effective_scope, :state, :created_at, :expires_at, \
                 :resolved_at, :decided_via, :decision_verdict, :decision_scope, :decision_reason)"
            ),
            named_params! {
                ":schema_version": RUNTIME_RECORD_SCHEMA_VERSION,
                ":id": request.id().to_string(),
                ":run_id": request.run_id().to_string(),
                ":tool_call_id": request.tool_call_id().to_string(),
                ":tool_id": encode_text(APPROVAL, "tool_id", request.tool().id.as_str())?,
                ":tool_version": encode_text(
                    APPROVAL,
                    "tool_version",
                    &request.tool().version.to_string(),
                )?,
                ":capabilities_json": encode_capabilities(request.capabilities())?,
                ":input_hash": request.input_hash().to_hex(),
                ":input_summary": encode_text(
                    APPROVAL,
                    "input_summary",
                    request.input_summary(),
                )?,
                ":project_id": workspace.project_id().map(|id| id.to_string()),
                ":canonical_root": encode_path(
                    APPROVAL,
                    "canonical_root",
                    workspace.canonical_root(),
                )?,
                ":risk": request.risk().as_str(),
                ":requested_scope": request.requested_scope().as_str(),
                ":effective_scope": request.effective_scope().as_str(),
                ":state": request.state().as_str(),
                ":created_at": encode_timestamp(APPROVAL, "created_at", request.created_at())?,
                ":expires_at": encode_optional_timestamp(
                    APPROVAL,
                    "expires_at",
                    request.expires_at(),
                )?,
                ":resolved_at": encode_optional_timestamp(
                    APPROVAL,
                    "resolved_at",
                    request.resolved_at(),
                )?,
                ":decided_via": request.decision().map(|decision| decision.decided_via().as_str()),
                ":decision_verdict": request
                    .decision()
                    .map(|decision| decision.verdict().as_str()),
                ":decision_scope": request
                    .decision()
                    .and_then(ApprovalDecision::scope)
                    .map(ApprovalScope::as_str),
                ":decision_reason": decision_reason(request)?,
            },
        )
        .map(|_| ())
        .map_err(|error| {
            insert_failed(
                Containment {
                    record: APPROVAL,
                    parent: "tool call of that run",
                },
                &request.id(),
                "recording an approval request",
                error,
            )
        })
}

/// Writes back only the columns a resolution changes.
///
/// The binding fields — run, workspace, tool identity, capabilities, input hash,
/// and both scopes as they were asked for — are never named here. A grant is
/// matched against exactly those, so an update statement that could rewrite one
/// would let a resolution quietly re-target the approval a human answered.
pub(super) fn update_resolution(
    connection: &Connection,
    request: &ApprovalRequest,
) -> Result<(), StoreError> {
    let updated = connection
        .execute(
            "UPDATE approvals SET state = :state, effective_scope = :effective_scope, \
             resolved_at = :resolved_at, decided_via = :decided_via, \
             decision_verdict = :decision_verdict, decision_scope = :decision_scope, \
             decision_reason = :decision_reason WHERE id = :id",
            named_params! {
                ":id": request.id().to_string(),
                ":state": request.state().as_str(),
                // The one binding column a resolution may move, and only
                // downwards: a human narrowing a run-wide request to a single
                // call has to leave a record whose breadth is what was allowed.
                ":effective_scope": request.effective_scope().as_str(),
                ":resolved_at": encode_optional_timestamp(
                    APPROVAL,
                    "resolved_at",
                    request.resolved_at(),
                )?,
                ":decided_via": request.decision().map(|decision| decision.decided_via().as_str()),
                ":decision_verdict": request
                    .decision()
                    .map(|decision| decision.verdict().as_str()),
                ":decision_scope": request
                    .decision()
                    .and_then(ApprovalDecision::scope)
                    .map(ApprovalScope::as_str),
                ":decision_reason": decision_reason(request)?,
            },
        )
        .map_err(|error| query_failed("resolving an approval request", error))?;
    missing_row(APPROVAL, &request.id(), updated)
}

pub(super) fn load(connection: &Connection, id: ApprovalId) -> Result<ApprovalRequest, StoreError> {
    let mut statement = connection
        .prepare_cached(&format!(
            "SELECT {APPROVAL_COLUMNS} FROM approvals WHERE id = :id"
        ))
        .map_err(|error| query_failed("preparing the approval query", error))?;
    statement
        .query_row(named_params! { ":id": id.to_string() }, |row| {
            Ok(decode(row))
        })
        .map_err(|error| row_failed(APPROVAL, &id, "loading an approval request", error))?
}

/// Every unanswered request, oldest first, across every run.
///
/// This is the restart listing: a run interrupted mid-question leaves its row
/// exactly as it was, and the next start finds it here.
pub(super) fn list_pending(connection: &Connection) -> Result<Vec<ApprovalRequest>, StoreError> {
    list(
        connection,
        &format!(
            "SELECT {APPROVAL_COLUMNS} FROM approvals WHERE state = :state \
             ORDER BY created_at, id"
        ),
        named_params! { ":state": ApprovalState::Pending.as_str() },
        "listing pending approval requests",
    )
}

/// Every request of one run, oldest first, whatever its state.
pub(super) fn list_for_run(
    connection: &Connection,
    run_id: RunId,
) -> Result<Vec<ApprovalRequest>, StoreError> {
    list(
        connection,
        &format!(
            "SELECT {APPROVAL_COLUMNS} FROM approvals WHERE run_id = :run_id \
             ORDER BY created_at, id"
        ),
        named_params! { ":run_id": run_id.to_string() },
        "listing the approval requests of a run",
    )
}

/// The live grants of one run, in the order they were granted.
pub(super) fn list_grants(
    connection: &Connection,
    run_id: RunId,
) -> Result<Vec<ApprovalGrant>, StoreError> {
    let granted = list(
        connection,
        &format!(
            "SELECT {APPROVAL_COLUMNS} FROM approvals WHERE run_id = :run_id AND state = :state \
             ORDER BY resolved_at, id"
        ),
        named_params! {
            ":run_id": run_id.to_string(),
            ":state": ApprovalState::Granted.as_str(),
        },
        "listing the grants of a run",
    )?;
    // `grant` answers `Some` for exactly the `granted` rows the query selected,
    // so nothing is dropped here; going through it rather than rebuilding the
    // grant from columns keeps one construction path.
    Ok(granted.iter().filter_map(ApprovalRequest::grant).collect())
}

fn list(
    connection: &Connection,
    sql: &str,
    parameters: &[(&str, &dyn rusqlite::ToSql)],
    operation: &'static str,
) -> Result<Vec<ApprovalRequest>, StoreError> {
    let mut statement = connection
        .prepare_cached(sql)
        .map_err(|error| query_failed(operation, error))?;
    let rows = statement
        .query_map(parameters, |row| Ok(decode(row)))
        .map_err(|error| query_failed(operation, error))?;
    let mut requests = Vec::new();
    for row in rows {
        requests.push(row.map_err(|error| query_failed(operation, error))??);
    }
    Ok(requests)
}

fn decode(row: &Row<'_>) -> Result<ApprovalRequest, StoreError> {
    // Probe first: a future row may spell a state, a scope, or a surface in a
    // way this build cannot decode, and the caller needs to be told to upgrade.
    schema_version(row, APPROVAL)?;

    let id: ApprovalId = decode_id(APPROVAL, "id", &text(row, APPROVAL, "id")?)?;
    let tool = ToolIdentity::parse(
        &text(row, APPROVAL, "tool_id")?,
        &text(row, APPROVAL, "tool_version")?,
    )
    .map_err(|error| encoding("tool", error))?;
    let workspace = WorkspaceBinding::new(
        optional_text(row, APPROVAL, "project_id")?
            .map(|stored| decode_id(APPROVAL, "project_id", &stored))
            .transpose()?,
        PathBuf::from(text(row, APPROVAL, "canonical_root")?),
    );

    let pending = PendingApproval::new(
        decode_id(APPROVAL, "run_id", &text(row, APPROVAL, "run_id")?)?,
        decode_id(
            APPROVAL,
            "tool_call_id",
            &text(row, APPROVAL, "tool_call_id")?,
        )?,
        tool,
        InputHash::parse(&text(row, APPROVAL, "input_hash")?)
            .map_err(|error| encoding("input_hash", error))?,
        workspace,
        decode_spelling(RiskLevel::ALL, RiskLevel::as_str, row, "risk")?,
        decode_timestamp(APPROVAL, "created_at", &text(row, APPROVAL, "created_at")?)?,
    )
    .requesting(decode_spelling(
        ApprovalScope::ALL,
        ApprovalScope::as_str,
        row,
        "requested_scope",
    )?)
    .with_capabilities(decode_capabilities(&text(
        row,
        APPROVAL,
        "capabilities_json",
    )?)?)
    .summarized_as(text(row, APPROVAL, "input_summary")?);
    let pending = match decode_optional_timestamp(
        APPROVAL,
        "expires_at",
        optional_text(row, APPROVAL, "expires_at")?,
    )? {
        Some(expires_at) => pending.expiring_at(expires_at),
        None => pending,
    };

    let state = decode_spelling(ApprovalState::ALL, ApprovalState::as_str, row, "state")?;
    let resolved_at = decode_optional_timestamp(
        APPROVAL,
        "resolved_at",
        optional_text(row, APPROVAL, "resolved_at")?,
    )?;
    let decision = decode_decision(row, id, resolved_at)?;

    // The domain re-checks every cross-column claim the row makes, so an edit
    // that widened a scope or invented a decision fails to load rather than
    // reaching the matcher.
    ApprovalRequest::from_stored(
        id,
        pending,
        decode_spelling(
            ApprovalScope::ALL,
            ApprovalScope::as_str,
            row,
            "effective_scope",
        )?,
        state,
        resolved_at,
        decision,
    )
    .map_err(StoreError::Approval)
}

/// Rebuilds the decision from the columns that carry it.
///
/// This layer owns only what a column can be wrong about on its own: a spelling
/// this build does not define, and a decision stored in pieces. A verdict
/// without a surface is a corrupt row rather than a partial answer, and is
/// refused by name exactly as `failure_kind` without `failure_message` is.
///
/// Whether a decision *belongs* on this row at all — whether the state agrees
/// with the verdict, whether an unanswered request is carrying one — is a claim
/// across columns, and [`ApprovalRequest::from_stored`] re-checks every one of
/// those. Duplicating them here would mean two places to keep in step and a
/// refusal whose wording depended on which fired first.
fn decode_decision(
    row: &Row<'_>,
    id: ApprovalId,
    resolved_at: Option<time::OffsetDateTime>,
) -> Result<Option<ApprovalDecision>, StoreError> {
    let (verdict, decided_via) = match (
        optional_text(row, APPROVAL, "decision_verdict")?,
        optional_text(row, APPROVAL, "decided_via")?,
    ) {
        (None, None) => return Ok(None),
        (Some(verdict), Some(decided_via)) => (verdict, decided_via),
        _ => {
            return Err(column_encoding(
                "decision_verdict",
                "decision_verdict and decided_via must be stored together".to_owned(),
            ));
        }
    };
    let Some(decided_at) = resolved_at else {
        return Err(column_encoding(
            "resolved_at",
            "a decided approval must record when it was decided".to_owned(),
        ));
    };

    let verdict = ApprovalVerdict::from_stored(&verdict)
        .ok_or_else(|| unknown_spelling("decision_verdict", &verdict))?;
    let decided_via = DecidedVia::from_stored(&decided_via)
        .ok_or_else(|| unknown_spelling("decided_via", &decided_via))?;
    let scope = optional_text(row, APPROVAL, "decision_scope")?
        .map(|stored| {
            ApprovalScope::from_stored(&stored)
                .ok_or_else(|| unknown_spelling("decision_scope", &stored))
        })
        .transpose()?;
    // A granted row with no scope, or a denied row carrying one, would each read
    // as the other to anything skimming the table.
    if (verdict == ApprovalVerdict::Granted) != scope.is_some() {
        return Err(column_encoding(
            "decision_scope",
            "a granted approval records the scope it authorized and a denied one records none"
                .to_owned(),
        ));
    }

    Ok(Some(ApprovalDecision::from_stored(
        id,
        verdict,
        scope,
        decided_via,
        decided_at,
        optional_text(row, APPROVAL, "decision_reason")?,
    )))
}

/// Reads a column holding one spelling from a closed table of them.
fn decode_spelling<T: Copy>(
    all: &[T],
    spelling: fn(T) -> &'static str,
    row: &Row<'_>,
    field: &'static str,
) -> Result<T, StoreError> {
    let stored = text(row, APPROVAL, field)?;
    all.iter()
        .copied()
        .find(|value| spelling(*value) == stored)
        .ok_or_else(|| unknown_spelling(field, &stored))
}

/// Borrows the decider's explanation after proving it fits its column.
fn decision_reason(request: &ApprovalRequest) -> Result<Option<&str>, StoreError> {
    request
        .decision()
        .and_then(ApprovalDecision::reason)
        .map(|reason| encode_text(APPROVAL, "decision_reason", reason))
        .transpose()
}

fn encode_capabilities(capabilities: &[Capability]) -> Result<String, StoreError> {
    let encoded = serde_json::to_string(capabilities)
        .map_err(|error| encoding("capabilities_json", error))?;
    encode_text(APPROVAL, "capabilities_json", &encoded).map(ToOwned::to_owned)
}

fn decode_capabilities(stored: &str) -> Result<Vec<Capability>, StoreError> {
    serde_json::from_str(stored).map_err(|error| encoding("capabilities_json", error))
}

fn encoding(field: &'static str, reason: impl std::fmt::Display) -> StoreError {
    column_encoding(field, reason.to_string())
}

fn column_encoding(field: &'static str, reason: String) -> StoreError {
    StoreError::ColumnEncoding {
        record: APPROVAL,
        field,
        reason,
    }
}

fn unknown_spelling(field: &'static str, stored: &str) -> StoreError {
    column_encoding(
        field,
        format!("{stored:?} is not a spelling this build understands"),
    )
}
