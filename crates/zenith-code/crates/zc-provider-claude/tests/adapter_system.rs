//! `ClaudeAdapter.test.ts`, part 3: tasks and workflows, stream failures, system subtypes,
//! usage-limit rows.

mod support;

use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{json, Value};
use support::*;
use zc_ports::adapter::ProviderAdapter;

const SUBAGENT_MODEL: &str = "claude-synthetic-subagent[expanded]";

#[tokio::test]
async fn workflow_member_coalescing_identical_snapshots_suppress_changes_emit() {
    let mut harness = Harness::default();
    harness.start(json!({})).await;
    harness.send(json!({"input": "run workflow"})).await;
    let snapshot = |tokens: i64| {
        json!([
            {"type": "workflow_phase", "index": 0, "title": "Work"},
            {"type": "workflow_agent", "index": 0, "state": "running", "label": "member-0", "phaseIndex": 0, "tokens": tokens},
            {"type": "workflow_agent", "index": 1, "state": "running", "label": "member-1", "phaseIndex": 0, "tokens": 50}
        ])
    };
    let tick = |total: i64, members: Value| {
        json!({"type": "system", "subtype": "task_progress", "task_id": "wf-coalesce", "description": "Coalescing workflow",
            "usage": {"total_tokens": total, "tool_uses": 1, "duration_ms": 10}, "workflow_progress": members, "uuid": format!("wf-tick-{total}"), "session_id": "sdk-session"})
    };
    harness.emit(tick(100, snapshot(10)));
    harness.emit(tick(200, snapshot(10)));
    harness.emit(tick(300, snapshot(20)));
    let mut progress = Vec::new();
    loop {
        let event = harness.next_event().await;
        if event["type"] != "task.progress" {
            continue;
        }
        let done = event["payload"]["taskId"] == "wf-coalesce:wf:0" && event["payload"]["typedUsage"]["totalTokens"] == 20;
        progress.push(event);
        if done {
            break;
        }
    }
    let count = |id: &str| progress.iter().filter(|e| e["payload"]["taskId"] == id).count();
    assert_eq!(count("wf-coalesce:wf:0"), 2);
    assert_eq!(count("wf-coalesce:wf:1"), 1);
    assert_eq!(count("wf-coalesce"), 3);
    let coordinator = progress.iter().find(|e| e["payload"]["taskId"] == "wf-coalesce").unwrap();
    assert_eq!(coordinator["payload"]["phases"], json!([{"index": 0, "title": "Work"}]));
    let member = progress.iter().find(|e| e["payload"]["taskId"] == "wf-coalesce:wf:1").unwrap();
    assert_eq!(
        member["payload"],
        json!({"taskId": "wf-coalesce:wf:1", "description": "member-1", "status": "pending", "title": "member-1", "typedUsage": {"totalTokens": 50},
            "parentAgentId": "wf-coalesce", "agentIndex": 1, "phaseIndex": 0, "timelineBypass": true})
    );
}

async fn task_events(harness: &mut Harness, count: usize) -> Vec<Value> {
    let mut events = Vec::new();
    while events.len() < count {
        let event = harness.next_event().await;
        if event["type"].as_str().unwrap().starts_with("task.") {
            events.push(event);
        }
    }
    events
}

#[tokio::test]
async fn task_started_carries_model_and_effort_and_subagent_snapshots_refine_the_model() {
    let mut harness = Harness::default();
    harness
        .start(json!({"modelSelection": {"instanceId": "claudeAgent", "model": CAPABLE, "options": [{"id": "effort", "value": "max"}]}}))
        .await;
    harness.send(json!({"input": "spawn an agent"})).await;
    harness.emit(json!({"type": "system", "subtype": "task_started", "task_id": "task-model", "description": "Agent M", "task_type": "local_agent", "tool_use_id": "toolu_agent_m", "uuid": "task-model-uuid", "session_id": "sdk-session"}));
    harness.emit(json!({"type": "assistant", "parent_tool_use_id": "toolu_agent_m", "message": {"model": SUBAGENT_MODEL, "content": []}, "uuid": "subagent-snapshot-uuid", "session_id": "sdk-session"}));
    harness.emit(json!({"type": "system", "subtype": "task_progress", "task_id": "task-model", "description": "Agent M", "usage": {"total_tokens": 100, "tool_uses": 1, "duration_ms": 10}, "uuid": "task-model-progress-uuid", "session_id": "sdk-session"}));
    let events = task_events(&mut harness, 2).await;
    assert_eq!(events[0]["type"], json!("task.started"));
    assert_eq!(events[0]["payload"]["model"], json!(CAPABLE));
    assert_eq!(events[0]["payload"]["effort"], json!("max"));
    assert_eq!(events[1]["type"], json!("task.progress"));
    assert_eq!(events[1]["payload"]["model"], json!(SUBAGENT_MODEL));
    assert_eq!(events[1]["payload"]["effort"], json!("max"));
}

