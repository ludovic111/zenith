//! Errors, shaped like the TypeScript persistence errors (`persistence/Errors.ts`).
//!
//! `PersistenceSqlError` and `PersistenceDecodeError` carry an `operation` such as
//! `"ProjectionThreadRepository.upsert:query"`, an optional `detail` that only ever names the
//! SQLite condition (`"SQLITE(2067) constraint failed"`, never query data) and an optional
//! correlation id. The same strings are kept here so logs and diagnostics read the same.

use std::fmt;

use rusqlite::ffi;

/// What a repository error is about, without any sensitive value (`Errors.ts`
/// `PersistenceErrorCorrelation`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Correlation {
    SessionId(String),
    CurrentSessionId(String),
    PairingLinkId(String),
    ThreadId(String),
}

/// The class of a SQLite failure, as Effect's `classifySqliteError` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlErrorKind {
    UniqueViolation,
    Constraint,
    /// `SQLITE_BUSY` / `SQLITE_LOCKED` (Effect's `LockTimeoutError`).
    LockTimeout,
    Connection,
    Authentication,
    Authorization,
    Unknown,
}

/// Why the migrator stopped (Effect's `MigrationError.kind`, plus `NewerSchema`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationErrorKind {
    BadState,
    Failed,
    Duplicates,
    Locked,
    /// The database records a migration this build does not know: a newer server wrote it.
    NewerSchema,
}

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// `PersistenceSqlError`.
    #[error("{}", sql_message(.operation, .detail))]
    Sql {
        operation: String,
        detail: Option<String>,
        kind: SqlErrorKind,
        correlation: Option<Correlation>,
        #[source]
        cause: Option<Box<rusqlite::Error>>,
    },
    /// `PersistenceDecodeError`: a stored value does not have the shape the row needs.
    #[error("Decode error in {operation}: {issue}")]
    Decode {
        operation: String,
        issue: String,
        correlation: Option<Correlation>,
    },
    /// Effect's `MigrationError`.
    #[error("{message}")]
    Migration {
        kind: MigrationErrorKind,
        message: String,
        #[source]
        cause: Option<Box<DbError>>,
    },
    /// The writer (or reader) thread is gone.
    #[error("the database connection is closed")]
    Closed,
    /// A closure run on the database thread panicked. The transaction it was in was rolled back.
    #[error("database task panicked: {0}")]
    Panicked(String),
}

fn sql_message(operation: &str, detail: &Option<String>) -> String {
    match detail {
        None => format!("SQL error in {operation}"),
        Some(detail) => format!("SQL error in {operation}: {detail}"),
    }
}

impl DbError {
    /// A SQL error for `operation`, classified and described from the driver error.
    pub fn sql(operation: impl Into<String>, cause: rusqlite::Error) -> Self {
        DbError::Sql {
            operation: operation.into(),
            detail: sqlite_condition(&cause),
            kind: classify(&cause),
            correlation: None,
            cause: Some(Box::new(cause)),
        }
    }

    pub fn decode(operation: impl Into<String>, issue: impl Into<String>) -> Self {
        DbError::Decode {
            operation: operation.into(),
            issue: issue.into(),
            correlation: None,
        }
    }

    pub fn with_correlation(mut self, value: Correlation) -> Self {
        match &mut self {
            DbError::Sql { correlation, .. } | DbError::Decode { correlation, .. } => *correlation = Some(value),
            _ => {}
        }
        self
    }

    pub fn operation(&self) -> Option<&str> {
        match self {
            DbError::Sql { operation, .. } | DbError::Decode { operation, .. } => Some(operation),
            _ => None,
        }
    }

    pub fn correlation(&self) -> Option<&Correlation> {
        match self {
            DbError::Sql { correlation, .. } | DbError::Decode { correlation, .. } => correlation.as_ref(),
            _ => None,
        }
    }

