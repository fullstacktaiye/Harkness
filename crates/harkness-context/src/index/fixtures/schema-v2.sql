CREATE INDEX chunks_by_id ON chunks (chunk_id);

CREATE INDEX file_versions_by_content ON file_versions (content_sha256);

CREATE INDEX files_by_file_version ON files (file_version_id);

CREATE INDEX symbols_by_name ON symbols (name);

CREATE TABLE chunks (
    file_version_id TEXT    NOT NULL REFERENCES file_versions(file_version_id) ON DELETE CASCADE,
    chunk_id        TEXT    NOT NULL,
    anchor          TEXT    NOT NULL,
    ordinal         INTEGER NOT NULL,
    start_byte      INTEGER NOT NULL,
    end_byte        INTEGER NOT NULL,
    start_line      INTEGER,
    end_line        INTEGER,
    chunk_sha256    TEXT    NOT NULL,
    symbol_id       TEXT,
    PRIMARY KEY (file_version_id, chunk_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE contents (
    content_sha256 TEXT    PRIMARY KEY,
    byte_size      INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

CREATE TABLE file_versions (
    file_version_id  TEXT    PRIMARY KEY,
    content_sha256   TEXT    NOT NULL REFERENCES contents(content_sha256) ON DELETE CASCADE,
    path             BLOB    NOT NULL,
    language         TEXT,
    transcoded       INTEGER NOT NULL DEFAULT 0,
    truncated        INTEGER NOT NULL DEFAULT 0,
    chunking_version TEXT,
    parser_version   TEXT
) STRICT, WITHOUT ROWID;

CREATE TABLE files (
    worktree_id      TEXT    NOT NULL REFERENCES worktrees(worktree_id) ON DELETE CASCADE,
    path             BLOB    NOT NULL,
    file_version_id  TEXT             REFERENCES file_versions(file_version_id),
    byte_size        INTEGER NOT NULL,
    mtime_ns         INTEGER,
    file_class       TEXT    NOT NULL,
    symlink          INTEGER NOT NULL,
    boundary         TEXT,
    unreadable       INTEGER NOT NULL,
    classify_version INTEGER NOT NULL,
    generation       INTEGER NOT NULL,
    PRIMARY KEY (worktree_id, path)
) STRICT, WITHOUT ROWID;

CREATE TABLE index_meta (
    id                  INTEGER PRIMARY KEY CHECK (id = 1),
    schema_version      INTEGER NOT NULL,
    parser_version      TEXT    NOT NULL,
    chunking_version    TEXT    NOT NULL,
    ranking_version     TEXT    NOT NULL,
    classify_version    TEXT    NOT NULL,
    index_generation    INTEGER NOT NULL,
    repository_identity TEXT    NOT NULL,
    created_at          TEXT    NOT NULL,
    last_opened_at      TEXT    NOT NULL
) STRICT;

CREATE TABLE pending_files (
    worktree_id      TEXT    NOT NULL REFERENCES worktrees(worktree_id) ON DELETE CASCADE,
    generation       INTEGER NOT NULL,
    path             BLOB    NOT NULL,
    file_version_id  TEXT             REFERENCES file_versions(file_version_id),
    keep_version     INTEGER NOT NULL,
    removed          INTEGER NOT NULL,
    byte_size        INTEGER NOT NULL,
    mtime_ns         INTEGER,
    file_class       TEXT    NOT NULL,
    symlink          INTEGER NOT NULL,
    boundary         TEXT,
    unreadable       INTEGER NOT NULL,
    classify_version INTEGER NOT NULL,
    PRIMARY KEY (worktree_id, generation, path)
) STRICT, WITHOUT ROWID;

CREATE TABLE symbols (
    file_version_id TEXT    NOT NULL REFERENCES file_versions(file_version_id) ON DELETE CASCADE,
    symbol_id       TEXT    NOT NULL,
    name            TEXT    NOT NULL,
    qualified_path  TEXT    NOT NULL,
    kind            TEXT    NOT NULL,
    start_byte      INTEGER NOT NULL,
    end_byte        INTEGER NOT NULL,
    start_line      INTEGER,
    end_line        INTEGER,
    PRIMARY KEY (file_version_id, symbol_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE worktrees (
    worktree_id        TEXT    PRIMARY KEY,
    root_path          BLOB    NOT NULL,
    next_generation    INTEGER NOT NULL DEFAULT 0,
    last_generation    INTEGER NOT NULL DEFAULT 0,
    last_reconciled_at TEXT
) STRICT, WITHOUT ROWID;