#[tokio::test]
async fn a_subagent_snapshot_that_beats_task_started_still_wins_over_the_seed() {
    let mut harness = Harness::default();
    harness
        .start(json!({"modelSelection": {"instanceId": "claudeAgent", "model": CAPABLE, "options": [{"id": "effort", "value": "max"}]}}))
        .await;
    harness.send(json!({"input": "spawn an agent"})).await;
    harness.emit(json!({"type": "assistant", "parent_tool_use_id": "toolu_agent_early", "message": {"model": SUBAGENT_MODEL, "content": []}, "uuid": "early-snapshot-uuid", "session_id": "sdk-session"}));
    harness.emit(json!({"type": "system", "subtype": "task_started", "task_id": "task-early", "description": "Agent E", "task_type": "local_agent", "tool_use_id": "toolu_agent_early", "uuid": "task-early-uuid", "session_id": "sdk-session"}));
    harness.emit(json!({"type": "system", "subtype": "task_progress", "task_id": "task-early", "description": "Agent E", "usage": {"total_tokens": 100, "tool_uses": 1, "duration_ms": 10}, "uuid": "task-early-progress-uuid", "session_id": "sdk-session"}));
    let events = task_events(&mut harness, 2).await;
    assert_eq!(events[0]["payload"]["model"], json!(SUBAGENT_MODEL));
    assert_eq!(events[0]["payload"]["effort"], json!("max"));
    assert_eq!(events[1]["payload"]["model"], json!(SUBAGENT_MODEL));
}

#[tokio::test]
async fn closes_the_session_when_the_claude_stream_aborts_after_a_turn_starts() {
    let mut harness = Harness::default();
    harness.start(json!({})).await;
    let turn = harness.send(json!({"input": "hello"})).await;
    harness.query().fail("All fibers interrupted without error");
    let events = harness.take_until("session.exited").await;
    assert_eq!(
        types(&events),
        vec![
            "session.started",
            "session.configured",
            "session.state.changed",
            "turn.started",
            "turn.completed",
            "session.exited"
        ]
    );
    let completed = &events[4];
    assert_eq!(completed["turnId"], json!(turn.turn_id.as_str()));
    assert_eq!(completed["payload"]["state"], json!("interrupted"));
    assert_eq!(completed["payload"]["errorMessage"], json!("Claude runtime interrupted."));
    assert_eq!(
        completed["payload"]["tokenUsage"],
        json!({"usageStatus": "unavailable", "usageScope": "main_agent", "hasSubagents": false})
    );
    assert!(!harness.adapter.has_session(&THREAD_ID.into()).await);
    assert!(harness.adapter.list_sessions().await.is_empty());
    assert_eq!(harness.query().close_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn keeps_claude_stream_failure_events_structural() {
    let mut harness = Harness::default();
    harness.start(json!({})).await;
    harness.send(json!({"input": "hello"})).await;
    harness.query().fail("credential material that must stay in the cause chain");
    let events = harness.take_until("session.exited").await;
    let error = first_of(&events, "runtime.error");
    assert_eq!(error["payload"]["message"], json!("Claude runtime stream failed."));
    assert_eq!(
        error["payload"]["detail"],
        json!({"failureCount": 1, "failureTags": ["ProviderAdapterProcessError"]})
    );
    let completed = first_of(&events, "turn.completed");
    assert_eq!(completed["payload"]["state"], json!("failed"));
    assert_eq!(completed["payload"]["errorMessage"], json!("Claude runtime stream failed."));
    assert!(!events.iter().any(|e| e.to_string().contains("credential material")));
}

#[tokio::test]
async fn closes_the_previous_session_before_replacing_an_existing_thread_session() {
    let mut harness = Harness::default();
    let first = harness.start(json!({})).await;
    let first_query = harness.query();
    let second = harness.start(json!({"resumeCursor": first.resume_cursor})).await;
    let events = harness.take(6).await;
    assert_eq!(harness.factory.count(), 2);
    assert_eq!(first_query.close_calls.load(Ordering::SeqCst), 1);
    assert_eq!(harness.query().close_calls.load(Ordering::SeqCst), 0);
    assert!(harness.adapter.has_session(&THREAD_ID.into()).await);
    let sessions = harness.adapter.list_sessions().await;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].resume_cursor, second.resume_cursor);
    assert_eq!(
        types(&events),
        vec![
            "session.started",
            "session.configured",
            "session.state.changed",
            "session.started",
            "session.configured",
            "session.state.changed"
        ]
    );
    assert!(harness.drain(Duration::from_millis(50)).await.iter().all(|e| e["type"] != "session.exited"));
    // The resumed session keeps the first session's Claude id.
    assert_eq!(
        harness.factory.last().options.resume,
        first.resume_cursor.as_ref().and_then(|c| c["resume"].as_str()).map(str::to_string)
    );
    assert_eq!(harness.factory.last().options.session_id, None);
}