    pub fn sql_kind(&self) -> Option<SqlErrorKind> {
        match self {
            DbError::Sql { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    pub fn is_decode(&self) -> bool {
        matches!(self, DbError::Decode { .. })
    }

    /// `SQLITE_BUSY` / `SQLITE_LOCKED` once `busy_timeout` ran out.
    pub fn is_busy(&self) -> bool {
        self.sql_kind() == Some(SqlErrorKind::LockTimeout)
    }

    pub fn is_constraint(&self) -> bool {
        matches!(self.sql_kind(), Some(SqlErrorKind::Constraint | SqlErrorKind::UniqueViolation))
    }
}

/// `SQLITE(<extended code>) <sqlite3_errstr>`, what `Errors.ts` reads from node:sqlite's
/// `errcode`/`errstr`. Never the driver message, which can quote data.
pub fn sqlite_condition(error: &rusqlite::Error) -> Option<String> {
    let code = extended_code(error)?;
    // SAFETY: sqlite3_errstr returns a pointer to a static, NUL-terminated English string.
    let text = unsafe {
        let ptr = ffi::sqlite3_errstr(code);
        if ptr.is_null() {
            return Some(format!("SQLITE({code})"));
        }
        std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
    };
    Some(format!("SQLITE({code}) {text}"))
}

fn extended_code(error: &rusqlite::Error) -> Option<i32> {
    match error {
        rusqlite::Error::SqliteFailure(inner, _) => Some(inner.extended_code),
        _ => None,
    }
}

/// Effect's `classifySqliteError`, on the numeric code.
pub fn classify(error: &rusqlite::Error) -> SqlErrorKind {
    let Some(code) = extended_code(error) else {
        return SqlErrorKind::Unknown;
    };
    if code == ffi::SQLITE_CONSTRAINT_UNIQUE {
        return SqlErrorKind::UniqueViolation;
    }
    match code & 0xff {
        23 => SqlErrorKind::Authentication,
        3 => SqlErrorKind::Authorization,
        19 => SqlErrorKind::Constraint,
        5 | 6 => SqlErrorKind::LockTimeout,
        14 => SqlErrorKind::Connection,
        _ => SqlErrorKind::Unknown,
    }
}

pub type Result<T, E = DbError> = std::result::Result<T, E>;

/// The error a repository body raises before it is given its operation name: either the
/// driver failed, or a stored value did not decode.
#[derive(Debug)]
pub(crate) enum Raw {
    Sql(rusqlite::Error),
    Decode(String),
    /// Already named (a transaction's BEGIN/COMMIT failing, a nested repository call).
    Db(DbError),
}

impl From<DbError> for Raw {
    fn from(value: DbError) -> Self {
        Raw::Db(value)
    }
}

impl From<rusqlite::Error> for Raw {
    fn from(value: rusqlite::Error) -> Self {
        Raw::Sql(value)
    }
}

impl fmt::Display for Raw {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Raw::Sql(error) => write!(f, "{error}"),
            Raw::Decode(issue) => write!(f, "{issue}"),
            Raw::Db(error) => write!(f, "{error}"),
        }
    }
}

pub(crate) type RawResult<T> = std::result::Result<T, Raw>;

/// Names a repository failure: `sql_op` for driver errors, `decode_op` for decode errors
/// (the `toPersistenceSqlOrDecodeError(sqlOperation, decodeOperation)` of the TS layers).
pub(crate) fn named<T>(result: RawResult<T>, sql_op: &str, decode_op: &str) -> Result<T> {
    result.map_err(|raw| match raw {
        Raw::Sql(error) => DbError::sql(sql_op, error),
        Raw::Decode(issue) => DbError::decode(decode_op, issue),
        Raw::Db(error) => error,
    })
}

/// `toPersistenceSqlError(operation)` alone: a decode failure is reported as a SQL error of
/// that operation whose detail is the issue (the TS projection repositories do this).
pub(crate) fn named_sql<T>(result: RawResult<T>, operation: &str) -> Result<T> {
    result.map_err(|raw| match raw {
        Raw::Sql(error) => DbError::sql(operation, error),
        Raw::Decode(issue) => DbError::Sql {
            operation: operation.to_string(),
            detail: Some(issue),
            kind: SqlErrorKind::Unknown,
            correlation: None,
            cause: None,
        },
        Raw::Db(error) => error,
    })
}
