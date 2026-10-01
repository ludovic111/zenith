//! The stream-json transport against a fake `claude` (`tests/fixtures/fake-claude.mjs`, run by
//! node): argv/env, `initialize` ordering, control requests both ways, cancellation, exit
//! errors, close escalation, and one adapter turn end to end.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use zc_ports::adapter::ProviderAdapter;
use zc_provider_claude::options::{build_spawn_spec, ClaudeQueryOptions};
use zc_provider_claude::query::{CanUseToolRequest, ClaudeQueryRuntime, MessageReceiver, PermissionResult, QueryCallbacks, QueryError, UserDialogRequest};
use zc_provider_claude::ProcessQuery;

fn find_on_path(name: &str) -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH");
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| panic!("{name} not on PATH"))
}

struct Fake {
    dir: tempfile::TempDir,
    launcher: PathBuf,
    record: PathBuf,
}

impl Fake {
    /// A `#!/bin/sh` launcher (a native-looking binary path) running the node stub on `script`.
    fn new(script: Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let stub = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-claude.mjs");
        let launcher = dir.path().join("claude");
        std::fs::write(
            &launcher,
            format!("#!/bin/sh\nexec \"{}\" \"{}\" \"$@\"\n", find_on_path("node").display(), stub.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(dir.path().join("script.json"), script.to_string()).unwrap();
        let record = dir.path().join("record.jsonl");
        Self { dir, launcher, record }
    }

    fn env(&self) -> zc_provider_claude::home::Env {
        zc_provider_claude::home::Env::from([
            ("PATH".to_string(), std::env::var("PATH").unwrap()),
            ("FAKE_CLAUDE_SCRIPT".to_string(), self.dir.path().join("script.json").display().to_string()),
            ("FAKE_CLAUDE_RECORD".to_string(), self.record.display().to_string()),
            ("FAKE_MARKER".to_string(), "kept".to_string()),
            ("NODE_OPTIONS".to_string(), "--max-old-space-size=64".to_string()),
            ("DEBUG".to_string(), "1".to_string()),
        ])
    }

    fn options(&self) -> ClaudeQueryOptions {
        ClaudeQueryOptions {
            cwd: Some(self.dir.path().display().to_string()),
            path_to_claude_code_executable: self.launcher.display().to_string(),
            permission_mode: Some("default".into()),
            include_partial_messages: true,
            can_use_tool: true,
            env: self.env(),
            ..ClaudeQueryOptions::default()
        }
    }

    fn records(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.record)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn stdin(&self) -> Vec<Value> {
        self.records().into_iter().filter_map(|entry| entry.get("stdin").cloned()).collect()
    }

    fn response_to(&self, request_id: &str) -> Value {
        self.stdin()
            .into_iter()
            .find(|line| line["type"] == "control_response" && line["response"]["request_id"] == request_id)
            .unwrap_or_else(|| panic!("no response to {request_id}: {:#?}", self.records()))["response"]
            .clone()
    }
}

async fn wait_exit(query: &ProcessQuery, within: Duration) {
    let deadline = Instant::now() + within;
    while !query.has_exited() {
        assert!(Instant::now() < deadline, "the fake claude did not exit");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn drain(mut messages: MessageReceiver) -> Vec<Result<Value, QueryError>> {
    let mut out = Vec::new();
    while let Some(item) = tokio::time::timeout(Duration::from_secs(10), messages.recv())
        .await
        .expect("message stream stalled")
    {
        out.push(item);
    }
    out
}

#[derive(Default)]
struct TestCallbacks {
    cancelled: AtomicBool,
    dialogs: Mutex<Vec<UserDialogRequest>>,
    tools: Mutex<Vec<CanUseToolRequest>>,
}

#[async_trait]
impl QueryCallbacks for TestCallbacks {
    async fn can_use_tool(&self, request: CanUseToolRequest, cancel: CancellationToken) -> PermissionResult {
        self.tools.lock().unwrap().push(request.clone());
        if request.tool_name == "Write" {
            cancel.cancelled().await;
            self.cancelled.store(true, Ordering::SeqCst);
            return PermissionResult::Deny {
                message: "User cancelled tool execution.".into(),
            };
        }
        PermissionResult::Allow {
            updated_input: request.input,
            updated_permissions: None,
        }
    }

    async fn on_user_dialog(&self, request: UserDialogRequest, _cancel: CancellationToken) -> Option<Value> {
        self.dialogs.lock().unwrap().push(request);
        Some(json!({"behavior": "completed", "result": "compact"}))
    }
}

fn control_request(id: &str, request: Value) -> Value {
    json!({"step": "send", "send": {"type": "control_request", "request_id": id, "request": request}})
}

/// Steps are written with a redundant `step` tag for readability; the fake ignores it.
fn script(steps: Vec<Value>) -> Value {
    Value::Array(steps)
}

#[tokio::test]
async fn speaks_initialize_first_then_user_messages_and_ends_input_after_the_first_result() {
    let init =
        json!({"commands": [{"name": "review", "description": "Review changes", "argumentHint": ""}], "models": [], "account": {"subscriptionType": "max"}});
    let fake = Fake::new(script(vec![
        json!({"wait": "initialize", "respond": init}),
        json!({"wait": "user"}),
        json!({"raw": "this is not json"}),
        json!({"send": {"type": "keep_alive"}}),
        json!({"send": {"type": "system", "subtype": "init", "session_id": "sdk-session-fake"}}),
        json!({"send": {"type": "result", "subtype": "success", "is_error": false, "result": "OK", "session_id": "sdk-session-fake"}}),
    ]));
    let options = fake.options();
    let (prompt_tx, prompt_rx) = tokio::sync::mpsc::unbounded_channel();
    let callbacks: Arc<dyn QueryCallbacks> = Arc::new(TestCallbacks::default());
    let (query, messages) = ProcessQuery::spawn(&options, Some(prompt_rx), Some(callbacks));
    let user =
        json!({"type": "user", "session_id": "", "parent_tool_use_id": null, "message": {"role": "user", "content": [{"type": "text", "text": "reply OK"}]}});
    prompt_tx.send(user.clone()).unwrap();
    drop(prompt_tx);
    assert_eq!(query.initialization_result().await.unwrap(), init);
    let items = drain(messages).await;
    let kinds: Vec<Value> = items.iter().map(|item| item.as_ref().unwrap()["type"].clone()).collect();
    assert_eq!(kinds, vec![json!("system"), json!("result")]);
    wait_exit(&query, Duration::from_secs(5)).await;

    let records = fake.records();
    let expected = build_spawn_spec(&options);
    assert_eq!(records[0]["argv"], json!(expected.args));
    assert_eq!(
        records[0]["env"],
        json!({"CLAUDE_CODE_ENTRYPOINT": "sdk-ts", "CLAUDE_AGENT_SDK_VERSION": zc_provider_claude::options::CLAUDE_AGENT_SDK_VERSION, "FAKE_MARKER": "kept"})
    );
    assert_eq!(records[0]["cwd"], json!(std::fs::canonicalize(fake.dir.path()).unwrap().display().to_string()));
    let stdin = fake.stdin();
    assert_eq!(stdin[0]["type"], json!("control_request"));
    assert_eq!(stdin[0]["request"], json!({"subtype": "initialize", "systemPrompt": [""]}));
    assert_eq!(stdin[0]["request_id"].as_str().unwrap().len(), 11);
    assert_eq!(stdin[1], user);
    assert_eq!(records.last().unwrap(), &json!({"eof": true}));
}

#[tokio::test]
async fn answers_inbound_control_requests_and_honours_control_cancel_request() {
    let fake = Fake::new(script(vec![
        json!({"wait": "initialize", "respond": {}}),
        control_request(
            "perm-1",
            json!({"subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": "ls"}, "permission_suggestions": [{"type": "setMode", "mode": "default", "destination": "session"}], "tool_use_id": "toolu_1"}),
        ),
        json!({"wait": "response:perm-1"}),
        control_request(
            "perm-2",
            json!({"subtype": "can_use_tool", "tool_name": "Write", "input": {"file_path": "a.txt"}, "tool_use_id": "toolu_2"}),
        ),
        json!({"sleep": 50}),
        json!({"send": {"type": "control_cancel_request", "request_id": "perm-2"}}),
        control_request(
            "dialog-1",
            json!({"subtype": "request_user_dialog", "dialog_kind": "resume_return", "payload": {"sessionAgeMinutes": 90}}),
        ),
        json!({"wait": "response:dialog-1"}),
        control_request("elicit-1", json!({"subtype": "elicitation", "mcp_server_name": "docs", "message": "Sign in?"})),
        json!({"wait": "response:elicit-1"}),
        control_request("hook-1", json!({"subtype": "hook_callback", "callback_id": "cb-9"})),
        json!({"wait": "response:hook-1"}),
        control_request("odd-1", json!({"subtype": "frobnicate"})),
        json!({"wait": "response:odd-1"}),
        json!({"send": {"type": "result", "subtype": "success", "is_error": false, "result": "done", "session_id": "s"}}),
    ]));
    let callbacks = Arc::new(TestCallbacks::default());
    let (query, messages) = ProcessQuery::spawn(&fake.options(), None, Some(callbacks.clone() as Arc<dyn QueryCallbacks>));
    let mut messages = messages;
    let result = tokio::time::timeout(Duration::from_secs(10), messages.recv()).await.unwrap().unwrap().unwrap();
    assert_eq!(result["type"], json!("result"));
    assert!(callbacks.cancelled.load(Ordering::SeqCst));
    query.close().unwrap();
    wait_exit(&query, Duration::from_secs(5)).await;

    assert_eq!(
        fake.response_to("perm-1"),
        json!({"subtype": "success", "request_id": "perm-1", "response": {"behavior": "allow", "updatedInput": {"command": "ls"}, "toolUseID": "toolu_1"}})
    );
    let tools = callbacks.tools.lock().unwrap();
    assert_eq!(
        tools[0].suggestions,
        Some(vec![json!({"type": "setMode", "mode": "default", "destination": "session"})])
    );
    assert_eq!(tools[0].tool_use_id.as_deref(), Some("toolu_1"));
    assert_eq!(callbacks.dialogs.lock().unwrap()[0].dialog_kind, "resume_return");
    assert_eq!(fake.response_to("dialog-1")["response"], json!({"behavior": "completed", "result": "compact"}));
    assert_eq!(fake.response_to("elicit-1")["response"], json!({"action": "decline"}));
    assert_eq!(
        fake.response_to("hook-1"),
        json!({"subtype": "error", "request_id": "hook-1", "error": "No hook callback found for ID: cb-9"})
    );
    assert_eq!(fake.response_to("odd-1")["error"], json!("Unsupported control request subtype: frobnicate"));
}

#[tokio::test]
async fn sends_runtime_controls_and_surfaces_control_errors() {
    let fake = Fake::new(script(vec![
        json!({"wait": "initialize", "respond": {}}),
        json!({"wait": "interrupt", "respond": {}}),
        json!({"wait": "set_model", "respond": {}}),
        json!({"wait": "set_model", "respond": {}}),
        json!({"wait": "set_permission_mode", "error": "Invalid permission mode: nope"}),
        json!({"wait": "get_usage", "respond": {"five_hour": {"utilization": 5}}}),
    ]));
    let (query, _messages) = ProcessQuery::spawn(&fake.options(), None, Some(Arc::new(TestCallbacks::default())));
    query.initialization_result().await.unwrap();
    query.interrupt().await.unwrap();
    query.set_model(Some("claude-x[1m]")).await.unwrap();
    query.set_model(None).await.unwrap();
    assert_eq!(query.set_permission_mode("nope").await.unwrap_err().message, "Invalid permission mode: nope");
    assert_eq!(query.get_usage().await.unwrap(), json!({"five_hour": {"utilization": 5}}));
    query.close().unwrap();
    wait_exit(&query, Duration::from_secs(5)).await;
    let requests: Vec<Value> = fake
        .stdin()
        .into_iter()
        .filter(|line| line["type"] == "control_request")
        .map(|line| line["request"].clone())
        .collect();
    assert_eq!(
        requests,
        vec![
            json!({"subtype": "initialize", "systemPrompt": [""]}),
            json!({"subtype": "interrupt"}),
            json!({"subtype": "set_model", "model": "claude-x[1m]"}),
            json!({"subtype": "set_model"}),
            json!({"subtype": "set_permission_mode", "mode": "nope"}),
            json!({"subtype": "get_usage"}),
        ]
    );
    assert_eq!(query.interrupt().await.unwrap_err().message, "Query closed before response received");
}

#[tokio::test]
async fn reports_exit_codes_with_the_stderr_tail() {
    let fake = Fake::new(script(vec![
        json!({"wait": "initialize", "respond": {}}),
        json!({"stderr": "boom: not logged in\n"}),
        json!({"sleep": 100}),
        json!({"exit": 3}),
    ]));
    let (query, messages) = ProcessQuery::spawn(&fake.options(), None, None);
    let items = drain(messages).await;
    assert_eq!(
        items,
        vec![Err(QueryError::new("Claude Code process exited with code 3. stderr: boom: not logged in"))]
    );
    assert!(query.has_exited());
}

#[tokio::test]
async fn reports_the_last_error_result_instead_of_a_bare_exit_code() {
    let error_result =
        json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "errors": ["Credit balance is too low", " "], "session_id": "s"});
    let fake = Fake::new(script(vec![
        json!({"wait": "initialize", "respond": {}}),
        json!({"send": error_result}),
        json!({"exit": 1}),
    ]));
    let (_query, messages) = ProcessQuery::spawn(&fake.options(), None, None);
    let items = drain(messages).await;
    assert_eq!(
        items,
        vec![
            Ok(error_result),
            Err(QueryError::new("Claude Code returned an error result: Credit balance is too low"))
        ]
    );
}

#[tokio::test]
async fn reports_a_missing_native_binary_like_the_sdk() {
    let options = ClaudeQueryOptions {
        path_to_claude_code_executable: "/nonexistent/zc-test/claude".into(),
        ..ClaudeQueryOptions::default()
    };
    let (query, messages) = ProcessQuery::spawn(&options, None, None);
    let message = "Claude Code native binary not found at /nonexistent/zc-test/claude. Please ensure Claude Code is installed via native installer or specify a valid path with options.pathToClaudeCodeExecutable.";
    assert_eq!(drain(messages).await, vec![Err(QueryError::new(message))]);
    assert_eq!(query.initialization_result().await.unwrap_err().message, message);
}

#[tokio::test]
async fn close_sends_sigterm_after_the_grace_period_then_sigkill() {
    let fake = Fake::new(script(vec![
        json!({"wait": "initialize", "respond": {}}),
        json!({"onSigterm": "exit"}),
        json!({"hang": true}),
    ]));
    let (query, _messages) = ProcessQuery::spawn(&fake.options(), None, None);
    query.initialization_result().await.unwrap();
    let started = Instant::now();
    query.close().unwrap();
    wait_exit(&query, Duration::from_secs(6)).await;
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(1900) && elapsed < Duration::from_millis(4000), "{elapsed:?}");
    assert!(fake.records().contains(&json!({"sigterm": true})));

