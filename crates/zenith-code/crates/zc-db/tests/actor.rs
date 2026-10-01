//! The writer actor, transactions and savepoints (nodeSqliteClient + Effect's
//! `makeWithTransaction`), the pragmas of `persistence/Layers/Sqlite.ts` (port of
//! `Sqlite.test.ts`), and gate (3): another process writing the same file, as the CLI does.

use std::io::{BufRead, BufReader, Write as _};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use zc_db::repos::auth_sessions;
use zc_db::{Conn, Db, DbError, DbOptions};

fn count(conn: &Conn, sql: &str) -> i64 {
    conn.raw().query_row(sql, [], |row| row.get(0)).unwrap()
}

#[tokio::test]
async fn calls_run_in_order_on_one_connection() {
    let db = Db::open_in_memory().unwrap();
    db.call(|conn| Ok(conn.execute_batch("CREATE TABLE probe(id INTEGER PRIMARY KEY, v TEXT)")?))
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for index in 0..50 {
        let db = db.clone();
        tasks.push(tokio::spawn(async move {
            db.call(move |conn| Ok(conn.execute("INSERT INTO probe(v) VALUES (?1)", [format!("v{index}")])?))
                .await
        }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    let rows = db.call(|conn| Ok(count(conn, "SELECT COUNT(*) FROM probe"))).await.unwrap();
    assert_eq!(rows, 50);
}

#[tokio::test]
async fn transactions_commit_and_roll_back_with_savepoints() {
    let db = Db::open_in_memory().unwrap();
    db.call(|conn| Ok(conn.execute_batch("CREATE TABLE probe(v TEXT)")?)).await.unwrap();

    // An error rolls the whole transaction back.
    let result = db
        .transaction(|conn| {
            conn.execute("INSERT INTO probe VALUES ('rolled-back')", [])?;
            Err::<(), _>(DbError::decode("test", "abort"))
        })
        .await;
    assert!(result.is_err());

    // A failing nested transaction rolls back to its savepoint only.
    db.transaction(|conn| {
        assert_eq!(conn.transaction_depth(), 1);
        conn.execute("INSERT INTO probe VALUES ('outer')", [])?;
        let inner: Result<(), DbError> = conn.transaction(|conn| {
            assert_eq!(conn.transaction_depth(), 2);
            conn.execute("INSERT INTO probe VALUES ('inner')", [])?;
            Err(DbError::decode("test", "inner abort"))
        });
        assert!(inner.is_err());
        conn.transaction(|conn| {
            conn.execute("INSERT INTO probe VALUES ('inner-kept')", [])?;
            Ok::<_, DbError>(())
        })?;
        Ok(())
    })
    .await
    .unwrap();
    let values: Vec<String> = db
        .call(|conn| {
            let mut statement = conn.prepare("SELECT v FROM probe ORDER BY rowid")?;
            let values = statement.query_map([], |row| row.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(values)
        })
        .await
        .unwrap();
    assert_eq!(values, vec!["outer".to_string(), "inner-kept".to_string()]);
    let autocommit = db.call(|conn| Ok(conn.raw().is_autocommit())).await.unwrap();
    assert!(autocommit);
}

#[tokio::test]
async fn a_panic_rolls_back_and_the_actor_keeps_serving() {
    let db = Db::open_in_memory().unwrap();
    db.call(|conn| Ok(conn.execute_batch("CREATE TABLE probe(v TEXT)")?)).await.unwrap();
    let result = db
        .transaction(|conn| -> zc_db::Result<()> {
            conn.execute("INSERT INTO probe VALUES ('x')", [])?;
            panic!("boom");
        })
        .await;
    assert!(matches!(result, Err(DbError::Panicked(message)) if message == "boom"));
    let rows = db.call(|conn| Ok(count(conn, "SELECT COUNT(*) FROM probe"))).await.unwrap();
    assert_eq!(rows, 0);
    let autocommit = db.call(|conn| Ok(conn.raw().is_autocommit())).await.unwrap();
    assert!(autocommit);
}

#[test]
fn call_blocking_works_outside_a_runtime() {
    let db = Db::open_in_memory().unwrap();
    let value: i64 = db.call_blocking(|conn| Ok(count(conn, "SELECT 41 + 1"))).unwrap();
    assert_eq!(value, 42);
}

#[test]
fn applies_the_server_pragmas() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("state.sqlite"), DbOptions { migrate: false, readers: 0 }).unwrap();
    let (timeout, foreign_keys, mode, limit): (i64, i64, String, i64) = db
        .call_blocking(|conn| {
            let raw = conn.raw();
            Ok((
                raw.query_row("PRAGMA busy_timeout", [], |r| r.get(0))?,
                raw.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?,
                raw.query_row("PRAGMA journal_mode", [], |r| r.get(0))?,
                raw.query_row("PRAGMA journal_size_limit", [], |r| r.get(0))?,
            ))
        })
        .unwrap();
    assert_eq!((timeout, foreign_keys, mode.as_str(), limit), (5000, 1, "wal", 33_554_432));
    // node:sqlite rejects double-quoted string literals; so does zc-db.
    let rejected = db
        .call_blocking(|conn| Ok(conn.raw().query_row("SELECT \"not-a-column\"", [], |row| row.get::<_, String>(0)).is_err()))
        .unwrap();
    assert!(rejected);
}

#[test]
fn shrinks_the_wal_back_to_the_size_limit_after_a_large_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    let wal = dir.path().join("state.sqlite-wal");
    let conn = Conn::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE wal_probe(payload BLOB)").unwrap();
    let limit = zc_db::conn::WAL_SIZE_LIMIT_BYTES as f64;
    let rows = ((limit * 1.25) / 4000.0).ceil() as i64;
    conn.execute(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?1) INSERT INTO wal_probe(payload) SELECT randomblob(4000) FROM n",
        [rows],
    )
    .unwrap();
    assert!(std::fs::metadata(&wal).unwrap().len() as f64 > limit);
    conn.execute("INSERT INTO wal_probe(payload) VALUES (x'00')", []).unwrap();
    assert!(std::fs::metadata(&wal).unwrap().len() as f64 <= limit);
}

