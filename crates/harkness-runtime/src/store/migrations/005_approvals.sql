-- Migration 5: durable approval requests and the decisions that resolve them.
--
-- Approvals get their own table rather than joining the per-record
-- `approvals_json` audit history, because they are a different thing. That
-- column is a bounded, ordered trail of decisions already made about one
-- record. This table is the *queue*: a question with an identity, a lifecycle,
-- an expiry, and the binding fields a grant is matched on, which has to be
-- listed across runs on restart and answered from either front end.
--
-- Every write here is short. The waiting call holds no transaction at all: the
-- request is inserted and committed before any surface is notified, and the
-- decision is a second, equally short write.

CREATE TABLE approvals (
    schema_version    INTEGER NOT NULL,
    id                TEXT    NOT NULL PRIMARY KEY,
    run_id            TEXT    NOT NULL REFERENCES runs (id),
    tool_call_id      TEXT    NOT NULL,
    -- The resolved identity the answer authorizes. Both halves are matched on,
    -- because a new version is code the approver never saw.
    tool_id           TEXT    NOT NULL,
    tool_version      TEXT    NOT NULL,
    -- The descriptor's declared capabilities as a JSON array of strings, sorted
    -- and deduplicated. A capability-scoped grant is matched by subset against
    -- this, so the set is stored rather than the one capability a scope names.
    capabilities_json TEXT    NOT NULL,
    -- Hex SHA-256 of the canonical encoding of the validated input. An exact
    -- call grant is bound to it, so changing any input field defeats the match.
    input_hash        TEXT    NOT NULL,
    -- A human-readable digest, never the input. The raw value stays in
    -- `tool_calls.input_json`, where a surface can expand it on demand.
    input_summary     TEXT    NOT NULL,
    -- Workspace identity, both halves. NULL project_id mirrors `tasks`: a run
    -- may target a workspace the catalog does not know, and absent matches only
    -- absent.
    project_id        TEXT,
    canonical_root    TEXT    NOT NULL,
    risk              TEXT    NOT NULL,
    -- What was asked for and what may actually be granted. Both are stored so a
    -- record whose scope the risk ceiling reduced shows the downgrade instead of
    -- claiming a breadth that was never honored.
    requested_scope   TEXT    NOT NULL,
    effective_scope   TEXT    NOT NULL,
    state             TEXT    NOT NULL,
    created_at        TEXT    NOT NULL,
    expires_at        TEXT,
    -- When the request left `pending`, whether or not anybody answered it.
    resolved_at       TEXT,
    -- The decision, present only when a human made one. An expired, cancelled,
    -- or superseded request has a resolved_at and no decision, because nobody
    -- answered it and recording a refusal here would make the audit claim one.
    decided_via       TEXT,
    decision_verdict  TEXT,
    decision_scope    TEXT,
    decision_reason   TEXT,
    -- Containment is enforced by the database rather than re-checked in Rust: an
    -- approval may only hold a tool call that already belongs to the run it
    -- claims, exactly as an event or an artifact may.
    FOREIGN KEY (tool_call_id, run_id) REFERENCES tool_calls (id, run_id)
) STRICT;

-- Serves the restart listing directly: pending first, then in the order a
-- surface presents them.
CREATE INDEX approvals_by_state ON approvals (state, created_at, id);
CREATE INDEX approvals_by_run ON approvals (run_id, created_at, id);
