//! One SQLite connection, used the way `packages/shared/src/nodeSqliteClient.ts` uses
//! node:sqlite's `DatabaseSync`: a cache of 200 prepared statements, `BEGIN`/`COMMIT` for a
//! transaction and `SAVEPOINT effect_sql_<n>` for a nested one (Effect's `makeWithTransaction`).

use std::cell::Cell;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use crate::error::{DbError, Result};

/// `PRAGMA busy_timeout` (ms): the CLI writes the same file from another process.
pub const BUSY_TIMEOUT_MS: u64 = 5000;
/// `persistence/Layers/Sqlite.ts` `WAL_SIZE_LIMIT_BYTES`: the -wal file is cut back to this on
/// the first commit after a WAL reset.
pub const WAL_SIZE_LIMIT_BYTES: u64 = 32 * 1024 * 1024;
/// nodeSqliteClient's `prepareCacheSize` default.
pub const STATEMENT_CACHE_CAPACITY: usize = 200;

/// A connection plus the transaction depth that decides between `BEGIN` and `SAVEPOINT`.
///
/// Repositories take `&Conn`, so the same call works on its own (autocommit) or inside a
/// transaction the caller opened, exactly like an Effect `SqlClient` call does.
pub struct Conn {
    inner: Connection,
    depth: Cell<u32>,
    read_only: bool,
}

