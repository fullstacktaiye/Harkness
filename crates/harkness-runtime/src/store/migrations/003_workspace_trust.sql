-- Migration 3: trust decisions bound to a project identity and canonical root.
--
-- Trust is separate from the project catalog because it belongs to runtime
-- policy state and must never make a read-only catalog operation rewrite
-- projects.json. One row per project records the latest explicit decision; an
-- absent row is untrusted.

CREATE TABLE workspace_trust (
    schema_version INTEGER NOT NULL,
    project_id     TEXT    NOT NULL PRIMARY KEY,
    canonical_root TEXT   NOT NULL,
    state          TEXT    NOT NULL,
    decided_at     TEXT    NOT NULL
) STRICT;
