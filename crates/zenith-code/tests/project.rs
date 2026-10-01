//! WP-25 end to end: the assembled server (`App::build` on a temp base dir) driven over the RPC
//! protocol in memory.
//!
//! - `subscribeProjectClones` + `projectClone.start` clone a local bare repository (through
//!   `file://`, so git reports transfer progress): the stream shows the clone running, its
//!   progress, then done; the project exists, its repository identity is refreshed, and a thread
//!   can be created in it.
//! - `agentSessions.scan` and `agentSessions.import` on fixture Claude/Codex homes (set in the
//!   temp server's settings.json) create the imported threads in the server's projections.
//! - The descriptor advertises `repositoryIdentity` and `projectCloneTracking`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use zc_contracts::{ProjectId, ThreadId};
use zc_rpc::{AuthContext, ConnectionSetup, Inbound, Outbound, RpcServer};
use zenith_code::app::App;
use zenith_code::cli::{resolve_serve_config, ServeArgs};

fn git(cwd: &Path, args: &[&str]) {
    let output = std::process::Command::new("git").args(args).current_dir(cwd).output().unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

async fn build_app(root: &Path) -> App {
    let base = root.join("base");
    let args = ServeArgs {
        cwd: Some(root.to_string_lossy().into_owned()),
        mode: None,
        port: Some(0),
        host: Some("127.0.0.1".into()),
        base_dir: Some(base.to_string_lossy().into_owned()),
        dev_url: None,
        no_browser: true,
        bootstrap_fd: None,
        auto_bootstrap_project_from_cwd: false,
        log_websocket_events: false,
        tailscale_serve: false,
        tailscale_serve_port: None,
        static_dir: None,
    };
    let config = resolve_serve_config(&args, None).await.unwrap();
    // The agent homes the scanner reads: fixtures, never the real ones.
    let settings_path = config.paths.settings_path.clone();
    std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
    std::fs::write(
        &settings_path,
        json!({"providers": {"claudeAgent": {"homePath": root.join("claude")}, "codex": {"homePath": root.join("codex")}}}).to_string(),
    )
    .unwrap();
    App::build(config).await.unwrap()
}

struct Client {
    tx: mpsc::UnboundedSender<Inbound>,
    rx: mpsc::UnboundedReceiver<Outbound>,
    next: u64,
}

impl Client {
    fn connect(server: &Arc<RpcServer>) -> Self {
        let (tx, in_rx) = mpsc::unbounded();
        let (out_tx, rx) = mpsc::unbounded();
        let server = server.clone();
        let setup = ConnectionSetup {
            auth: AuthContext::new(["orchestration:read", "orchestration:operate"]),
            ..Default::default()
        };
        tokio::spawn(async move { server.serve_socket(setup, in_rx, out_tx.sink_map_err(|_| ())).await });
        Self { tx, rx, next: 0 }
    }

    fn request(&mut self, tag: &str, payload: Value) -> String {
        self.next += 1;
        let id = self.next.to_string();
        let frame = json!({"_tag": "Request", "id": id, "tag": tag, "payload": payload, "headers": []});
        self.tx.unbounded_send(Inbound::Text(frame.to_string())).unwrap();
        id
    }

    fn ack(&self, id: &str) {
        self.tx
            .unbounded_send(Inbound::Text(json!({"_tag": "Ack", "requestId": id}).to_string()))
            .unwrap();
    }

    async fn recv(&mut self) -> Value {
        match tokio::time::timeout(Duration::from_secs(60), self.rx.next()).await {
            Ok(Some(Outbound::Text(text))) => serde_json::from_str(&text).unwrap(),
            other => panic!("expected a frame, got {other:?}"),
        }
    }

    /// The next frame of `id`, acknowledging stream chunks (other requests' frames go to
    /// `others`).
    async fn frame_of(&mut self, id: &str, others: &mut Vec<Value>) -> Value {
        loop {
            let frame = self.recv().await;
            if frame["requestId"] == id {
                if frame["_tag"] == "Chunk" {
                    self.ack(id);
                }
                return frame;
            }
            if frame["_tag"] == "Chunk" {
                self.ack(frame["requestId"].as_str().unwrap_or_default());
            }
            others.push(frame);
        }
    }

    async fn call(&mut self, tag: &str, payload: Value, others: &mut Vec<Value>) -> Value {
        let id = self.request(tag, payload);
        let exit = self.frame_of(&id, others).await;
        assert_eq!(exit["_tag"], "Exit", "{exit}");
        assert_eq!(exit["exit"]["_tag"], "Success", "{tag}: {exit}");
        exit["exit"]["value"].clone()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clones_a_local_repository_through_the_rpc() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    // A source repository with some history, and its bare copy.
    let source = root.join("source");
    std::fs::create_dir_all(&source).unwrap();
    git(&source, &["init", "-q", "--initial-branch=main"]);
    git(&source, &["config", "user.email", "test@example.com"]);
    git(&source, &["config", "user.name", "Test User"]);
    for index in 0..40 {
        std::fs::write(source.join(format!("file-{index}.txt")), format!("content {index}\n").repeat(200)).unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-q", "-m", &format!("Commit {index}")]);
    }
    git(&root, &["clone", "-q", "--bare", "source", "sample-app.git"]);
    let remote_url = format!("file://{}", root.join("sample-app.git").display());
    let destination = root.join("projects").join("sample-app");

    let app = build_app(&root).await;
    let capabilities = &app.environment.descriptor_json()["capabilities"];
    assert_eq!(capabilities["repositoryIdentity"], true);
    assert_eq!(capabilities["projectCloneTracking"], true);

    let mut client = Client::connect(&app.rpc);
    let mut others = Vec::new();
    let subscription = client.request("subscribeProjectClones", json!({}));
    let first = client.frame_of(&subscription, &mut others).await;
    assert_eq!(first["_tag"], "Chunk");
    assert_eq!(first["values"], json!([[]]));

    let result = client
        .call(
            "projectClone.start",
            json!({
                "projectId": "project-clone-1",
                "title": " Sample app ",
                "createdAt": "2026-01-01T00:00:00.000Z",
                "remoteUrl": remote_url,
                "destinationPath": destination,
            }),
            &mut others,
        )
        .await;
    assert_eq!(
        result,
        json!({"projectId": "project-clone-1", "cwd": destination, "remoteUrl": remote_url, "repository": null})
    );

    // The stream: running (with progress), then done.
    let mut lists: Vec<Value> = Vec::new();
    loop {
        let frame = client.frame_of(&subscription, &mut others).await;
        assert_eq!(frame["_tag"], "Chunk", "{frame}");
        let values = frame["values"].as_array().unwrap().clone();
        let done = values.iter().any(|list| list[0]["phase"] == "done");
        lists.extend(values);
        if done {
            break;
        }
    }
    let snapshots: Vec<&Value> = lists.iter().filter_map(|list| list.get(0)).collect();
    assert!(snapshots.iter().any(|s| s["phase"] == "running"));
    let progressed: Vec<&str> = snapshots
        .iter()
        .filter_map(|s| s["stage"].as_str())
        .filter(|stage| *stage != "connecting")
        .collect();
    assert!(!progressed.is_empty(), "git reported no progress: {snapshots:?}");
    let done = snapshots.last().unwrap();
    assert_eq!(done["phase"], "done");
    assert_eq!(done["percent"], 100);
    assert_eq!(done["remoteUrl"], remote_url);
    assert_eq!(done["destinationPath"], json!(destination));
    assert!(done["endedAt"].is_string());
    let sequences: Vec<i64> = snapshots.iter().map(|s| s["sequence"].as_i64().unwrap()).collect();
    assert!(sequences.windows(2).all(|w| w[0] < w[1]), "{sequences:?}");
    assert!(destination.join("file-39.txt").exists());

    // The project exists (title trimmed), and its identity was refreshed after the clone.
    let state = &app.state;
    let project = state.reads.get_project_shell_by_id(&ProjectId::new("project-clone-1")).await.unwrap().unwrap();
    assert_eq!(project.title, "Sample app");
    assert_eq!(project.workspace_root, destination.to_string_lossy());
    let mut identity = None;
    for _ in 0..50 {
        let shell = state.reads.get_project_shell_by_id(&ProjectId::new("project-clone-1")).await.unwrap().unwrap();
        identity = shell.repository_identity.clone().flatten();
        if identity.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let identity = identity.expect("the cloned project has a repository identity");
    assert_eq!(identity.locator.remote_url, remote_url);

    // The clone landed: threads may start in it.
    client
        .call(
            "orchestration.dispatchCommand",
            json!({
                "type": "thread.create", "commandId": "create-thread-in-clone", "threadId": "thread-in-clone", "projectId": "project-clone-1",
                "title": "First thread", "modelSelection": {"instanceId": "codex", "model": "gpt-5-codex"}, "runtimeMode": "full-access",
                "interactionMode": "default", "branch": null, "worktreePath": null, "createdAt": "2026-01-01T00:00:01.000Z",
            }),
            &mut others,
        )
        .await;

    // Nothing to cancel or retry once done; a finished clone leaves the list after 30 s.
    assert_eq!(
        client.call("projectClone.cancel", json!({"projectId": "project-clone-1"}), &mut others).await,
        json!({"applied": false})
    );
    assert_eq!(
        client.call("projectClone.retry", json!({"projectId": "project-clone-1"}), &mut others).await,
        json!({"applied": false})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_clone_blocks_threads_until_it_is_retried() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let missing = format!("file://{}", root.join("missing.git").display());
    let destination = root.join("projects").join("missing");
    let app = build_app(&root).await;
    let mut client = Client::connect(&app.rpc);
    let mut others = Vec::new();
    let subscription = client.request("subscribeProjectClones", json!({}));
    client.frame_of(&subscription, &mut others).await;
    client
        .call(
            "projectClone.start",
            json!({"projectId": "project-clone-2", "title": "Missing", "createdAt": "2026-01-01T00:00:00.000Z", "remoteUrl": missing, "destinationPath": destination}),
            &mut others,
        )
        .await;
    loop {
        let frame = client.frame_of(&subscription, &mut others).await;
        let failed = frame["values"].as_array().unwrap().iter().find(|list| list[0]["phase"] == "failed").cloned();
        if let Some(list) = failed {
            assert!(list[0]["error"].as_str().unwrap().contains("does not appear to be a git repository"), "{list}");
            break;
        }
    }
    let id = client.request(
        "orchestration.dispatchCommand",
        json!({
            "type": "thread.create", "commandId": "create-thread-in-failed-clone", "threadId": "thread-in-failed-clone", "projectId": "project-clone-2",
            "title": "First thread", "modelSelection": {"instanceId": "codex", "model": "gpt-5-codex"}, "runtimeMode": "full-access",
            "interactionMode": "default", "branch": null, "worktreePath": null, "createdAt": "2026-01-01T00:00:01.000Z",
        }),
    );
    let exit = client.frame_of(&id, &mut others).await;
    assert_eq!(exit["exit"]["_tag"], "Failure");
    assert_eq!(
        exit["exit"]["cause"][0]["error"]["message"],
        "The repository was not cloned. Retry the clone first."
    );
    // A second clone into the same destination is refused while this one is tracked.
    let id = client.request(
        "projectClone.start",
        json!({"projectId": "project-clone-3", "title": "Again", "createdAt": "2026-01-01T00:00:00.000Z", "remoteUrl": missing, "destinationPath": destination}),
    );
    let exit = client.frame_of(&id, &mut others).await;
    assert_eq!(exit["exit"]["cause"][0]["error"]["_tag"], "SourceControlRepositoryError");
    // Deleting the project forgets the clone.
    client
        .call(
            "orchestration.dispatchCommand",
            json!({"type": "project.delete", "commandId": "delete-failed-clone", "projectId": "project-clone-2"}),
            &mut others,
        )
        .await;
    loop {
        let frame = client.frame_of(&subscription, &mut others).await;
        if frame["values"]
            .as_array()
            .unwrap()
            .iter()
            .any(|list| list.as_array().is_some_and(Vec::is_empty))
        {
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scans_and_imports_agent_sessions_through_the_rpc() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let now = zc_core::now_millis();
    let iso = |ms: i64| zc_core::time::iso_from_millis(ms);
    let claude_file = root.join("claude/projects/-workspace/123e4567-e89b-42d3-a456-426614174000.jsonl");
    std::fs::create_dir_all(claude_file.parent().unwrap()).unwrap();
    std::fs::write(
        &claude_file,
        [
            json!({"type": "user", "cwd": workspace, "sessionId": "123e4567-e89b-42d3-a456-426614174000", "timestamp": iso(now - 120_000), "message": {"role": "user", "content": "Fix the project"}}).to_string(),
            json!({"type": "assistant", "sessionId": "123e4567-e89b-42d3-a456-426614174000", "timestamp": iso(now - 60_000), "message": {"role": "assistant", "model": "claude-sonnet-5", "content": [{"type": "text", "text": "Done"}]}}).to_string(),
        ]
        .join("\n"),
    )
    .unwrap();
    let day = &iso(now)[..10];
    let codex_file = root.join(format!("codex/sessions/{}/rollout-sample.jsonl", day.replace('-', "/")));
    std::fs::create_dir_all(codex_file.parent().unwrap()).unwrap();
    std::fs::write(
        &codex_file,
        [
            json!({"type": "session_meta", "payload": {"id": "codex-sample-session", "cwd": workspace}}).to_string(),
            json!({"type": "event_msg", "timestamp": iso(now - 30_000), "payload": {"type": "user_message", "message": "Review this code"}}).to_string(),
        ]
        .join("\n"),
    )
    .unwrap();

    let app = build_app(&root).await;
    app.state.settings.start().await.unwrap();
    let mut client = Client::connect(&app.rpc);
    let mut others = Vec::new();
    let scan = client.call("agentSessions.scan", json!({}), &mut others).await;
    let candidates = scan["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 1, "{scan}");
    assert_eq!(candidates[0]["path"], json!(workspace));
    assert_eq!(candidates[0]["sources"], json!(["claudeAgent", "codex"]));
    assert_eq!(candidates[0]["threadCount"], 2);
    assert_eq!(candidates[0]["alreadyImported"], false);

    client
        .call(
            "orchestration.dispatchCommand",
            json!({"type": "project.create", "commandId": "create-imported-project", "projectId": "project-import", "title": "Workspace", "workspaceRoot": workspace, "createdAt": iso(now)}),
            &mut others,
        )
        .await;
    let result = client
        .call(
            "agentSessions.import",
            json!({"projectId": "project-import", "expectedWorkspaceRoot": workspace}),
            &mut others,
        )
        .await;
    assert_eq!(result, json!({"importedCount": 2, "skippedCount": 0}));
    let claude_thread = app
        .state
        .reads
        .get_thread_shell_by_id(&ThreadId::new("import:claudeAgent:123e4567-e89b-42d3-a456-426614174000"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claude_thread.title, "Fix the project");
    assert!(app
        .state
        .reads
        .get_thread_shell_by_id(&ThreadId::new("import:codex:codex-sample-session"))
        .await
        .unwrap()
        .is_some());
    // A second import finds both sessions already imported.
    let again = client.call("agentSessions.import", json!({"projectId": "project-import"}), &mut others).await;
    assert_eq!(again, json!({"importedCount": 2, "skippedCount": 0}));
    let rescan = client.call("agentSessions.scan", json!({}), &mut others).await;
    assert_eq!(rescan["candidates"][0]["projectId"], "project-import");
    assert_eq!(rescan["candidates"][0]["alreadyImported"], true);

    let missing = client.request("agentSessions.import", json!({"projectId": "project-missing"}));
    let exit = client.frame_of(&missing, &mut others).await;
    assert_eq!(
        exit["exit"]["cause"][0]["error"],
        json!({"_tag": "AgentSessionImportProjectNotFoundError", "projectId": "project-missing"})
    );
}
