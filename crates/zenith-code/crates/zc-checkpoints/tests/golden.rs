//! Golden comparison against the TypeScript server: the same scripted scenario runs through the
//! TS `CheckpointReactor` (`golden/reactor_oracle.mjs`, node with the real effect/contracts
//! packages) and through the Rust reactor, each on its own copy of a scripted repository. The
//! orchestration events (types, payloads, sequences), the receipts, the turn diffs, the
//! provider rollbacks and the tree of every checkpoint ref must match.
//!
//! Needs `node` and `code/apps/server/node_modules` (symlinked from a checkout that has them);
//! without them the test prints why and passes vacuously. Server-generated ids (UUIDs), the
//! repository path and wall-clock timestamps are normalized.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use common::*;
use futures::StreamExt;
use regex::Regex;
use serde_json::{json, Value};
use zc_checkpoints::reactor::{CheckpointReactor, CheckpointReactorDeps, NoWorkspaceEntries};
use zc_checkpoints::receipts::RuntimeReceiptBus;
use zc_checkpoints::CheckpointDiffQuery;
use zc_contracts::{OrchestrationGetFullThreadDiffInput, OrchestrationGetTurnDiffInput};

fn server_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../code/apps/server")
}

fn oracle_available() -> Result<(), String> {
    if !server_dir().join("node_modules/effect").exists() {
        return Err(format!("{} has no node_modules", server_dir().display()));
    }
    match Command::new("node").arg("--version").output() {
        Ok(output) if output.status.success() => Ok(()),
        _ => Err("node is not installed".into()),
    }
}