    let stubborn = Fake::new(script(vec![
        json!({"wait": "initialize", "respond": {}}),
        json!({"onSigterm": "ignore"}),
        json!({"hang": true}),
    ]));
    let (query, _messages) = ProcessQuery::spawn(&stubborn.options(), None, None);
    query.initialization_result().await.unwrap();
    let started = Instant::now();
    query.close().unwrap();
    wait_exit(&query, Duration::from_secs(10)).await;
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(6900) && elapsed < Duration::from_millis(9000), "{elapsed:?}");
    assert!(stubborn.records().contains(&json!({"sigterm": true})));
}

#[tokio::test]
async fn the_adapter_runs_a_turn_against_the_fake_cli() {
    let session_id = "sdk-session-e2e";
    let event = |uuid: &str, event: Value| json!({"send": {"type": "stream_event", "session_id": session_id, "uuid": uuid, "parent_tool_use_id": null, "event": event}});
    let fake = Fake::new(script(vec![
        json!({"wait": "initialize", "respond": {"commands": [], "models": []}}),
        json!({"wait": "user"}),
        json!({"send": {"type": "system", "subtype": "init", "session_id": session_id, "uuid": "init-1", "model": "claude-test", "permissionMode": "default", "cwd": "/tmp", "tools": [], "mcp_servers": [], "slash_commands": [], "skills": [], "plugins": [], "apiKeySource": "none", "claude_code_version": "test", "output_style": "default"}}),
        event("s1", json!({"type": "message_start", "message": {"id": "msg-1"}})),
        event(
            "s2",
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        ),
        event(
            "s3",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "OK"}}),
        ),
        event("s4", json!({"type": "content_block_stop", "index": 0})),
        json!({"send": {"type": "result", "subtype": "success", "is_error": false, "result": "OK", "num_turns": 1, "stop_reason": "end_turn", "session_id": session_id, "uuid": "result-1"}}),
    ]));
    let home = tempfile::tempdir().unwrap();
    let settings = serde_json::from_value(json!({"binaryPath": fake.launcher.display().to_string(), "homePath": home.path().display().to_string()})).unwrap();
    let options = zc_provider_claude::ClaudeAdapterOptions::new(settings, "claudeAgent".into(), fake.env(), home.path().join("attachments"));
    let adapter = zc_provider_claude::ClaudeAdapter::new(options);
    let mut events = adapter.subscribe_events();
    adapter
        .start_session(
            serde_json::from_value(json!({"threadId": "thread-e2e", "provider": "claudeAgent", "runtimeMode": "approval-required", "cwd": fake.dir.path()}))
                .unwrap(),
        )
        .await
        .unwrap();
    let turn = adapter
        .send_turn(serde_json::from_value(json!({"threadId": "thread-e2e", "input": "reply OK", "attachments": []})).unwrap())
        .await
        .unwrap();
    let mut seen = Vec::new();
    use futures::StreamExt;
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(10), events.next()).await {
        let event = serde_json::to_value(&event).unwrap();
        let done = event["type"] == "turn.completed";
        seen.push(event);
        if done {
            break;
        }
    }
    let kinds: Vec<&str> = seen.iter().filter_map(|event| event["type"].as_str()).collect();
    assert!(kinds.contains(&"thread.started") && kinds.contains(&"content.delta"), "{kinds:?}");
    let completed = seen.last().unwrap();
    assert_eq!(completed["type"], json!("turn.completed"));
    assert_eq!(completed["turnId"], json!(turn.turn_id.as_str()));
    assert_eq!(completed["payload"]["state"], json!("completed"));
    adapter.stop_session(&"thread-e2e".into()).await.unwrap();

    let records = fake.records();
    let argv: Vec<String> = serde_json::from_value(records[0]["argv"].clone()).unwrap();
    assert!(argv.windows(2).any(|pair| pair == ["--permission-prompt-tool", "stdio"]), "{argv:?}");
    assert!(argv.windows(2).any(|pair| pair == ["--permission-mode", "default"]), "{argv:?}");
    let stdin = fake.stdin();
    assert_eq!(stdin[0]["request"]["subtype"], json!("initialize"));
    assert!(stdin[0]["request"]["appendSystemPrompt"].as_str().unwrap().contains("zenith code"));
    assert_eq!(stdin[1]["type"], json!("user"));
}
