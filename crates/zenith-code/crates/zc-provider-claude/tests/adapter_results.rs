//! `ClaudeAdapter.test.ts`, part 2: result classification, usage limits, interrupts, token usage.

mod support;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use support::*;
use zc_ports::adapter::ProviderAdapter;

async fn completed_turn_for(harness: &mut Harness, messages: Vec<Value>) -> Vec<Value> {
    harness.start(json!({})).await;
    harness.send(json!({"input": "hello"})).await;
    for message in messages {
        harness.emit(message);
    }
    harness.take_until("turn.completed").await
}

fn result_message(fields: Value) -> Value {
    with(json!({"type": "result", "session_id": "sdk-session-x", "uuid": "result-x"}), fields)
}

fn with(mut base: Value, fields: Value) -> Value {
    for (key, value) in fields.as_object().unwrap() {
        if value.is_null() && key != "parent_tool_use_id" && key != "stop_reason" {
            base.as_object_mut().unwrap().remove(key);
        } else {
            base[key] = value.clone();
        }
    }
    base
}

#[tokio::test]
async fn treats_user_aborted_claude_results_as_interrupted_without_a_runtime_error() {
    let mut harness = Harness::default();
    let events = completed_turn_for(
        &mut harness,
        vec![result_message(
            json!({"subtype": "error_during_execution", "is_error": false, "errors": ["Error: Request was aborted."], "stop_reason": "tool_use",
            "usage": {"input_tokens": 12, "cache_read_input_tokens": 3, "cache_creation_input_tokens": 1, "output_tokens": 4}}),
        )],
    )
    .await;
    assert_eq!(
        types(&events),
        vec![
            "session.started",
            "session.configured",
            "session.state.changed",
            "turn.started",
            "thread.started",
            "thread.token-usage.updated",
            "turn.completed"
        ]
    );
    let payload = &events[6]["payload"];
    assert_eq!(payload["state"], json!("interrupted"));
    assert_eq!(payload["errorMessage"], json!("Error: Request was aborted."));
    assert_eq!(payload["stopReason"], json!("tool_use"));
    assert_eq!(
        payload["tokenUsage"],
        json!({"usageStatus": "partial", "usageScope": "main_agent", "inputTokens": 16, "cachedInputTokens": 3, "cacheCreationTokens": 1, "outputTokens": 4, "hasSubagents": false})
    );
}

#[tokio::test]
async fn treats_aborted_tools_results_as_interrupted_and_hides_ede_diagnostic_errors() {
    let mut harness = Harness::default();
    let events = completed_turn_for(
        &mut harness,
        vec![result_message(json!({"subtype": "error_during_execution", "is_error": true, "errors": ["[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=tool_use"], "stop_reason": "tool_use", "terminal_reason": "aborted_tools"}))],
    )
    .await;
    assert_eq!(
        types(&events),
        vec![
            "session.started",
            "session.configured",
            "session.state.changed",
            "turn.started",
            "thread.started",
            "turn.completed"
        ]
    );
    assert_eq!(events[5]["payload"]["state"], json!("interrupted"));
    assert!(events[5]["payload"].get("errorMessage").is_none());
}

#[tokio::test]
async fn fails_a_turn_when_the_result_carries_a_give_up_terminal_reason() {
    let mut harness = Harness::default();
    let events = completed_turn_for(
        &mut harness,
        vec![result_message(
            json!({"subtype": "success", "is_error": false, "result": "", "errors": [], "stop_reason": null, "terminal_reason": "api_error"}),
        )],
    )
    .await;
    assert_eq!(
        types(&events),
        vec![
            "session.started",
            "session.configured",
            "session.state.changed",
            "turn.started",
            "thread.started",
            "runtime.error",
            "turn.completed"
        ]
    );
    assert_eq!(events[6]["payload"]["state"], json!("failed"));
    assert_eq!(events[6]["payload"]["errorMessage"], json!("Claude gave up after repeated API errors."));
    assert_eq!(events[6]["payload"]["stopReason"], Value::Null);
}

