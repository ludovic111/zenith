//! Golden comparison of `/mcp` with the TypeScript server.
//!
//! Each `golden/<name>.script.json` is played against the Rust router here and was played
//! against the real `McpHttpServer.layer` by `code/apps/server/scripts/mcp-oracle.ts`, whose
//! transcript is `golden/<name>.ts.json`: HTTP status, MCP headers and body of every step,
//! the commands the engine received, the requests the browser host received. Both sides run
//! the same fixtures (thread and project shells, scripted dispatch rejections, a scripted
//! browser host answering per operation).
//!
//! ```sh
//! cd code
//! node apps/server/scripts/mcp-oracle.ts run ../crates/zenith-code/crates/zc-mcp/tests/golden/protocol.script.json \
//!   --out ../crates/zenith-code/crates/zc-mcp/tests/golden/protocol.ts.json
//! ```
//!
//! The TypeScript transcript says "T3 Code" where the built server says the brand; that and
//! session ids, command uuids and broker request ids are normalized before comparing.
//! `ZC_MCP_ORACLE=1` re-runs the oracle first (needs `code/node_modules`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::Request;
use futures::StreamExt;
use http_body_util::BodyExt;
use serde_json::{json, Map, Value};
use tower::ServiceExt;
use zc_mcp::broker::{PreviewAutomationHost, PreviewAutomationResponse};
use zc_mcp::tools::DispatchFailure;
use zc_mcp::{
    McpCapability, McpCredentialRequest, McpHttpOptions, McpServices, McpSessionRegistry, McpSessionRegistryOptions, PreviewAutomationBroker,
    PullRequestBackend, Toolkit,
};

const RECORDED_HEADERS: [&str; 6] = [
    "content-type",
    "mcp-session-id",
    "mcp-protocol-version",
    "www-authenticate",
    "allow",
    "cache-control",
];

struct ScriptedBackend {
    thread_id: String,
    thread: Value,
    project: Value,
    reject_link: Vec<i64>,
    reject_unlink: Vec<i64>,
    commands: Mutex<Vec<Value>>,
}

#[async_trait]
impl PullRequestBackend for ScriptedBackend {
    async fn thread_shell(&self, thread_id: &str) -> Result<Option<Value>, String> {
        Ok((thread_id == self.thread_id && !self.thread.is_null()).then(|| self.thread.clone()))
    }

    async fn project_shell(&self, _project_id: &str) -> Result<Option<Value>, String> {
        Ok((!self.project.is_null()).then(|| self.project.clone()))
    }

    async fn dispatch(&self, command: Value) -> Result<(), DispatchFailure> {
        let number = command["number"].as_i64().unwrap_or(0);
        let rejected = match command["type"].as_str() {
            Some("thread.pull-request.link") => self.reject_link.contains(&number),
            Some("thread.pull-request.unlink") => self.reject_unlink.contains(&number),
            _ => false,
        };
        if rejected {
            return Err(DispatchFailure::Invariant);
        }
        self.commands.lock().unwrap().push(command);
        Ok(())
    }
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))).unwrap()
}

fn capability(name: &str) -> McpCapability {
    zc_mcp::scope::capability_from_str(name).expect("known capability")
}

