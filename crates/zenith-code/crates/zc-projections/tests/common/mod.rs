//! Helpers shared by the gate and scenario tests: the TS oracle, table dumps, JSON diffs, and
//! the Rust side of every oracle request.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::types::Value as SqlValue;
use serde_json::{json, Value};
use zc_contracts::{OrchestrationSearchThreadsInput, OrchestrationThreadDetailWindow};
use zc_projections::activity_payload::project_thread_detail_snapshot;
use zc_projections::query::ThreadDetailQuery;
use zc_projections::ProjectionSnapshotQuery;

pub const PROJECTION_TABLES: &[(&str, &str)] = &[
    ("projection_projects", "project_id"),
    ("projection_threads", "thread_id"),
    ("projection_thread_messages", "message_id"),
    ("projection_thread_activities", "activity_id"),
    ("projection_thread_sessions", "thread_id"),
    ("projection_turns", "thread_id, turn_id, pending_message_id"),
    ("projection_pending_approvals", "request_id"),
    ("projection_thread_proposed_plans", "plan_id"),
    ("projection_thread_pull_requests", "thread_id, host, repository, number"),
    ("projection_state", "projector"),
];

pub fn code_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../../code")
}

pub fn node_available() -> bool {
    code_dir().join("apps/server/node_modules").exists() && Command::new("node").arg("--version").output().is_ok()
}

pub type Rows = BTreeMap<String, Vec<(String, SqlValue)>>;

pub fn table_rows(path: &Path, table: &str, key: &str) -> Rows {
    let conn = rusqlite::Connection::open(path).unwrap();
    let columns: Vec<String> = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .map(Result::unwrap)
        // AUTOINCREMENT row ids are not event-derived: a rebuild numbers turns anew.
        .filter(|column| column != "row_id")
        .collect();
    let key_columns: Vec<&str> = key.split(',').map(str::trim).collect();
    let mut statement = conn.prepare(&format!("SELECT {} FROM {table}", columns.join(", "))).unwrap();
    let mut rows = statement.query([]).unwrap();
    let mut out = Rows::new();
    while let Some(row) = rows.next().unwrap() {
        let values: Vec<(String, SqlValue)> = columns
            .iter()
            .enumerate()
            .map(|(index, column)| (column.clone(), row.get::<_, SqlValue>(index).unwrap()))
            .collect();
        let key = key_columns
            .iter()
            .map(|column| format!("{:?}", values.iter().find(|(name, _)| name == column).unwrap().1))
            .collect::<Vec<_>>()
            .join("|");
        assert!(out.insert(key, values).is_none(), "duplicate key in {table}");
    }
    out
}

/// Keys whose rows differ (or exist on one side only), with the differing columns.
pub fn diff_rows(left: &Rows, right: &Rows) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    for (key, values) in left {
        match right.get(key) {
            None => {
                out.insert(key.clone(), vec!["<only left>".to_string()]);
            }
            Some(other) => {
                let columns: Vec<String> = values
                    .iter()
                    .zip(other)
                    .filter(|((_, a), (_, b))| a != b)
                    .map(|((name, _), _)| name.clone())
                    .collect();
                if !columns.is_empty() {
                    out.insert(key.clone(), columns);
                }
            }
        }
    }
    for key in right.keys() {
        if !left.contains_key(key) {
            out.insert(key.clone(), vec!["<only right>".to_string()]);
        }
    }
    out
}

