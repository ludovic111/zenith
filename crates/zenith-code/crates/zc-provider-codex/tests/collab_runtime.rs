//! Gate 1 (runtime level), ported from `CodexCollabRuntime.integration.test.ts`: the REAL session
//! runtime against the repo's scripted mock app-server (`testFixtures/codexCollabMockPeer.mjs`),
//! which answers the handshake with responses captured from codex-cli 0.145
//! (`codexMultiAgentWire.json`) and replays a scripted notification sequence on `turn/start`.
//!
//! Needs `node` on PATH (the peer is a plain Node script); skipped otherwise.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedReceiver;
use zc_contracts::{ProviderApprovalDecision, ProviderEvent, RuntimeMode, ServerProviderModel, ThreadId};
use zc_provider_codex::session_runtime::{CodexRuntime, CodexSessionRuntime, CodexSessionRuntimeOptions, SendTurnInput};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../code/apps/server/src/provider/testFixtures")
}

fn wire() -> Value {
    serde_json::from_str(&std::fs::read_to_string(fixtures_dir().join("codexMultiAgentWire.json")).unwrap()).unwrap()
}

fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

struct Script {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl Script {
    fn new(script: Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("collab-script.json");
        std::fs::write(&path, script.to_string()).unwrap();
        Self { _dir: dir, path }
    }

    fn sidecar(&self, suffix: &str) -> Vec<Value> {
        let path = PathBuf::from(format!("{}.{suffix}", self.path.display()));
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

fn root() -> String {
    wire()["rootThreadId"].as_str().unwrap().to_owned()
}

fn children() -> (String, String) {
    let wire = wire();
    (
        wire["childThreadIds"][0].as_str().unwrap().to_owned(),
        wire["childThreadIds"][1].as_str().unwrap().to_owned(),
    )
}

fn runtime(thread: &str, script: &Script, mode: RuntimeMode) -> (Arc<CodexSessionRuntime>, UnboundedReceiver<ProviderEvent>) {
    let mut environment: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    environment.insert("T3_CODEX_COLLAB_SCRIPT".into(), script.path.to_string_lossy().into_owned());
    let mut options = CodexSessionRuntimeOptions::new(
        ThreadId::new(thread),
        fixtures_dir().join("codexCollabMockPeer.sh").to_string_lossy().into_owned(),
        std::env::temp_dir().to_string_lossy().into_owned(),
        mode,
    );
    options.environment = Some(environment);
    let runtime = CodexSessionRuntime::spawn(options).unwrap();
    let events = runtime.take_events().unwrap();
    (runtime, events)
}

/// Events until (and including) the first matching one.
async fn until(events: &mut UnboundedReceiver<ProviderEvent>, mut done: impl FnMut(&ProviderEvent) -> bool) -> Vec<ProviderEvent> {
    let mut seen = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(15), events.recv())
            .await
            .expect("an event in time")
            .expect("the stream stays open");
        let stop = done(&event);
        seen.push(event);
        if stop {
            return seen;
        }
    }
}

fn payload<'a>(event: &'a ProviderEvent, key: &str) -> &'a Value {
    event.payload.as_ref().and_then(|payload| payload.get(key)).unwrap_or(&Value::Null)
}

fn notification(method: &str, predicate: impl Fn(&Value) -> bool) -> Value {
    wire()["notifications"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["method"] == method && predicate(&entry["params"]))
        .cloned()
        .unwrap()
}

fn started_activity(child: &str) -> Value {
    let mut entry = notification("item/completed", |params| {
        params["item"]["type"] == "subAgentActivity" && params["item"]["kind"] == "started"
    });
    entry["params"]["item"]["agentThreadId"] = json!(child);
    entry["params"]["item"]["agentPath"] = json!("/root/model-check");
    entry
}

fn spawned_thread(child: &str) -> Value {
    let mut entry = notification("thread/started", |_| true);
    let thread = &mut entry["params"]["thread"];
    thread["id"] = json!(child);
    thread["sessionId"] = json!(child);
    thread["parentThreadId"] = json!(root());
    thread["agentNickname"] = json!("model-check");
    thread["agentRole"] = json!("verifier");
    thread["source"] = json!({"subAgent": {"thread_spawn": {"agent_nickname": "model-check", "agent_path": "/root/model-check", "agent_role": "verifier", "depth": 1, "parent_thread_id": root()}}});
    entry
}

