-- Migration 2: the append-only run event log and the artifact metadata table.
--
-- Migration 1 records what a run *is* at this moment. These two tables record
-- how it got there and where the content too large for a row actually lives.
-- They land together because they share one contract: a state change and the
-- event describing it commit in a single transaction, and an event whose
-- payload outgrows the inline threshold becomes an artifact plus a reference,
-- so neither table can be reasoned about without the other.
--
-- Both tables are STRICT and both carry the durable-record schema version their
-- siblings carry, so a row written by a future build reads as an upgrade
-- request rather than as a corrupt column.

-- Migration 1 gave `steps` a redundant `UNIQUE (id, run_id)` so a tool call's
-- denormalized run could not disagree with the run its step belongs to. The two
-- tables below need the same guarantee against tool calls and artifacts, and a
-- unique index is the parent key a composite foreign key can reference.
CREATE UNIQUE INDEX tool_calls_identity_by_run ON tool_calls (id, run_id);

CREATE TABLE artifacts (
    schema_version INTEGER NOT NULL,
    id             TEXT    NOT NULL PRIMARY KEY,
    run_id         TEXT    NOT NULL REFERENCES runs (id),
    step_id        TEXT,
    tool_call_id   TEXT,
    -- A caller-facing label such as `build.log`. It never becomes a path
    -- component: the file is named by `id`, so a hostile name is inert text.
    name           TEXT    NOT NULL,
    media_type     TEXT    NOT NULL,
    byte_size      INTEGER NOT NULL,
    -- Hex SHA-256 of the bytes as they were stored, so any consumer can prove
    -- an artifact has not been altered since it was recorded.
    sha256         TEXT    NOT NULL,
    -- Relative to the data directory, and derivable from (run_id, id). It is
    -- stored anyway so the layout is legible from the database alone, and it is
    -- checked against the derived form on every read: a tampered row cannot
    -- redirect a read or a write at an arbitrary file.
    storage_path   TEXT    NOT NULL,
    created_at     TEXT    NOT NULL,
    -- What was true when the artifact was finalized. Reads probe the file and
    -- refine this, so a deleted file degrades a read rather than failing it.
    availability   TEXT    NOT NULL,
    -- Containment is enforced by the database rather than re-checked in Rust: an
    -- artifact may only name a step or a call that already belongs to the run it
    -- claims. A NULL association satisfies a composite key, so "no step" stays
    -- unconstrained while "that step" must be this run's.
    FOREIGN KEY (step_id, run_id) REFERENCES steps (id, run_id),
    FOREIGN KEY (tool_call_id, run_id) REFERENCES tool_calls (id, run_id),
    -- The parent key `run_events.artifact_id` needs, for the same reason.
    UNIQUE (id, run_id)
) STRICT;

CREATE INDEX artifacts_by_run ON artifacts (run_id);

CREATE TABLE run_events (
    schema_version INTEGER NOT NULL,
    run_id         TEXT    NOT NULL REFERENCES runs (id),
    -- Per-run monotonic, allocated inside the transaction that inserts the row.
    seq            INTEGER NOT NULL,
    at             TEXT    NOT NULL,
    -- Free text rather than a checked enumeration: a kind added by a later
    -- build must not require a migration, and a kind this build does not know
    -- renders as an opaque timeline entry instead of failing the read.
    kind           TEXT    NOT NULL,
    step_id        TEXT,
    tool_call_id   TEXT,
    artifact_id    TEXT,
    -- Bounded by the inline payload threshold; a payload above it is written to
    -- an artifact and replaced here by a reference to it.
    payload_json   TEXT    NOT NULL,
    -- WITHOUT ROWID makes (run_id, seq) the storage order, so reading a run's
    -- timeline in sequence is a sequential scan rather than an index lookup per
    -- row, and the primary key is what enforces per-run uniqueness.
    PRIMARY KEY (run_id, seq),
    -- Every association is to something of this run. A timeline that could name
    -- another run's step would be a worse failure than a refused write: nothing
    -- downstream re-checks it, and the wrong step would simply be rendered.
    FOREIGN KEY (step_id, run_id) REFERENCES steps (id, run_id),
    FOREIGN KEY (tool_call_id, run_id) REFERENCES tool_calls (id, run_id),
    FOREIGN KEY (artifact_id, run_id) REFERENCES artifacts (id, run_id)
) STRICT, WITHOUT ROWID;
