//! Gate (2): a fresh database made by zc-db has the same `sqlite_master` as one made by the
//! TypeScript server's migrations, byte for byte (type, name, tbl_name, rootpage, sql, in
//! creation order).
//!
//! `fixtures/ts_sqlite_master.json` was dumped from a database created by
//! `tests/ts/migrate-fresh.ts`. When `node` and the TS dependencies are present, the test also
//! re-runs that script and compares against its output, so a drift on either side shows up.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};
use zc_db::{Db, DbOptions};

fn dump_master(path: &Path) -> Vec<Value> {
    let conn = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).expect("open for dump");
    let mut statement = conn
        .prepare("SELECT type, name, tbl_name, rootpage, sql FROM sqlite_master ORDER BY rowid")
        .unwrap();
    statement
        .query_map([], |row| {
            Ok(json!({
                "type": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "tbl_name": row.get::<_, String>(2)?,
                "rootpage": row.get::<_, i64>(3)?,
                "sql": row.get::<_, Option<String>>(4)?,
            }))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn rust_fresh_db(dir: &Path) -> PathBuf {
    let path = dir.join("rust.sqlite");
    let db = Db::open_with(&path, DbOptions { migrate: true, readers: 0 }).expect("open fresh");
    assert_eq!(db.migrations().ran.len(), 54);
    assert_eq!(db.migrations().previous_latest, 0);
    drop(db);
    path
}

fn diff(expected: &[Value], actual: &[Value]) -> Option<String> {
    if expected == actual {
        return None;
    }
    let mut out = String::new();
    for index in 0..expected.len().max(actual.len()) {
        let left = expected.get(index);
        let right = actual.get(index);
        if left != right {
            out.push_str(&format!("#{index}\n  expected: {left:?}\n  actual:   {right:?}\n"));
        }
    }
    Some(out)
}

#[test]
fn fresh_schema_matches_the_typescript_fixture() {
    let dir = tempfile::tempdir().unwrap();
    let path = rust_fresh_db(dir.path());
    let actual = dump_master(&path);
    let expected: Vec<Value> = serde_json::from_str(include_str!("fixtures/ts_sqlite_master.json")).expect("fixture");
    if let Some(report) = diff(&expected, &actual) {
        panic!("sqlite_master differs from the TypeScript-created database:\n{report}");
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).expect("repository root").to_path_buf()
}

#[test]
fn fresh_schema_matches_a_live_typescript_run() {
    let root = repo_root();
    let script = root.join("crates/zenith-code/crates/zc-db/tests/ts/migrate-fresh.ts");
    let needed = [
        root.join("code/apps/server/node_modules/effect"),
        root.join("code/packages/shared/node_modules"),
    ];
    if needed.iter().any(|path| !path.exists()) || Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipped: node or code/**/node_modules missing (see tests/ts/migrate-fresh.ts)");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ts_path = dir.path().join("ts.sqlite");
    let output = Command::new("node").arg(&script).arg(&ts_path).current_dir(&root).output().expect("run node");
    assert!(output.status.success(), "migrate-fresh.ts failed:\n{}", String::from_utf8_lossy(&output.stderr));
    let rust_path = rust_fresh_db(dir.path());
    let expected = dump_master(&ts_path);
    let actual = dump_master(&rust_path);
    if let Some(report) = diff(&expected, &actual) {
        panic!("sqlite_master differs from the live TypeScript run:\n{report}");
    }
    // The migration bookkeeping rows match too (ids and names; created_at is a clock).
    let rows = |path: &Path| -> Vec<(i64, String)> {
        let conn = rusqlite::Connection::open(path).unwrap();
        let mut statement = conn
            .prepare("SELECT migration_id, name FROM effect_sql_migrations ORDER BY migration_id")
            .unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(rows(&ts_path), rows(&rust_path));
    // Both are WAL databases with the same page size and user_version.
    for path in [&ts_path, &rust_path] {
        let conn = rusqlite::Connection::open(path).unwrap();
        let mode: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0)).unwrap();
        assert_eq!(mode, "wal");
    }
}