fn auth_failure_assistant() -> Value {
    json!({"type": "assistant", "session_id": "sdk-session-auth", "uuid": "assistant-auth", "parent_tool_use_id": null, "error": "authentication_failed", "is_api_error_message": true,
        "message": {"id": "assistant-message-auth", "model": "<synthetic>", "content": [{"type": "text", "text": "Not logged in · Please run /login"}]}})
}

#[tokio::test]
async fn reports_the_real_cause_when_an_expired_login_is_followed_by_another_outcome() {
    let cases: Vec<(Value, &str, Option<&str>)> = vec![
        (
            json!({"subtype": "success", "is_error": false, "terminal_reason": "api_error", "errors": []}),
            "failed",
            Some("claude auth login"),
        ),
        (
            json!({"subtype": "success", "is_error": true, "errors": []}),
            "failed",
            Some("claude auth login"),
        ),
        (
            json!({"subtype": "success", "is_error": false, "terminal_reason": "prompt_too_long", "errors": []}),
            "failed",
            Some("prompt exceeds the model's context window"),
        ),
        (
            json!({"subtype": "error_during_execution", "is_error": true, "errors": ["Tool execution failed: EACCES"]}),
            "failed",
            Some("EACCES"),
        ),
        (
            json!({"subtype": "error_during_execution", "is_error": true, "terminal_reason": "aborted_tools", "errors": []}),
            "interrupted",
            None,
        ),
        (
            json!({"subtype": "error_during_execution", "is_error": true, "errors": ["cancelled"]}),
            "cancelled",
            Some("cancelled"),
        ),
    ];
    for (result, state, error) in cases {
        let mut harness = Harness::default();
        let events = completed_turn_for(&mut harness, vec![auth_failure_assistant(), result_message(result.clone())]).await;
        let payload = &events.last().unwrap()["payload"];
        assert_eq!(payload["state"], json!(state), "{result}");
        match error {
            None => assert!(payload.get("errorMessage").is_none(), "{result}"),
            Some(fragment) => assert!(payload["errorMessage"].as_str().unwrap_or_default().contains(fragment), "{result}: {payload}"),
        }
    }
}

const USAGE_LIMIT_MESSAGE: &str = "Claude usage limit reached. Send the message again once the limit resets.";
const GENERIC_API_ERROR_MESSAGE: &str = "Claude gave up after repeated API errors.";

fn rate_limit_assistant() -> Value {
    json!({"type": "assistant", "session_id": "sdk-session-limit", "uuid": "assistant-limit", "parent_tool_use_id": null, "error": "rate_limit",
        "message": {"id": "assistant-message-limit", "model": "<synthetic>", "content": [{"type": "text", "text": "You've hit your session limit"}]}})
}

fn rate_limit_result() -> Value {
    json!({"type": "result", "subtype": "success", "is_error": true, "terminal_reason": "api_error", "session_id": "sdk-session-limit", "uuid": "result-limit"})
}

#[tokio::test]
async fn fails_a_usage_limited_turn_with_the_limit_it_parked_on() {
    let mut harness = Harness::default();
    let now_s = 1_772_323_200i64;
    let events = completed_turn_for(
        &mut harness,
        vec![
            json!({"type": "rate_limit_event", "rate_limit_info": {"status": "rejected", "rateLimitType": "five_hour", "resetsAt": now_s + 7200}, "session_id": "sdk-session-limit", "uuid": "rate-limit-rejected"}),
            result_message(json!({"subtype": "success", "is_error": false, "terminal_reason": "api_error", "errors": []})),
        ],
    )
    .await;
    let payload = &events.last().unwrap()["payload"];
    assert_eq!(payload["state"], json!("failed"));
    assert_eq!(payload["errorMessage"], json!(USAGE_LIMIT_MESSAGE));
    let warning = first_of(&events, "runtime.warning");
    assert_eq!(
        warning["payload"]["message"],
        json!("Claude usage limit reached. This turn is paused until the 5-hour limit resets in 2h.")
    );
}