#[tokio::test]
async fn stop_session_ends_the_prompt_stream_without_an_error() {
    let mut harness = Harness::default();
    harness.start(json!({})).await;
    let created = harness.factory.last();
    harness.adapter.stop_session(&THREAD_ID.into()).await.unwrap();
    let next = tokio::time::timeout(Duration::from_secs(1), created.prompt.lock().await.recv())
        .await
        .expect("the prompt ends");
    assert!(next.is_none());
    let events = harness.take_until("session.exited").await;
    assert_eq!(events.last().unwrap()["payload"], json!({"reason": "Session stopped", "exitKind": "graceful"}));
}

#[tokio::test]
async fn forwards_claude_task_progress_summaries_for_subagent_updates() {
    let mut harness = Harness::default();
    harness.start(json!({})).await;
    harness.emit(
        json!({"type": "system", "subtype": "task_progress", "task_id": "task-subagent-1", "description": "Running background teammate",
        "summary": "Code reviewer checked the migration edge cases.", "usage": {"total_tokens": 123, "tool_uses": 4, "duration_ms": 987},
        "session_id": "sdk-session-task-summary", "uuid": "task-progress-1"}),
    );
    let events = harness.take(6).await;
    let progress = first_of(&events, "task.progress");
    assert_eq!(progress["payload"]["summary"], json!("Code reviewer checked the migration edge cases."));
    assert_eq!(progress["payload"]["description"], json!("Running background teammate"));
    assert_eq!(progress["payload"]["typedUsage"], json!({"totalTokens": 123, "toolUses": 4, "durationMs": 987}));
}

