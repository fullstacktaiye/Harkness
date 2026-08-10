//! Statement-level reads and writes for the four core record types.
//!
//! Writes take a durable record apart into columns; reads put the wire record
//! back together and hand it to the domain's own `TryFrom`, so every stored row
//! re-earns its record type by passing the same lifecycle rules a freshly built
//! record passes. A row edited outside Harkness therefore fails to load instead
//! of entering the process as an impossible record.

use rusqlite::{Connection, Row, named_params};

use crate::domain::{
    RUNTIME_RECORD_SCHEMA_VERSION, Run, RunId, RunWire, Step, StepId, StepWire, Task, TaskId,
    TaskWire, ToolCall, ToolCallId, ToolCallWire,
};

use super::column::{
    decode_approvals, decode_execution_state, decode_failure, decode_id, decode_optional_payload,
    decode_optional_timestamp, decode_ordinal, decode_owner_pid, decode_payload, decode_revision,
    decode_timestamp, decode_tool_call_state, encode_approvals, encode_failure,
    encode_optional_payload, encode_optional_timestamp, encode_path, encode_payload,
    encode_revision, encode_timestamp,
};
use super::error::{Containment, StoreError, insert_failed, query_failed};

const TASK: &str = "task";
const RUN: &str = "run";
const STEP: &str = "step";
const TOOL_CALL: &str = "tool_call";

pub(super) const RUN_COLUMNS: &str = "schema_version, id, task_id, state, revision, created_at, \
     updated_at, started_at, finished_at, failure_kind, failure_message, approvals_json, owner_pid";

const STEP_COLUMNS: &str = "schema_version, id, run_id, ordinal, title, state, revision, \
     created_at, updated_at, started_at, finished_at, failure_kind, failure_message, \
     approvals_json";

const TOOL_CALL_COLUMNS: &str = "schema_version, id, run_id, step_id, tool_id, tool_version, \
     input_json, output_json, state, revision, created_at, updated_at, started_at, finished_at, \
     failure_kind, failure_message, approvals_json";

// -- tasks ------------------------------------------------------------------

pub(super) fn insert_task(connection: &Connection, task: &Task) -> Result<(), StoreError> {
    connection
        .execute(
            "INSERT INTO tasks (schema_version, id, title, workspace_root, project_id, created_at) \
             VALUES (:schema_version, :id, :title, :workspace_root, :project_id, :created_at)",
            named_params! {
                ":schema_version": RUNTIME_RECORD_SCHEMA_VERSION,
                ":id": task.id().to_string(),
                ":title": task.title(),
                ":workspace_root": encode_path(TASK, "workspace_root", task.workspace_root())?,
                ":project_id": task.project_id().map(|id| id.to_string()),
                ":created_at": encode_timestamp(TASK, "created_at", task.created_at())?,
            },
        )
        .map(|_| ())
        .map_err(|error| {
            // A task is the root of the containment tree and declares no
            // foreign key, so only the primary-key branch is reachable here.
            insert_failed(
                Containment {
                    record: TASK,
                    parent: TASK,
                },
                &task.id(),
                "inserting a task",
                error,
            )
        })
}

pub(super) fn load_task(connection: &Connection, id: TaskId) -> Result<Task, StoreError> {
    let mut statement = connection
        .prepare_cached(
            "SELECT schema_version, id, title, workspace_root, project_id, created_at \
             FROM tasks WHERE id = :id",
        )
        .map_err(|error| query_failed("preparing the task query", error))?;
    let wire = statement
        .query_row(named_params! { ":id": id.to_string() }, |row| {
            Ok(task_wire(row))
        })
        .map_err(|error| row_failed(TASK, &id, "loading a task", error))??;
    Task::try_from(wire).map_err(|source| StoreError::InvalidRecord {
        record: TASK,
        source,
    })
}

fn task_wire(row: &Row<'_>) -> Result<TaskWire, StoreError> {
    Ok(TaskWire {
        schema_version: schema_version(row, TASK)?,
        id: decode_id(TASK, "id", &text(row, TASK, "id")?)?,
        title: text(row, TASK, "title")?,
        workspace_root: text(row, TASK, "workspace_root")?.into(),
        project_id: optional_text(row, TASK, "project_id")?
            .map(|stored| decode_id(TASK, "project_id", &stored))
            .transpose()?,
        created_at: decode_timestamp(TASK, "created_at", &text(row, TASK, "created_at")?)?,
    })
}