#[tokio::test]
async fn classifies_the_terminal_api_failure_after_assistant_rate_limits() {
    let cases: Vec<(Vec<Value>, &str)> = vec![
        (vec![rate_limit_assistant()], USAGE_LIMIT_MESSAGE),
        (
            vec![rate_limit_assistant(), with(rate_limit_assistant(), json!({"error": null}))],
            GENERIC_API_ERROR_MESSAGE,
        ),
        (
            vec![rate_limit_assistant(), with(rate_limit_assistant(), json!({"error": "server_error"}))],
            GENERIC_API_ERROR_MESSAGE,
        ),
        (
            vec![with(rate_limit_assistant(), json!({"parent_tool_use_id": "nested-tool"}))],
            GENERIC_API_ERROR_MESSAGE,
        ),
        (
            vec![
                rate_limit_assistant(),
                with(rate_limit_assistant(), json!({"error": null, "parent_tool_use_id": "nested-tool"})),
            ],
            USAGE_LIMIT_MESSAGE,
        ),
    ];
    for (messages, expected) in cases {
        let mut harness = Harness::default();
        let mut all: Vec<Value> = messages
            .into_iter()
            .enumerate()
            .map(|(i, m)| with(m, json!({"uuid": format!("assistant-{i}")})))
            .collect();
        all.push(rate_limit_result());
        let events = completed_turn_for(&mut harness, all).await;
        let errors = of_type(&events, "runtime.error");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0]["payload"]["message"], json!(expected));
        let payload = &events.last().unwrap()["payload"];
        assert_eq!(payload["state"], json!("failed"));
        assert_eq!(payload["errorMessage"], json!(expected));
    }
}

#[tokio::test]
async fn names_repeated_usage_limits_without_carrying_them_into_a_later_turn() {
    let mut harness = Harness::default();
    harness.start(json!({})).await;
    for (index, expected) in [USAGE_LIMIT_MESSAGE, USAGE_LIMIT_MESSAGE, GENERIC_API_ERROR_MESSAGE].into_iter().enumerate() {
        harness.send(json!({"input": "again"})).await;
        if index == 0 {
            harness.emit(json!({"type": "rate_limit_event", "rate_limit_info": {"status": "rejected", "rateLimitType": "five_hour"}, "session_id": "sdk-session-limit", "uuid": "limit-rejected"}));
        }
        if index < 2 {
            harness.emit(with(rate_limit_assistant(), json!({"uuid": format!("assistant-limit-{index}")})));
        }
        harness.emit(with(rate_limit_result(), json!({"uuid": format!("result-limit-{index}")})));
        let events = harness.take_until("turn.completed").await;
        let payload = &events.last().unwrap()["payload"];
        assert_eq!(payload["state"], json!("failed"));
        assert_eq!(payload["errorMessage"], json!(expected), "turn {index}");
    }
}