#[tokio::test]
async fn consumes_undeclared_and_ux_internal_system_subtypes_without_warning_rows() {
    let mut harness = Harness::default();
    harness.start(json!({})).await;
    for message in [
        json!({"type": "system", "subtype": "background_tasks_changed", "tasks": [{"task_id": "t1", "task_type": "local_agent", "description": "Say hi"}], "session_id": "session", "uuid": "roster"}),
        json!({"type": "system", "subtype": "vcs_state_changed", "kind": "push", "cwd": "/tmp/worktree", "session_id": "session", "uuid": "vcs"}),
        json!({"type": "system", "subtype": "code_change_published", "provider": "github", "url": "https://github.com/example/repo/pull/1", "repo": "example/repo", "identifier": "1", "session_id": "session", "uuid": "ccp"}),
        json!({"type": "system", "subtype": "task_updated", "task_id": "t1", "patch": {"status": "running"}, "session_id": "session", "uuid": "tu"}),
        json!({"type": "system", "subtype": "commands_changed", "session_id": "session", "uuid": "cc"}),
        json!({"type": "system", "subtype": "local_command_output", "session_id": "session", "uuid": "lco"}),
        json!({"type": "system", "subtype": "plugin_install", "session_id": "session", "uuid": "pi"}),
        json!({"type": "system", "subtype": "memory_recall", "session_id": "session", "uuid": "mr"}),
        json!({"type": "system", "subtype": "elicitation_complete", "session_id": "session", "uuid": "ec"}),
        json!({"type": "system", "subtype": "control_request_progress", "request_id": "ctrl-1", "status": "started", "session_id": "session", "uuid": "crp"}),
        json!({"type": "system", "subtype": "worker_shutting_down", "reason": "host_exit", "session_id": "session", "uuid": "wsd"}),
        json!({"type": "system", "subtype": "informational", "content": "Loaded 3 skills", "level": "notice", "session_id": "session", "uuid": "info"}),
        json!({"type": "prompt_suggestion", "suggestion": "try this", "session_id": "session", "uuid": "ps"}),
        json!({"type": "conversation_reset", "new_conversation_id": "conv-2", "session_id": "session", "uuid": "cr"}),
        json!({"type": "system", "subtype": "notification", "key": "context", "text": "low priority note", "priority": "low", "session_id": "session", "uuid": "notif"}),
        json!({"type": "system", "subtype": "model_refusal_fallback", "trigger": "refusal", "direction": "retry", "original_model": "claude-fable-5", "fallback_model": "claude-opus-4-8",
            "request_id": "req_test", "api_refusal_category": "cyber", "api_refusal_explanation": null, "content": "Safeguards flagged this message. Switched to Opus 4.8.", "session_id": "session", "uuid": "mrf"}),
        json!({"type": "system", "subtype": "notification", "key": "limit", "text": "context window nearly full", "priority": "high", "session_id": "session", "uuid": "notif-high"}),
        json!({"type": "system", "subtype": "informational", "content": "Stop hook prevented continuation", "level": "warning", "prevent_continuation": true, "session_id": "session", "uuid": "info-warn"}),
        json!({"type": "system", "subtype": "model_refusal_no_fallback", "original_model": "claude-opus-5", "request_id": null, "api_refusal_explanation": "The request was declined by the API.", "content": "Model refused", "session_id": "session", "uuid": "mrnf"}),
        json!({"type": "system", "subtype": "session_state_changed", "state": "running", "session_id": "session", "uuid": "ssc-run"}),
        json!({"type": "system", "subtype": "session_state_changed", "state": "requires_action", "session_id": "session", "uuid": "ssc-req"}),
        json!({"type": "system", "subtype": "session_state_changed", "state": "idle", "session_id": "session", "uuid": "ssc-idle"}),
        json!({"type": "system", "subtype": "api_retry", "attempt": 3, "max_retries": 10, "retry_delay_ms": 1000, "error_status": 502, "error": {"type": "api_error"}, "session_id": "session", "uuid": "retry"}),
    ] {
        harness.emit(message);
    }
    let mut events = Vec::new();
    loop {
        let event = harness.next_event().await;
        let done = event["type"] == "session.state.changed" && event["payload"]["reason"] == "api_retry:3/10";
        events.push(event);
        if done {
            break;
        }
    }
    let warnings: Vec<&Value> = of_type(&events, "runtime.warning").into_iter().map(|e| &e["payload"]["message"]).collect();
    assert_eq!(
        warnings,
        vec![
            &json!("Safeguards flagged this message. Switched to Opus 4.8."),
            &json!("context window nearly full"),
            &json!("Stop hook prevented continuation"),
            &json!("The request was declined by the API.")
        ]
    );
    let states: Vec<String> = events
        .iter()
        .filter(|e| e["type"] == "session.state.changed" && e["payload"]["reason"].as_str().is_some_and(|r| r.contains("session_state")))
        .map(|e| format!("{}:{}", e["payload"]["state"].as_str().unwrap(), e["payload"]["reason"].as_str().unwrap()))
        .collect();
    assert_eq!(
        states,
        vec![
            "running:session_state:running",
            "waiting:session_state:requires_action",
            "ready:session_state:idle"
        ]
    );
    assert_eq!(first_of(&events, "task.updated")["payload"], json!({"taskId": "t1", "status": "running"}));
}

