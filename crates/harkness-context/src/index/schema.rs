//! The cache's table layout, and which component owns each table.
//!
//! # One statement, one version
//!
//! [`INDEX_SCHEMA`] is the *whole* layout, applied in one transaction when a
//! cache is created. There is no migration ladder here and none may be added:
//! the cache is disposable (ADR-0004), so an older layout is quarantined and
//! rebuilt rather than upgraded, and a data-preserving migration would be a
//! promise about derived rows that nothing needs kept.
//!
//! That is why [`INDEX_SCHEMA_VERSION`] is bumped by *any* change to the
//! statement below — a column, an index, a constraint — and why
//! [`fixtures/schema-v2.sql`](https://github.com/fullstacktaiye/harkness/blob/main/crates/harkness-context/src/index/fixtures/schema-v2.sql)
//! is committed beside it. The fixture is what a test compares the live
//! `sqlite_schema` against, so editing the DDL without bumping the version
//! fails a test rather than leaving already-written caches silently addressed
//! by a build that expects different columns.
//!
//! # `files` and `pending_files` are two tables for one reason
//!
//! A `files` row is what readers see. A batch in flight needs somewhere to put
//! the row it is *going* to publish, and it cannot be the same row: overwriting
//! it and tagging it with an uncommitted generation makes the committed record
//! invisible for the length of the batch, and an abandoned batch then strands
//! it above the watermark where the next `begin` deletes it — a file that still
//! exists, gone from the index.
//!
//! `pending_files` is therefore a staging table keyed by
//! `(worktree_id, generation, path)`. A batch writes only there; `files` is
//! untouched until the commit copies the staged rows across, sweeps, and moves
//! the watermark in one transaction. The generation in its key is what lets two
//! batches on one worktree stage side by side without erasing each other.
//!
//! # The keying invariant
//!
//! Exactly one table is per-worktree: [`files`](INDEX_SCHEMA) — and its staging
//! twin, which is the same rows before anybody can see them. Everything
//! beneath it — `contents`, `file_versions`, `chunks`, `symbols` — is derived
//! from bytes and paths rather than from a checkout, so two linked worktrees at
//! one commit share every row but their own `files`. That is what makes the
//! repository-keyed cache root pay for itself, and it is why the read API in
//! [`store`](super::store) takes a [`WorktreeKey`](super::WorktreeKey) on every
//! call and publishes no join-free content query: a query that skipped the
//! `files` join would answer one worktree's question with another's rows.
//!
//! # Where the issue's sketch was wrong, and why
//!
//! [#114] proposed hanging `chunks` and `symbols` directly off `contents`,
//! keyed by the content digest alone. That cannot be right once [#113] is
//! read: which chunker runs is chosen from the file's *class and path*, so the
//! same bytes at `notes.md` and at `notes.rs` chunk differently, and a
//! [`ChunkId`](crate::ChunkId) absorbs the path deliberately so that two files
//! sharing content keep separate chunk identities. Chunking is a function of
//! `(path, bytes)`, which is exactly what a
//! [`FileVersionId`](crate::FileVersionId) names — so `file_versions` sits
//! between `contents` and the derived rows, and the deduplication the issue
//! asked for still holds where it was asked for: two worktrees of one
//! repository see the same paths, so they share one `file_versions` row and one
//! set of chunks.
//!
//! [#113]: https://github.com/fullstacktaiye/harkness/issues/113
//! [#114]: https://github.com/fullstacktaiye/harkness/issues/114

use super::IndexComponent;

/// Newest cache table layout this build understands.
///
/// Bumped from `1` by [#114]: the single-row metadata table gained
/// `classify_version` and `last_opened_at`, and the six content tables below
/// joined it. A cache written at `1` holds no content tables at all, so a build
/// that adopted one would address columns that are not there — which is what
/// the quarantine-and-recreate path exists for.
///
/// [#114]: https://github.com/fullstacktaiye/harkness/issues/114
pub const INDEX_SCHEMA_VERSION: u32 = 2;