#[tokio::test]
async fn preserves_terminal_failure_evidence() {
    let api_error = json!({"subtype": "success", "is_error": false, "terminal_reason": "api_error", "errors": []});
    let mut cases: Vec<(&str, Value, Option<&str>, &str)> = vec![
        (
            "auth",
            json!({"subtype": "error_during_execution", "is_error": true, "terminal_reason": "api_error", "errors": ["Tool execution failed: EACCES"]}),
            Some("EACCES"),
            "failed",
        ),
        (
            "auth",
            json!({"subtype": "success", "is_error": true, "terminal_reason": "api_error", "api_error_status": 529, "errors": []}),
            Some("overloaded (529)"),
            "failed",
        ),
        (
            "auth",
            json!({"subtype": "success", "is_error": true, "errors": ["Tool execution failed: EACCES"]}),
            Some("EACCES"),
            "failed",
        ),
        ("recovered", api_error.clone(), Some("repeated API errors"), "failed"),
        ("nested-auth", api_error.clone(), Some("repeated API errors"), "failed"),
        (
            "assistant-rate-limit",
            json!({"subtype": "success", "is_error": true, "terminal_reason": "api_error", "api_error_status": 529, "errors": []}),
            Some("overloaded (529)"),
            "failed",
        ),
        (
            "assistant-rate-limit",
            json!({"subtype": "success", "is_error": true, "terminal_reason": "api_error", "errors": ["Tool execution failed: EACCES"]}),
            Some("EACCES"),
            "failed",
        ),
        (
            "assistant-rate-limit",
            json!({"subtype": "error_during_execution", "is_error": true, "terminal_reason": "aborted_tools", "errors": []}),
            None,
            "interrupted",
        ),
        ("recovered", json!({"subtype": "success", "is_error": false, "errors": []}), None, "completed"),
    ];
    for evidence in ["recovered-missing-reset", "recovered-next-reset", "recovered-warning", "two-windows-recovered"] {
        cases.push((evidence, api_error.clone(), Some("repeated API errors"), "failed"));
    }
    for evidence in ["two-windows-one-recovered", "rejected-again"] {
        cases.push((evidence, api_error.clone(), Some("usage limit reached"), "failed"));
    }
    for (evidence, result, expected, state) in cases {
        let mut harness = Harness::default();
        let mut messages = Vec::new();
        let resets_at = 1_772_323_200i64 + 7200;
        let limit = |status: &str, kind: &str, resets: Option<i64>, uuid: &str| {
            let mut info = json!({"status": status, "rateLimitType": kind});
            if let Some(resets) = resets {
                info["resetsAt"] = json!(resets);
            }
            json!({"type": "rate_limit_event", "rate_limit_info": info, "session_id": "sdk-audit", "uuid": uuid})
        };
        if evidence == "assistant-rate-limit" {
            messages.push(rate_limit_assistant());
        } else if evidence == "auth" || evidence == "nested-auth" {
            messages.push(json!({"type": "assistant", "session_id": "sdk-audit", "uuid": "audit-auth",
                "parent_tool_use_id": if evidence == "nested-auth" { json!("synthetic-parent-tool") } else { Value::Null },
                "error": "authentication_failed", "is_api_error_message": true,
                "message": {"id": "audit-message", "model": "synthetic-audit-model", "content": [{"type": "text", "text": "Not logged in. Please run /login"}]}}));
        } else {
            messages.push(limit("rejected", "five_hour", Some(resets_at), "audit-limit-rejected"));
            if evidence.starts_with("two-windows") {
                messages.push(limit("rejected", "seven_day", Some(resets_at), "audit-weekly-rejected"));
            }
            let status = if evidence == "recovered-warning" { "allowed_warning" } else { "allowed" };
            let resets = match evidence {
                "recovered-missing-reset" => None,
                "recovered-next-reset" => Some(resets_at + 18000),
                _ => Some(resets_at),
            };
            messages.push(limit(status, "five_hour", resets, "audit-limit-allowed"));
            if evidence == "two-windows-recovered" || evidence == "rejected-again" {
                let (status, kind) = if evidence == "rejected-again" {
                    ("rejected", "five_hour")
                } else {
                    ("allowed", "seven_day")
                };
                messages.push(limit(status, kind, Some(resets_at), "audit-final-quota-update"));
            }
        }
        messages.push(with(
            result.clone(),
            json!({"type": "result", "session_id": "sdk-audit", "uuid": "audit-result"}),
        ));
        let events = completed_turn_for(&mut harness, messages).await;
        let payload = &events.last().unwrap()["payload"];
        assert_eq!(payload["state"], json!(state), "{evidence} {result}");
        match expected {
            Some(fragment) => assert!(payload["errorMessage"].as_str().unwrap_or_default().contains(fragment), "{evidence}: {payload}"),
            None => assert!(payload.get("errorMessage").is_none(), "{evidence}: {payload}"),
        }
        if evidence == "rejected-again" {
            assert_eq!(of_type(&events, "runtime.warning").len(), 1);
        }
    }
}