pub fn run_oracle(args: &[&str]) -> String {
    let output = Command::new("node")
        .arg("scripts/projections-oracle.ts")
        .args(args)
        .current_dir(code_dir().join("apps/server"))
        .output()
        .expect("run node");
    assert!(output.status.success(), "oracle failed: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

/// Recursively compares two JSON values, ignoring object key order; collects the paths that
/// differ.
pub fn json_diff(path: &str, left: &Value, right: &Value, out: &mut Vec<String>) {
    match (left, right) {
        (Value::Object(a), Value::Object(b)) => {
            for (key, value) in a {
                match b.get(key) {
                    Some(other) => json_diff(&format!("{path}.{key}"), value, other, out),
                    None => out.push(format!("{path}.{key}: only rust")),
                }
            }
            for key in b.keys() {
                if !a.contains_key(key) {
                    out.push(format!("{path}.{key}: only ts"));
                }
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                out.push(format!("{path}: length {} vs {}", a.len(), b.len()));
            }
            for (index, (x, y)) in a.iter().zip(b).enumerate() {
                json_diff(&format!("{path}[{index}]"), x, y, out);
            }
        }
        (Value::Number(a), Value::Number(b)) => {
            if a.as_f64() != b.as_f64() {
                out.push(format!("{path}: {a} vs {b}"));
            }
        }
        _ => {
            if left != right {
                let short = |value: &Value| {
                    let text = value.to_string();
                    text.chars().take(120).collect::<String>()
                };
                out.push(format!("{path}: {} vs {}", short(left), short(right)));
            }
        }
    }
}

pub fn to<T: serde::Serialize>(value: T) -> Value {
    serde_json::to_value(value).unwrap()
}

/// Runs one oracle request on the Rust side and encodes the answer like the TS oracle.
pub async fn rust_answer(query: &ProjectionSnapshotQuery, method: &str, args: &[Value]) -> Value {
    let s = |index: usize| args.get(index).and_then(Value::as_str).unwrap_or("").to_string();
    let enc = |value: Result<Value, zc_db::DbError>| match value {
        Ok(value) => json!({"ok": value}),
        Err(error) => json!({"error": format!("{error}")}),
    };
    let window = |value: Option<&Value>| -> Option<OrchestrationThreadDetailWindow> {
        value
            .filter(|value| !value.is_null())
            .map(|value| serde_json::from_value(value.clone()).unwrap())
    };
    match method {
        "getShellSnapshot" => enc(query
            .get_shell_snapshot(args.first().and_then(|a| a.get("unsettledOnly")).and_then(Value::as_bool).unwrap_or(false))
            .await
            .map(to)),
        "getArchivedShellSnapshot" => enc(query.get_archived_shell_snapshot().await.map(to)),
        "getSnapshot" => enc(query.get_snapshot().await.map(to)),
        "getCommandReadModel" => enc(query.get_command_read_model().await.map(to)),
        "getThreadDetailSnapshot" => enc(query.get_thread_detail_snapshot(&s(0), window(args.get(1))).await.map(to)),
        "getThreadDetailSnapshotProjected" => {
            let reasoning = args.get(2).and_then(Value::as_bool).unwrap_or(false);
            enc(query
                .get_thread_detail_snapshot(&s(0), window(args.get(1)))
                .await
                .map(|snapshot| to(snapshot.map(|snapshot| project_thread_detail_snapshot(snapshot, reasoning)))))
        }
        "getThreadDetailById" => {
            let kinds = args
                .get(1)
                .and_then(|value| value.get("activityKinds"))
                .and_then(Value::as_array)
                .map(|kinds| kinds.iter().filter_map(Value::as_str).map(str::to_owned).collect());
            enc(query
                .get_thread_detail_by_id(&s(0), Some(ThreadDetailQuery { activity_kinds: kinds }))
                .await
                .map(to))
        }
        "getThreadShellById" => enc(query.get_thread_shell_by_id(&s(0)).await.map(to)),
        "getProjectShellById" => enc(query.get_project_shell_by_id(&s(0)).await.map(to)),
        "getProjectShells" => enc(query
            .get_project_shells(
                args.first()
                    .and_then(Value::as_array)
                    .map(|ids| ids.iter().filter_map(Value::as_str).map(str::to_owned).collect()),
            )
            .await
            .map(to)),
        "getActiveProjectByWorkspaceRoot" => enc(query.get_active_project_by_workspace_root(&s(0)).await.map(to)),
        "searchThreads" => {
            let input: OrchestrationSearchThreadsInput = serde_json::from_value(args[0].clone()).unwrap();
            enc(query.search_threads(&input).await.map(to))
        }
        "listActivitiesByKind" => enc(query.list_activities_by_kind(&s(0)).await.map(to)),
        "getUserInputActivity" => {
            let input = &args[0];
            enc(query
                .get_user_input_activity(input["threadId"].as_str().unwrap_or(""), input["requestId"].as_str().unwrap_or(""))
                .await
                .map(to))
        }
        "getThreadRuntimeContext" => enc(query.get_thread_runtime_context(&s(0)).await.map(|context| {
            to(context.map(|context| {
                let mut out = json!({
                    "id": context.id,
                    "projectId": context.project_id,
                    "title": context.title,
                    "titleState": context.title_state,
                    "session": context.session,
                });
                if out["titleState"].is_null() {
                    out["titleState"] = Value::Null;
                }
                out
            }))
        })),
        "getTurnStartMessage" => {
            let input = &args[0];
            enc(query
                .get_turn_start_message(input["threadId"].as_str().unwrap_or(""), input["messageId"].as_str().unwrap_or(""))
                .await
                .map(|message| {
                    to(message.map(|message| {
                        json!({
                            "message": message.message,
                            "hasOtherUserMessages": message.has_other_user_messages,
                        })
                    }))
                }))
        }
        "getThreadCheckpointContext" => enc(query.get_thread_checkpoint_context(&s(0)).await.map(|context| {
            to(context.map(|context| {
                json!({
                    "threadId": context.thread_id,
                    "projectId": context.project_id,
                    "workspaceRoot": context.workspace_root,
                    "worktreePath": context.worktree_path,
                    "checkpoints": context.checkpoints,
                })
            }))
        })),
        "getFullThreadDiffContext" => {
            let count = args.get(1).and_then(Value::as_i64).unwrap_or(0);
            enc(query.get_full_thread_diff_context(&s(0), count).await.map(|context| {
                to(context.map(|context| {
                    json!({
                        "threadId": context.thread_id,
                        "projectId": context.project_id,
                        "workspaceRoot": context.workspace_root,
                        "worktreePath": context.worktree_path,
                        "latestCheckpointTurnCount": context.latest_checkpoint_turn_count,
                        "toCheckpointRef": context.to_checkpoint_ref,
                    })
                }))
            }))
        }
        "getFirstActiveThreadIdByProjectId" => enc(query.get_first_active_thread_id_by_project_id(&s(0)).await.map(to)),
        "getImportedAgentSessionSources" => enc(query.get_imported_agent_session_sources(&s(0)).await.map(|rows| {
            to(rows
                .into_iter()
                .map(|row| json!({"threadId": row.thread_id, "source": row.source}))
                .collect::<Vec<_>>())
        })),
        "listThreadsWithPullRequests" => enc(query.list_threads_with_pull_requests().await.map(|rows| {
            to(rows
                .into_iter()
                .map(|row| {
                    json!({
                        "id": row.id,
                        "projectId": row.project_id,
                        "settledOverride": row.settled_override,
                        "settledAt": row.settled_at,
                        "pullRequests": row.pull_requests,
                    })
                })
                .collect::<Vec<_>>())
        })),
        "getDeletedWorktreeThreads" => enc(query.get_deleted_worktree_threads().await.map(|rows| {
            to(rows
                .into_iter()
                .map(|row| {
                    json!({
                        "id": row.id,
                        "projectId": row.project_id,
                        "branch": row.branch,
                        "worktreePath": row.worktree_path,
                        "workspaceRoot": row.workspace_root,
                        "deletedAt": row.deleted_at,
                    })
                })
                .collect::<Vec<_>>())
        })),
        "getSnapshotSequence" => enc(query.get_snapshot_sequence().await.map(|sequence| json!({"snapshotSequence": sequence}))),
        "getCounts" => enc(query.get_counts().await.map(|counts| {
            json!({
                "projectCount": counts.project_count,
                "threadCount": counts.thread_count,
            })
        })),
        "getEventReplayStats" => {
            let input = &args[0];
            enc(query
                .get_event_replay_stats(
                    input["fromSequenceExclusive"].as_i64().unwrap_or(0),
                    input["toSequenceInclusive"].as_i64().unwrap_or(0),
                )
                .await
                .map(|stats| json!({"eventCount": stats.event_count, "payloadBytes": stats.payload_bytes})))
        }
        other => panic!("unknown method {other}"),
    }
}