fn rejected(kind: &str, resets_at: Option<i64>, uuid: &str) -> Value {
    let mut info = json!({"status": "rejected", "rateLimitType": kind});
    if let Some(resets) = resets_at {
        info["resetsAt"] = json!(resets);
    }
    json!({"type": "rate_limit_event", "rate_limit_info": info, "session_id": "sdk-session-limit", "uuid": uuid})
}

/// Push an `api_retry` heartbeat and collect everything up to it.
async fn drain_sdk(harness: &mut Harness, collected: &mut Vec<Value>) {
    harness.emit(json!({"type": "system", "subtype": "api_retry", "attempt": 1, "max_retries": 1, "retry_delay_ms": 0, "error_status": 429, "error": {"type": "rate_limit_error"}, "session_id": "sdk-session-limit", "uuid": "usage-limit-drain"}));
    loop {
        let event = harness.next_event().await;
        let done = event["type"] == "session.state.changed" && event["payload"]["reason"] == "api_retry:1/1";
        collected.push(event);
        if done {
            return;
        }
    }
}

fn warning_messages(events: &[Value]) -> Vec<String> {
    of_type(events, "runtime.warning")
        .into_iter()
        .map(|e| e["payload"]["message"].as_str().unwrap().to_string())
        .collect()
}

const NOW_S: i64 = 1_772_323_200;

#[tokio::test]
async fn surfaces_a_rejected_claude_usage_limit_once_per_turn() {
    let mut harness = Harness::default();
    let mut events = Vec::new();
    harness.start(json!({})).await;
    harness.send(json!({"input": "hello"})).await;
    let info = json!({"status": "rejected", "rateLimitType": "five_hour", "utilization": 1, "resetsAt": NOW_S + 4 * 3600 + 90});
    let message = json!({"type": "rate_limit_event", "rate_limit_info": info, "session_id": "sdk-session-limit", "uuid": "rate-limit-rejected"});
    harness.emit(message.clone());
    drain_sdk(&mut harness, &mut events).await;
    harness.clock.0.fetch_add(5 * 60 * 1000, Ordering::SeqCst);
    harness.emit(message.clone());
    drain_sdk(&mut harness, &mut events).await;
    let rows = warning_messages(&events);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0],
        "Claude usage limit reached. This turn is paused until the 5-hour limit resets in 4h 2m."
    );
    assert_eq!(first_of(&events, "runtime.warning")["payload"]["detail"], info);
    assert_eq!(of_type(&events, "account.rate-limits.updated").len(), 2);
    let mut drift = message.clone();
    drift["rate_limit_info"]["utilization"] = json!(0.99);
    harness.emit(drift);
    drain_sdk(&mut harness, &mut events).await;
    assert_eq!(warning_messages(&events).len(), 1);
    harness.emit(result_success("sdk-session-limit", "result-limit"));
    drain_sdk(&mut harness, &mut events).await;
    harness.send(json!({"input": "retry"})).await;
    harness.emit(message);
    drain_sdk(&mut harness, &mut events).await;
    assert_eq!(warning_messages(&events).len(), 2);
}

#[tokio::test]
async fn keeps_allowed_and_malformed_claude_rate_limit_events_out_of_the_work_log() {
    let mut harness = Harness::default();
    let mut events = Vec::new();
    harness.start(json!({})).await;
    harness.send(json!({"input": "hello"})).await;
    harness.emit(json!({"type": "rate_limit_event", "rate_limit_info": {"status": "allowed", "rateLimitType": "five_hour", "utilization": 0.4}, "session_id": "s", "uuid": "rl-1"}));
    harness.emit(json!({"type": "rate_limit_event", "rate_limit_info": {"status": "allowed_warning", "rateLimitType": "five_hour", "utilization": 0.9}, "session_id": "s", "uuid": "rl-2"}));
    harness.emit(json!({"type": "rate_limit_event", "session_id": "s", "uuid": "rl-malformed"}));
    drain_sdk(&mut harness, &mut events).await;
    assert!(warning_messages(&events).is_empty());
    assert_eq!(of_type(&events, "account.rate-limits.updated").len(), 2);
}