#[tokio::test]
async fn reports_the_same_claude_config_and_cwd_used_by_the_spawned_query() {
    let cases: Vec<(&str, Option<&str>)> = vec![
        ("./synthetic config's $literal", None),
        ("", Some(".synthetic config's $literal")),
        ("", Some(" /synthetic/path with edge spaces ")),
    ];
    for (home_path, inherited) in cases {
        let mut env = zc_provider_claude::home::Env::new();
        if let Some(inherited) = inherited {
            env.insert("CLAUDE_CONFIG_DIR".into(), inherited.into());
        }
        let mut harness = Harness::new(HarnessConfig {
            claude_config: Some(json!({"homePath": home_path})),
            environment: Some(env),
            ..HarnessConfig::default()
        });
        let cwd = "/tmp/synthetic-audit-project";
        harness.start(json!({"cwd": cwd})).await;
        harness.send(json!({"input": "synthetic"})).await;
        harness.emit(auth_failure_assistant());
        harness.emit(result_message(
            json!({"subtype": "success", "is_error": false, "terminal_reason": "api_error", "errors": []}),
        ));
        let events = harness.take_until("turn.completed").await;
        let options = harness.factory.last().options.clone();
        let expected_dir = if home_path.is_empty() {
            inherited.unwrap().to_string()
        } else {
            zc_core::paths::resolve_path(std::path::Path::new(home_path)).to_string_lossy().into_owned()
        };
        assert_eq!(options.env.get("CLAUDE_CONFIG_DIR"), Some(&expected_dir));
        assert_eq!(options.cwd.as_deref(), Some(cwd));
        let message = events.last().unwrap()["payload"]["errorMessage"].as_str().unwrap().to_string();
        assert!(
            message.contains(&format!("CLAUDE_CONFIG_DIR set to {}", serde_json::to_string(&expected_dir).unwrap())),
            "{message}"
        );
        assert!(message.contains(&format!("from {}", serde_json::to_string(cwd).unwrap())));
        assert!(!message.contains("CLAUDE_CONFIG_DIR="));
    }
}

#[tokio::test]
async fn fails_a_turn_for_every_dead_turn_terminal_reason() {
    for reason in [
        "blocking_limit",
        "rapid_refill_breaker",
        "prompt_too_long",
        "image_error",
        "model_error",
        "malformed_tool_use_exhausted",
        "budget_exhausted",
        "structured_output_retry_exhausted",
        "tool_deferred_unavailable",
        "turn_setup_failed",
    ] {
        let mut harness = Harness::default();
        let events = completed_turn_for(
            &mut harness,
            vec![result_message(
                json!({"subtype": "success", "is_error": false, "result": "", "errors": [], "stop_reason": null, "terminal_reason": reason}),
            )],
        )
        .await;
        let payload = &events.last().unwrap()["payload"];
        assert_eq!(payload["state"], json!("failed"), "{reason}");
        assert!(payload["errorMessage"].as_str().is_some_and(|m| !m.is_empty()), "{reason}");
    }
}

#[tokio::test]
async fn preserves_behavior_for_an_unknown_runtime_terminal_reason() {
    for subtype in ["success", "error_during_execution"] {
        let mut harness = Harness::default();
        let errors = if subtype == "success" { json!([]) } else { json!(["Provider error detail"]) };
        let events = completed_turn_for(
            &mut harness,
            vec![result_message(json!({"subtype": subtype, "is_error": subtype != "success", "result": "", "errors": errors, "stop_reason": null, "terminal_reason": "future_terminal_reason"}))],
        )
        .await;
        let payload = &events.last().unwrap()["payload"];
        assert_eq!(payload["state"], json!(if subtype == "success" { "completed" } else { "failed" }));
        if subtype == "success" {
            assert!(payload.get("errorMessage").is_none());
        } else {
            assert_eq!(payload["errorMessage"], json!("Provider error detail"));
        }
    }
}

