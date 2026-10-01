//! The WP-09 gates on a COPY of the live database (`~/.zenith/code/userdata/state.sqlite`, or
//! `ZC_PROJECTIONS_LIVE_DB`). The original is only read by the file copy; every database
//! below is a copy in a temporary directory. Ignored by default (personal data, node needed):
//!
//! ```sh
//! cargo test -p zc-projections --test live_gates -- --ignored --nocapture
//! ```
//!
//! 1. **Bootstrap**: three copies. A keeps the TS-built projections. B has its projection
//!    tables emptied and rebuilt by the Rust pipeline; C the same with the TS pipeline run
//!    from source (`code/apps/server/scripts/projections-oracle.ts`). B must equal C row for
//!    row; A may differ from B only where it also differs from C (rows the TS code of the time
//!    built differently, and the attachment-cleanup cursor).
//! 2. **Snapshots**: on copy C, every snapshot query (shell, archived shell, command read
//!    model, full snapshot, thread details with and without windows and cursors, thread
//!    shells, search, …) answered by TS and by Rust must be the same JSON.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};
use zc_contracts::RepositoryIdentity;
use zc_db::{Db, DbOptions};
use zc_projections::{FixedRepositoryIdentities, NoThreadLiveState, ProjectionPipeline, ProjectionSnapshotQuery};

use common::*;

fn live_db() -> Option<PathBuf> {
    let path = match std::env::var_os("ZC_PROJECTIONS_LIVE_DB") {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".zenith/code/userdata/state.sqlite"),
    };
    path.exists().then_some(path)
}

/// A consistent copy: the three files, then a checkpoint into a single file on the copy.
fn copy_live(source: &Path, dir: &Path, name: &str) -> PathBuf {
    let staging = dir.join(format!("{name}-staging"));
    std::fs::create_dir_all(&staging).unwrap();
    let staged = staging.join("state.sqlite");
    for suffix in ["", "-wal", "-shm"] {
        let from = PathBuf::from(format!("{}{suffix}", source.display()));
        if from.exists() {
            std::fs::copy(&from, PathBuf::from(format!("{}{suffix}", staged.display()))).unwrap();
        }
    }
    let target = dir.join(format!("{name}.sqlite"));
    let conn = rusqlite::Connection::open(&staged).unwrap();
    let check: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0)).unwrap();
    assert_eq!(check, "ok", "the copy is consistent");
    conn.execute("VACUUM INTO ?1", [target.to_string_lossy().to_string()]).unwrap();
    drop(conn);
    std::fs::remove_dir_all(&staging).unwrap();
    target
}

fn clear_projections(path: &Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    for (table, _) in PROJECTION_TABLES {
        conn.execute(&format!("DELETE FROM {table}"), []).unwrap();
    }
}

struct Copies {
    _dir: tempfile::TempDir,
    original: PathBuf,
    rust: PathBuf,
    ts: PathBuf,
    attachments: PathBuf,
}