fn child_settings(thread: &str, model: &str, effort: &str) -> Value {
    json!({"method": "thread/settings/updated", "params": {"threadId": thread, "threadSettings": {
        "approvalPolicy": "on-request", "approvalsReviewer": "auto_review",
        "collaborationMode": {"mode": "default", "settings": {"model": model}},
        "cwd": "/workspace/repo", "effort": effort, "model": model, "modelProvider": "openai",
        "sandboxPolicy": {"type": "dangerFullAccess"}
    }}})
}

macro_rules! require_node {
    () => {
        if !node_available() {
            eprintln!("node is not available: skipping");
            return;
        }
    };
}

#[tokio::test(flavor = "multi_thread")]
async fn looks_up_child_model_metadata_once_after_activity_registration() {
    require_node!();
    let (child_a, child_b) = children();
    let mut interacted = started_activity(&child_b);
    interacted["params"]["item"]["kind"] = json!("interacted");
    let script = Script::new(json!({
        "rootThreadId": root(),
        "recordRequests": true,
        "notifications": [started_activity(&child_a), started_activity(&child_a), interacted, {"method": "thread/closed", "params": {"threadId": child_b}}, spawned_thread(&root())],
        "childResumeSnapshots": {child_a.clone(): {"model": "gpt-5.6-luna", "reasoningEffort": "low"}},
    }));
    let (runtime, mut events) = runtime("thread-collab-model-activity", &script, RuntimeMode::FullAccess);
    let session = runtime.start().await.unwrap();
    assert_eq!(session.model.as_deref(), Some("gpt-5.6-sol"));
    runtime
        .send_turn(SendTurnInput {
            input: Some("start one child".into()),
            ..SendTurnInput::default()
        })
        .await
        .unwrap();
    let seen = until(&mut events, |event| {
        event.method == "collabAgent/metadataUpdated" && payload(event, "agentThreadId") == child_a.as_str()
    })
    .await;
    let metadata = seen.last().unwrap();
    assert_eq!(payload(metadata, "model"), "gpt-5.6-luna");
    assert_eq!(payload(metadata, "effort"), "low");
    assert_eq!(
        script.sidecar("requests"),
        vec![json!({"method": "thread/resume", "params": {"threadId": child_a, "excludeTurns": true}})]
    );
    runtime.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn keeps_child_settings_and_reroutes_newer_than_the_resume_snapshot() {
    require_node!();
    let (child_a, _) = children();
    let status_changed = notification("thread/status/changed", |params| params["threadId"] == child_a.as_str());
    let script = Script::new(json!({
        "rootThreadId": root(),
        "recordRequests": true,
        "notifications": [
            child_settings(&child_a, "child-before", "medium"),
            spawned_thread(&child_a),
            child_settings(&child_a, "child-after", "high"),
            {"method": "model/rerouted", "params": {"threadId": child_a, "turnId": format!("{child_a}-turn"), "fromModel": "child-after", "toModel": "child-rerouted", "reason": "highRiskCyberActivity"}},
            {"method": "model/rerouted", "params": {"threadId": root(), "turnId": format!("{}-turn", root()), "fromModel": "gpt-5.6-sol", "toModel": "root-rerouted", "reason": "highRiskCyberActivity"}},
        ],
        "childResumeSnapshots": {child_a.clone(): {"model": "stale-snapshot", "reasoningEffort": "low", "notifications": [status_changed]}},
    }));
    let (runtime, mut events) = runtime("thread-collab-model-spawn", &script, RuntimeMode::FullAccess);
    runtime.start().await.unwrap();
    runtime
        .send_turn(SendTurnInput {
            input: Some("start one spawned child".into()),
            ..SendTurnInput::default()
        })
        .await
        .unwrap();
    let seen = until(&mut events, |event| {
        event.method == "collabAgent/statusChanged" && payload(event, "agentThreadId") == child_a.as_str()
    })
    .await;
    let started = seen.iter().find(|event| event.method == "collabAgent/started").unwrap();
    assert_eq!(payload(started, "model"), "child-before");
    assert_eq!(payload(started, "effort"), "medium");
    let status = seen.last().unwrap();
    assert_eq!(payload(status, "model"), "child-rerouted");
    assert_eq!(payload(status, "effort"), "high");
    assert!(
        seen.iter()
            .any(|event| event.method == "model/rerouted" && payload(event, "threadId") == root().as_str()),
        "the root reroute stays on the parent path"
    );
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event.method.as_str(), "thread/settings/updated" | "model/rerouted") && payload(event, "threadId") == child_a.as_str()),
        "child metadata notifications must not leak to the parent path"
    );
    assert_eq!(script.sidecar("requests").len(), 1);
    runtime.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn does_not_delay_the_parent_turn_when_the_child_lookup_fails() {
    require_node!();
    let (child_a, _) = children();
    for (name, snapshot) in [("hang", json!({"hang": true})), ("error", json!({"error": "child unavailable"}))] {
        let marker = format!("lookup-{name}");
        let script = Script::new(json!({
            "rootThreadId": root(),
            "recordRequests": true,
            "resumeRequestMarker": marker,
            "notifications": [started_activity(&child_a)],
            "childResumeSnapshots": {child_a.clone(): snapshot},
        }));
        let (runtime, mut events) = runtime(&format!("thread-collab-model-{name}"), &script, RuntimeMode::FullAccess);
        runtime.start().await.unwrap();
        runtime
            .send_turn(SendTurnInput {
                input: Some("finish without child metadata".into()),
                ..SendTurnInput::default()
            })
            .await
            .unwrap();
        let seen = until(&mut events, |event| {
            event.method == "serverRequest/resolved" && payload(event, "requestId") == marker.as_str()
        })
        .await;
        assert!(seen.iter().any(|event| event.method == "turn/completed"));
        assert_eq!(script.sidecar("requests").len(), 1);
        runtime.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn replays_the_captured_fan_out_into_agent_events_without_child_leaks() {
    require_node!();
    let (child_a, child_b) = children();
    let root = root();
    let mut notifications: Vec<Value> = wire()["notifications"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["method"] != "turn/completed")
        .cloned()
        .collect();
    notifications.extend([
        json!({"method": "item/completed", "params": {"threadId": root, "item": {"type": "collabAgentToolCall", "id": "call_fixture_wait", "tool": "wait", "status": "completed", "senderThreadId": root, "receiverThreadIds": [child_a, child_b]}}}),
        json!({"method": "turn/completed", "params": {"threadId": child_a, "turn": {"id": format!("{child_a}-turn-1"), "status": "completed", "items": []}}}),
        json!({"method": "thread/closed", "params": {"threadId": child_b}}),
        json!({"method": "serverRequest/resolved", "params": {"threadId": child_a, "requestId": "req-1"}}),
    ]);
    let script = Script::new(json!({"rootThreadId": root, "notifications": notifications}));
    let (runtime, mut events) = runtime("thread-collab-integration", &script, RuntimeMode::FullAccess);
    runtime.start().await.unwrap();
    runtime
        .send_turn(SendTurnInput {
            input: Some("fan out".into()),
            ..SendTurnInput::default()
        })
        .await
        .unwrap();
    let seen = until(&mut events, |event| event.method == "turn/completed").await;
    let methods: Vec<&str> = seen.iter().map(|event| event.method.as_str()).collect();
    for expected in [
        "collabAgent/activity",
        "collabAgent/turnCompleted",
        "collabAgent/closed",
        "serverRequest/resolved",
        "turn/completed",
    ] {
        assert!(methods.contains(&expected), "{expected} missing from {methods:?}");
    }
    assert!(seen
        .iter()
        .any(|event| event.method == "collabAgent/turnCompleted" && payload(event, "agentThreadId") == child_a.as_str()));
    assert!(seen
        .iter()
        .any(|event| event.method == "collabAgent/closed" && payload(event, "agentThreadId") == child_b.as_str()));
    let leaked: Vec<&str> = seen
        .iter()
        .filter(|event| {
            let thread = payload(event, "threadId");
            (thread == child_a.as_str() || thread == child_b.as_str()) && event.method.starts_with("thread/")
        })
        .map(|event| event.method.as_str())
        .collect();
    assert!(leaked.is_empty(), "child thread/* lifecycle leaked: {leaked:?}");
    runtime.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_interrupts_every_live_child_regardless_of_registration_timing() {
    require_node!();
    let (child_a, child_b) = children();
    let root = root();
    let turn_started = |child: &str| notification("turn/started", |params| params["threadId"] == child);
    let registration = |child: &str| {
        notification("item/completed", |params| {
            params["item"]["type"] == "subAgentActivity" && params["item"]["agentThreadId"] == child
        })
    };
    let mut memory_thread = notification("thread/started", |_| true);
    memory_thread["params"]["thread"]["id"] = json!("memory-consolidation-thread");
    memory_thread["params"]["thread"]["sessionId"] = json!("memory-consolidation-thread");
    memory_thread["params"]["thread"]["source"] = json!("unknown");
    memory_thread["params"]["thread"]["threadSource"] = json!("memory_consolidation");
    let mut memory_turn = turn_started(&child_a);
    memory_turn["params"]["threadId"] = json!("memory-consolidation-thread");
    memory_turn["params"]["turn"]["id"] = json!("memory-consolidation-turn");
    let script = Script::new(json!({
        "rootThreadId": root,
        "holdTurnOpen": true,
        "hangInterruptFor": child_a,
        "notifications": [turn_started(&child_a), registration(&child_a), memory_thread, memory_turn, registration(&child_b), turn_started(&child_b)],
    }));
    let (runtime, mut events) = runtime("thread-collab-stop", &script, RuntimeMode::FullAccess);
    runtime.start().await.unwrap();
    runtime
        .send_turn(SendTurnInput {
            input: Some("fan out and hang".into()),
            ..SendTurnInput::default()
        })
        .await
        .unwrap();
    until(&mut events, |event| {
        event.method == "collabAgent/turnStarted" && payload(event, "agentThreadId") == child_b.as_str()
    })
    .await;
    runtime.interrupt_turn(None).await.unwrap();
    let interrupted: BTreeSet<String> = script
        .sidecar("interrupts")
        .iter()
        .filter_map(|entry| entry["threadId"].as_str().map(str::to_owned))
        .collect();
    for expected in [&child_a, &child_b, &"memory-consolidation-thread".to_owned(), &root] {
        assert!(interrupted.contains(expected), "{expected} was not interrupted: {interrupted:?}");
    }
    runtime.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_answers_a_parked_permission_approval_with_a_withheld_grant() {
    require_node!();
    let script = Script::new(json!({
        "rootThreadId": root(),
        "holdTurnOpen": true,
        "notifications": [],
        "serverRequests": [{"method": "item/permissions/requestApproval", "label": "perm-1", "params": {
            "cwd": "/tmp/project", "itemId": "app_1", "permissions": {"network": {"enabled": true}},
            "reason": "Fetch data from api.example.com", "startedAtMs": 1_778_000_000_000_i64, "threadId": "${threadId}", "turnId": "${turnId}"
        }}],
    }));
    let (runtime, mut events) = runtime("thread-codex-permission-stop", &script, RuntimeMode::FullAccess);
    runtime.start().await.unwrap();
    runtime
        .send_turn(SendTurnInput {
            input: Some("use the connected app".into()),
            ..SendTurnInput::default()
        })
        .await
        .unwrap();
    until(&mut events, |event| event.method == "item/permissions/requestApproval").await;
    runtime.interrupt_turn(None).await.unwrap();
    let seen = until(&mut events, |event| {
        event.method == "serverRequest/resolved" && event.request_kind.is_some_and(|kind| kind.as_str() == "permission")
    })
    .await;
    assert!(
        seen.last().unwrap().request_id.is_some(),
        "the receipt correlates back to the canonical request"
    );
    let recorded = script.sidecar("approvalResponses");
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0]["label"], "perm-1");
    assert_eq!(recorded[0]["result"], json!({"permissions": {}}));
    runtime.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_targets_the_active_turn_when_a_follow_up_is_queued() {
    require_node!();
    let active = "019fe3e8-f908-7f31-8d51-283f4a47897a";
    let queued = "019fe3eb-8faf-7de3-a85b-ac64c7f9c8c3";
    let script = Script::new(json!({
        "rootThreadId": root(), "holdTurnOpen": true, "onlyFirstTurnStarts": true,
        "turnIds": [active, queued], "expectedActiveTurnId": active, "notifications": [],
    }));
    let (runtime, _events) = runtime("thread-codex-queued-stop", &script, RuntimeMode::FullAccess);
    runtime.start().await.unwrap();
    runtime
        .send_turn(SendTurnInput {
            input: Some("keep working".into()),
            ..SendTurnInput::default()
        })
        .await
        .unwrap();
    runtime
        .send_turn(SendTurnInput {
            input: Some("queued follow-up".into()),
            ..SendTurnInput::default()
        })
        .await
        .unwrap();
    runtime.interrupt_turn(None).await.unwrap();
    let interrupts = script.sidecar("interrupts");
    assert_eq!(interrupts.last().unwrap(), &json!({"threadId": root(), "turnId": active}));
    runtime.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn returns_each_mcp_elicitation_response_to_codex() {
    require_node!();
    let cases = [
        (ProviderApprovalDecision::Accept, json!({"action": "accept", "content": {"approval": "once"}})),
        (
            ProviderApprovalDecision::AcceptForSession,
            json!({"action": "accept", "_meta": {"persist": "session"}, "content": {"approval": "session"}}),
        ),
        (
            ProviderApprovalDecision::AcceptAlways,
            json!({"action": "accept", "_meta": {"persist": "always"}, "content": {"approval": "always"}}),
        ),
        (ProviderApprovalDecision::Decline, json!({"action": "decline"})),
        (ProviderApprovalDecision::Cancel, json!({"action": "cancel"})),
    ];
    for (decision, response) in cases {
        let request = json!({"id": 7001, "method": "mcpServer/elicitation/request", "params": {
            "mode": "form", "message": "Allow ChatGPT to use Safari?", "serverName": "computer-use", "threadId": root(),
            "turnId": wire()["responses"]["turnStart"]["turn"]["id"], "_meta": {"app_name": "Safari", "persist": ["session", "always"]},
            "requestedSchema": {"type": "object", "properties": {"approval": {"type": "string", "enum": ["once", "session", "always"]}}, "required": ["approval"]}
        }});
        let script = Script::new(
            json!({"rootThreadId": root(), "holdTurnOpen": true, "completeTurnOnServerResponse": true, "notifications": [], "serverRequests": [request]}),
        );
        let (runtime, mut events) = runtime("thread-codex-mcp-elicitation", &script, RuntimeMode::Auto);
        runtime.start().await.unwrap();
        runtime
            .send_turn(SendTurnInput {
                input: Some("Open Safari".into()),
                ..SendTurnInput::default()
            })
            .await
            .unwrap();
        let seen = until(&mut events, |event| event.method == "mcpServer/elicitation/request").await;
        let approval = seen.last().unwrap();
        assert_eq!(approval.request_kind.map(|kind| kind.as_str()), Some("mcp-elicitation"));
        runtime.respond_to_request(approval.request_id.as_ref().unwrap(), decision).await.unwrap();
        until(&mut events, |event| event.method == "turn/completed").await;
        let recorded = script.sidecar("responses");
        assert_eq!(recorded[0]["id"], 7001);
        assert_eq!(recorded[0]["result"], response, "{decision:?}");
        runtime.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn restores_the_runtime_context_after_the_root_thread_compacts() {
    require_node!();
    let (child_a, _) = children();
    let compacted = |thread: &str| json!({"method": "item/completed", "params": {"threadId": thread, "turnId": format!("{thread}-turn"), "completedAtMs": 0, "item": {"type": "contextCompaction", "id": format!("compaction-{thread}")}}});
    let script = Script::new(json!({"rootThreadId": root(), "recordRequests": true, "notifications": [compacted(&child_a), compacted(&root())]}));
    let mut environment: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    environment.insert("T3_CODEX_COLLAB_SCRIPT".into(), script.path.to_string_lossy().into_owned());
    let mut options = CodexSessionRuntimeOptions::new(
        ThreadId::new("thread-compaction-context"),
        fixtures_dir().join("codexCollabMockPeer.sh").to_string_lossy().into_owned(),
        std::env::temp_dir().to_string_lossy().into_owned(),
        RuntimeMode::FullAccess,
    );
    options.environment = Some(environment);
    options.models = Some(Arc::new(|| {
        Box::pin(async {
            vec![
                serde_json::from_value::<ServerProviderModel>(json!({"slug": "gpt-5.6-sol", "name": "GPT-5.6 Sol", "isCustom": false, "capabilities": null}))
                    .unwrap(),
            ]
        })
    }));
    let runtime = CodexSessionRuntime::spawn(options).unwrap();
    let mut events = runtime.take_events().unwrap();
    runtime.start().await.unwrap();
    runtime
        .send_turn(SendTurnInput {
            input: Some("keep going".into()),
            interaction_mode: Some(zc_contracts::ProviderInteractionMode::Default),
            ..SendTurnInput::default()
        })
        .await
        .unwrap();
    until(&mut events, |event| event.method == "turn/completed").await;
    let requests = script.sidecar("requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["method"], "thread/inject_items");
    assert_eq!(requests[0]["params"]["threadId"], root().as_str());
    let items = requests[0]["params"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["role"], "developer");
    let text = items[0]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.starts_with("<t3_code_runtime><runtime_info>") && text.ends_with("</t3_code_runtime>"),
        "{text}"
    );
    assert!(text.contains("as GPT-5.6 Sol (model slug: gpt-5.6-sol)"), "{text}");
    runtime.close().await;
}
