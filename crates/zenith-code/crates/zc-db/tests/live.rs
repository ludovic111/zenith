//! Gate (1): a copy of the live database (`~/.zenith/code/userdata/state.sqlite`, plus its
//! `-wal`/`-shm`; override with `ZC_DB_LIVE_DB`) opens with zc-db: the migrations are a no-op,
//! every repository reads every real row, and writes work on the copy. The original is never
//! opened: the three files are copied into a temporary directory first. Skipped when there is
//! no live database (CI).
//!
//! Run with `--nocapture` to see the counts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;
use zc_db::repos::*;
use zc_db::{Conn, Db, DbOptions};

fn live_db() -> Option<PathBuf> {
    let path = match std::env::var_os("ZC_DB_LIVE_DB") {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".zenith/code/userdata/state.sqlite"),
    };
    path.exists().then_some(path)
}

/// Copies the database and its WAL files (WAL first is not needed: the copy is opened only
/// after all three are written, and a torn copy would show up as a failed integrity check).
fn copy_live(source: &Path, dir: &Path) -> PathBuf {
    let target = dir.join("state.sqlite");
    for suffix in ["", "-wal", "-shm"] {
        let from = PathBuf::from(format!("{}{suffix}", source.display()));
        if from.exists() {
            std::fs::copy(&from, PathBuf::from(format!("{}{suffix}", target.display()))).unwrap();
        }
    }
    target
}

fn count(conn: &Conn, table: &str) -> i64 {
    conn.raw().query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0)).unwrap()
}