impl Conn {
    /// Opens (creating if needed) the database read-write and applies the server's pragmas.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|error| DbError::Sql {
                    operation: "open:makeDirectory".into(),
                    detail: Some(error.to_string()),
                    kind: crate::error::SqlErrorKind::Connection,
                    correlation: None,
                    cause: None,
                })?;
            }
        }
        let inner = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| DbError::sql("open", error))?;
        let conn = Self::wrap(inner, false)?;
        conn.apply_server_pragmas()?;
        Ok(conn)
    }

    /// A private in-memory database (`SqlitePersistenceMemory`), with the same pragmas.
    pub fn open_in_memory() -> Result<Self> {
        let inner = Connection::open_in_memory().map_err(|error| DbError::sql("open", error))?;
        let conn = Self::wrap(inner, false)?;
        conn.apply_server_pragmas()?;
        Ok(conn)
    }

    /// A read-only connection to a WAL database, for snapshot reads off the writer thread.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let inner = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| DbError::sql("open", error))?;
        let conn = Self::wrap(inner, true)?;
        conn.inner
            .busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))
            .map_err(|error| DbError::sql("pragma:busy_timeout", error))?;
        conn.inner
            .execute_batch("PRAGMA query_only = ON;")
            .map_err(|error| DbError::sql("pragma:query_only", error))?;
        Ok(conn)
    }

    fn wrap(inner: Connection, read_only: bool) -> Result<Self> {
        inner.set_prepared_statement_cache_capacity(STATEMENT_CACHE_CAPACITY);
        // node:sqlite opens with double-quoted string literals disabled
        // (`enableDoubleQuotedStringLiterals: false`) and foreign keys on. Match it, so SQL that
        // the TypeScript server would reject is rejected here too.
        use rusqlite::config::DbConfig;
        for config in [DbConfig::SQLITE_DBCONFIG_DQS_DML, DbConfig::SQLITE_DBCONFIG_DQS_DDL] {
            inner.set_db_config(config, false).map_err(|error| DbError::sql("open:dbconfig", error))?;
        }
        inner
            .set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_FKEY, true)
            .map_err(|error| DbError::sql("open:dbconfig", error))?;
        Ok(Self {
            inner,
            depth: Cell::new(0),
            read_only,
        })
    }

    /// `persistence/Layers/Sqlite.ts` `setup`, in the same order.
    fn apply_server_pragmas(&self) -> Result<()> {
        let pragmas = [
            "PRAGMA busy_timeout = 5000;".to_string(),
            "PRAGMA foreign_keys = ON;".to_string(),
            "PRAGMA journal_mode = WAL;".to_string(),
            format!("PRAGMA journal_size_limit = {WAL_SIZE_LIMIT_BYTES};"),
        ];
        for pragma in pragmas {
            // Each of these returns a row; run them as a query and drop the result.
            let mut statement = self.inner.prepare(&pragma).map_err(|error| DbError::sql("pragma", error))?;
            let mut rows = statement.query([]).map_err(|error| DbError::sql("pragma", error))?;
            while rows.next().map_err(|error| DbError::sql("pragma", error))?.is_some() {}
        }
        Ok(())
    }

    /// The rusqlite connection. Prefer [`Conn::prepare`] (cached) for repeated statements.
    pub fn raw(&self) -> &Connection {
        &self.inner
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// A statement from the 200-entry cache (nodeSqliteClient's `prepareCache`).
    pub fn prepare(&self, sql: &str) -> rusqlite::Result<rusqlite::CachedStatement<'_>> {
        self.inner.prepare_cached(sql)
    }

    /// Runs one statement that returns no rows; returns the number of changed rows.
    pub fn execute<P: rusqlite::Params>(&self, sql: &str, params: P) -> rusqlite::Result<usize> {
        self.prepare(sql)?.execute(params)
    }

    /// Runs one or more statements without parameters.
    pub fn execute_batch(&self, sql: &str) -> rusqlite::Result<()> {
        self.inner.execute_batch(sql)
    }

    /// The current nesting depth: 0 outside a transaction.
    pub fn transaction_depth(&self) -> u32 {
        self.depth.get()
    }

    /// Runs `f` in a transaction: `BEGIN … COMMIT` at the top level, `SAVEPOINT effect_sql_<n>`
    /// when nested (a savepoint is not released on success, as in Effect; the outer `COMMIT`
    /// ends it). Any error, or a panic, rolls back (`ROLLBACK` / `ROLLBACK TO SAVEPOINT …`)
    /// and is returned (or resumed) unchanged.
    pub fn transaction<R, E>(&self, f: impl FnOnce(&Conn) -> Result<R, E>) -> Result<R, E>
    where
        E: From<DbError>,
    {
        self.transaction_with("BEGIN", f)
    }

    /// [`Conn::transaction`] that takes the write lock up front (`BEGIN IMMEDIATE`) when it is
    /// the outermost one. Not what the TS client does (it always says `BEGIN`), but a
    /// read-then-write transaction racing another process's commit can otherwise fail with
    /// `SQLITE_BUSY_SNAPSHOT` instead of waiting out `busy_timeout`. Nested calls are
    /// savepoints either way.
    pub fn immediate_transaction<R, E>(&self, f: impl FnOnce(&Conn) -> Result<R, E>) -> Result<R, E>
    where
        E: From<DbError>,
    {
        self.transaction_with("BEGIN IMMEDIATE", f)
    }

    fn transaction_with<R, E>(&self, top_level_begin: &str, f: impl FnOnce(&Conn) -> Result<R, E>) -> Result<R, E>
    where
        E: From<DbError>,
    {
        let id = self.depth.get();
        let begin = if id == 0 {
            top_level_begin.to_string()
        } else {
            format!("SAVEPOINT effect_sql_{id}")
        };
        self.inner
            .execute_batch(&begin)
            .map_err(|error| E::from(DbError::sql("transaction:begin", error)))?;
        self.depth.set(id + 1);
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| f(self)));
        self.depth.set(id);
        match outcome {
            Ok(Ok(value)) => {
                if id == 0 {
                    if let Err(error) = self.inner.execute_batch("COMMIT") {
                        self.rollback(id);
                        return Err(E::from(DbError::sql("transaction:commit", error)));
                    }
                }
                Ok(value)
            }
            Ok(Err(error)) => {
                self.rollback(id);
                Err(error)
            }
            Err(payload) => {
                self.rollback(id);
                panic::resume_unwind(payload)
            }
        }
    }

    fn rollback(&self, id: u32) {
        let result = if id == 0 {
            if self.inner.is_autocommit() {
                // SQLite already rolled the transaction back (SQLITE_FULL, interrupt, …).
                return;
            }
            self.inner.execute_batch("ROLLBACK")
        } else {
            self.inner.execute_batch(&format!("ROLLBACK TO SAVEPOINT effect_sql_{id}"))
        };
        if let Err(error) = result {
            tracing::error!(%error, savepoint = id, "sqlite rollback failed");
        }
    }
}

impl From<rusqlite::Error> for DbError {
    fn from(error: rusqlite::Error) -> Self {
        DbError::sql("query", error)
    }
}

/// `PRAGMA table_info(<table>)` has a column named `column`: the idempotency check of the
/// later migrations.
pub fn has_column(conn: &Conn, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut statement = conn.raw().prepare(&format!("PRAGMA table_info({table})"))?;
    let names = statement.query_map([], |row| row.get::<_, String>("name"))?;
    for name in names {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}
