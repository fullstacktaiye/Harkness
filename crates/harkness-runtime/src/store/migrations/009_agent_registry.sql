-- Migration 9: per-subject trust records, and the runtime state of one ACP agent.
--
-- `agents.json` holds what a user *configured* — an identifier, a command, its
-- arguments, its environment allowlist, and whether it is enabled. Everything
-- here is what Harkness *observed*: the grant a user made against an exact
-- executable, and what the last conversation with that program established.
--
-- The split is the point. The configuration file stays small, diffable and
-- hand-editable, and nothing a health check learns can corrupt it; a database
-- that is deleted costs a re-trust and a re-check rather than a registration.

-- Trust records for every external subject kind, not for agents alone: #158
-- stores an MCP server here, #170 a recipe, #164 a forge account. The table is
-- created by this migration because an ACP agent is the first subject anything
-- persists.
--
-- A row is addressed by its own `id` and never by a projection of the record's
-- fields. `TrustRecord::check` ignores the display name and the executable path
-- and accepts a semver-compatible upgrade, so equality over those fields is a
-- compatibility relation rather than a key — and a revoked record and a later
-- grant about one subject would collide on such a key, letting an upsert
-- overwrite the one decision the state machine exists to preserve.
CREATE TABLE integration_trust_records (
    schema_version INTEGER NOT NULL,
    id             TEXT    NOT NULL PRIMARY KEY,
    -- Which vocabulary `subject_ref` is spelled in. Kept beside the record
    -- rather than read out of it so the subject lookup is one index seek, and
    -- re-checked against the decoded record on load.
    subject_kind   TEXT    NOT NULL,
    -- The subject's own identifier — an `agents.json` registration id for an
    -- agent. Deliberately absent from the record itself: a `TrustRecord` says
    -- what was trusted, and this says what it was trusted *for*.
    subject_ref    TEXT    NOT NULL,
    -- The frozen `TrustRecordWire` form, schema-versioned independently of the
    -- run records. One strict JSON value rather than a column per field, so the
    -- state, its invalidation reason, the identity basis it was granted
    -- against, and the grant time cannot be observed from different writes.
    record_json    TEXT    NOT NULL,
    -- When this row was written, which is not `granted_at`: a re-grant moves
    -- the grant time on the record, and the audit order is the order rows
    -- arrived.
    recorded_at    TEXT    NOT NULL
) STRICT;

-- Serves the only lookup there is: every record about one subject, oldest
-- first, so a reader sees the decision history in the order it happened.
CREATE INDEX integration_trust_records_by_subject
    ON integration_trust_records (subject_kind, subject_ref, recorded_at, id);

-- One row per registered agent, replaced whole by each observation.
--
-- There is no foreign key to a registration, because a registration is a line
-- in `agents.json` and not a row. A removed agent's state is deleted with it;
-- state left behind for an identifier a user later reuses would answer for a
-- program nobody checked.
CREATE TABLE agent_runtime_state (
    schema_version INTEGER NOT NULL,
    agent_id       TEXT    NOT NULL PRIMARY KEY,
    -- Whether the agent wants a person to authenticate it. A state label only:
    -- no credential is stored here or anywhere else in Harkness.
    auth_status    TEXT    NOT NULL,
    -- Tag and payload, exactly as `approvals` spells a decision: the tag says
    -- which answer this is and the column beside it carries the one value that
    -- answer has. `advertised_protocol_version` is present only for
    -- `unsupported_protocol_version`, and preserves what the agent selected so
    -- a surface can name it rather than saying "some other version".
    compatibility  TEXT    NOT NULL,
    advertised_protocol_version INTEGER,
    -- The last *successful* `initialize`: the version the agent reports for
    -- itself, the version both sides negotiated, and the capability snapshot.
    -- One strict versioned JSON value, because a capability set is a composite
    -- that has to be read back exactly as it was written; NULL until an
    -- `initialize` has ever succeeded.
    last_initialize_json TEXT,
    -- The last health check, successful or not, with its typed failure kind and
    -- how far teardown had to go. NULL until one has ever run.
    last_health_json TEXT,
    updated_at     TEXT    NOT NULL
) STRICT;