#[tokio::test]
async fn fails_a_turn_when_a_success_result_reports_a_529_overload() {
    let mut harness = Harness::default();
    let events = completed_turn_for(
        &mut harness,
        vec![result_message(
            json!({"subtype": "success", "is_error": true, "api_error_status": 529, "result": "", "errors": [], "stop_reason": null}),
        )],
    )
    .await;
    let payload = &events.last().unwrap()["payload"];
    assert_eq!(payload["state"], json!("failed"));
    assert_eq!(payload["errorMessage"], json!("Claude API is overloaded (529). Try again shortly."));
}

#[tokio::test]
async fn interrupt_turn_settles_live_tasks_and_closes_the_provider_session() {
    let mut harness = Harness::default();
    harness.start(json!({})).await;
    harness.send(json!({"input": "spawn agents"})).await;
    harness.emit(json!({"type": "system", "subtype": "task_started", "task_id": "task-live", "description": "Agent A", "task_type": "local_agent", "uuid": "task-live-uuid", "session_id": "sdk-session"}));
    harness.emit(json!({"type": "system", "subtype": "task_started", "task_id": "task-settled", "description": "Agent B", "task_type": "local_agent", "uuid": "task-settled-uuid", "session_id": "sdk-session"}));
    harness.emit(json!({"type": "system", "subtype": "task_notification", "task_id": "task-settled", "status": "completed", "output_file": "/tmp/task-settled.jsonl", "summary": "done", "uuid": "task-settled-done-uuid", "session_id": "sdk-session"}));
    let before = harness.take_until("task.completed").await;
    assert_eq!(of_type(&before, "task.started").len(), 2);
    harness.adapter.interrupt(THREAD_ID).await.unwrap();
    assert_eq!(harness.query().close_calls.load(Ordering::SeqCst), 1);
    assert!(harness.adapter.list_sessions().await.is_empty());
    let after = harness.take_until("session.exited").await;
    let stopped = of_type(&after, "task.completed");
    assert_eq!(stopped.len(), 1);
    assert_eq!(
        stopped[0]["payload"],
        json!({"taskId": "task-live", "status": "stopped", "taskType": "local_agent", "title": "Agent A"})
    );
}

#[tokio::test]
async fn interrupt_turn_lets_claude_abort_the_turn_before_closing_the_session() {
    let mut harness = Harness::default();
    harness.start(json!({})).await;
    harness.send(json!({"input": "hello"})).await;
    let close_calls_at_interrupt = Arc::new(Mutex::new(None));
    let seen = close_calls_at_interrupt.clone();
    harness.query().set_interrupt(move |query| {
        *seen.lock().unwrap() = Some(query.close_calls.load(Ordering::SeqCst));
        query.emit(json!({"type": "result", "subtype": "error_during_execution", "is_error": false, "errors": ["Error: Request was aborted."], "session_id": "sdk-session", "uuid": "result-interrupted"}));
    });
    harness.adapter.interrupt(THREAD_ID).await.unwrap();
    assert_eq!(*close_calls_at_interrupt.lock().unwrap(), Some(0));
    assert_eq!(harness.query().close_calls.load(Ordering::SeqCst), 1);
    let events = harness.take_until("turn.completed").await;
    assert_eq!(events.last().unwrap()["payload"]["state"], json!("interrupted"));
}

#[tokio::test]
async fn interrupt_turn_closes_the_session_when_claude_never_aborts_the_turn() {
    let harness = Harness::default();
    harness.start(json!({})).await;
    harness.send(json!({"input": "hello"})).await;
    harness.query().set_interrupt(|_| {});
    harness.adapter.interrupt(THREAD_ID).await.unwrap();
    assert_eq!(harness.query().close_calls.load(Ordering::SeqCst), 1);
    assert!(!harness.adapter.has_session(&THREAD_ID.into()).await);
}