#[tokio::test]
async fn stays_quiet_when_no_turn_is_parked_by_the_claude_limit() {
    let mut harness = Harness::default();
    let mut events = Vec::new();
    harness.start(json!({})).await;
    let resets_at = NOW_S + 3600;
    harness.emit(json!({"type": "rate_limit_event", "rate_limit_info": {"status": "rejected", "rateLimitType": "five_hour", "utilization": 1, "resetsAt": resets_at}, "session_id": "s", "uuid": "rl-idle"}));
    drain_sdk(&mut harness, &mut events).await;
    harness.send(json!({"input": "hello"})).await;
    for overage in [
        json!({"overageStatus": "allowed"}),
        json!({"overageStatus": "allowed_warning"}),
        json!({"isUsingOverage": true}),
        json!({"overageInUse": true}),
    ] {
        let mut info = json!({"status": "rejected", "rateLimitType": "five_hour", "resetsAt": resets_at, "utilization": 1});
        for (k, v) in overage.as_object().unwrap() {
            info[k] = v.clone();
        }
        harness.emit(json!({"type": "rate_limit_event", "rate_limit_info": info, "session_id": "s", "uuid": "rl-overage"}));
    }
    drain_sdk(&mut harness, &mut events).await;
    assert!(warning_messages(&events).is_empty());
    assert_eq!(of_type(&events, "account.rate-limits.updated").len(), 5);
}

#[tokio::test]
async fn still_surfaces_the_pause_when_overage_is_exhausted_too() {
    let mut harness = Harness::default();
    let mut events = Vec::new();
    harness.start(json!({})).await;
    harness.send(json!({"input": "hello"})).await;
    harness.emit(json!({"type": "rate_limit_event", "rate_limit_info": {"status": "rejected", "rateLimitType": "five_hour", "resetsAt": NOW_S + 3600, "overageStatus": "rejected"}, "session_id": "s", "uuid": "dual"}));
    drain_sdk(&mut harness, &mut events).await;
    assert_eq!(warning_messages(&events).len(), 1);
}

#[tokio::test]
async fn keeps_one_row_per_window_when_two_claude_limits_interleave() {
    let mut harness = Harness::default();
    let mut events = Vec::new();
    harness.start(json!({})).await;
    harness.send(json!({"input": "hello"})).await;
    for message in [
        rejected("five_hour", Some(NOW_S + 7200), "l1"),
        rejected("seven_day", Some(NOW_S + 48 * 3600), "l2"),
        rejected("five_hour", Some(NOW_S + 7200), "l3"),
    ] {
        harness.emit(message);
        drain_sdk(&mut harness, &mut events).await;
    }
    assert_eq!(
        warning_messages(&events),
        vec![
            "Claude usage limit reached. This turn is paused until the 5-hour limit resets in 2h.",
            "Claude usage limit reached. This turn is paused until the 7-day limit resets in 48h."
        ]
    );
}

#[tokio::test]
async fn re_announces_a_claude_limit_for_a_synthetic_turn() {
    let mut harness = Harness::default();
    let mut events = Vec::new();
    harness.start(json!({})).await;
    harness.send(json!({"input": "hello"})).await;
    harness.emit(rejected("five_hour", Some(NOW_S + 7200), "rl-1"));
    drain_sdk(&mut harness, &mut events).await;
    harness.emit(result_success("sdk-session-synthetic", "result-synthetic"));
    drain_sdk(&mut harness, &mut events).await;
    harness.emit(assistant(
        "sdk-session-synthetic",
        "assistant-synthetic",
        "assistant-message-synthetic",
        json!([{"type": "text", "text": "Following up"}]),
    ));
    drain_sdk(&mut harness, &mut events).await;
    harness.emit(rejected("five_hour", Some(NOW_S + 7200), "rl-2"));
    drain_sdk(&mut harness, &mut events).await;
    assert_eq!(warning_messages(&events).len(), 2);
    let synthetic = events
        .iter()
        .find(|e| e["type"] == "turn.started" && e["raw"]["method"] == "claude/synthetic-turn-start")
        .expect("a synthetic turn");
    assert_eq!(synthetic["providerRefs"]["providerTurnId"], synthetic["turnId"]);
}