/// Plays a script against the Rust `/mcp` and returns the transcript.
async fn play(script: &Value) -> Value {
    let thread_id = script["threadId"].as_str().unwrap().to_owned();
    let registry = McpSessionRegistry::new(script["environmentId"].as_str().unwrap(), McpSessionRegistryOptions::default());
    registry.set_listen_address(Some("127.0.0.1"), 0);
    let backend = Arc::new(ScriptedBackend {
        thread_id: thread_id.clone(),
        thread: script["thread"].clone(),
        project: script["project"].clone(),
        reject_link: script["rejectLink"].as_array().unwrap().iter().filter_map(Value::as_i64).collect(),
        reject_unlink: script["rejectUnlink"].as_array().unwrap().iter().filter_map(Value::as_i64).collect(),
        commands: Mutex::new(Vec::new()),
    });
    let broker = PreviewAutomationBroker::new();
    let artifacts = tempfile::tempdir().unwrap();
    let toolkit = Toolkit::new(McpServices {
        broker: broker.clone(),
        pull_requests: backend.clone(),
        attachments_dir: artifacts.path().join("attachments"),
        browser_artifacts_dir: artifacts.path().join("browser-artifacts"),
    });
    let router = zc_mcp::router(
        registry.clone(),
        toolkit,
        McpHttpOptions {
            server_name: zc_mcp::BRAND_NAME.into(),
            server_version: "0.0.43".into(),
            allowed_origins: Vec::new(),
        },
    );

    let host_requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    if let Some(host) = script.get("host").filter(|host| host.is_object()) {
        let client_id = host["clientId"].as_str().unwrap().to_owned();
        let mut events = broker.connect(PreviewAutomationHost {
            client_id: client_id.clone(),
            environment_id: script["environmentId"].as_str().unwrap().to_owned(),
            supported_operations: host["supportedOperations"]
                .as_array()
                .map(|ops| ops.iter().filter_map(Value::as_str).map(str::to_owned).collect()),
        });
        let responses = host["responses"].clone();
        let host_broker = broker.clone();
        let recorded = host_requests.clone();
        tokio::spawn(async move {
            while let Some(event) = events.next().await {
                if event["type"] == "connected" {
                    continue;
                }
                let mut request = event["request"].clone();
                let request_id = request["requestId"].as_str().unwrap().to_owned();
                request["requestId"] = json!("<request>");
                recorded.lock().unwrap().push(request.clone());
                let operation = request["operation"].as_str().unwrap();
                let response = responses
                    .get(operation)
                    .cloned()
                    .unwrap_or_else(|| json!({"ok": false, "error": {"_tag": "PreviewAutomationExecutionError", "message": "unscripted"}}));
                host_broker.respond(PreviewAutomationResponse {
                    client_id: client_id.clone(),
                    connection_id: event["connectionId"].as_str().unwrap().to_owned(),
                    request_id,
                    ok: response["ok"].as_bool().unwrap(),
                    result: response.get("result").cloned(),
                    error: response.get("error").cloned(),
                });
            }
        });
        // The host registers when its stream is first polled.
        while broker.hosts().is_empty() {
            tokio::task::yield_now().await;
        }
    }

    let mut current_credential: Option<String> = None;
    let mut current_header = String::new();
    let mut session: Option<String> = None;
    let mut steps = Vec::new();
    for step in script["steps"].as_array().unwrap() {
        let token = match step["token"].as_str().unwrap() {
            "none" => String::new(),
            "bad" => "Bearer not-a-real-token".to_owned(),
            name => {
                if current_credential.as_deref() != Some(name) {
                    let capabilities = script["credentials"][name]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|c| capability(c.as_str().unwrap()))
                        .collect();
                    let issued = registry.issue_for_thread(McpCredentialRequest {
                        thread_id: thread_id.clone(),
                        provider_instance_id: "codex".into(),
                        capabilities,
                    });
                    current_credential = Some(name.to_owned());
                    current_header = issued.config.authorization_header;
                }
                current_header.clone()
            }
        };
        let mut request = Request::builder().method(step["method"].as_str().unwrap()).uri("/mcp");
        if let Some(headers) = step["headers"].as_object() {
            for (name, value) in headers {
                let value = match value.as_str().unwrap() {
                    "$session" => session.clone().unwrap_or_else(|| "missing-session".into()),
                    other => other.to_owned(),
                };
                request = request.header(name.as_str(), value);
            }
        }
        if !token.is_empty() {
            request = request.header("authorization", token);
        }
        let body = step["body"].as_str().map(|body| Body::from(body.to_owned())).unwrap_or_else(Body::empty);
        let response = router.clone().oneshot(request.body(body).unwrap()).await.unwrap();
        let status = response.status().as_u16();
        let mut headers = Map::new();
        for name in RECORDED_HEADERS {
            if let Some(value) = response.headers().get(name) {
                let value = value.to_str().unwrap().to_owned();
                if name == "mcp-session-id" {
                    session = Some(value);
                    headers.insert(name.into(), json!("<session>"));
                } else {
                    headers.insert(name.into(), json!(value));
                }
            }
        }
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        let mut body: Value = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        };
        if let Some(tools) = body.pointer_mut("/result/tools").and_then(Value::as_array_mut) {
            for tool in tools.iter_mut() {
                *tool = tool["name"].clone();
            }
        }
        steps.push(json!({"name": step["name"], "status": status, "headers": headers, "body": body}));
    }
    let uuid = regex::Regex::new("[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$").unwrap();
    let commands: Vec<Value> = backend
        .commands
        .lock()
        .unwrap()
        .iter()
        .map(|command| {
            let mut command = command.clone();
            command["commandId"] = json!(uuid.replace(command["commandId"].as_str().unwrap(), "<uuid>"));
            command
        })
        .collect();
    let host_requests = host_requests.lock().unwrap().clone();
    json!({"steps": steps, "commands": commands, "hostRequests": host_requests})
}