#[tokio::test]
async fn keeps_the_session_available_when_process_close_fails() {
    let harness = Harness::default();
    harness.start(json!({})).await;
    *harness.query().close_error.lock().unwrap() = Some("close failed".into());
    let error = harness.adapter.interrupt(THREAD_ID).await.unwrap_err();
    assert_eq!(err_json(&error)["_tag"], json!("ProviderAdapterProcessError"));
    assert_eq!(harness.query().close_calls.load(Ordering::SeqCst), 1);
    assert!(harness.adapter.has_session(&THREAD_ID.into()).await);
    let sessions = harness.adapter.list_sessions().await;
    assert_eq!(sessions[0].status, zc_contracts::ProviderSessionStatus::Ready);
}

#[tokio::test]
async fn stop_all_attempts_every_session_when_one_process_close_fails() {
    let harness = Harness::default();
    harness.start(json!({})).await;
    let first = harness.query();
    harness.start(json!({"threadId": "thread-claude-resume"})).await;
    let second = harness.query();
    *first.close_error.lock().unwrap() = Some("close failed".into());
    assert!(harness.adapter.stop_all().await.is_err());
    assert_eq!(first.close_calls.load(Ordering::SeqCst), 1);
    assert_eq!(second.close_calls.load(Ordering::SeqCst), 1);
    assert!(harness.adapter.has_session(&THREAD_ID.into()).await);
    assert!(!harness.adapter.has_session(&"thread-claude-resume".into()).await);
}

#[tokio::test]
async fn completes_with_result_usage() {
    let mut harness = Harness::default();
    let s = "sdk-session-result-usage";
    let events = completed_turn_for(
        &mut harness,
        vec![
            json!({"type": "assistant", "session_id": s, "uuid": "a1", "parent_tool_use_id": null, "message": {"id": "m1", "role": "assistant", "content": [], "usage": {"input_tokens": 80, "output_tokens": 20}}}),
            json!({"type": "assistant", "session_id": s, "uuid": "a2", "parent_tool_use_id": null, "message": {"id": "m2", "role": "assistant", "content": [], "usage": {"input_tokens": 180, "output_tokens": 20}}}),
            json!({"type": "assistant", "session_id": s, "uuid": "a3", "parent_tool_use_id": null, "message": {"id": "m3", "role": "assistant", "content": []}}),
            json!({"type": "result", "subtype": "success", "is_error": false, "duration_ms": 1234, "duration_api_ms": 1200, "num_turns": 1, "result": "done", "stop_reason": "end_turn", "session_id": s,
                "usage": {"input_tokens": 400, "cache_read_input_tokens": 90, "cache_creation_input_tokens": 10, "output_tokens": 50, "output_tokens_details": {"thinking_tokens": 30}},
                "modelUsage": {CAPABLE: {"contextWindow": 200000, "maxOutputTokens": 64000}}}),
        ],
    )
    .await;
    assert_eq!(
        first_of(&events, "thread.token-usage.updated")["payload"]["usage"],
        json!({"usedTokens": 200, "lastUsedTokens": 200, "totalProcessedTokens": 550, "inputTokens": 180, "outputTokens": 20, "maxTokens": 200000})
    );
    assert_eq!(
        first_of(&events, "turn.completed")["payload"]["tokenUsage"],
        json!({"usageStatus": "complete", "usageScope": "main_agent", "inputTokens": 500, "cachedInputTokens": 90, "cacheCreationTokens": 10, "reasoningTokens": 30, "outputTokens": 50, "hasSubagents": false})
    );
}

#[tokio::test]
async fn treats_omitted_claude_cache_counters_as_zero_contributions() {
    let mut harness = Harness::default();
    let events = completed_turn_for(
        &mut harness,
        vec![json!({"type": "result", "subtype": "success", "is_error": false, "duration_ms": 100, "num_turns": 1, "result": "done", "stop_reason": "end_turn", "session_id": "s", "usage": {"input_tokens": 42, "output_tokens": 9}})],
    )
    .await;
    assert_eq!(
        events.last().unwrap()["payload"]["tokenUsage"],
        json!({"usageStatus": "complete", "usageScope": "main_agent", "inputTokens": 42, "outputTokens": 9, "hasSubagents": false})
    );
}