#[tokio::test]
async fn drops_an_unusable_claude_reset_time_not_the_row_or_the_session() {
    let mut harness = Harness::default();
    let mut events = Vec::new();
    harness.start(json!({})).await;
    harness.send(json!({"input": "hello"})).await;
    harness.emit(rejected("five_hour", None, "rl-1"));
    harness.emit(json!({"type": "rate_limit_event", "rate_limit_info": {"status": "rejected", "rateLimitType": "seven_day", "resetsAt": 1e20}, "session_id": "s", "uuid": "rl-2"}));
    drain_sdk(&mut harness, &mut events).await;
    assert_eq!(
        warning_messages(&events),
        vec![
            "Claude usage limit reached. This turn is paused until the 5-hour limit resets.",
            "Claude usage limit reached. This turn is paused until the 7-day limit resets."
        ]
    );
    assert!(events.iter().all(|e| e["type"] != "session.exited" && e["type"] != "runtime.error"));
    harness.send(json!({"input": "still here"})).await;
}

#[tokio::test]
async fn warns_for_unmapped_claude_limits_and_names_the_probed_model_bucket() {
    let names = zc_provider_claude::usage_limits::make_scoped_limit_names();
    let mut harness = Harness::new(HarnessConfig {
        scoped_limit_names: Some(names.clone()),
        ..HarnessConfig::default()
    });
    let mut events = Vec::new();
    harness.start(json!({})).await;
    harness.send(json!({"input": "hello"})).await;
    harness.emit(rejected("seven_day_overage_included", None, "r1"));
    harness.emit(rejected("future_window", None, "r2"));
    drain_sdk(&mut harness, &mut events).await;
    assert!(of_type(&events, "account.rate-limits.updated").is_empty());
    names.write().unwrap().overage_included = Some("Model A".into());
    harness.emit(json!({"type": "rate_limit_event", "rate_limit_info": {"status": "rejected", "rateLimitType": "seven_day_overage_included", "utilization": 1, "resetsAt": NOW_S + 3600}, "session_id": "s", "uuid": "r3"}));
    drain_sdk(&mut harness, &mut events).await;
    assert_eq!(
        warning_messages(&events),
        vec![
            "Claude usage limit reached. This turn is paused until the 7-day model limit resets.",
            "Claude usage limit reached. This turn is paused until the limit resets.",
            "Claude usage limit reached. This turn is paused until the 7-day Model A limit resets in 1h."
        ]
    );
    assert_eq!(of_type(&events, "account.rate-limits.updated").len(), 1);
}

#[tokio::test]
async fn consumes_claude_command_lifecycle_notifications_silently() {
    let mut harness = Harness::default();
    let session_id = "6e81554e-5cff-4b37-8a39-f3a9051ac234";
    harness.start(json!({})).await;
    harness.emit(json!({"type": "system", "subtype": "notification", "key": "ready", "text": "command lifecycle test ready", "priority": "high", "session_id": session_id, "uuid": "ready"}));
    loop {
        let event = harness.next_event().await;
        if event["type"] == "runtime.warning" {
            break;
        }
    }
    for (state, uuid) in [("started", "command-started"), ("completed", "command-completed")] {
        harness.emit(json!({"type": "command_lifecycle", "command_uuid": "4cd8e8a3-df7a-425d-b6c9-4053abc0b8fd", "state": state, "session_id": session_id, "uuid": uuid}));
    }
    harness.emit(json!({"type": "system", "subtype": "notification", "key": "processed", "text": "command lifecycle messages processed", "priority": "high", "session_id": session_id, "uuid": "processed"}));
    let events = harness.take_until("runtime.warning").await;
    assert_eq!(types(&events), vec!["runtime.warning"]);
    assert_eq!(events[0]["payload"]["message"], json!("command lifecycle messages processed"));
}
