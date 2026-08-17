-- Migration 7: process liveness leases, and the retry provenance of a run.
--
-- `owner_pid` on `runs` has recorded which process claimed a run since
-- migration 1, and it is not enough to decide anything: a process identifier is
-- reused, so a row naming a live pid is not evidence that the process holding
-- it is the one that wrote the row. A lease adds the two things that make the
-- question answerable — a unique identity per coordinator, and an advisory lock
-- file the *kernel* releases when its holder dies, whatever killed it.
--
-- The row is the durable record and the file is the liveness oracle. Neither
-- alone would do: a row cannot notice a `SIGKILL`, and a lock file with no row
-- names nothing.

CREATE TABLE runtime_leases (
    schema_version INTEGER NOT NULL,
    id             TEXT    NOT NULL PRIMARY KEY,
    -- Audit only. Nothing decides liveness from it, for the reason above; it is
    -- here so a person reading the database can find the process in a log.
    pid            INTEGER NOT NULL,
    acquired_at    TEXT    NOT NULL,
    -- Refreshed by the owning coordinator's housekeeping thread. It can only
    -- ever *widen* the window in which a lease is treated as alive, never
    -- shorten it: the lock file is what says a holder is gone.
    renewed_at     TEXT    NOT NULL,
    -- Set when the lease was given up deliberately, or when a recovery sweep
    -- found it dead. A released lease is never probed again.
    released_at    TEXT
) STRICT;

-- Deliberately no foreign key from `runs.lease_id`. A lease is the process that
-- drove a run, not a containment parent: a run must stay loadable after its
-- lease row is collected, and collecting one must not have to rewrite history.
ALTER TABLE runs ADD COLUMN lease_id TEXT;

-- A retry is a new run for the same task and never a rewrite of the one it
-- follows, so the relationship is a column on the *new* row. The self-reference
-- is a real containment claim and is enforced: a retry naming a run that does
-- not exist would make its own provenance unreadable.
ALTER TABLE runs ADD COLUMN retry_of TEXT REFERENCES runs (id);

-- Honest-by-default: true when the attempt this run follows started any tool
-- call that could write. v0.3 never rolls back or re-applies a partial
-- mutation, so the flag is the only warning a front end has.
ALTER TABLE runs ADD COLUMN workspace_may_be_modified INTEGER NOT NULL DEFAULT 0;

-- Serves the startup sweep's candidate query directly. It is O(non-terminal
-- runs) rather than O(runs), which is what keeps recovery off the critical path
-- of a store that has recorded a year of history.
CREATE INDEX runs_by_state ON runs (state, created_at, id);
CREATE INDEX runs_by_lease ON runs (lease_id);
CREATE INDEX runs_by_retry_of ON runs (retry_of);