// -- runs -------------------------------------------------------------------

pub(super) fn insert_run(connection: &Connection, run: &Run) -> Result<(), StoreError> {
    let (failure_kind, failure_message) = encode_failure(run.failure());
    connection
        .execute(
            &format!(
                "INSERT INTO runs ({RUN_COLUMNS}) VALUES (:schema_version, :id, :task_id, :state, \
                 :revision, :created_at, :updated_at, :started_at, :finished_at, :failure_kind, \
                 :failure_message, :approvals_json, :owner_pid)"
            ),
            named_params! {
                ":schema_version": RUNTIME_RECORD_SCHEMA_VERSION,
                ":id": run.id().to_string(),
                ":task_id": run.task_id().to_string(),
                ":state": run.state().as_str(),
                ":revision": encode_revision(RUN, run.revision())?,
                ":created_at": encode_timestamp(RUN, "created_at", run.created_at())?,
                ":updated_at": encode_timestamp(RUN, "updated_at", run.updated_at())?,
                ":started_at": encode_optional_timestamp(RUN, "started_at", run.started_at())?,
                ":finished_at": encode_optional_timestamp(RUN, "finished_at", run.finished_at())?,
                ":failure_kind": failure_kind,
                ":failure_message": failure_message,
                ":approvals_json": encode_approvals(RUN, run.approvals())?,
                ":owner_pid": None::<i64>,
            },
        )
        .map(|_| ())
        .map_err(|error| {
            insert_failed(
                Containment {
                    record: RUN,
                    parent: TASK,
                },
                &run.id(),
                "inserting a run",
                error,
            )
        })
}

pub(super) fn update_run(connection: &Connection, run: &Run) -> Result<(), StoreError> {
    let (failure_kind, failure_message) = encode_failure(run.failure());
    let updated = connection
        .execute(
            "UPDATE runs SET state = :state, revision = :revision, updated_at = :updated_at, \
             started_at = :started_at, finished_at = :finished_at, failure_kind = :failure_kind, \
             failure_message = :failure_message, approvals_json = :approvals_json WHERE id = :id",
            named_params! {
                ":id": run.id().to_string(),
                ":state": run.state().as_str(),
                ":revision": encode_revision(RUN, run.revision())?,
                ":updated_at": encode_timestamp(RUN, "updated_at", run.updated_at())?,
                ":started_at": encode_optional_timestamp(RUN, "started_at", run.started_at())?,
                ":finished_at": encode_optional_timestamp(RUN, "finished_at", run.finished_at())?,
                ":failure_kind": failure_kind,
                ":failure_message": failure_message,
                ":approvals_json": encode_approvals(RUN, run.approvals())?,
            },
        )
        .map_err(|error| query_failed("updating a run", error))?;
    missing_row(RUN, &run.id(), updated)
}

pub(super) fn set_run_owner(
    connection: &Connection,
    id: RunId,
    owner_pid: Option<u32>,
) -> Result<(), StoreError> {
    let updated = connection
        .execute(
            "UPDATE runs SET owner_pid = :owner_pid WHERE id = :id",
            named_params! {
                ":id": id.to_string(),
                ":owner_pid": owner_pid.map(i64::from),
            },
        )
        .map_err(|error| query_failed("claiming a run", error))?;
    missing_row(RUN, &id, updated)
}

pub(super) fn run_owner(connection: &Connection, id: RunId) -> Result<Option<u32>, StoreError> {
    let mut statement = connection
        .prepare_cached("SELECT owner_pid FROM runs WHERE id = :id")
        .map_err(|error| query_failed("preparing the run owner query", error))?;
    let stored = statement
        .query_row(named_params! { ":id": id.to_string() }, |row| {
            row.get::<_, Option<i64>>(0)
        })
        .map_err(|error| row_failed(RUN, &id, "loading a run owner", error))?;
    decode_owner_pid(RUN, stored)
}

