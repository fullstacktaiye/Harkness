-- Migration 1: the four core run-history record types.
--
-- Every table is STRICT so a wrong-typed binding fails at write time instead of
-- silently storing an affinity-converted value. Timestamps are RFC 3339 UTC
-- TEXT written at fixed nanosecond precision, so lexicographic comparison and
-- chronological comparison agree and the recency index can serve keyset pages.
--
-- Lifecycle columns mirror the durable record wire format: the persisted state
-- spelling, the optimistic-concurrency revision, the four lifecycle instants,
-- structured failure detail, and the approval audit history. Reads rebuild the
-- wire record from these columns and re-run every domain rule before handing a
-- record back, so a hand-edited row cannot enter the process as a valid record.

CREATE TABLE tasks (
    schema_version INTEGER NOT NULL,
    id             TEXT    NOT NULL PRIMARY KEY,
    title          TEXT    NOT NULL,
    workspace_root TEXT    NOT NULL,
    project_id     TEXT,
    created_at     TEXT    NOT NULL
) STRICT;

CREATE TABLE runs (
    schema_version  INTEGER NOT NULL,
    id              TEXT    NOT NULL PRIMARY KEY,
    task_id         TEXT    NOT NULL REFERENCES tasks (id),
    state           TEXT    NOT NULL,
    revision        INTEGER NOT NULL,
    created_at      TEXT    NOT NULL,
    updated_at      TEXT    NOT NULL,
    started_at      TEXT,
    finished_at     TEXT,
    failure_kind    TEXT,
    failure_message TEXT,
    approvals_json  TEXT    NOT NULL,
    -- Interruption-detection groundwork for the recovery pass: the process that
    -- currently claims the run, or NULL when no process owns it. Detecting a
    -- stale owner is deliberately not implemented here.
    owner_pid       INTEGER
) STRICT;

-- Serves the newest-first keyset page directly: the leading columns are the
-- cursor key in the order the listing scans them.
CREATE INDEX runs_by_recency ON runs (created_at DESC, id DESC);
CREATE INDEX runs_by_task ON runs (task_id);

CREATE TABLE steps (
    schema_version  INTEGER NOT NULL,
    id              TEXT    NOT NULL PRIMARY KEY,
    run_id          TEXT    NOT NULL REFERENCES runs (id),
    ordinal         INTEGER NOT NULL,
    title           TEXT    NOT NULL,
    state           TEXT    NOT NULL,
    revision        INTEGER NOT NULL,
    created_at      TEXT    NOT NULL,
    updated_at      TEXT    NOT NULL,
    started_at      TEXT,
    finished_at     TEXT,
    failure_kind    TEXT,
    failure_message TEXT,
    approvals_json  TEXT    NOT NULL,
    UNIQUE (run_id, ordinal),
    -- Redundant against the primary key on its own, but it gives tool_calls a
    -- composite key to reference so a call's denormalized run_id cannot
    -- disagree with the run its step belongs to.
    UNIQUE (id, run_id)
) STRICT;

CREATE TABLE tool_calls (
    schema_version  INTEGER NOT NULL,
    id              TEXT    NOT NULL PRIMARY KEY,
    run_id          TEXT    NOT NULL REFERENCES runs (id),
    step_id         TEXT    NOT NULL,
    tool_id         TEXT    NOT NULL,
    tool_version    TEXT    NOT NULL,
    -- Bounded by the inline payload threshold; larger values belong in the
    -- artifact store rather than in a row.
    input_json      TEXT    NOT NULL,
    output_json     TEXT,
    state           TEXT    NOT NULL,
    revision        INTEGER NOT NULL,
    created_at      TEXT    NOT NULL,
    updated_at      TEXT    NOT NULL,
    started_at      TEXT,
    finished_at     TEXT,
    failure_kind    TEXT,
    failure_message TEXT,
    approvals_json  TEXT    NOT NULL,
    -- Containment is enforced by the database rather than re-checked in Rust:
    -- a call may only name a step that already belongs to the run it claims.
    FOREIGN KEY (step_id, run_id) REFERENCES steps (id, run_id)
) STRICT;

CREATE INDEX tool_calls_by_run ON tool_calls (run_id, created_at);
CREATE INDEX tool_calls_by_step ON tool_calls (step_id, created_at);
