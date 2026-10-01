//! Ported from `CodexSessionRuntime.test.ts` (the pure parts) and `CodexCollabWire.test.ts`.

use serde_json::{json, Value};

use super::*;
use crate::instructions::{build_codex_additional_context, build_codex_developer_instructions, CodexRuntimeInfo};

fn wire() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../code/apps/server/src/provider/testFixtures/codexMultiAgentWire.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn context(model: &str, effort: &str) -> Value {
    serde_json::to_value(build_codex_additional_context(
        &CodexRuntimeInfo {
            model: model.into(),
            model_name: None,
            reasoning_effort: effort.into(),
        },
        ToolAvailability::browser_only(true),
    ))
    .unwrap()
}

fn params(input: TurnStartParamsInput) -> Value {
    build_turn_start_params(&input)
}

#[test]
fn currency_skill_aliases_become_dollar_skills() {
    for symbol in ["€", "£", "¥", "₹", "₩", "₿", "𑿝"] {
        let prose = format!("{symbol}20 {symbol}20k {symbol}100M {symbol}1e6 5{symbol}review");
        let built = params(TurnStartParamsInput {
            thread_id: "provider-thread-1".into(),
            runtime_mode: Some(RuntimeMode::FullAccess),
            prompt: Some(format!("{symbol}review {symbol}2spec $existing {prose} {symbol}last")),
            ..TurnStartParamsInput::default()
        });
        assert_eq!(
            built["input"],
            json!([{"type": "text", "text": format!("$review $2spec $existing {prose} $last")}]),
            "{symbol}"
        );
    }
}

#[test]
fn plan_collaboration_mode() {
    let built = params(TurnStartParamsInput {
        thread_id: "provider-thread-1".into(),
        runtime_mode: Some(RuntimeMode::FullAccess),
        prompt: Some("Make a plan".into()),
        model: Some("gpt-5.3-codex".into()),
        effort: Some("medium".into()),
        interaction_mode: Some(ProviderInteractionMode::Plan),
        ..TurnStartParamsInput::default()
    });
    assert_eq!(
        built,
        json!({
            "threadId": "provider-thread-1",
            "approvalPolicy": "never",
            "approvalsReviewer": "user",
            "sandboxPolicy": {"type": "dangerFullAccess"},
            "input": [{"type": "text", "text": "Make a plan"}],
            "model": "gpt-5.3-codex",
            "effort": "medium",
            "collaborationMode": {"mode": "plan", "settings": {"model": "gpt-5.3-codex", "reasoning_effort": "medium", "developer_instructions": build_codex_developer_instructions(ProviderInteractionMode::Plan)}},
            "additionalContext": context("gpt-5.3-codex", "medium"),
        })
    );
}

#[test]
fn default_collaboration_mode_and_image_attachments() {
    let built = params(TurnStartParamsInput {
        thread_id: "provider-thread-1".into(),
        runtime_mode: Some(RuntimeMode::AutoAcceptEdits),
        prompt: Some("Implement it".into()),
        model: Some("gpt-5.3-codex".into()),
        interaction_mode: Some(ProviderInteractionMode::Default),
        attachments: vec!["/tmp/generated.png".into()],
        ..TurnStartParamsInput::default()
    });
    assert_eq!(
        built,
        json!({
            "threadId": "provider-thread-1",
            "approvalPolicy": "on-request",
            "approvalsReviewer": "user",
            "sandboxPolicy": {"type": "workspaceWrite"},
            "input": [{"type": "text", "text": "Implement it"}, {"type": "localImage", "path": "/tmp/generated.png"}],
            "model": "gpt-5.3-codex",
            "collaborationMode": {"mode": "default", "settings": {"model": "gpt-5.3-codex", "reasoning_effort": "medium", "developer_instructions": build_codex_developer_instructions(ProviderInteractionMode::Default)}},
            "additionalContext": context("gpt-5.3-codex", "medium"),
        })
    );
}

