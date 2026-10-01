//! The migration protocol of Effect's `Migrator` (`effect/unstable/sql/Migrator.ts`), with the
//! 54 migrations of `apps/server/src/persistence/Migrations.ts`.
//!
//! 1. `CREATE TABLE IF NOT EXISTS "effect_sql_migrations" (…)` (same text as Effect, so
//!    `sqlite_master` matches).
//! 2. In one transaction (`BEGIN`): read the latest `migration_id`; insert one row per pending
//!    migration (`{id, name}`, `created_at` defaulting to `current_timestamp`); a constraint
//!    error there means another process is migrating, which is "Locked": roll back and carry on
//!    with nothing run; then run each pending migration in order; `COMMIT`.
//! 3. Any failure rolls the whole transaction back.
//!
//! One rule is added: a database whose latest migration is above [`LATEST_MIGRATION_ID`] was
//! written by a newer server, and opening it is refused (`MigrationErrorKind::NewerSchema`).
//! Effect would silently carry on.

mod m050;
mod steps;

use crate::conn::Conn;
use crate::error::{DbError, MigrationErrorKind, Result};

pub(crate) use crate::conn::has_column;

/// The migrations table Effect's Migrator creates (default `table`).
pub const MIGRATIONS_TABLE: &str = "effect_sql_migrations";

type Step = fn(&Conn) -> rusqlite::Result<()>;

/// `(id, name, run)`; the key Effect logs is `{id}_{name}`.
pub const MIGRATIONS: &[(i64, &str, Step)] = &[
    (1, "OrchestrationEvents", steps::m001),
    (2, "OrchestrationCommandReceipts", steps::m002),
    (3, "CheckpointDiffBlobs", steps::m003),
    (4, "ProviderSessionRuntime", steps::m004),
    (5, "Projections", steps::m005),
    (6, "ProjectionThreadSessionRuntimeModeColumns", steps::m006),
    (7, "ProjectionThreadMessageAttachments", steps::m007),
    (8, "ProjectionThreadActivitySequence", steps::m008),
    (9, "ProviderSessionRuntimeMode", steps::m009),
    (10, "ProjectionThreadsRuntimeMode", steps::m010),
    (11, "OrchestrationThreadCreatedRuntimeMode", steps::m011),
    (12, "ProjectionThreadsInteractionMode", steps::m012),
    (13, "ProjectionThreadProposedPlans", steps::m013),
    (14, "ProjectionThreadProposedPlanImplementation", steps::m014),
    (15, "ProjectionTurnsSourceProposedPlan", steps::m015),
    (16, "CanonicalizeModelSelections", steps::m016),
    (17, "ProjectionThreadsArchivedAt", steps::m017),
    (18, "ProjectionThreadsArchivedAtIndex", steps::m018),
    (19, "ProjectionSnapshotLookupIndexes", steps::m019),
    (20, "AuthAccessManagement", steps::m020),
    (21, "AuthSessionClientMetadata", steps::m021),
    (22, "AuthSessionLastConnectedAt", steps::m022),
    (23, "ProjectionThreadShellSummary", steps::m023),
    (24, "BackfillProjectionThreadShellSummary", steps::m024),
    (25, "CleanupInvalidProjectionPendingApprovals", steps::m025),
    (26, "CanonicalizeModelSelectionOptions", steps::m026),
    (27, "ProviderSessionRuntimeInstanceId", steps::m027),
    (28, "ProjectionThreadSessionInstanceId", steps::m028),
    (29, "ProjectionThreadDetailOrderingIndexes", steps::m029),
    (30, "ProjectionThreadShellArchiveIndexes", steps::m030),
    (31, "AuthAuthorizationScopes", steps::m031),
    (32, "AuthPairingProofKeyThumbprint", steps::m032),
    (33, "ProjectionThreadsSettled", steps::m033),
    (34, "ProjectionThreadsSnoozed", steps::m034),
    (35, "ProjectionThreadTitleRegeneration", steps::m035),
    (36, "ProjectionThreadsPinned", steps::m036),
    (37, "ProjectionTurnsKeysetIndex", steps::m037),
    (38, "ProjectionThreadsPinOrderKey", steps::m038),
    (39, "ProjectionProjectsDefaultThreadEnvMode", steps::m039),
    (40, "ProjectionProjectFaviconPath", steps::m040),
    (41, "AuthSessionClientConnection", steps::m041),
    (42, "ProjectionThreadLinkedPullRequest", steps::m042),
    (43, "ProjectionThreadsUnsettledAt", steps::m043),
    (44, "ClearAutomaticProjectModelDefaults", steps::m044),
    (45, "ProjectionProjectsAutoPull", steps::m045),
    (46, "RepairAutomaticSettlementTimestamps", steps::m046),
    (47, "ProjectionProjectIcon", steps::m047),
    (48, "ProjectionThreadBranchPullRequest", steps::m048),
    (49, "ProjectionThreadsActiveOrderKey", steps::m049),
    (50, "ProjectionThreadPullRequests", m050::m050),
    (51, "ProjectionThreadMessageContext", steps::m051),
    (52, "ProjectionThreadTitleState", steps::m052),
    (53, "PullRequestFilesViewed", steps::m053),
    (54, "ProjectionThreadsAutoSettleDisabledAt", steps::m054),
];

