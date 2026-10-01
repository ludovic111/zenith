//! `zc-db`: zenith code's SQLite persistence, the Rust port of
//! `apps/server/src/persistence/**` and `packages/shared/src/nodeSqliteClient.ts` (WP-05).
//!
//! - [`Db`]: a cloneable handle to one writer thread that owns the read-write connection and
//!   runs closures in order (the TS client's single connection behind a 1-permit semaphore),
//!   plus optional read-only WAL connections ([`Db::read`]).
//! - [`Conn`]: the connection the closures receive: a 200-entry statement cache and
//!   `BEGIN`/`SAVEPOINT effect_sql_<n>` transactions ([`Conn::transaction`]).
//! - [`migrations`]: Effect's Migrator protocol and the 54 migrations, verbatim.
//! - [`repos`]: every repository, as functions over `&Conn`.
//!
//! This crate takes a database path and nothing else; `zc-core` owns where the file lives
//! (`<baseDir>/userdata/state.sqlite`).

pub mod collate;
pub mod conn;
pub mod db;
pub mod error;
pub mod migrations;
pub mod pr_keys;
pub mod repos;
pub mod time;

pub use conn::Conn;
pub use db::{Db, DbOptions};
pub use error::{Correlation, DbError, MigrationErrorKind, Result, SqlErrorKind};
pub use migrations::{MigrationOutcome, LATEST_MIGRATION_ID};
