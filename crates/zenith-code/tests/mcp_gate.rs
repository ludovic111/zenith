//! WP-27 gate: the real server (`App::build` + `App::startup` on a temp base dir) serves `/mcp`
//! to an MCP client holding a registry-issued credential, and `list_thread_pull_requests`
//! reads the temp database through the projections after `link_pull_request` went through the
//! orchestration engine.
//!
//! Two clients: a plain JSON-RPC client here (always runs), and the official MCP TypeScript
//! SDK client (`tests/mcp/sdk_client.mjs`) when `node` and `code/node_modules` are there.
//!
//! WP-28 gate: over the real WebSocket, a client plays the desktop (preview tabs and their
//! events, `preview.*`) and the browser host (`previewAutomation.connect` / `respond`) while an
//! agent calls `preview_status` over `/mcp`.

use std::path::{Path, PathBuf};

use clap::Parser;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use zc_mcp::McpCapability;
use zc_ports::OrchestrationDispatch;
use zenith_code::app::App;
use zenith_code::cli::{resolve_serve_config, ServeArgs};

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    serve: ServeArgs,
}

struct Server {
    app: App,
    port: u16,
    stop: CancellationToken,
    _dir: tempfile::TempDir,
}

const THREAD_ID: &str = "thread-gate";

async fn start() -> Server {
    std::env::set_var("ZENITH_CODE_SKIP_AUTO_PULL", "1");
    std::env::set_var("ZENITH_NO_STARTUP_TOKEN", "1");
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("home");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let cli = Cli::parse_from([
        "serve",
        workspace.to_str().unwrap(),
        "--base-dir",
        base.to_str().unwrap(),
        "--host",
        "127.0.0.1",
        "--port",
        "1",
    ]);
    let config = resolve_serve_config(&cli.serve, None).await.unwrap();
    let app = App::build(config).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = CancellationToken::new();
    tokio::spawn(zenith_code::server::serve(listener, app.router.clone(), app.rpc.clone(), {
        let stop = stop.clone();
        async move { stop.cancelled().await }
    }));
    app.startup(port).await.unwrap();

    let engine: &dyn OrchestrationDispatch = &app.state.engine;
    let selection = json!({"instanceId": "codex", "model": "gpt-5-codex"});
    for command in [
        json!({"type": "project.create", "commandId": "cmd-project", "projectId": "project-gate", "title": "Gate",
               "workspaceRoot": workspace.to_string_lossy(), "defaultModelSelection": selection, "createdAt": "2026-10-01T00:00:00.000Z"}),
        json!({"type": "thread.create", "commandId": "cmd-thread", "threadId": THREAD_ID, "projectId": "project-gate",
               "title": "Gate thread", "modelSelection": selection, "interactionMode": "default", "runtimeMode": "full-access",
               "branch": null, "worktreePath": null, "createdAt": "2026-10-01T00:00:00.000Z"}),
    ] {
        engine.dispatch(serde_json::from_value(command).unwrap(), None).await.unwrap();
    }
    Server { app, port, stop, _dir: dir }
}

impl Server {
    /// What the provider service does when a session starts.
    fn credential(&self, capabilities: &[McpCapability]) -> zc_mcp::McpProviderSessionConfig {
        self.app.state.mcp_sessions.prepare_session(THREAD_ID, "codex", capabilities)
    }

    async fn stop(self) {
        self.app.shutdown().await;
        self.stop.cancel();
    }
}

async fn rpc(client: &reqwest::Client, url: &str, authorization: &str, session: Option<&str>, body: Value) -> (u16, reqwest::header::HeaderMap, Value) {
    let mut request = client
        .post(url)
        .header("authorization", authorization)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .json(&body);
    if let Some(session) = session {
        request = request.header("mcp-session-id", session).header("mcp-protocol-version", "2025-06-18");
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let text = response.text().await.unwrap();
    (
        status,
        headers,
        if text.is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap() },
    )
}

fn structured(result: &Value) -> &Value {
    assert_eq!(result["result"]["isError"], false, "{result}");
    &result["result"]["structuredContent"]
}