fn make_copies() -> Option<Copies> {
    let source = live_db()?;
    if !node_available() {
        eprintln!("skipped: node or code/apps/server/node_modules missing");
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let original = copy_live(&source, dir.path(), "original");
    let rust = dir.path().join("rust.sqlite");
    let ts = dir.path().join("ts.sqlite");
    std::fs::copy(&original, &rust).unwrap();
    std::fs::copy(&original, &ts).unwrap();
    let attachments = dir.path().join("attachments");
    std::fs::create_dir_all(&attachments).unwrap();
    Some(Copies {
        original,
        rust,
        ts,
        attachments,
        _dir: dir,
    })
}

fn rust_bootstrap(copies: &Copies) -> std::time::Duration {
    clear_projections(&copies.rust);
    let db = Db::open_with(&copies.rust, DbOptions { migrate: true, readers: 0 }).unwrap();
    let pipeline = ProjectionPipeline::new(&copies.attachments);
    let started = std::time::Instant::now();
    let pipeline_clone = pipeline.clone();
    db.call_blocking(move |conn| pipeline_clone.bootstrap(conn)).unwrap();
    started.elapsed()
}

#[test]
#[ignore = "needs the live database and node"]
fn gate_1_bootstrap_rebuilds_the_projections_like_typescript() {
    let Some(copies) = make_copies() else {
        return;
    };
    let rust_time = rust_bootstrap(&copies);
    let ts_report = run_oracle(&["bootstrap", "--db", copies.ts.to_str().unwrap()]);
    eprintln!("rust bootstrap {rust_time:?}; ts {}", ts_report.trim());

    let mut failures = Vec::new();
    for (table, key) in PROJECTION_TABLES {
        let original = table_rows(&copies.original, table, key);
        let rust = table_rows(&copies.rust, table, key);
        let ts = table_rows(&copies.ts, table, key);
        let rust_vs_ts = diff_rows(&rust, &ts);
        let original_vs_rust = diff_rows(&original, &rust);
        let original_vs_ts = diff_rows(&original, &ts);
        eprintln!(
            "{table}: {} rows; rust≠ts {}; original≠rust {}; original≠ts {}",
            rust.len(),
            rust_vs_ts.len(),
            original_vs_rust.len(),
            original_vs_ts.len()
        );
        for (key, columns) in original_vs_rust.iter() {
            eprintln!("   original≠rust {key}: {columns:?}");
        }
        if !rust_vs_ts.is_empty() {
            for (key, columns) in rust_vs_ts.iter().take(10) {
                eprintln!("   RUST≠TS {key}: {columns:?}");
            }
            failures.push(format!("{table}: rust differs from the TS rebuild"));
        }
        if original_vs_rust != original_vs_ts {
            failures.push(format!("{table}: rust differs from the original beyond what TS does"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

fn sql_strings(path: &Path, sql: &str) -> Vec<String> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut statement = conn.prepare(sql).unwrap();
    statement.query_map([], |row| row.get::<_, String>(0)).unwrap().map(Result::unwrap).collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the live database and node"]
async fn gate_2_snapshot_queries_answer_like_typescript() {
    let Some(copies) = make_copies() else {
        return;
    };
    // Gate 2 runs on the TS-rebuilt copy, so both sides read identical projections.
    run_oracle(&["bootstrap", "--db", copies.ts.to_str().unwrap()]);
    let db_path = copies.ts.clone();

    let identities_text = run_oracle(&["identities", "--db", db_path.to_str().unwrap()]);
    let identities_file = copies.attachments.join("../identities.json");
    std::fs::write(&identities_file, &identities_text).unwrap();
    let identities: HashMap<String, Option<RepositoryIdentity>> = serde_json::from_str(identities_text.trim()).unwrap();

    let threads = sql_strings(&db_path, "SELECT thread_id FROM projection_threads ORDER BY created_at");
    let projects = sql_strings(&db_path, "SELECT project_id FROM projection_projects ORDER BY created_at");
    let roots = sql_strings(&db_path, "SELECT workspace_root FROM projection_projects ORDER BY created_at");
    let user_messages = sql_strings(
        &db_path,
        "SELECT thread_id || char(31) || message_id FROM projection_thread_messages WHERE role = 'user' ORDER BY created_at LIMIT 40",
    );
    let kinds = sql_strings(&db_path, "SELECT DISTINCT kind FROM projection_thread_activities ORDER BY kind");
    let max_sequence: i64 = rusqlite::Connection::open(&db_path)
        .unwrap()
        .query_row("SELECT COALESCE(MAX(sequence), 0) FROM orchestration_events", [], |row| row.get(0))
        .unwrap();

    let mut requests: Vec<Value> = vec![
        json!({"method": "getShellSnapshot"}),
        json!({"method": "getShellSnapshot", "args": [{"unsettledOnly": true}]}),
        json!({"method": "getArchivedShellSnapshot"}),
        json!({"method": "getCommandReadModel"}),
        json!({"method": "getSnapshot"}),
        json!({"method": "getSnapshotSequence"}),
        json!({"method": "getCounts"}),
        json!({"method": "listThreadsWithPullRequests"}),
        json!({"method": "getDeletedWorktreeThreads"}),
        json!({"method": "getProjectShells"}),
        json!({"method": "getProjectShells", "args": [projects.iter().take(2).collect::<Vec<_>>()]}),
        json!({"method": "getProjectShells", "args": [[]]}),
        json!({"method": "getEventReplayStats", "args": [{"fromSequenceExclusive": max_sequence - 500, "toSequenceInclusive": max_sequence}]}),
        json!({"method": "getEventReplayStats", "args": [{"fromSequenceExclusive": 0, "toSequenceInclusive": max_sequence}]}),
    ];
    for query in ["the", "e", "a", "zz-no-match", "%", "_", "!", "Rust", "  "] {
        requests.push(json!({"method": "searchThreads", "args": [{"query": query}]}));
        requests.push(json!({"method": "searchThreads", "args": [{"query": query, "limit": 3}]}));
    }
    for kind in &kinds {
        requests.push(json!({"method": "listActivitiesByKind", "args": [kind]}));
    }
    for project in &projects {
        requests.push(json!({"method": "getProjectShellById", "args": [project]}));
        requests.push(json!({"method": "getFirstActiveThreadIdByProjectId", "args": [project]}));
        requests.push(json!({"method": "getImportedAgentSessionSources", "args": [project]}));
    }
    for root in &roots {
        requests.push(json!({"method": "getActiveProjectByWorkspaceRoot", "args": [root]}));
    }
    for pair in &user_messages {
        let (thread, message) = pair.split_once('\u{1f}').unwrap();
        requests.push(json!({"method": "getTurnStartMessage", "args": [{"threadId": thread, "messageId": message}]}));
    }
    for thread in &threads {
        requests.push(json!({"method": "getThreadShellById", "args": [thread]}));
        requests.push(json!({"method": "getThreadRuntimeContext", "args": [thread]}));
        requests.push(json!({"method": "getThreadCheckpointContext", "args": [thread]}));
        requests.push(json!({"method": "getFullThreadDiffContext", "args": [thread, 1]}));
        requests.push(json!({"method": "getThreadDetailById", "args": [thread]}));
        requests.push(json!({"method": "getThreadDetailById", "args": [thread, {"activityKinds": ["tool.completed", "approval.requested"]}]}));
        requests.push(json!({"method": "getThreadDetailById", "args": [thread, {"activityKinds": []}]}));
        requests.push(json!({"method": "getThreadDetailSnapshot", "args": [thread]}));
        requests.push(json!({"method": "getThreadDetailSnapshotProjected", "args": [thread, null, false]}));
        requests.push(json!({"method": "getThreadDetailSnapshotProjected", "args": [thread, null, true]}));
        for turn_limit in [1, 2, 5] {
            requests.push(json!({"method": "getThreadDetailSnapshot", "args": [thread, {"turnLimit": turn_limit}]}));
        }
        requests.push(json!({"method": "getThreadDetailSnapshot", "args": [thread, {"turnLimit": 1, "beforeCursor": "garbage"}]}));
        requests.push(json!({"method": "getUserInputActivity", "args": [{"threadId": thread, "requestId": "none"}]}));
    }
    requests.push(json!({"method": "getThreadShellById", "args": ["no-such-thread"]}));
    requests.push(json!({"method": "getThreadDetailSnapshot", "args": ["no-such-thread"]}));

    let query = ProjectionSnapshotQuery::new(
        Db::open_with(&db_path, DbOptions { migrate: true, readers: 1 }).unwrap(),
        Arc::new(FixedRepositoryIdentities(identities)),
        Arc::new(NoThreadLiveState),
    );

    // Paging: follow the cursors TS hands out, page by page, on both sides.
    let mut total = 0;
    let mut different: Vec<String> = Vec::new();
    let mut text_identical = 0;
    let mut errors = 0;
    let mut nulls = 0;
    let mut reordered: BTreeMap<String, usize> = BTreeMap::new();
    let mut round = requests;
    let mut follow_ups_done = false;
    while !round.is_empty() {
        let requests_file = copies.attachments.join("../requests.json");
        std::fs::write(&requests_file, serde_json::to_string(&round).unwrap()).unwrap();
        let ts_lines = run_oracle(&[
            "query",
            "--db",
            db_path.to_str().unwrap(),
            "--requests",
            requests_file.to_str().unwrap(),
            "--identities",
            identities_file.to_str().unwrap(),
        ]);
        let ts_answers: Vec<Value> = ts_lines.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(ts_answers.len(), round.len());
        let mut next_round = Vec::new();
        for (request, ts) in round.iter().zip(&ts_answers) {
            let method = request["method"].as_str().unwrap();
            let args: Vec<Value> = request["args"].as_array().cloned().unwrap_or_default();
            let rust = rust_answer(&query, method, &args).await;
            total += 1;
            let mut diffs = Vec::new();
            json_diff("", &rust, ts, &mut diffs);
            if ts.get("error").is_some() {
                errors += 1;
            } else if ts["ok"].is_null() {
                nulls += 1;
            }
            if diffs.is_empty() {
                if serde_json::to_string(&rust).unwrap() == serde_json::to_string(ts).unwrap() {
                    text_identical += 1;
                } else {
                    *reordered.entry(method.to_string()).or_default() += 1;
                }
            } else {
                different.push(format!(
                    "{method} {}: {}",
                    serde_json::to_string(&args).unwrap().chars().take(100).collect::<String>(),
                    diffs.iter().take(6).cloned().collect::<Vec<_>>().join("; ")
                ));
            }
            if method == "getThreadDetailSnapshot" {
                if let Some(cursor) = ts["ok"]["page"]["beforeCursor"].as_str() {
                    let turn_limit = args[1]["turnLimit"].clone();
                    next_round.push(json!({"method": "getThreadDetailSnapshot", "args": [args[0], {"turnLimit": turn_limit, "beforeCursor": cursor}]}));
                    next_round.push(
                        json!({"method": "getThreadDetailSnapshotProjected", "args": [args[0], {"turnLimit": turn_limit, "beforeCursor": cursor}, false]}),
                    );
                    // A cursor from another thread falls back to the first page.
                    if !follow_ups_done {
                        follow_ups_done = true;
                        next_round.push(json!({"method": "getThreadDetailSnapshot", "args": ["no-such-thread", {"turnLimit": 1, "beforeCursor": cursor}]}));
                        for thread in threads.iter().take(3) {
                            next_round.push(json!({"method": "getThreadDetailSnapshot", "args": [thread, {"turnLimit": 2, "beforeCursor": cursor}]}));
                        }
                    }
                }
            }
        }
        if !next_round.is_empty() {
            eprintln!("   following {} page cursors", next_round.len());
        }
        round = next_round;
    }
    eprintln!(
        "{total} queries compared: {} equal as JSON ({text_identical} byte-identical), {} different",
        total - different.len(),
        different.len()
    );
    eprintln!("   TS answered {errors} errors and {nulls} empty results (both sides compared)");
    eprintln!("   same JSON, other key order: {reordered:?}");
    let resolved = identities_text.matches("canonicalKey").count();
    eprintln!("   {resolved} repository identities resolved by git");
    for line in &different {
        eprintln!("   {line}");
    }
    assert!(different.is_empty(), "{} queries differ", different.len());
}