pub(super) fn load_run(connection: &Connection, id: RunId) -> Result<Run, StoreError> {
    let mut statement = connection
        .prepare_cached(&format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = :id"))
        .map_err(|error| query_failed("preparing the run query", error))?;
    let wire = statement
        .query_row(named_params! { ":id": id.to_string() }, |row| {
            Ok(run_wire(row))
        })
        .map_err(|error| row_failed(RUN, &id, "loading a run", error))??;
    run_from_wire(wire)
}

pub(super) fn run_from_wire(wire: RunWire) -> Result<Run, StoreError> {
    Run::try_from(wire).map_err(|source| StoreError::InvalidRecord {
        record: RUN,
        source,
    })
}

pub(super) fn run_wire(row: &Row<'_>) -> Result<RunWire, StoreError> {
    Ok(RunWire {
        schema_version: schema_version(row, RUN)?,
        id: decode_id(RUN, "id", &text(row, RUN, "id")?)?,
        task_id: decode_id(RUN, "task_id", &text(row, RUN, "task_id")?)?,
        state: decode_execution_state(RUN, &text(row, RUN, "state")?)?,
        revision: decode_revision(RUN, integer(row, RUN, "revision")?)?,
        created_at: decode_timestamp(RUN, "created_at", &text(row, RUN, "created_at")?)?,
        updated_at: decode_timestamp(RUN, "updated_at", &text(row, RUN, "updated_at")?)?,
        started_at: decode_optional_timestamp(
            RUN,
            "started_at",
            optional_text(row, RUN, "started_at")?,
        )?,
        finished_at: decode_optional_timestamp(
            RUN,
            "finished_at",
            optional_text(row, RUN, "finished_at")?,
        )?,
        failure: decode_failure(
            RUN,
            optional_text(row, RUN, "failure_kind")?,
            optional_text(row, RUN, "failure_message")?,
        )?,
        approvals: decode_approvals(RUN, &text(row, RUN, "approvals_json")?)?,
    })
}

// -- steps ------------------------------------------------------------------

pub(super) fn insert_step(connection: &Connection, step: &Step) -> Result<(), StoreError> {
    let (failure_kind, failure_message) = encode_failure(step.failure());
    connection
        .execute(
            &format!(
                "INSERT INTO steps ({STEP_COLUMNS}) VALUES (:schema_version, :id, :run_id, \
                 :ordinal, :title, :state, :revision, :created_at, :updated_at, :started_at, \
                 :finished_at, :failure_kind, :failure_message, :approvals_json)"
            ),
            named_params! {
                ":schema_version": RUNTIME_RECORD_SCHEMA_VERSION,
                ":id": step.id().to_string(),
                ":run_id": step.run_id().to_string(),
                ":ordinal": step.ordinal(),
                ":title": step.title(),
                ":state": step.state().as_str(),
                ":revision": encode_revision(STEP, step.revision())?,
                ":created_at": encode_timestamp(STEP, "created_at", step.created_at())?,
                ":updated_at": encode_timestamp(STEP, "updated_at", step.updated_at())?,
                ":started_at": encode_optional_timestamp(STEP, "started_at", step.started_at())?,
                ":finished_at": encode_optional_timestamp(STEP, "finished_at", step.finished_at())?,
                ":failure_kind": failure_kind,
                ":failure_message": failure_message,
                ":approvals_json": encode_approvals(STEP, step.approvals())?,
            },
        )
        .map(|_| ())
        .map_err(|error| duplicate_ordinal(step, error))
}

fn duplicate_ordinal(step: &Step, error: rusqlite::Error) -> StoreError {
    if let rusqlite::Error::SqliteFailure(failure, _) = &error
        && failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
    {
        return StoreError::DuplicateStepOrdinal {
            run_id: step.run_id().to_string(),
            ordinal: step.ordinal(),
        };
    }
    insert_failed(
        Containment {
            record: STEP,
            parent: RUN,
        },
        &step.id(),
        "inserting a step",
        error,
    )
}

pub(super) fn update_step(connection: &Connection, step: &Step) -> Result<(), StoreError> {
    let (failure_kind, failure_message) = encode_failure(step.failure());
    let updated = connection
        .execute(
            "UPDATE steps SET state = :state, revision = :revision, updated_at = :updated_at, \
             started_at = :started_at, finished_at = :finished_at, failure_kind = :failure_kind, \
             failure_message = :failure_message, approvals_json = :approvals_json WHERE id = :id",
            named_params! {
                ":id": step.id().to_string(),
                ":state": step.state().as_str(),
                ":revision": encode_revision(STEP, step.revision())?,
                ":updated_at": encode_timestamp(STEP, "updated_at", step.updated_at())?,
                ":started_at": encode_optional_timestamp(STEP, "started_at", step.started_at())?,
                ":finished_at": encode_optional_timestamp(STEP, "finished_at", step.finished_at())?,
                ":failure_kind": failure_kind,
                ":failure_message": failure_message,
                ":approvals_json": encode_approvals(STEP, step.approvals())?,
            },
        )
        .map_err(|error| query_failed("updating a step", error))?;
    missing_row(STEP, &step.id(), updated)
}

pub(super) fn load_step(connection: &Connection, id: StepId) -> Result<Step, StoreError> {
    let mut statement = connection
        .prepare_cached(&format!("SELECT {STEP_COLUMNS} FROM steps WHERE id = :id"))
        .map_err(|error| query_failed("preparing the step query", error))?;
    let wire = statement
        .query_row(named_params! { ":id": id.to_string() }, |row| {
            Ok(step_wire(row))
        })
        .map_err(|error| row_failed(STEP, &id, "loading a step", error))??;
    step_from_wire(wire)
}

pub(super) fn load_run_steps(
    connection: &Connection,
    run_id: RunId,
) -> Result<Vec<Step>, StoreError> {
    let mut statement = connection
        .prepare_cached(&format!(
            "SELECT {STEP_COLUMNS} FROM steps WHERE run_id = :run_id ORDER BY ordinal"
        ))
        .map_err(|error| query_failed("preparing the run steps query", error))?;
    let rows = statement
        .query_map(named_params! { ":run_id": run_id.to_string() }, |row| {
            Ok(step_wire(row))
        })
        .map_err(|error| query_failed("listing the steps of a run", error))?;
    let mut steps = Vec::new();
    for row in rows {
        let wire = row.map_err(|error| query_failed("reading a step row", error))??;
        steps.push(step_from_wire(wire)?);
    }
    Ok(steps)
}

fn step_from_wire(wire: StepWire) -> Result<Step, StoreError> {
    Step::try_from(wire).map_err(|source| StoreError::InvalidRecord {
        record: STEP,
        source,
    })
}

fn step_wire(row: &Row<'_>) -> Result<StepWire, StoreError> {
    Ok(StepWire {
        schema_version: schema_version(row, STEP)?,
        id: decode_id(STEP, "id", &text(row, STEP, "id")?)?,
        run_id: decode_id(STEP, "run_id", &text(row, STEP, "run_id")?)?,
        ordinal: decode_ordinal(STEP, integer(row, STEP, "ordinal")?)?,
        title: text(row, STEP, "title")?,
        state: decode_execution_state(STEP, &text(row, STEP, "state")?)?,
        revision: decode_revision(STEP, integer(row, STEP, "revision")?)?,
        created_at: decode_timestamp(STEP, "created_at", &text(row, STEP, "created_at")?)?,
        updated_at: decode_timestamp(STEP, "updated_at", &text(row, STEP, "updated_at")?)?,
        started_at: decode_optional_timestamp(
            STEP,
            "started_at",
            optional_text(row, STEP, "started_at")?,
        )?,
        finished_at: decode_optional_timestamp(
            STEP,
            "finished_at",
            optional_text(row, STEP, "finished_at")?,
        )?,
        failure: decode_failure(
            STEP,
            optional_text(row, STEP, "failure_kind")?,
            optional_text(row, STEP, "failure_message")?,
        )?,
        approvals: decode_approvals(STEP, &text(row, STEP, "approvals_json")?)?,
    })
}

// -- tool calls -------------------------------------------------------------

pub(super) fn insert_tool_call(connection: &Connection, call: &ToolCall) -> Result<(), StoreError> {
    let (failure_kind, failure_message) = encode_failure(call.failure());
    connection
        .execute(
            &format!(
                "INSERT INTO tool_calls ({TOOL_CALL_COLUMNS}) VALUES (:schema_version, :id, \
                 :run_id, :step_id, :tool_id, :tool_version, :input_json, :output_json, :state, \
                 :revision, :created_at, :updated_at, :started_at, :finished_at, :failure_kind, \
                 :failure_message, :approvals_json)"
            ),
            named_params! {
                ":schema_version": RUNTIME_RECORD_SCHEMA_VERSION,
                ":id": call.id().to_string(),
                ":run_id": call.run_id().to_string(),
                ":step_id": call.step_id().to_string(),
                ":tool_id": call.tool_id(),
                ":tool_version": call.tool_version(),
                ":input_json": encode_payload(TOOL_CALL, "input", call.input())?,
                ":output_json": encode_optional_payload(TOOL_CALL, "output", call.output())?,
                ":state": call.state().as_str(),
                ":revision": encode_revision(TOOL_CALL, call.revision())?,
                ":created_at": encode_timestamp(TOOL_CALL, "created_at", call.created_at())?,
                ":updated_at": encode_timestamp(TOOL_CALL, "updated_at", call.updated_at())?,
                ":started_at": encode_optional_timestamp(TOOL_CALL, "started_at", call.started_at())?,
                ":finished_at": encode_optional_timestamp(TOOL_CALL, "finished_at", call.finished_at())?,
                ":failure_kind": failure_kind,
                ":failure_message": failure_message,
                ":approvals_json": encode_approvals(TOOL_CALL, call.approvals())?,
            },
        )
        .map(|_| ())
        .map_err(|error| {
            insert_failed(
                Containment {
                    record: TOOL_CALL,
                    parent: STEP,
                },
                &call.id(),
                "inserting a tool call",
                error,
            )
        })
}

pub(super) fn update_tool_call(connection: &Connection, call: &ToolCall) -> Result<(), StoreError> {
    let (failure_kind, failure_message) = encode_failure(call.failure());
    let updated = connection
        .execute(
            "UPDATE tool_calls SET state = :state, revision = :revision, \
             updated_at = :updated_at, started_at = :started_at, finished_at = :finished_at, \
             failure_kind = :failure_kind, failure_message = :failure_message, \
             output_json = :output_json, approvals_json = :approvals_json WHERE id = :id",
            named_params! {
                ":id": call.id().to_string(),
                ":state": call.state().as_str(),
                ":revision": encode_revision(TOOL_CALL, call.revision())?,
                ":updated_at": encode_timestamp(TOOL_CALL, "updated_at", call.updated_at())?,
                ":started_at": encode_optional_timestamp(TOOL_CALL, "started_at", call.started_at())?,
                ":finished_at": encode_optional_timestamp(TOOL_CALL, "finished_at", call.finished_at())?,
                ":failure_kind": failure_kind,
                ":failure_message": failure_message,
                ":output_json": encode_optional_payload(TOOL_CALL, "output", call.output())?,
                ":approvals_json": encode_approvals(TOOL_CALL, call.approvals())?,
            },
        )
        .map_err(|error| query_failed("updating a tool call", error))?;
    missing_row(TOOL_CALL, &call.id(), updated)
}

pub(super) fn load_tool_call(
    connection: &Connection,
    id: ToolCallId,
) -> Result<ToolCall, StoreError> {
    let mut statement = connection
        .prepare_cached(&format!(
            "SELECT {TOOL_CALL_COLUMNS} FROM tool_calls WHERE id = :id"
        ))
        .map_err(|error| query_failed("preparing the tool call query", error))?;
    let wire = statement
        .query_row(named_params! { ":id": id.to_string() }, |row| {
            Ok(tool_call_wire(row))
        })
        .map_err(|error| row_failed(TOOL_CALL, &id, "loading a tool call", error))??;
    tool_call_from_wire(wire)
}

pub(super) fn load_run_tool_calls(
    connection: &Connection,
    run_id: RunId,
) -> Result<Vec<ToolCall>, StoreError> {
    let mut statement = connection
        .prepare_cached(&format!(
            "SELECT {TOOL_CALL_COLUMNS} FROM tool_calls WHERE run_id = :run_id \
             ORDER BY created_at, id"
        ))
        .map_err(|error| query_failed("preparing the run tool call query", error))?;
    let rows = statement
        .query_map(named_params! { ":run_id": run_id.to_string() }, |row| {
            Ok(tool_call_wire(row))
        })
        .map_err(|error| query_failed("listing the tool calls of a run", error))?;
    let mut calls = Vec::new();
    for row in rows {
        let wire = row.map_err(|error| query_failed("reading a tool call row", error))??;
        calls.push(tool_call_from_wire(wire)?);
    }
    Ok(calls)
}

fn tool_call_from_wire(wire: ToolCallWire) -> Result<ToolCall, StoreError> {
    ToolCall::try_from(wire).map_err(|source| StoreError::InvalidRecord {
        record: TOOL_CALL,
        source,
    })
}

fn tool_call_wire(row: &Row<'_>) -> Result<ToolCallWire, StoreError> {
    Ok(ToolCallWire {
        schema_version: schema_version(row, TOOL_CALL)?,
        id: decode_id(TOOL_CALL, "id", &text(row, TOOL_CALL, "id")?)?,
        run_id: decode_id(TOOL_CALL, "run_id", &text(row, TOOL_CALL, "run_id")?)?,
        step_id: decode_id(TOOL_CALL, "step_id", &text(row, TOOL_CALL, "step_id")?)?,
        tool_id: text(row, TOOL_CALL, "tool_id")?,
        tool_version: text(row, TOOL_CALL, "tool_version")?,
        input: decode_payload(TOOL_CALL, "input", &text(row, TOOL_CALL, "input_json")?)?,
        state: decode_tool_call_state(TOOL_CALL, &text(row, TOOL_CALL, "state")?)?,
        revision: decode_revision(TOOL_CALL, integer(row, TOOL_CALL, "revision")?)?,
        created_at: decode_timestamp(
            TOOL_CALL,
            "created_at",
            &text(row, TOOL_CALL, "created_at")?,
        )?,
        updated_at: decode_timestamp(
            TOOL_CALL,
            "updated_at",
            &text(row, TOOL_CALL, "updated_at")?,
        )?,
        started_at: decode_optional_timestamp(
            TOOL_CALL,
            "started_at",
            optional_text(row, TOOL_CALL, "started_at")?,
        )?,
        finished_at: decode_optional_timestamp(
            TOOL_CALL,
            "finished_at",
            optional_text(row, TOOL_CALL, "finished_at")?,
        )?,
        failure: decode_failure(
            TOOL_CALL,
            optional_text(row, TOOL_CALL, "failure_kind")?,
            optional_text(row, TOOL_CALL, "failure_message")?,
        )?,
        output: decode_optional_payload(
            TOOL_CALL,
            "output",
            optional_text(row, TOOL_CALL, "output_json")?,
        )?,
        approvals: decode_approvals(TOOL_CALL, &text(row, TOOL_CALL, "approvals_json")?)?,
    })
}

// -- shared column plumbing -------------------------------------------------

fn schema_version(row: &Row<'_>, record: &'static str) -> Result<u32, StoreError> {
    let stored = integer(row, record, "schema_version")?;
    u32::try_from(stored).map_err(|_| StoreError::ColumnEncoding {
        record,
        field: "schema_version",
        reason: format!("{stored} is not a representable schema version"),
    })
}

fn text(row: &Row<'_>, record: &'static str, field: &'static str) -> Result<String, StoreError> {
    row.get(field).map_err(|error| column(record, field, error))
}

fn optional_text(
    row: &Row<'_>,
    record: &'static str,
    field: &'static str,
) -> Result<Option<String>, StoreError> {
    row.get(field).map_err(|error| column(record, field, error))
}

fn integer(row: &Row<'_>, record: &'static str, field: &'static str) -> Result<i64, StoreError> {
    row.get(field).map_err(|error| column(record, field, error))
}

fn column(record: &'static str, field: &'static str, error: rusqlite::Error) -> StoreError {
    StoreError::ColumnEncoding {
        record,
        field,
        reason: error.to_string(),
    }
}

/// Turns SQLite's "no rows" into the store's own absence error.
fn row_failed(
    record: &'static str,
    id: &dyn std::fmt::Display,
    operation: &'static str,
    error: rusqlite::Error,
) -> StoreError {
    if matches!(error, rusqlite::Error::QueryReturnedNoRows) {
        return StoreError::NotFound {
            record,
            id: id.to_string(),
        };
    }
    query_failed(operation, error)
}

fn missing_row(
    record: &'static str,
    id: &dyn std::fmt::Display,
    updated: usize,
) -> Result<(), StoreError> {
    if updated == 0 {
        return Err(StoreError::NotFound {
            record,
            id: id.to_string(),
        });
    }
    Ok(())
}