#[tokio::test]
async fn an_mcp_client_with_a_registry_credential_lists_and_links_thread_pull_requests() {
    let server = start().await;
    let config = server.credential(&[McpCapability::PullRequests]);
    // The endpoint the drivers hand to the agents is this server's.
    assert_eq!(config.endpoint, format!("http://127.0.0.1:{}/mcp", server.port));
    let client = reqwest::Client::new();
    let url = config.endpoint.clone();

    // No or a foreign credential: 401.
    let (status, headers, body) = rpc(&client, &url, "Bearer nope", None, json!({"jsonrpc": "2.0", "id": 0, "method": "ping"})).await;
    assert_eq!(status, 401);
    assert_eq!(headers["www-authenticate"], "Bearer");
    assert_eq!(body["error"], "invalid_mcp_credential");

    let auth = config.authorization_header.as_str();
    let (status, headers, body) = rpc(
        &client,
        &url,
        auth,
        None,
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "gate", "version": "1"}}}),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        body["result"]["serverInfo"],
        json!({"name": "zenith", "version": zenith_code::app::SERVER_VERSION})
    );
    let session = headers["mcp-session-id"].to_str().unwrap().to_owned();
    let (status, _, _) = rpc(
        &client,
        &url,
        auth,
        Some(&session),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    assert_eq!(status, 202);

    let (_, _, tools) = rpc(&client, &url, auth, Some(&session), json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).await;
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert_eq!(names.len(), 21);
    assert!(names.contains(&"list_thread_pull_requests"));

    let call =
        |id: i64, name: &str, arguments: Value| json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": arguments}});
    let (_, _, empty) = rpc(&client, &url, auth, Some(&session), call(3, "list_thread_pull_requests", json!({}))).await;
    assert_eq!(structured(&empty), &json!({"pullRequests": [], "chains": []}));

    let (_, _, linked) = rpc(
        &client,
        &url,
        auth,
        Some(&session),
        call(4, "link_pull_request", json!({"url": "https://github.com/Acme/Widgets/pull/42"})),
    )
    .await;
    assert_eq!(
        structured(&linked),
        &json!({"host": "github.com", "repository": "acme/widgets", "number": 42, "url": "https://github.com/Acme/Widgets/pull/42", "alreadyLinked": false})
    );
    let (_, _, again) = rpc(
        &client,
        &url,
        auth,
        Some(&session),
        call(
            5,
            "link_pull_request",
            json!({"repository": "acme/widgets", "number": 42, "host": "github.com"}),
        ),
    )
    .await;
    assert_eq!(structured(&again)["alreadyLinked"], true);

    let (_, _, listed) = rpc(&client, &url, auth, Some(&session), call(6, "list_thread_pull_requests", json!({}))).await;
    let listed = structured(&listed);
    assert_eq!(listed["pullRequests"].as_array().unwrap().len(), 1);
    let entry = &listed["pullRequests"][0];
    assert_eq!(
        (entry["host"].as_str(), entry["repository"].as_str(), entry["number"].as_i64()),
        (Some("github.com"), Some("acme/widgets"), Some(42))
    );
    assert_eq!(entry["source"], "agent");
    assert_eq!(entry["state"], Value::Null);
    assert_eq!(listed["chains"], json!([{"kind": "derived", "numbers": [42]}]));

    // The projection the web app reads has the link too.
    let shell = server
        .app
        .state
        .reads
        .get_thread_shell_by_id(&zc_ports::contracts::ThreadId::new(THREAD_ID))
        .await
        .unwrap()
        .unwrap();
    let shell = serde_json::to_value(shell).unwrap();
    assert_eq!(shell["pullRequests"][0]["number"], 42);

    let (_, _, unlinked) = rpc(
        &client,
        &url,
        auth,
        Some(&session),
        call(7, "unlink_pull_request", json!({"url": "https://github.com/acme/widgets/pull/42"})),
    )
    .await;
    assert_eq!(structured(&unlinked)["wasLinked"], true);

    // Revocation (a new session for the thread, or the session's end) closes the door.
    server.app.state.mcp_sessions.revoke_thread(THREAD_ID);
    let (status, _, _) = rpc(&client, &url, auth, Some(&session), json!({"jsonrpc": "2.0", "id": 8, "method": "ping"})).await;
    assert_eq!(status, 401);
    server.stop().await;
}

/// `code/node_modules`' copy of `@modelcontextprotocol/sdk`, when installed.
fn mcp_sdk_dir() -> Option<PathBuf> {
    let pnpm = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../code/node_modules/.pnpm");
    std::fs::read_dir(&pnpm)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("@modelcontextprotocol+sdk@"))
        .map(|entry| entry.path().join("node_modules/@modelcontextprotocol/sdk"))
        .find(|path| path.join("dist/esm/client/index.js").exists())
}