#[test]
fn fallback_model_and_effort_match_in_settings_and_instructions() {
    let built = params(TurnStartParamsInput {
        thread_id: "provider-thread-1".into(),
        runtime_mode: Some(RuntimeMode::FullAccess),
        prompt: Some("Go".into()),
        interaction_mode: Some(ProviderInteractionMode::Default),
        ..TurnStartParamsInput::default()
    });
    assert_eq!(built["collaborationMode"]["settings"]["model"], crate::model::DEFAULT_MODEL);
    assert_eq!(built["collaborationMode"]["settings"]["reasoning_effort"], "medium");
    assert!(built["additionalContext"]["t3_code_runtime"]["value"]
        .as_str()
        .unwrap()
        .contains(&format!("as {} with medium", crate::model::DEFAULT_MODEL)));
}

#[test]
fn runtime_context_names_the_model_by_display_name_and_slug() {
    let built = params(TurnStartParamsInput {
        thread_id: "provider-thread-1".into(),
        runtime_mode: Some(RuntimeMode::FullAccess),
        model: Some("gpt-5.3-codex".into()),
        model_name: Some("GPT-5.3-Codex".into()),
        effort: Some("high".into()),
        interaction_mode: Some(ProviderInteractionMode::Plan),
        ..TurnStartParamsInput::default()
    });
    assert!(built["additionalContext"]["t3_code_runtime"]["value"]
        .as_str()
        .unwrap()
        .contains("as GPT-5.3-Codex (model slug: gpt-5.3-codex) with high reasoning effort"));
}

#[test]
fn auto_mode_routes_approvals_to_the_auto_reviewer() {
    let built = params(TurnStartParamsInput {
        thread_id: "provider-thread-1".into(),
        runtime_mode: Some(RuntimeMode::Auto),
        prompt: Some("Ship it".into()),
        ..TurnStartParamsInput::default()
    });
    assert_eq!(
        built,
        json!({"threadId": "provider-thread-1", "approvalPolicy": "on-request", "approvalsReviewer": "auto_review", "sandboxPolicy": {"type": "workspaceWrite"}, "input": [{"type": "text", "text": "Ship it"}]})
    );
}

#[test]
fn no_collaboration_mode_without_an_interaction_mode() {
    let built = params(TurnStartParamsInput {
        thread_id: "provider-thread-1".into(),
        runtime_mode: Some(RuntimeMode::ApprovalRequired),
        prompt: Some("Review".into()),
        ..TurnStartParamsInput::default()
    });
    assert_eq!(
        built,
        json!({"threadId": "provider-thread-1", "approvalPolicy": "untrusted", "approvalsReviewer": "user", "sandboxPolicy": {"type": "readOnly"}, "input": [{"type": "text", "text": "Review"}]})
    );
}

#[test]
fn turn_start_params_match_the_protocol_schema() {
    let built = params(TurnStartParamsInput {
        thread_id: "t".into(),
        runtime_mode: Some(RuntimeMode::Auto),
        prompt: Some("x".into()),
        attachments: vec!["/tmp/a.png".into()],
        model: Some("gpt-5.4".into()),
        service_tier: Some("fast".into()),
        effort: Some("high".into()),
        interaction_mode: Some(ProviderInteractionMode::Plan),
        ..TurnStartParamsInput::default()
    });
    let typed: zc_codex_protocol::TurnStartParams = serde_json::from_value(built.clone()).unwrap();
    assert!(matches!(typed.input[1], zc_codex_protocol::UserInput::LocalImage(_)));
    let _: zc_codex_protocol::CollaborationMode = serde_json::from_value(built["collaborationMode"].clone()).unwrap();
}

#[test]
fn detects_inline_mcp_configuration() {
    assert!(!has_configured_mcp_server(None));
    assert!(!has_configured_mcp_server(Some(&["--model".into(), "gpt-5.4".into()])));
    assert!(has_configured_mcp_server(Some(&[
        "-c".into(),
        "mcp_servers.t3-code.url=\"http://127.0.0.1/mcp\"".into()
    ])));
}

fn thread_started(thread_id: &str, source: Value, thread_source: Option<&str>) -> (String, Value) {
    let mut thread = json!({"cliVersion": "0.0.0", "createdAt": 0, "cwd": "/tmp/project", "ephemeral": true, "id": thread_id, "modelProvider": "openai", "preview": "", "projectId": null, "sessionId": thread_id, "source": source, "status": {"type": "idle"}, "turns": [], "updatedAt": 0});
    if let Some(thread_source) = thread_source {
        thread["threadSource"] = json!(thread_source);
    }
    ("thread/started".into(), json!({"thread": thread}))
}