/// The whole cache layout, applied in one transaction at creation.
///
/// `STRICT` on every table so a column cannot quietly hold a value of another
/// type, and `WITHOUT ROWID` on every table whose primary key *is* its identity
/// — which is all of them except `index_meta`, whose one row is addressed by a
/// constant.
///
/// A path is a `BLOB` rather than a `TEXT`. Git reports byte strings and a
/// filename on Unix may hold any byte except `/` and NUL, so a `TEXT` column
/// would either refuse ordinary files or store a lossy rewrite of their names —
/// and a lossy name is a name that cannot be opened. [`RepoPath`] round-trips
/// through these bytes exactly.
///
/// What is *not* stored is as deliberate as what is. `file_versions` records
/// `truncated`, because a file whose chunk set hit its budget is only partly
/// indexed and nothing short of re-chunking could tell; it does not record which
/// chunker ran, because that is a pure function of the class and path the row
/// already holds, and a derivable column is one that can disagree with what it
/// was derived from.
///
/// [`RepoPath`]: crate::RepoPath
pub const INDEX_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS index_meta (
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

CREATE TABLE IF NOT EXISTS worktrees (
    worktree_id        TEXT    PRIMARY KEY,
    root_path          BLOB    NOT NULL,
    next_generation    INTEGER NOT NULL DEFAULT 0,
    last_generation    INTEGER NOT NULL DEFAULT 0,
    last_reconciled_at TEXT
) STRICT, WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS contents (
    content_sha256 TEXT    PRIMARY KEY,
    byte_size      INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS file_versions (
    file_version_id  TEXT    PRIMARY KEY,
    content_sha256   TEXT    NOT NULL REFERENCES contents(content_sha256) ON DELETE CASCADE,
    path             BLOB    NOT NULL,
    language         TEXT,
    transcoded       INTEGER NOT NULL DEFAULT 0,
    truncated        INTEGER NOT NULL DEFAULT 0,
    chunking_version TEXT,
    parser_version   TEXT
) STRICT, WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS file_versions_by_content ON file_versions (content_sha256);

CREATE TABLE IF NOT EXISTS files (
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
CREATE INDEX IF NOT EXISTS files_by_file_version ON files (file_version_id);

CREATE TABLE IF NOT EXISTS pending_files (
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

CREATE TABLE IF NOT EXISTS chunks (
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
CREATE INDEX IF NOT EXISTS chunks_by_id ON chunks (chunk_id);

CREATE TABLE IF NOT EXISTS symbols (
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
CREATE INDEX IF NOT EXISTS symbols_by_name ON symbols (name);";

/// Tables no component owns, which therefore survive every component skew.
///
/// `files` is here on purpose and it is the interesting entry. A row of it is a
/// true record that a path existed at a size with a modification time, and only
/// its *class* was decided by the rules `classify_version` names — so a
/// classification upgrade leaves the row and marks it, rather than throwing
/// away a walk of the whole repository. The marking is the row's own
/// `classify_version` column, which is why nothing rewrites it in bulk.
pub const CORE_TABLES: &[&str] = &[
    "index_meta",
    "worktrees",
    "contents",
    "file_versions",
    "files",
    "pending_files",
];

impl IndexComponent {
    /// Tables whose every row was produced by this component's version.
    ///
    /// A skew empties exactly these and nothing else. The list is data rather
    /// than a `match` arm spread through the invalidation code so that a test
    /// can hold the schema to it — `every_table_is_owned_once` — and a table
    /// added without an owner fails there instead of silently surviving the
    /// upgrade that invalidated it.
    #[must_use]
    pub const fn owned_tables(self) -> &'static [&'static str] {
        match self {
            Self::Parser => &["symbols"],
            Self::Chunking => &["chunks"],
            // [#121] has not landed, so no table holds a score yet. The empty
            // list is the honest statement of that, and it is what the
            // ranking-skew path already reads — so the issue that adds a
            // scoring table adds it here and inherits the invalidation.
            //
            // [#121]: https://github.com/fullstacktaiye/harkness/issues/121
            Self::Ranking => &[],
            // A file row is not the classifier's product; only its `file_class`
            // is, and the row records which rules decided it.
            Self::Classify => &[],
        }
    }

    /// Columns this component's skew nulls out, as `(table, column)`.
    ///
    /// A `file_versions` row records the version its derived rows were produced
    /// under. Emptying `chunks` without clearing `chunking_version` would leave
    /// every file version claiming to be chunked when none of them is, and the
    /// reconciler would skip exactly the work the invalidation created.
    #[must_use]
    pub const fn cleared_columns(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Parser => &[("file_versions", "parser_version")],
            Self::Chunking => &[("file_versions", "chunking_version")],
            Self::Ranking | Self::Classify => &[],
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use rusqlite::Connection;

    use super::{CORE_TABLES, INDEX_SCHEMA, INDEX_SCHEMA_VERSION};
    use crate::index::IndexComponent;

    /// The frozen layout, as `sqlite_schema` renders it.
    const FROZEN: &str = include_str!("fixtures/schema-v2.sql");

    /// Renders a database's own account of its layout, ordered and normalized.
    ///
    /// Read back from `sqlite_schema` rather than compared against
    /// [`INDEX_SCHEMA`] as a string, because the two can differ: SQLite stores
    /// the statement it was given, and a build that applied the DDL in more
    /// than one step, or through a later `ALTER`, would still match its own
    /// source while presenting a different database.
    pub(crate) fn rendered_schema(connection: &Connection) -> String {
        let mut statement = connection
            .prepare(
                "SELECT type, name, sql FROM sqlite_schema \
                 WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' \
                 ORDER BY type, name",
            )
            .expect("sqlite_schema is readable");
        let rows = statement
            .query_map([], |row| {
                let sql: String = row.get(2)?;
                Ok(format!("{};\n", sql.trim()))
            })
            .expect("the schema rows are readable")
            .collect::<Result<Vec<_>, _>>()
            .expect("the schema rows are readable");
        rows.join("\n")
    }

    fn fresh() -> Connection {
        let connection = Connection::open_in_memory().expect("an in-memory database opens");
        connection
            .execute_batch(INDEX_SCHEMA)
            .expect("the schema applies");
        connection
    }

    /// The guard the frozen fixture exists for. A column, an index, or a
    /// constraint that changed without a version bump would leave already
    /// written caches adopted by a build addressing different columns.
    #[test]
    fn the_layout_matches_the_frozen_snapshot() {
        let rendered = rendered_schema(&fresh());
        assert_eq!(
            rendered, FROZEN,
            "the cache layout changed.\n\
             Bump INDEX_SCHEMA_VERSION and commit the new rendering as \
             src/index/fixtures/schema-v<version>.sql; a released layout is \
             replaced, never edited."
        );
        assert!(
            FROZEN.contains("classify_version"),
            "the frozen fixture should be the version-{INDEX_SCHEMA_VERSION} layout"
        );
    }

    /// Every table belongs to exactly one component, or to none deliberately.
    /// A table with no owner survives the upgrade that invalidated it, and one
    /// with two would be emptied by a skew that has nothing to do with it.
    #[test]
    fn every_table_is_owned_once() {
        let connection = fresh();
        let mut statement = connection
            .prepare(
                "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            )
            .expect("sqlite_schema is readable");
        let tables = statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("the table names are readable")
            .collect::<Result<Vec<_>, _>>()
            .expect("the table names are readable");

        for table in &tables {
            let owners = [
                IndexComponent::Parser,
                IndexComponent::Chunking,
                IndexComponent::Ranking,
                IndexComponent::Classify,
            ]
            .into_iter()
            .filter(|component| component.owned_tables().contains(&table.as_str()))
            .count();
            let core = usize::from(CORE_TABLES.contains(&table.as_str()));
            assert_eq!(
                owners + core,
                1,
                "'{table}' is owned by {owners} component(s) and listed {core} time(s) as core"
            );
        }

        for component in [
            IndexComponent::Parser,
            IndexComponent::Chunking,
            IndexComponent::Ranking,
            IndexComponent::Classify,
        ] {
            for owned in component.owned_tables() {
                assert!(
                    tables.iter().any(|table| table == owned),
                    "{component} claims '{owned}', which the schema does not define"
                );
            }
            for (table, column) in component.cleared_columns() {
                assert!(
                    tables.iter().any(|name| name == table),
                    "{component} clears '{table}.{column}', and '{table}' does not exist"
                );
            }
        }
    }

    /// Applying the schema twice is what a create racing another process's
    /// create does, so it has to be a no-op rather than an error.
    #[test]
    fn the_schema_is_idempotent() {
        let connection = fresh();
        connection
            .execute_batch(INDEX_SCHEMA)
            .expect("the schema applies a second time");
        assert_eq!(rendered_schema(&connection), FROZEN);
    }

    /// Rewrites the frozen snapshot from the layout this build applies.
    ///
    /// Run it only when [`INDEX_SCHEMA_VERSION`] has been bumped, and commit the
    /// result as a *new* file: a released layout is replaced, never edited, so
    /// that a build meeting a cache written at the old version still knows what
    /// it was looking at.
    ///
    /// ```sh
    /// cargo test -p harkness-context -- --ignored regenerate_the_frozen_schema
    /// ```
    #[test]
    #[ignore = "rewrites a committed fixture; run only when the layout version is bumped"]
    fn regenerate_the_frozen_schema() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/index/fixtures")
            .join(format!("schema-v{INDEX_SCHEMA_VERSION}.sql"));
        std::fs::create_dir_all(path.parent().expect("the fixture has a directory"))
            .expect("the fixture directory is writable");
        std::fs::write(&path, rendered_schema(&fresh())).expect("the fixture is writable");
    }

    /// `STRICT` is what keeps a column from quietly holding another type, and
    /// it is worth checking rather than trusting: a table declared without it
    /// accepts a text generation and reads it back as one.
    #[test]
    fn every_table_is_strict() {
        let connection = fresh();
        let mut statement = connection
            .prepare("SELECT name, sql FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
            .expect("sqlite_schema is readable");
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("the schema rows are readable")
            .collect::<Result<Vec<_>, _>>()
            .expect("the schema rows are readable");
        for (name, sql) in rows {
            assert!(sql.contains("STRICT"), "'{name}' is not a STRICT table");
        }
    }
}