#[tokio::test]
async fn the_official_mcp_typescript_sdk_client_talks_to_the_rust_server() {
    let Some(sdk) = mcp_sdk_dir() else {
        eprintln!("skipped: no @modelcontextprotocol/sdk under code/node_modules");
        return;
    };
    let server = start().await;
    let config = server.credential(&[McpCapability::PullRequests]);
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/mcp/sdk_client.mjs");
    let output = tokio::process::Command::new("node")
        .arg(&script)
        .env("MCP_SDK_DIR", &sdk)
        .env("MCP_URL", &config.endpoint)
        .env("MCP_AUTHORIZATION", &config.authorization_header)
        .output()
        .await
        .expect("node runs");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["server"], json!({"name": "zenith", "version": zenith_code::app::SERVER_VERSION}));
    assert_eq!(report["capabilities"]["tools"], json!({"listChanged": true}));
    assert!(report["sessionId"].is_string());
    assert_eq!(report["tools"].as_array().unwrap().len(), 21);
    assert!(report["linkDescription"].as_str().unwrap().contains("so zenith tracks it"));
    assert_eq!(report["link"]["structuredContent"]["alreadyLinked"], false);
    let listed = &report["list"]["structuredContent"]["pullRequests"];
    assert_eq!(listed[0]["number"], 42);
    assert_eq!(listed[0]["source"], "agent");
    // A pull-requests-only credential: the preview tools refuse, with the next step.
    assert_eq!(report["preview"]["isError"], true);
    assert!(report["preview"]["content"][0]["text"].as_str().unwrap().contains("Agent browser access"));
    // Bad parameters are a JSON-RPC InvalidParams error, which the SDK throws.
    assert_eq!(report["invalid"]["code"], -32602);
    assert!(report["invalid"]["message"].as_str().unwrap().contains("greater than or equal to 1"));
    // DELETE ended the session.
    assert_eq!(report["terminated"], Value::Null);
    server.stop().await;
}

/// A paired WebSocket client speaking Effect RPC frames.
struct Socket {
    stream: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
}

