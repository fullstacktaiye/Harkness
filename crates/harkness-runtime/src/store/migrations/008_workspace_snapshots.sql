-- Migration 8: workspace snapshots as durable evidence.
--
-- A snapshot names the exact workspace a run read: repository identity,
-- worktree root, HEAD, branch, the staged, dirty and untracked path sets and
-- their digests, and the configuration and index generations it was taken
-- under. Every context item a run is later audited against points at one, so it
-- has to live on the evidence side of ADR-0004's split rather than in the
-- disposable index cache. Deleting `<data_dir>/context/` must cost warm-up time
-- and nothing else; a snapshot stored there would make it cost the audit trail.
--
-- `payload_json` is the frozen `harkness-context` wire form, versioned by that
-- crate's own `schema_version` field inside the document rather than by this
-- table's. The two ladders are deliberately independent: a context record and a
-- run record evolve for different reasons, exactly as an integration trust
-- record does.
--
-- The envelope columns are denormalized *out* of that payload so the table is
-- legible from the database alone and so a run's captures can be found without
-- parsing every document. They are compared against the payload on load, and a
-- row where they disagree is refused by name — the same rule an artifact's
-- `storage_path` follows.

CREATE TABLE workspace_snapshots (
    schema_version  INTEGER NOT NULL,
    id              TEXT    NOT NULL PRIMARY KEY,
    -- NULL for a standalone capture. A snapshot taken to answer "what does this
    -- workspace look like" belongs to nobody, and forcing one to name a run
    -- would mean inventing a run to hold it.
    run_id          TEXT    REFERENCES runs (id),
    project_id      TEXT    NOT NULL,
    -- The composite identity ADR-0008 fixes. Derivable from `payload_json` and
    -- stored anyway, so "has this workspace been captured before" is an index
    -- seek rather than a scan of every document.
    snapshot_digest TEXT    NOT NULL,
    payload_json    TEXT    NOT NULL,
    captured_at     TEXT    NOT NULL
) STRICT;

CREATE INDEX snapshots_by_run ON workspace_snapshots (run_id, captured_at, id);
