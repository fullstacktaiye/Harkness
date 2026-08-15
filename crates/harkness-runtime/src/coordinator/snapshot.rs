use crate::approval::ApprovalRequest;
use crate::domain::{
    Run, RunWireRef, Step, StepWireRef, Task, TaskWireRef, ToolCall, ToolCallWireRef,
};
use crate::store::{Artifact, StoredEvent};
use serde::{Serialize, Serializer};
use serde_json::{Map, Value, json};
use time::format_description::well_known::Rfc3339;

/// Complete durable view of one run for either front end.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct RunSnapshot {
    /// User request this run attempts.
    pub task: Task,
    /// Current run lifecycle record.
    pub run: Run,
    /// Steps in ordinal order.
    pub steps: Vec<Step>,
    /// Calls in creation order.
    pub tool_calls: Vec<ToolCall>,
    /// Approval requests in creation order.
    pub approvals: Vec<ApprovalRequest>,
    /// Artifact metadata belonging to the run.
    pub artifacts: Vec<Artifact>,
    /// Full persisted timeline in sequence order.
    pub events: Vec<StoredEvent>,
}

impl Serialize for RunSnapshot {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut object = Map::new();
        object.insert(
            "task".to_owned(),
            serde_json::to_value(TaskWireRef::from(&self.task))
                .map_err(serde::ser::Error::custom)?,
        );
        object.insert(
            "run".to_owned(),
            serde_json::to_value(RunWireRef::from(&self.run)).map_err(serde::ser::Error::custom)?,
        );
        object.insert(
            "steps".to_owned(),
            self.steps
                .iter()
                .map(|step| serde_json::to_value(StepWireRef::from(step)))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array)
                .map_err(serde::ser::Error::custom)?,
        );
        object.insert(
            "tool_calls".to_owned(),
            self.tool_calls
                .iter()
                .map(|call| serde_json::to_value(ToolCallWireRef::from(call)))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array)
                .map_err(serde::ser::Error::custom)?,
        );
        object.insert(
            "approvals".to_owned(),
            Value::Array(self.approvals.iter().map(approval_value).collect()),
        );
        object.insert(
            "artifacts".to_owned(),
            Value::Array(self.artifacts.iter().map(artifact_value).collect()),
        );
        object.insert(
            "events".to_owned(),
            Value::Array(self.events.iter().map(event_value).collect()),
        );
        Value::Object(object).serialize(serializer)
    }
}

fn timestamp(at: time::OffsetDateTime) -> String {
    at.format(&Rfc3339)
        .expect("a UTC OffsetDateTime is always RFC 3339 representable")
}

fn approval_value(request: &ApprovalRequest) -> Value {
    let decision = request.decision().map(|decision| {
        json!({
            "verdict": decision.verdict().as_str(),
            "scope": decision.scope().map(crate::approval::ApprovalScope::as_str),
            "decided_at": timestamp(decision.decided_at()),
            "decided_via": decision.decided_via().as_str(),
            "reason": decision.reason(),
        })
    });
    json!({
        "id": request.id(),
        "run_id": request.run_id(),
        "tool_call_id": request.tool_call_id(),
        "tool": request.tool().to_string(),
        "input_hash": request.input_hash().to_hex(),
        "input_summary": request.input_summary(),
        "workspace": {
            "project_id": request.workspace().project_id(),
            // Lossy, and flagged as such, rather than embedded as a `Path`.
            // `json!` expands a value to `to_value(..).unwrap()`, and `Path`'s
            // `Serialize` *errors* on non-UTF-8 — so a canonical root with a
            // non-UTF-8 component panicked here, inside an impl whose every
            // other field carefully reports a serialization error instead.
            "canonical_root": request.workspace().canonical_root().to_string_lossy(),
            "canonical_root_is_lossy":
                request.workspace().canonical_root().as_os_str().to_str().is_none(),
        },
        "risk": request.risk().as_str(),
        "requested_scope": request.requested_scope().as_str(),
        "effective_scope": request.effective_scope().as_str(),
        "state": request.state().as_str(),
        "created_at": timestamp(request.created_at()),
        "expires_at": request.expires_at().map(timestamp),
        "resolved_at": request.resolved_at().map(timestamp),
        "decision": decision,
    })
}

fn artifact_value(artifact: &Artifact) -> Value {
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
        "availability": artifact.availability().as_str(),
    })
}

fn event_value(stored: &StoredEvent) -> Value {
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
