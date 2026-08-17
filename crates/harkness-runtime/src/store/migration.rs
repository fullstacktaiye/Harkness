//! The versioned schema ladder applied to `runtime.db`.
//!
//! `PRAGMA user_version` records how far the ladder has been climbed. A build
//! applies every migration above that number in ascending order, each inside
//! its own transaction that also advances the recorded version, so an
//! interrupted upgrade either leaves the previous version intact or lands the
//! next one whole. A version above [`SCHEMA_VERSION`] is refused as an upgrade
//! request rather than treated as corruption, exactly as the project catalog
//! refuses a newer `projects.json`.
//!
//! # Two processes climbing the same ladder
//!
//! Reading `user_version` outside a write transaction only says what was true
//! at the moment of the read. Two Harkness processes starting against the same
//! new database both see version 0, and the second would replay a migration the
//! first has already committed — `CREATE TABLE tasks` against a database that
//! already has one. Each step therefore takes the write lock with `BEGIN
//! IMMEDIATE` and re-reads `user_version` underneath it, treating a version that
//! moved as another process's work rather than as a step still owed.

use rusqlite::{Connection, Transaction, TransactionBehavior};

use super::error::{StoreError, query_failed};

/// One numbered step of the schema ladder.
pub(super) struct Migration {
    /// `PRAGMA user_version` value this step establishes.
    pub(super) version: i64,
    /// Statements applied to reach that version.
    pub(super) statements: &'static str,
}

/// Every migration this build can apply, in ascending version order.
pub(super) const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        statements: include_str!("migrations/001_initial_schema.sql"),
    },
    Migration {
        version: 2,
        statements: include_str!("migrations/002_events_and_artifacts.sql"),
    },
    Migration {
        version: 3,
        statements: include_str!("migrations/003_workspace_trust.sql"),
    },
    Migration {
        version: 4,
        statements: include_str!("migrations/004_policy_decisions.sql"),
    },
    Migration {
        version: 5,
        statements: include_str!("migrations/005_approvals.sql"),
    },
    Migration {
        version: 6,
        statements: include_str!("migrations/006_approval_integration_identity.sql"),
    },
    Migration {
        version: 7,
        statements: include_str!("migrations/007_run_leases_and_retry.sql"),
    },
    Migration {
        version: 8,
        statements: include_str!("migrations/008_workspace_snapshots.sql"),
    },
];

/// Newest schema version this build understands.
pub const SCHEMA_VERSION: i64 = MIGRATIONS[MIGRATIONS.len() - 1].version;

/// Reads the schema version recorded in the database.
pub(super) fn recorded_version(connection: &Connection) -> Result<i64, StoreError> {
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|error| query_failed("reading the schema version", error))
}

/// Refuses a database written by a newer build before anything touches it.
///
/// This runs before the connection requests WAL, so refusing an unsupported
/// database leaves its bytes exactly as they were found.
pub(super) fn refuse_newer_schema(
    connection: &Connection,
    migrations: &[Migration],
) -> Result<(), StoreError> {
    let found = recorded_version(connection)?;
    let maximum = migrations.last().map_or(0, |migration| migration.version);
    if found > maximum {
        return Err(StoreError::SchemaTooNew { found, maximum });
    }
    Ok(())
}

/// Applies every pending migration in ascending order.
///
/// Each migration and its `user_version` bump share one `BEGIN IMMEDIATE`
/// transaction, so a crash between two migrations cannot leave a half-applied
/// schema recorded as complete, and a concurrent migrator cannot slip a commit
/// between the version this one read and the statements it runs.
///
/// The recorded version is re-derived after every step rather than iterated
/// over a snapshot: a step another process landed first is simply no longer
/// pending. The loop terminates because each turn either advances
/// `user_version` or observes that someone else did.
pub(super) fn apply(
    connection: &mut Connection,
    migrations: &[Migration],
) -> Result<(), StoreError> {
    let maximum = migrations.last().map_or(0, |migration| migration.version);
    loop {
        let found = recorded_version(connection)?;
        // A newer build may have climbed past this one's ladder while this
        // process was opening. That is the same refusal `refuse_newer_schema`
        // makes, arrived at a moment later.
        if found > maximum {
            return Err(StoreError::SchemaTooNew { found, maximum });
        }
        let Some(migration) = migrations.iter().find(|entry| entry.version > found) else {
            return Ok(());
        };

        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| query_failed("starting a migration", error))?;
        if recorded_version(&transaction)? >= migration.version {
            // Another process committed this step between the read above and
            // the write lock. Roll back and re-derive what is still pending.
            continue;
        }
        apply_one(&transaction, migration)?;
        transaction
            .commit()
            .map_err(|error| StoreError::Migration {
                version: migration.version,
                source: error,
            })?;
    }
}