#[tokio::test]
async fn uses_per_turn_result_usage_across_consecutive_claude_turns() {
    let mut harness = Harness::default();
    harness.start(json!({})).await;
    let model_usage =
        |n: i64| json!({"claude-opus-4-6": {"inputTokens": n * 10_000, "outputTokens": n * 1_000, "contextWindow": 200_000, "maxOutputTokens": 64_000}});
    harness.send(json!({"input": "first turn"})).await;
    harness.emit(
        json!({"type": "result", "subtype": "success", "is_error": false, "num_turns": 1, "stop_reason": "end_turn", "session_id": "s",
        "usage": {"input_tokens": 100, "cache_read_input_tokens": 20, "cache_creation_input_tokens": 5, "output_tokens": 10}, "modelUsage": model_usage(1)}),
    );
    let first = harness.take_until("turn.completed").await;
    harness.send(json!({"input": "second turn"})).await;
    harness.emit(
        json!({"type": "result", "subtype": "success", "is_error": false, "num_turns": 2, "stop_reason": "end_turn", "session_id": "s",
        "usage": {"input_tokens": 30, "cache_read_input_tokens": 2, "cache_creation_input_tokens": 3, "output_tokens": 7}, "modelUsage": model_usage(2)}),
    );
    let second = harness.take_until("turn.completed").await;
    assert_eq!(
        first.last().unwrap()["payload"]["tokenUsage"],
        json!({"usageStatus": "complete", "usageScope": "main_agent", "inputTokens": 125, "cachedInputTokens": 20, "cacheCreationTokens": 5, "outputTokens": 10, "hasSubagents": false})
    );
    assert_eq!(
        second.last().unwrap()["payload"]["tokenUsage"],
        json!({"usageStatus": "complete", "usageScope": "main_agent", "inputTokens": 35, "cachedInputTokens": 2, "cacheCreationTokens": 3, "outputTokens": 7, "hasSubagents": false})
    );
}

#[tokio::test]
async fn preserves_compacted_usage_when_completion_follows_an_older_assistant_frame() {
    let mut harness = Harness::default();
    let s = "sdk-session-compacted-usage";
    let events = completed_turn_for(
        &mut harness,
        vec![
            json!({"type": "assistant", "session_id": s, "uuid": "a", "parent_tool_use_id": null, "message": {"id": "m", "role": "assistant", "content": [], "usage": {"input_tokens": 180, "output_tokens": 20}}}),
            json!({"type": "system", "subtype": "compact_boundary", "compact_metadata": {"pre_tokens": 200, "post_tokens": 40}, "session_id": s, "uuid": "cb1"}),
            json!({"type": "system", "subtype": "compact_boundary", "compact_metadata": {"post_tokens": 40}, "session_id": s, "uuid": "cb2"}),
            json!({"type": "result", "subtype": "success", "is_error": false, "num_turns": 2, "result": "done", "stop_reason": "end_turn", "session_id": s,
                "usage": {"input_tokens": 400, "output_tokens": 50}, "modelUsage": {CAPABLE: {"contextWindow": 200000, "maxOutputTokens": 64000}}}),
        ],
    )
    .await;
    let compactions: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "thread.state.changed" && e["payload"]["state"] == "compacted")
        .collect();
    assert_eq!(compactions[0]["payload"]["beforeTokens"], json!(200));
    assert_eq!(compactions[0]["payload"]["afterTokens"], json!(40));
    assert!(compactions[1]["payload"].get("beforeTokens").is_none());
    assert_eq!(compactions[1]["payload"]["afterTokens"], json!(40));
    let last_usage = events.iter().rev().find(|e| e["type"] == "thread.token-usage.updated").unwrap();
    assert_eq!(
        last_usage["payload"]["usage"],
        json!({"usedTokens": 40, "totalProcessedTokens": 450, "maxTokens": 200000})
    );
}