#[tokio::test]
async fn reads_use_a_read_only_connection_that_does_not_wait_for_the_writer() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("state.sqlite"), DbOptions { migrate: true, readers: 1 }).unwrap();
    // Hold the writer inside an open transaction with an uncommitted row.
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let writer = {
        let db = db.clone();
        tokio::spawn(async move {
            db.transaction(move |conn| {
                conn.execute(
                    "INSERT INTO projection_state (projector, last_applied_sequence, updated_at) VALUES ('p', 1, 'x')",
                    [],
                )?;
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .await
        })
    };
    tokio::task::spawn_blocking(move || entered_rx.recv().unwrap()).await.unwrap();
    // The reader answers while the writer is busy, and does not see the uncommitted row.
    let seen = tokio::time::timeout(Duration::from_secs(5), db.read(|conn| Ok(count(conn, "SELECT COUNT(*) FROM projection_state"))))
        .await
        .expect("read must not wait for the writer")
        .unwrap();
    assert_eq!(seen, 0);
    let writes_rejected = db.read(|conn| Ok(conn.execute("DELETE FROM projection_state", []).is_err())).await.unwrap();
    assert!(writes_rejected);
    release_tx.send(()).unwrap();
    writer.await.unwrap().unwrap();
    let seen = db.read(|conn| Ok(count(conn, "SELECT COUNT(*) FROM projection_state"))).await.unwrap();
    assert_eq!(seen, 1);
}

// ---------------------------------------------------------------- gate (3): another process

/// Child-process modes, selected by environment variables when this test binary re-runs itself.
#[test]
fn child_process_helper() {
    if let Ok(path) = std::env::var("ZC_DB_HOLD_LOCK") {
        let hold_ms: u64 = std::env::var("ZC_DB_HOLD_MS").unwrap().parse().unwrap();
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        println!("locked");
        std::io::stdout().flush().unwrap();
        std::thread::sleep(Duration::from_millis(hold_ms));
        conn.execute_batch("COMMIT").unwrap();
        return;
    }
    if let Ok(path) = std::env::var("ZC_DB_CLI_WRITER") {
        // What `auth pairing create` / `auth session issue` do from the CLI: open the same file
        // with zc-db (no actor) and write, many times.
        let conn = Conn::open(Path::new(&path)).unwrap();
        println!("ready");
        std::io::stdout().flush().unwrap();
        for index in 0..200 {
            let input = session(&format!("cli-{index}"));
            if index % 2 == 0 {
                // Autocommit, as the CLI's single statements run.
                auth_sessions::create(&conn, &input).unwrap();
            } else {
                // The server's replace-and-create transaction (UPDATE … RETURNING, INSERT).
                auth_sessions::create_replacing_active(&conn, &input, input.issued_at).unwrap();
            }
        }
    }
}

fn session(id: &str) -> auth_sessions::CreateAuthSession {
    auth_sessions::CreateAuthSession {
        session_id: id.into(),
        subject: "owner".into(),
        scopes: vec!["orchestration:read".into()],
        method: "bearer-access-token".into(),
        client: auth_sessions::ClientMetadata {
            label: None,
            ip_address: None,
            user_agent: None,
            device_type: "unknown".into(),
            os: None,
            browser: None,
        },
        issued_at: "2026-10-01T00:00:00.000Z".parse().unwrap(),
        expires_at: "2027-10-01T00:00:00.000Z".parse().unwrap(),
    }
}

fn spawn_child(env: &[(&str, String)]) -> (std::process::Child, BufReader<std::process::ChildStdout>) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "child_process_helper", "--nocapture", "--test-threads=1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command.spawn().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    (child, stdout)
}