/// The built server writes the brand where the TypeScript source says "T3 Code".
fn brand(value: &mut Value) {
    match value {
        Value::String(text) => *text = zc_mcp::brand(text),
        Value::Array(items) => items.iter_mut().for_each(brand),
        Value::Object(map) => map.values_mut().for_each(brand),
        _ => {}
    }
}

fn run_oracle(name: &str) {
    let code = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../code");
    let script = golden_dir().join(format!("{name}.script.json"));
    let out = golden_dir().join(format!("{name}.ts.json"));
    let status = std::process::Command::new("node")
        .current_dir(&code)
        .arg("apps/server/scripts/mcp-oracle.ts")
        .arg("run")
        .arg(&script)
        .arg("--out")
        .arg(&out)
        .stderr(std::process::Stdio::null())
        .status()
        .expect("node runs the oracle");
    assert!(status.success(), "the oracle failed");
}

async fn compare(name: &str) {
    if std::env::var("ZC_MCP_ORACLE").as_deref() == Ok("1") {
        run_oracle(name);
    }
    let script = read_json(&golden_dir().join(format!("{name}.script.json")));
    let mut expected = read_json(&golden_dir().join(format!("{name}.ts.json")));
    brand(&mut expected);
    let actual = play(&script).await;
    let mut differences = Vec::new();
    let expected_steps = expected["steps"].as_array().unwrap();
    let actual_steps = actual["steps"].as_array().unwrap();
    assert_eq!(expected_steps.len(), actual_steps.len());
    for (expected, actual) in expected_steps.iter().zip(actual_steps) {
        if expected != actual {
            differences.push(format!(
                "step {}:\n  ts:   {}\n  rust: {}",
                expected["name"],
                serde_json::to_string(expected).unwrap(),
                serde_json::to_string(actual).unwrap()
            ));
        }
    }
    for key in ["commands", "hostRequests"] {
        if expected[key] != actual[key] {
            differences.push(format!(
                "{key}:\n  ts:   {}\n  rust: {}",
                serde_json::to_string(&expected[key]).unwrap(),
                serde_json::to_string(&actual[key]).unwrap()
            ));
        }
    }
    assert!(differences.is_empty(), "{} difference(s):\n{}", differences.len(), differences.join("\n"));
}

#[tokio::test]
async fn protocol_matches_the_typescript_server() {
    compare("protocol").await;
}

#[tokio::test]
async fn preview_tools_match_the_typescript_server() {
    compare("preview").await;
}