fn apply_one(transaction: &Transaction<'_>, migration: &Migration) -> Result<(), StoreError> {
    let failed = |error| StoreError::Migration {
        version: migration.version,
        source: error,
    };
    transaction
        .execute_batch(migration.statements)
        .map_err(failed)?;
    // `PRAGMA user_version` takes no bound parameter, so the version has to be
    // formatted into the statement. It is an integer constant from this table,
    // never caller input.
    transaction
        .execute_batch(&format!("PRAGMA user_version = {}", migration.version))
        .map_err(failed)
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::{
        MIGRATIONS, Migration, SCHEMA_VERSION, apply, recorded_version, refuse_newer_schema,
    };
    use crate::store::error::StoreError;

    #[test]
    fn migration_versions_ascend_without_gaps_from_one() {
        let versions = MIGRATIONS
            .iter()
            .map(|migration| migration.version)
            .collect::<Vec<_>>();
        let expected = (1..=i64::try_from(MIGRATIONS.len()).unwrap()).collect::<Vec<_>>();
        assert_eq!(versions, expected);
        assert_eq!(SCHEMA_VERSION, expected[expected.len() - 1]);
    }

    #[test]
    fn migrations_apply_in_order_and_record_user_version() {
        let mut connection = Connection::open_in_memory().unwrap();
        assert_eq!(recorded_version(&connection).unwrap(), 0);

        apply(&mut connection, MIGRATIONS).unwrap();
        assert_eq!(recorded_version(&connection).unwrap(), SCHEMA_VERSION);

        let tables = table_names(&connection);
        assert_eq!(
            tables,
            [
                "approvals",
                "artifacts",
                "run_events",
                "runs",
                "runtime_leases",
                "steps",
                "tasks",
                "tool_calls",
                "workspace_snapshots",
                "workspace_trust"
            ]
        );

        // Re-applying is a no-op rather than a duplicate-table failure.
        apply(&mut connection, MIGRATIONS).unwrap();
        assert_eq!(recorded_version(&connection).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn a_newer_schema_is_refused_as_upgrade_not_corruption() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 3))
            .unwrap();

        let error = refuse_newer_schema(&connection, MIGRATIONS).unwrap_err();
        assert_eq!(error.kind(), "schema_too_new");
        assert!(
            matches!(error, StoreError::SchemaTooNew { found, maximum }
                if found == SCHEMA_VERSION + 3 && maximum == SCHEMA_VERSION),
            "unexpected refusal: {error}"
        );
    }

    #[test]
    fn a_failed_migration_rolls_back_and_keeps_the_previous_version() {
        const BROKEN: &[Migration] = &[
            Migration {
                version: 1,
                statements: "CREATE TABLE kept (id TEXT PRIMARY KEY) STRICT;",
            },
            Migration {
                version: 2,
                statements: "CREATE TABLE half (id TEXT PRIMARY KEY) STRICT;\n\
                             CREATE TABLE half (id TEXT PRIMARY KEY) STRICT;",
            },
        ];

        let mut connection = Connection::open_in_memory().unwrap();
        let error = apply(&mut connection, BROKEN).unwrap_err();

        assert_eq!(error.kind(), "migration_failed");
        assert_eq!(recorded_version(&connection).unwrap(), 1);
        assert_eq!(table_names(&connection), ["kept"]);
    }

    fn table_names(connection: &Connection) -> Vec<String> {
        let mut statement = connection
            .prepare(
                "SELECT name FROM sqlite_schema \
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .unwrap();
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }
}