fn run_oracle(script: &str, request: &Value) -> Value {
    let script = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(script)).unwrap();
    let server = std::fs::canonicalize(server_dir()).unwrap();
    let mut child = Command::new("node")
        .args(["--no-warnings", "--input-type=module", "-e", &script])
        .current_dir(&server)
        .env("ZC_SERVER_SRC", server.join("src"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(request.to_string().as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "oracle failed: {}", String::from_utf8_lossy(&output.stderr));
    // The server logs on stdout too: the result follows the marker.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json = stdout.rsplit("@@ORACLE@@").next().unwrap_or_default();
    serde_json::from_str(json).unwrap_or_else(|e| panic!("oracle output: {e}: {stdout}"))
}

/// Replace what legitimately differs between two runs.
fn normalize(value: &mut Value, repo: &str) {
    let uuid = Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap();
    let iso = Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$").unwrap();
    fn walk(value: &mut Value, repo: &str, uuid: &Regex, iso: &Regex) {
        match value {
            Value::String(text) => {
                let replaced = uuid.replace_all(&text.replace(repo, "<repo>"), "<uuid>").into_owned();
                *text = if iso.is_match(&replaced) && replaced != NOW {
                    "<time>".into()
                } else {
                    replaced
                };
            }
            Value::Array(items) => items.iter_mut().for_each(|item| walk(item, repo, uuid, iso)),
            Value::Object(map) => map.values_mut().for_each(|item| walk(item, repo, uuid, iso)),
            _ => {}
        }
    }
    walk(value, repo, &uuid, &iso);
}

fn refs_and_trees(repo: &Path) -> Vec<(String, String)> {
    git(repo, &["for-each-ref", "--format=%(refname)", "refs/t3"])
        .lines()
        .map(|reference| {
            (
                reference.to_owned(),
                git(repo, &["rev-parse", &format!("{reference}^{{tree}}")]).trim().to_owned(),
            )
        })
        .collect()
}

fn event(kind: &str, turn: &str, extra: Value) -> Value {
    let mut event =
        json!({"type": kind, "eventId": format!("evt-{kind}-{turn}"), "provider": "codex", "createdAt": NOW, "threadId": "thread-1", "turnId": turn});
    event.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    event
}

/// The scenario: two captured turns with a whitespace-only edit, diffs (including an error), a
/// revert to turn 1, and an interrupted third turn.
fn steps(repo: &Path) -> Vec<Value> {
    let file = |name: &str| repo.join(name).to_string_lossy().into_owned();
    vec![
        json!({"op": "dispatch", "command": {"type": "project.create", "commandId": "cmd-project", "projectId": "project-1", "title": "Golden",
              "workspaceRoot": repo, "defaultModelSelection": model_selection(), "createdAt": NOW}}),
        json!({"op": "dispatch", "command": {"type": "thread.create", "commandId": "cmd-thread", "threadId": "thread-1", "projectId": "project-1",
              "title": "Thread", "modelSelection": model_selection(), "interactionMode": "default", "runtimeMode": "approval-required",
              "branch": null, "worktreePath": repo, "createdAt": NOW}}),
        json!({"op": "dispatch", "command": {"type": "thread.session.set", "commandId": "cmd-session", "threadId": "thread-1",
              "session": {"threadId": "thread-1", "status": "ready", "providerName": "codex", "runtimeMode": "approval-required",
                          "activeTurnId": null, "lastError": null, "updatedAt": NOW}, "createdAt": NOW}}),
        json!({"op": "emit", "event": event("turn.started", "turn-1", json!({}))}),
        json!({"op": "receipts", "count": 1}),
        json!({"op": "write", "path": file("README.md"), "contents": "v2\n"}),
        json!({"op": "write", "path": file("notes.txt"), "contents": "a\nb\n"}),
        json!({"op": "emit", "event": event("turn.completed", "turn-1", json!({"payload": {"state": "completed"}}))}),
        json!({"op": "receipts", "count": 2}),
        json!({"op": "drain"}),
        json!({"op": "emit", "event": event("turn.started", "turn-2", json!({}))}),
        json!({"op": "drain"}),
        json!({"op": "write", "path": file("notes.txt"), "contents": "a \nb\n"}),
        json!({"op": "write", "path": file("src.txt"), "contents": "fn main() {}\n"}),
        json!({"op": "emit", "event": event("turn.completed", "turn-2", json!({"payload": {"state": "failed"}}))}),
        json!({"op": "receipts", "count": 2}),
        json!({"op": "drain"}),
        json!({"op": "turnDiff", "input": {"threadId": "thread-1", "fromTurnCount": 0, "toTurnCount": 1}}),
        json!({"op": "turnDiff", "input": {"threadId": "thread-1", "fromTurnCount": 1, "toTurnCount": 2}}),
        json!({"op": "turnDiff", "input": {"threadId": "thread-1", "fromTurnCount": 1, "toTurnCount": 2, "ignoreWhitespace": false}}),
        json!({"op": "fullThreadDiff", "input": {"threadId": "thread-1", "toTurnCount": 2}}),
        json!({"op": "turnDiff", "input": {"threadId": "thread-1", "fromTurnCount": 0, "toTurnCount": 5}}),
        json!({"op": "turnDiff", "input": {"threadId": "thread-missing", "fromTurnCount": 0, "toTurnCount": 1}}),
        json!({"op": "dispatch", "command": {"type": "thread.checkpoint.revert", "commandId": "cmd-revert", "threadId": "thread-1", "turnCount": 1, "createdAt": NOW}}),
        json!({"op": "waitEvent", "type": "thread.reverted"}),
        json!({"op": "drain"}),
        json!({"op": "emit", "event": event("turn.started", "turn-3", json!({}))}),
        json!({"op": "drain"}),
        json!({"op": "write", "path": file("README.md"), "contents": "v4\n"}),
        json!({"op": "emit", "event": event("turn.aborted", "turn-3", json!({"payload": {"reason": "Interrupted by user."}}))}),
        json!({"op": "receipts", "count": 2}),
        json!({"op": "drain"}),
    ]
}

async fn run_rust(session: &Value, steps: &[Value]) -> Value {
    let engine = engine().await;
    let projections = EngineProjections { engine: engine.clone() };
    let providers = FakeProviders::new(Some(session.clone()));
    let bus = RuntimeReceiptBus::for_test();
    let mut receipts_sub = bus.subscribe_for_test().unwrap();
    let store = store();
    let reactor = CheckpointReactor::new(CheckpointReactorDeps {
        engine: Arc::new(engine.clone()),
        projections: Arc::new(projections.clone()),
        providers: providers.clone(),
        store: store.clone(),
        receipts: bus,
        workspace_entries: Arc::new(NoWorkspaceEntries),
        vcs_status: FakeVcsStatus::new(None),
        pull_requests: Arc::new(FakePullRequests::default()),
    });
    let _tasks = reactor.start();
    let diffs = CheckpointDiffQuery::new(Arc::new(projections.clone()), store);
    let mut results = Vec::new();
    let mut receipts = Vec::new();
    for step in steps {
        match step["op"].as_str().unwrap() {
            "dispatch" => {
                dispatch(&engine, step["command"].clone()).await;
            }
            "emit" => providers.emit(step["event"].clone()),
            "receipts" => {
                for _ in 0..step["count"].as_u64().unwrap() {
                    let receipt = tokio::time::timeout(std::time::Duration::from_secs(15), receipts_sub.recv())
                        .await
                        .unwrap()
                        .unwrap();
                    receipts.push(serde_json::to_value(receipt).unwrap());
                }
            }
            "drain" => {
                settle().await;
                reactor.drain().await;
                settle().await;
                reactor.drain().await;
            }
            "write" => std::fs::write(step["path"].as_str().unwrap(), step["contents"].as_str().unwrap()).unwrap(),
            "turnDiff" => {
                let input: OrchestrationGetTurnDiffInput = decode(step["input"].clone());
                results.push(match diffs.get_turn_diff(&input).await {
                    Ok(diff) => json!({"ok": diff}),
                    Err(error) => json!({"error": {"_tag": error.tag(), "message": error.message()}}),
                });
            }
            "fullThreadDiff" => {
                let input: OrchestrationGetFullThreadDiffInput = decode(step["input"].clone());
                results.push(match diffs.get_full_thread_diff(&input).await {
                    Ok(diff) => json!({"ok": diff}),
                    Err(error) => json!({"error": {"_tag": error.tag(), "message": error.message()}}),
                });
            }
            "waitEvent" => {
                let kind = step["type"].as_str().unwrap().to_owned();
                for _ in 0..1500 {
                    let events: Vec<_> = engine.read_events(0, None).collect().await;
                    if events.iter().flatten().any(|e| serde_json::to_value(e).unwrap()["type"] == json!(kind)) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
            other => panic!("unknown op {other}"),
        }
    }
    let events: Vec<Value> = engine
        .read_events(0, None)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|e| serde_json::to_value(e.unwrap()).unwrap())
        .collect();
    let rollbacks: Vec<Value> = providers
        .rollbacks()
        .into_iter()
        .map(|(thread_id, num_turns)| json!({"threadId": thread_id, "numTurns": num_turns}))
        .collect();
    json!({"events": events, "results": results, "receipts": receipts, "rollbacks": rollbacks})
}

#[tokio::test(flavor = "multi_thread")]
async fn checkpoint_capture_diff_and_restore_match_the_typescript_reactor() {
    if let Err(reason) = oracle_available() {
        eprintln!("skipping the TS golden comparison: {reason}");
        return;
    }
    let (_dir, root) = temp_dir();
    let ts_repo = root.join("ts").join("repo");
    let rust_repo = root.join("rust").join("repo");
    for repo in [&ts_repo, &rust_repo] {
        create_git_repository(repo);
    }
    let session = |repo: &Path| FakeProviders::session("thread-1", repo.to_str().unwrap(), "codex");

    let mut expected = run_oracle(
        "reactor_oracle.mjs",
        &json!({"baseDir": root.join("ts"), "session": session(&ts_repo), "steps": steps(&ts_repo)}),
    );
    let mut actual = run_rust(&session(&rust_repo), &steps(&rust_repo)).await;
    normalize(&mut expected, ts_repo.to_str().unwrap());
    normalize(&mut actual, rust_repo.to_str().unwrap());

    let expected_events = expected["events"].as_array().unwrap();
    // The comparison must cover the reactor's work, not two empty logs.
    let types: Vec<&str> = expected_events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    for kind in [
        "thread.turn-diff-completed",
        "thread.activity-appended",
        "thread.checkpoint-revert-requested",
        "thread.reverted",
    ] {
        assert!(types.contains(&kind), "the TS run lacks {kind}: {types:?}");
    }
    assert_eq!(expected["results"].as_array().unwrap().len(), 6);
    assert_eq!(expected["receipts"].as_array().unwrap().len(), 7);
    eprintln!("compared {} events: {types:?}", types.len());
    let actual_events = actual["events"].as_array().unwrap();
    for (index, (want, got)) in expected_events.iter().zip(actual_events).enumerate() {
        assert_eq!(got, want, "event #{index} differs");
    }
    assert_eq!(actual_events.len(), expected_events.len(), "event count");
    assert_eq!(actual["receipts"], expected["receipts"], "receipts");
    assert_eq!(actual["results"], expected["results"], "turn diffs");
    assert_eq!(actual["rollbacks"], expected["rollbacks"], "rollbacks");
    assert_eq!(refs_and_trees(&rust_repo), refs_and_trees(&ts_repo), "checkpoint ref trees");
    for file in ["README.md", "notes.txt"] {
        assert_eq!(
            std::fs::read_to_string(rust_repo.join(file)).ok(),
            std::fs::read_to_string(ts_repo.join(file)).ok(),
            "{file}"
        );
    }
    assert_eq!(rust_repo.join("src.txt").exists(), ts_repo.join("src.txt").exists());
}