#[test]
fn memory_consolidation_is_hidden_without_hiding_other_subagents() {
    let mut filter = MemoryConsolidationFilter::default();
    let (method, params) = thread_started("memory-thread", json!("unknown"), Some("memory_consolidation"));
    assert!(filter.should_suppress(&method, &params));
    assert!(filter.should_suppress(
        "item/agentMessage/delta",
        &json!({"delta": "internal", "itemId": "m", "threadId": "memory-thread", "turnId": "t"})
    ));
    assert!(!filter.should_suppress("serverRequest/resolved", &json!({"requestId": "memory-approval", "threadId": "memory-thread"})));
    assert!(filter.should_suppress("warning", &json!({"message": "internal warning", "threadId": "memory-thread"})));
    assert!(!filter.should_suppress(
        "item/agentMessage/delta",
        &json!({"delta": "normal", "itemId": "r", "threadId": "root-thread", "turnId": "t"})
    ));
    let (method, params) = thread_started("legacy-memory-thread", json!({"subAgent": "memory_consolidation"}), None);
    assert!(filter.should_suppress(&method, &params));
    for source in [
        json!({"subAgent": "review"}),
        json!({"subAgent": "compact"}),
        json!({"subAgent": {"thread_spawn": {"depth": 1, "parent_thread_id": "root-thread"}}}),
    ] {
        let (method, params) = thread_started("visible-subagent", source, None);
        assert!(!filter.should_suppress(&method, &params));
    }
}

#[test]
fn memory_consolidation_threads_are_forgotten_once_closed() {
    let mut filter = MemoryConsolidationFilter::default();
    let (method, params) = thread_started("memory-thread", json!("unknown"), Some("memory_consolidation"));
    filter.should_suppress(&method, &params);
    assert!(filter.should_suppress("thread/closed", &json!({"threadId": "memory-thread"})));
    assert!(!filter.should_suppress(
        "item/agentMessage/delta",
        &json!({"delta": "later", "itemId": "l", "threadId": "memory-thread", "turnId": "lt"})
    ));
}

#[test]
fn user_input_answers() {
    let answers: ProviderUserInputAnswers = serde_json::from_value(json!({"a": "one", "b": ["x", 2, "y"], "c": {"answers": ["z"]}, "d": []})).unwrap();
    assert_eq!(
        to_codex_user_input_answers(&answers).unwrap(),
        json!({"a": {"answers": ["one"]}, "b": {"answers": ["x", "y"]}, "c": {"answers": ["z"]}, "d": {"answers": []}})
    );
    let invalid: ProviderUserInputAnswers = serde_json::from_value(json!({"q": 3})).unwrap();
    assert_eq!(
        to_codex_user_input_answers(&invalid).unwrap_err().to_string(),
        "Invalid Codex user input answers for question 'q'"
    );
}

#[test]
fn stderr_classification() {
    assert_eq!(classify_codex_stderr_line("  "), None);
    assert_eq!(classify_codex_stderr_line("2026-01-01T00:00:00.000Z  INFO codex_core: hello"), None);
    assert_eq!(
        classify_codex_stderr_line("2026-01-01T00:00:00.000Z ERROR codex_core: state db missing rollout path for thread x"),
        None
    );
    assert_eq!(
        classify_codex_stderr_line("\u{1b}[31m2026-01-01T00:00:00.000Z ERROR codex_core: boom\u{1b}[0m").as_deref(),
        Some("2026-01-01T00:00:00.000Z ERROR codex_core: boom")
    );
    assert_eq!(
        classify_codex_stderr_line("failed to connect to websocket").as_deref(),
        Some("failed to connect to websocket")
    );
}

#[test]
fn spawn_spec_extends_the_server_environment_only_without_an_explicit_one() {
    let mut options = CodexSessionRuntimeOptions::new(ThreadId::new("t"), "codex", "/work", RuntimeMode::FullAccess);
    options.home_path = Some("~/.codex_work".into());
    options.launch_args = Some("--strict-config".into());
    let spec = options.spawn_spec();
    assert!(spec.extend_env);
    assert_eq!(spec.args, vec!["app-server", "--strict-config"]);
    assert!(!spec.env["CODEX_HOME"].starts_with('~'));
    options.environment = Some([("PATH".to_owned(), "/bin".to_owned())].into_iter().collect());
    let spec = options.spawn_spec();
    assert!(!spec.extend_env);
    assert_eq!(spec.env["PATH"], "/bin");
}