fn wait_for_line(reader: &mut BufReader<std::process::ChildStdout>, expected: &str) {
    let mut line = String::new();
    loop {
        line.clear();
        assert!(reader.read_line(&mut line).unwrap() > 0, "child exited before printing {expected}");
        // libtest prints "test child_process_helper ... " on the same line first.
        if line.trim_end().ends_with(expected) {
            return;
        }
    }
}

#[tokio::test]
async fn waits_out_a_concurrent_writer_instead_of_failing_with_busy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    let db = Db::open_with(&path, DbOptions { migrate: true, readers: 0 }).unwrap();
    let (mut child, mut stdout) = spawn_child(&[("ZC_DB_HOLD_LOCK", path.display().to_string()), ("ZC_DB_HOLD_MS", "600".into())]);
    wait_for_line(&mut stdout, "locked");
    let started = Instant::now();
    db.call(|conn| auth_sessions::create(conn, &session("server-1"))).await.unwrap();
    assert!(started.elapsed() >= Duration::from_millis(300), "the insert should have waited for the lock");
    child.wait().unwrap();
    let rows = db.call(|conn| Ok(count(conn, "SELECT COUNT(*) FROM auth_sessions"))).await.unwrap();
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn a_cli_process_and_the_server_write_the_same_file_concurrently() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    let db = Db::open_with(&path, DbOptions { migrate: true, readers: 1 }).unwrap();
    let (mut child, mut stdout) = spawn_child(&[("ZC_DB_CLI_WRITER", path.display().to_string())]);
    wait_for_line(&mut stdout, "ready");
    for index in 0..200 {
        let input = session(&format!("server-{index}"));
        if index % 2 == 0 {
            db.transaction(move |conn| auth_sessions::create(conn, &input)).await.unwrap();
        } else {
            // Read, then write: the shape that needs the write lock up front under contention.
            db.call(move |conn| {
                conn.immediate_transaction(|conn| {
                    auth_sessions::list_active(conn, input.issued_at, &[])?;
                    auth_sessions::create(conn, &input)
                })
            })
            .await
            .unwrap();
        }
    }
    assert!(child.wait().unwrap().success(), "the CLI-like writer failed");
    let rows = db.read(|conn| Ok(count(conn, "SELECT COUNT(*) FROM auth_sessions"))).await.unwrap();
    assert_eq!(rows, 400);
    // The migration bookkeeping from the second open was a no-op in the child.
    let migrations = db.read(|conn| Ok(count(conn, "SELECT COUNT(*) FROM effect_sql_migrations"))).await.unwrap();
    assert_eq!(migrations, 54);
}