impl Socket {
    async fn open(server: &Server) -> Self {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let pairing = server.app.state.auth.issue_startup_pairing_credential().await.unwrap();
        let client = reqwest::Client::new();
        let response = client
            .post(format!("http://127.0.0.1:{}/api/auth/browser-session", server.port))
            .json(&json!({"credential": pairing.credential}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let cookie = response.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_owned();
        let mut request = format!("ws://127.0.0.1:{}/ws", server.port).into_client_request().unwrap();
        request.headers_mut().insert("cookie", cookie.parse().unwrap());
        let (stream, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        Self { stream }
    }

    async fn send(&mut self, frame: Value) {
        use futures::SinkExt;
        self.stream
            .send(tokio_tungstenite::tungstenite::Message::Text(frame.to_string().into()))
            .await
            .unwrap();
    }

    async fn request(&mut self, id: i64, tag: &str, payload: Value) {
        self.send(json!({"_tag": "Request", "id": id, "tag": tag, "payload": payload, "headers": []}))
            .await;
    }

    /// The next frame for request `id` (every chunk is acked); other frames wait in `pending`.
    async fn next_for(&mut self, id: i64, pending: &mut Vec<Value>) -> Value {
        use futures::StreamExt;
        if let Some(index) = pending.iter().position(|frame| frame["requestId"] == id) {
            return pending.remove(index);
        }
        loop {
            let message = tokio::time::timeout(std::time::Duration::from_secs(10), self.stream.next())
                .await
                .expect("a frame")
                .unwrap()
                .unwrap();
            let tokio_tungstenite::tungstenite::Message::Text(text) = message else {
                continue;
            };
            let frame: Value = serde_json::from_str(&text).unwrap();
            if frame["_tag"] == "Chunk" {
                self.send(json!({"_tag": "Ack", "requestId": frame["requestId"]})).await;
            }
            if frame["requestId"] == id {
                return frame;
            }
            pending.push(frame);
        }
    }
}

#[tokio::test]
async fn preview_tabs_and_automation_round_trip_over_the_websocket() {
    let server = start().await;
    let mut socket = Socket::open(&server).await;
    let mut pending = Vec::new();

    // The desktop's preview panel: events, open, status report, list.
    socket.request(1, "subscribePreviewEvents", json!({})).await;
    socket.request(2, "preview.open", json!({"threadId": THREAD_ID, "url": "localhost:5173"})).await;
    let opened = socket.next_for(2, &mut pending).await;
    assert_eq!(opened["exit"]["_tag"], "Success", "{opened}");
    let snapshot = opened["exit"]["value"].clone();
    assert_eq!(snapshot["navStatus"], json!({"_tag": "Loading", "url": "http://localhost:5173/", "title": ""}));
    let event = socket.next_for(1, &mut pending).await;
    assert_eq!(event["values"][0]["type"], "opened");
    assert_eq!(event["values"][0]["snapshot"], snapshot);
    let tab_id = snapshot["tabId"].as_str().unwrap().to_owned();

    socket
        .request(
            3,
            "preview.reportStatus",
            json!({"threadId": THREAD_ID, "tabId": tab_id, "navStatus": {"_tag": "Success", "url": "http://localhost:5173/", "title": "Dev"}, "canGoBack": false, "canGoForward": false}),
        )
        .await;
    assert_eq!(socket.next_for(3, &mut pending).await["exit"], json!({"_tag": "Success", "value": null}));
    let navigated = socket.next_for(1, &mut pending).await;
    assert_eq!(navigated["values"][0]["type"], "navigated");
    socket.request(4, "preview.list", json!({"threadId": THREAD_ID})).await;
    let listed = socket.next_for(4, &mut pending).await;
    assert_eq!(listed["exit"]["value"]["sessions"][0]["navStatus"]["title"], "Dev");
    assert_eq!(listed["exit"]["value"]["revision"], navigated["values"][0]["revision"]);
    socket
        .request(5, "preview.navigate", json!({"threadId": THREAD_ID, "tabId": "tab_missing", "url": "x.test"}))
        .await;
    let missing = socket.next_for(5, &mut pending).await;
    assert_eq!(missing["exit"]["cause"][0]["error"]["_tag"], "PreviewSessionLookupError", "{missing}");

    // The desktop's browser host.
    let environment_id = server.app.state.environment_id.clone();
    socket
        .request(
            6,
            "previewAutomation.connect",
            json!({"clientId": "desktop-gate", "environmentId": environment_id}),
        )
        .await;
    let connected = socket.next_for(6, &mut pending).await;
    assert_eq!(connected["values"][0]["type"], "connected", "{connected}");
    let connection_id = connected["values"][0]["connectionId"].as_str().unwrap().to_owned();
    socket
        .request(
            7,
            "previewAutomation.focusHost",
            json!({"clientId": "desktop-gate", "environmentId": environment_id, "connectionId": connection_id, "focused": true,
                   "liveTabs": [{"threadId": THREAD_ID, "tabId": tab_id, "visible": true}]}),
        )
        .await;
    assert_eq!(socket.next_for(7, &mut pending).await["exit"]["_tag"], "Success");

    // An agent with browser access asks for the page.
    let config = server.credential(&[McpCapability::PullRequests, McpCapability::Preview]);
    let agent = tokio::spawn(async move {
        let client = reqwest::Client::new();
        let initialize = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "agent", "version": "1"}}});
        let (_, headers, _) = rpc(&client, &config.endpoint, &config.authorization_header, None, initialize).await;
        let session = headers["mcp-session-id"].to_str().unwrap().to_owned();
        let call = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "preview_status", "arguments": {}}});
        rpc(&client, &config.endpoint, &config.authorization_header, Some(&session), call).await.2
    });
    let request = socket.next_for(6, &mut pending).await;
    let routed = request["values"][0].clone();
    assert_eq!(routed["type"], "request");
    assert_eq!(routed["request"]["operation"], "status");
    assert_eq!(routed["request"]["threadId"], THREAD_ID);
    socket
        .request(
            8,
            "previewAutomation.respond",
            json!({"clientId": "desktop-gate", "connectionId": connection_id, "requestId": routed["request"]["requestId"], "ok": true,
                   "result": {"available": true, "visible": true, "tabId": tab_id, "url": "http://localhost:5173/", "title": "Dev", "loading": false}}),
        )
        .await;
    assert_eq!(socket.next_for(8, &mut pending).await["exit"]["_tag"], "Success");
    let answer = agent.await.unwrap();
    assert_eq!(structured(&answer)["tabId"], tab_id.as_str());
    assert_eq!(structured(&answer)["title"], "Dev");

    socket.request(9, "preview.close", json!({"threadId": THREAD_ID})).await;
    assert_eq!(socket.next_for(9, &mut pending).await["exit"]["_tag"], "Success");
    assert_eq!(socket.next_for(1, &mut pending).await["values"][0]["type"], "closed");
    server.stop().await;
}