// -- CodexCollabWire.test.ts --------------------------------------------------------------------

fn wire_thread_id(entry: &Value) -> Option<String> {
    entry["params"]["thread"]["id"]
        .as_str()
        .or_else(|| entry["params"]["threadId"].as_str())
        .map(str::to_owned)
}

#[test]
fn the_capture_is_a_real_two_child_fan_out_with_child_first_ordering() {
    let wire = wire();
    let notifications = wire["notifications"].as_array().unwrap();
    let children: Vec<String> = wire["childThreadIds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(wire["capturedWith"]["model"], "gpt-5.6-luna");
    let paths: Vec<&str> = notifications
        .iter()
        .filter(|entry| entry["params"]["item"]["type"] == "subAgentActivity")
        .map(|entry| entry["params"]["item"]["agentPath"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"/root/alpha") && paths.contains(&"/root/beta"));
    let first_child = notifications
        .iter()
        .position(|entry| wire_thread_id(entry).is_some_and(|id| children.contains(&id)))
        .unwrap();
    let first_registration = notifications
        .iter()
        .position(|entry| entry["params"]["item"]["type"] == "subAgentActivity" && entry["params"]["item"]["kind"] == "started")
        .unwrap();
    assert!(first_child < first_registration);
    let root_activity = notifications.iter().find(|entry| entry["params"]["item"]["agentPath"] == "/root").unwrap();
    assert_eq!(root_activity["params"]["item"]["agentThreadId"], wire["rootThreadId"]);
    for entry in notifications
        .iter()
        .filter(|entry| wire_thread_id(entry).is_some_and(|id| children.contains(&id)))
    {
        let method = entry["method"].as_str().unwrap();
        assert_eq!(route_codex_child_notification(method), ChildNotificationRoute::AgentEvent, "{method}");
    }
}

#[test]
fn every_captured_notification_decodes_against_the_pinned_protocol() {
    let wire = wire();
    for entry in wire["notifications"].as_array().unwrap() {
        let method = entry["method"].as_str().unwrap();
        let decoded = ServerNotification::decode(method, Some(entry["params"].clone()));
        assert!(matches!(decoded, Some(Ok(_))), "{method}: {decoded:?}");
    }
    let _: zc_codex_protocol::ThreadStartResponse = serde_json::from_value(wire["responses"]["threadStart"].clone()).unwrap();
    let _: zc_codex_protocol::TurnStartResponse = serde_json::from_value(wire["responses"]["turnStart"].clone()).unwrap();
}

#[test]
fn routing_table() {
    for method in [
        "turn/started",
        "turn/completed",
        "thread/status/changed",
        "thread/tokenUsage/updated",
        "thread/settings/updated",
        "model/rerouted",
        "item/started",
        "item/completed",
        "thread/closed",
        "error",
    ] {
        assert_eq!(route_codex_child_notification(method), ChildNotificationRoute::AgentEvent, "{method}");
    }
    for method in [
        "item/agentMessage/delta",
        "item/reasoning/textDelta",
        "item/commandExecution/outputDelta",
        "turn/plan/updated",
        "thread/name/updated",
    ] {
        assert_eq!(route_codex_child_notification(method), ChildNotificationRoute::Drop, "{method}");
    }
    for method in [
        "thread/started",
        "thread/status/changed",
        "thread/archived",
        "thread/unarchived",
        "thread/closed",
        "thread/compacted",
        "thread/name/updated",
        "thread/tokenUsage/updated",
        "turn/started",
        "turn/completed",
        "turn/plan/updated",
        "item/plan/delta",
        "thread/settings/updated",
        "model/rerouted",
    ] {
        assert_ne!(route_codex_child_notification(method), ChildNotificationRoute::Parent, "{method}");
        assert!(should_suppress_child_conversation_notification(method), "{method}");
    }
    for method in ["serverRequest/resolved", "thread/somethingBrandNew", "account/rateLimits/updated"] {
        assert_eq!(route_codex_child_notification(method), ChildNotificationRoute::Parent, "{method}");
    }
}