fn ids(conn: &Conn, sql: &str) -> Vec<String> {
    let mut statement = conn.raw().prepare(sql).unwrap();
    statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

#[test]
fn opens_reads_and_writes_a_copy_of_the_live_database() {
    let Some(source) = live_db() else {
        eprintln!("skipped: no live database (set ZC_DB_LIVE_DB)");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = copy_live(&source, dir.path());

    let db = Db::open_with(&path, DbOptions { migrate: true, readers: 1 }).expect("open the copy");
    let outcome = db.migrations().clone();
    assert!(outcome.ran.is_empty(), "migrations ran on the live copy: {:?}", outcome.ran);
    assert_eq!(outcome.previous_latest, 54);
    assert!(!outcome.locked);

    let report = db
        .call_blocking(|conn| {
            let integrity: String = conn.raw().query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
            assert_eq!(integrity, "ok");
            let mut report: BTreeMap<String, i64> = BTreeMap::new();

            // Event store: every event decodes, in pages, in sequence order.
            let events = event_store::read_all(conn)?;
            assert_eq!(events.len() as i64, count(conn, "orchestration_events"));
            assert!(events.windows(2).all(|pair| pair[0].sequence < pair[1].sequence));
            let head = event_store::latest_sequence(conn)?;
            assert_eq!(events.last().map(|event| event.sequence), Some(head).filter(|h| *h > 0));
            report.insert("events".into(), events.len() as i64);
            report.insert("events.head".into(), head);
            if let Some(first) = events.first() {
                let range = event_store::AggregateRange {
                    aggregate_kind: first.aggregate_kind.clone(),
                    aggregate_id: first.aggregate_id.clone(),
                    from_sequence_exclusive: 0,
                    to_sequence_inclusive: head,
                };
                let aggregate = event_store::read_aggregate_range(conn, &range, Some(1_000_000))?;
                let expected = events
                    .iter()
                    .filter(|e| e.aggregate_kind == first.aggregate_kind && e.aggregate_id == first.aggregate_id)
                    .count();
                assert_eq!(aggregate.len(), expected);
                let stats = event_store::get_aggregate_replay_stats(conn, &range, 1_000_000)?;
                assert_eq!(stats.event_count as usize, expected);
                assert!(event_store::has_event_after(conn, &first.aggregate_kind, &first.aggregate_id, None, 0)?);
            }
            // Receipts: a sample of command ids round-trips.
            let commands = ids(conn, "SELECT command_id FROM orchestration_command_receipts ORDER BY rowid DESC LIMIT 200");
            for command in &commands {
                assert!(command_receipts::get_by_command_id(conn, command)?.is_some());
            }
            report.insert("receipts".into(), count(conn, "orchestration_command_receipts"));

            // Projects.
            let projects = ids(conn, "SELECT project_id FROM projection_projects");
            for project in &projects {
                assert!(projects::get_by_id(conn, project)?.is_some(), "project {project}");
            }
            report.insert("projects".into(), projects.len() as i64);

            // Threads and everything per thread.
            let threads = ids(conn, "SELECT thread_id FROM projection_threads");
            let (mut messages, mut activities, mut turns_n, mut approvals, mut plans, mut links, mut sessions) = (0i64, 0i64, 0i64, 0i64, 0i64, 0i64, 0i64);
            for thread_id in &threads {
                let thread = threads::get_by_id(conn, thread_id)?.expect("thread");
                messages += thread_messages::list_by_thread_id(conn, thread_id)?.len() as i64;
                thread_messages::get_latest_user_message_at(conn, thread_id)?;
                activities += thread_activities::list_by_thread_id(conn, thread_id, None, None)?.len() as i64;
                thread_activities::list_user_input_lifecycle_by_thread_id(conn, thread_id)?;
                thread_activities::list_by_thread_id(conn, thread_id, Some(&["task.progress".to_string()]), Some(10))?;
                turns_n += turns::list_by_thread_id(conn, thread_id)?.len() as i64;
                turns::get_pending_turn_start_by_thread_id(conn, thread_id)?;
                approvals += pending_approvals::list_by_thread_id(conn, thread_id)?.len() as i64;
                pending_approvals::count_pending_by_thread_id(conn, thread_id)?;
                plans += proposed_plans::list_by_thread_id(conn, thread_id)?.len() as i64;
                proposed_plans::has_actionable_by_thread_id(conn, thread_id, thread.latest_turn_id.as_deref())?;
                links += thread_pull_requests::list_by_thread_id(conn, thread_id)?.len() as i64;
                if thread_sessions::get_by_thread_id(conn, thread_id)?.is_some() {
                    sessions += 1;
                }
                if let Some(turn_id) = &thread.latest_turn_id {
                    thread_messages::has_assistant_message_for_turn(conn, thread_id, turn_id, false)?;
                    turns::get_by_turn_id(conn, thread_id, turn_id)?;
                }
            }
            report.insert("threads".into(), threads.len() as i64);
            // Rows of threads that exist (projections can keep rows of deleted threads).
            let in_threads = |table: &str| -> i64 {
                conn.raw()
                    .query_row(
                        &format!("SELECT COUNT(*) FROM {table} WHERE thread_id IN (SELECT thread_id FROM projection_threads)"),
                        [],
                        |r| r.get(0),
                    )
                    .unwrap()
            };
            assert_eq!(messages, in_threads("projection_thread_messages"));
            assert_eq!(activities, in_threads("projection_thread_activities"));
            assert_eq!(turns_n, in_threads("projection_turns"));
            assert_eq!(approvals, in_threads("projection_pending_approvals"));
            assert_eq!(plans, in_threads("projection_thread_proposed_plans"));
            assert_eq!(links, in_threads("projection_thread_pull_requests"));
            assert_eq!(sessions, in_threads("projection_thread_sessions"));
            report.insert("messages".into(), messages);
            report.insert("activities".into(), activities);
            report.insert("turns".into(), turns_n);
            report.insert("approvals".into(), approvals);
            report.insert("plans".into(), plans);
            report.insert("pullRequestLinks".into(), links);
            report.insert("sessions".into(), sessions);

            // Messages, approvals and plans by id.
            for message in ids(conn, "SELECT message_id FROM projection_thread_messages ORDER BY rowid DESC LIMIT 200") {
                assert!(thread_messages::get_by_message_id(conn, &message)?.is_some());
            }
            for request in ids(conn, "SELECT request_id FROM projection_pending_approvals LIMIT 200") {
                assert!(pending_approvals::get_by_request_id(conn, &request)?.is_some());
            }
            let pr_keys: Vec<(String, String, i64)> = conn
                .raw()
                .prepare("SELECT host, repository, number FROM projection_thread_pull_requests")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (host, repository, number) in pr_keys {
                assert!(!thread_pull_requests::list_by_pull_request(conn, &host, &repository, number)?.is_empty());
            }

            // Cursors.
            let cursors = projection_state::list_all(conn)?;
            assert_eq!(cursors.len() as i64, count(conn, "projection_state"));
            for cursor in &cursors {
                assert!(cursor.last_applied_sequence <= head);
            }
            report.insert("cursors".into(), cursors.len() as i64);

            // Provider runtime: list skips nothing on this database.
            let runtimes = provider_session_runtime::list(conn, false)?;
            assert_eq!(runtimes.len() as i64, count(conn, "provider_session_runtime"));
            for runtime in &runtimes {
                assert!(provider_session_runtime::get_by_thread_id(conn, &runtime.thread_id)?.is_some());
            }
            provider_session_runtime::list(conn, true)?;
            report.insert("providerRuntimes".into(), runtimes.len() as i64);

            // Auth.
            for session in ids(conn, "SELECT session_id FROM auth_sessions") {
                assert!(auth_sessions::get_by_id(conn, &session)?.is_some());
            }
            let now = jiff::Timestamp::now();
            report.insert("authSessions".into(), count(conn, "auth_sessions"));
            report.insert("authSessionsActive".into(), auth_sessions::list_active(conn, now, &[])?.len() as i64);
            for credential in ids(conn, "SELECT credential FROM auth_pairing_links") {
                assert!(auth_pairing_links::get_by_credential(conn, &credential)?.is_some());
            }
            report.insert("pairingLinks".into(), count(conn, "auth_pairing_links"));
            auth_pairing_links::list_active(conn, now)?;
            report.insert("filesViewed".into(), count(conn, "pull_request_files_viewed"));
            Ok(report)
        })
        .expect("read every repository");
    eprintln!("live copy at {}:", path.display());
    for (key, value) in &report {
        eprintln!("  {key}: {value}");
    }

    // Writes on the copy: a dispatch-shaped transaction, then reads through the reader.
    let written = db
        .call_blocking(|conn| {
            conn.transaction(|conn| {
                let event = event_store::append(
                    conn,
                    &event_store::NewEvent {
                        event_id: "zc-db-live-gate-event".into(),
                        aggregate_kind: "project".into(),
                        aggregate_id: "zc-db-live-gate-project".into(),
                        occurred_at: zc_db::time::now_iso(),
                        command_id: Some("zc-db-live-gate-command".into()),
                        causation_event_id: None,
                        correlation_id: Some("zc-db-live-gate-command".into()),
                        metadata: json!({}),
                        event_type: "project.created".into(),
                        payload: json!({ "projectId": "zc-db-live-gate-project", "title": "Gate", "workspaceRoot": "/tmp/gate", "defaultModelSelection": null, "scripts": [], "createdAt": "2026-10-01T00:00:00.000Z", "updatedAt": "2026-10-01T00:00:00.000Z" }),
                    },
                )?;
                projects::upsert(
                    conn,
                    &projects::ProjectionProject {
                        project_id: "zc-db-live-gate-project".into(),
                        title: "Gate".into(),
                        workspace_root: "/tmp/gate".into(),
                        default_model_selection: None,
                        default_thread_env_mode: None,
                        auto_pull: false,
                        favicon_path: None,
                        project_icon: None,
                        scripts: json!([]),
                        created_at: "2026-10-01T00:00:00.000Z".into(),
                        updated_at: "2026-10-01T00:00:00.000Z".into(),
                        deleted_at: None,
                    },
                )?;
                command_receipts::upsert(
                    conn,
                    &command_receipts::CommandReceipt {
                        command_id: "zc-db-live-gate-command".into(),
                        aggregate_kind: "project".into(),
                        aggregate_id: "zc-db-live-gate-project".into(),
                        accepted_at: zc_db::time::now_iso(),
                        result_sequence: event.sequence,
                        status: "accepted".into(),
                        error: None,
                    },
                )?;
                Ok(event)
            })
        })
        .expect("write to the copy");
    let read_back = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(db.read(|conn| {
            Ok((
                projects::get_by_id(conn, "zc-db-live-gate-project")?.is_some(),
                event_store::latest_sequence(conn)?,
            ))
        }))
        .unwrap();
    assert_eq!(read_back, (true, written.sequence));
    assert_eq!(written.sequence, report["events.head"] + 1);
    drop(db);

    // The TypeScript server can reopen the copy after the Rust writes (rollback stays
    // possible): its migrator finds nothing to do.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).unwrap().to_path_buf();
    if root.join("code/apps/server/node_modules/effect").exists() && Command::new("node").arg("--version").output().is_ok() {
        let output = Command::new("node")
            .arg(root.join("crates/zenith-code/crates/zc-db/tests/ts/migrate-fresh.ts"))
            .arg(&path)
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(stdout.trim().lines().last(), Some("[]"), "TS ran migrations: {stdout}");
        eprintln!("  TS reopened the copy: no migration ran");
    }
}