/// The highest migration this build knows. No new migration may be added while the TS server
/// can still open the same database (plan §3.1); if one becomes unavoidable, number it 55+ and
/// land the same migration in TS.
pub const LATEST_MIGRATION_ID: i64 = 54;

/// What [`run`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationOutcome {
    /// `[id, name]` of every migration that ran, in order (empty when current or locked).
    pub ran: Vec<(i64, String)>,
    /// The latest migration id recorded before running (0 on a fresh database).
    pub previous_latest: i64,
    /// Another process held the migration rows (Effect's "Locked"): nothing ran.
    pub locked: bool,
}

/// Runs every pending migration (`runMigrations()`).
pub fn run(conn: &Conn) -> Result<MigrationOutcome> {
    run_through(conn, None)
}

/// `runMigrations({ toMigrationInclusive })`: only migrations up to `through` are known to the
/// loader. Used by migration tests that seed legacy data between steps.
pub fn run_through(conn: &Conn, through: Option<i64>) -> Result<MigrationOutcome> {
    exec(
        conn,
        r#"CREATE TABLE IF NOT EXISTS "effect_sql_migrations" (
  migration_id integer PRIMARY KEY NOT NULL,
  created_at datetime NOT NULL DEFAULT current_timestamp,
  name VARCHAR(255) NOT NULL
)"#,
    )
    .map_err(|error| DbError::sql("Migrator:ensureMigrationsTable", error))?;

    let result = conn.transaction(|conn| -> Result<MigrationOutcome> {
        let latest = latest_migration_id(conn)?;
        if latest > LATEST_MIGRATION_ID {
            return Err(DbError::Migration {
                kind: MigrationErrorKind::NewerSchema,
                message: format!(
                    "The database is at migration {latest}, newer than the {LATEST_MIGRATION_ID} \
                     this build knows: it was written by a newer zenith code server"
                ),
                cause: None,
            });
        }
        let required: Vec<&(i64, &str, Step)> = MIGRATIONS
            .iter()
            .filter(|(id, _, _)| *id > latest && through.is_none_or(|through| *id <= through))
            .collect();

        if !required.is_empty() {
            let placeholders = vec!["(?,?)"; required.len()].join(",");
            let insert = format!(r#"INSERT INTO "effect_sql_migrations" ("migration_id","name") VALUES {placeholders}"#);
            let mut values: Vec<rusqlite::types::Value> = Vec::with_capacity(required.len() * 2);
            for (id, name, _) in &required {
                values.push((*id).into());
                values.push(name.to_string().into());
            }
            if let Err(error) = conn.raw().execute(&insert, rusqlite::params_from_iter(values.iter())) {
                let error = DbError::sql("Migrator:insertMigrations", error);
                if error.is_constraint() {
                    return Err(DbError::Migration {
                        kind: MigrationErrorKind::Locked,
                        message: "Migrations already running".into(),
                        cause: None,
                    });
                }
                return Err(error);
            }
        }

        let mut ran = Vec::with_capacity(required.len());
        for (id, name, step) in required {
            tracing::debug!(migration_id = id, migration_name = name, "Running migration");
            step(conn).map_err(|error| DbError::Migration {
                kind: MigrationErrorKind::Failed,
                message: format!("Migration \"{id}_{name}\" failed"),
                cause: Some(Box::new(DbError::sql(format!("Migrator {id}_{name}"), error))),
            })?;
            ran.push((*id, name.to_string()));
        }
        Ok(MigrationOutcome {
            ran,
            previous_latest: latest,
            locked: false,
        })
    });

    match result {
        Err(DbError::Migration {
            kind: MigrationErrorKind::Locked,
            message,
            ..
        }) => {
            tracing::debug!("{message}");
            Ok(MigrationOutcome {
                locked: true,
                ..MigrationOutcome::default()
            })
        }
        Ok(outcome) => {
            if outcome.ran.is_empty() {
                tracing::debug!("Database schema is current");
            } else {
                let keys: Vec<String> = outcome.ran.iter().map(|(id, name)| format!("{id}_{name}")).collect();
                tracing::info!(migrations = ?keys, "Migrations ran successfully");
            }
            Ok(outcome)
        }
        Err(error) => Err(error),
    }
}

/// The latest recorded migration id, 0 when none.
pub fn latest_migration_id(conn: &Conn) -> Result<i64> {
    conn.raw()
        .query_row(
            r#"SELECT migration_id, name, created_at FROM "effect_sql_migrations" ORDER BY migration_id DESC"#,
            [],
            |row| row.get::<_, i64>(0),
        )
        .or_else(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => Ok(0),
            other => Err(other),
        })
        .map_err(|error| DbError::sql("Migrator:latestMigration", error))
}

/// Runs one statement of a migration (rows, if any, are discarded).
pub(crate) fn exec(conn: &Conn, sql: &str) -> rusqlite::Result<()> {
    let mut statement = conn.raw().prepare(sql)?;
    if statement.column_count() > 0 {
        let mut rows = statement.query([])?;
        while rows.next()?.is_some() {}
    } else {
        statement.execute([])?;
    }
    Ok(())
}

/// `sql\`…\`.pipe(Effect.catch(() => Effect.void))` (migration 023).
pub(crate) fn exec_ignoring_error(conn: &Conn, sql: &str) {
    if let Err(error) = exec(conn, sql) {
        tracing::debug!(%error, "migration statement failed and was ignored");
    }
}
