//! Port of `ProviderRuntimeIngestion.test.ts` (pacing, spill, approvals, runtime errors and
//! warnings, diffs, titles, token usage, compaction, tasks, user input) plus
//! `ProviderRuntimeIngestion.activity.test.ts` and `ProviderRuntimeIngestion.approval.test.ts`.

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use common::*;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::{watch, Notify};
use zc_ports::TaggedError;
use zc_reactors::ingestion::activities::runtime_event_to_activities;
use zc_reactors::RepositoryProbe;

fn session_status(thread: &Value) -> &str {
    thread["session"]["status"].as_str().unwrap_or("")
}

fn message<'a>(thread: &'a Value, id: &str) -> Option<&'a Value> {
    find(&thread["messages"], |m| s(m, "id") == id)
}

fn activity<'a>(thread: &'a Value, id: &str) -> Option<&'a Value> {
    find(&thread["activities"], |a| s(a, "id") == id)
}

fn has_kind(thread: &Value, kind: &str) -> bool {
    find(&thread["activities"], |a| s(a, "kind") == kind).is_some()
}

async fn started(h: &IngestionHarness, turn: &str) {
    h.emit(json!({"type": "turn.started", "eventId": format!("evt-started-{turn}"), "turnId": turn}));
    h.wait_for_thread(|t| session_status(t) == "running" && t["session"]["activeTurnId"] == turn)
        .await;
}

fn delta(id: &str, turn: &str, item: &str, text: &str) -> Value {
    json!({"type": "content.delta", "eventId": id, "turnId": turn, "itemId": item, "payload": {"streamKind": "assistant_text", "delta": text}})
}

fn completed(id: &str, turn: &str, item: &str) -> Value {
    json!({"type": "item.completed", "eventId": id, "turnId": turn, "itemId": item, "payload": {"itemType": "assistant_message", "status": "completed"}})
}

#[tokio::test]
async fn delivers_finished_paragraphs_while_the_rest_of_the_message_stays_buffered() {
    let h = IngestionHarness::new(Default::default()).await;
    let (turn, item) = ("turn-paragraph-flush", "item-paragraph-flush");
    started(&h, turn).await;
    let id = format!("assistant:{item}");
    h.advance_clock(1_000);
    h.emit(delta("evt-paragraph-1", turn, item, "First paragraph.\n\nSecond para"));
    let after_first = h.wait_for_thread(|t| message(t, &id).is_some()).await;
    assert_match(message(&after_first, &id).unwrap(), json!({"text": "First paragraph.\n\n", "streaming": true}));

    h.advance_clock(1_000);
    h.emit(delta("evt-paragraph-2", turn, item, "graph.\n\n```ts\nconst a = 1;\n\nconst b = 2;\n"));
    h.drain().await;
    assert_eq!(message(&h.thread().await, &id).unwrap()["text"], "First paragraph.\n\nSecond paragraph.\n\n");

    h.advance_clock(1_000);
    h.emit(delta("evt-paragraph-3", turn, item, "```\n\nTail without newline"));
    h.emit(completed("evt-paragraph-completed", turn, item));
    let done = h.wait_for_thread(|t| message(t, &id).is_some_and(|m| m["streaming"] == false)).await;
    assert_eq!(
        message(&done, &id).unwrap()["text"],
        "First paragraph.\n\nSecond paragraph.\n\n```ts\nconst a = 1;\n\nconst b = 2;\n```\n\nTail without newline"
    );
}

#[tokio::test]
async fn holds_every_paragraph_until_completion_in_turn_mode() {
    let h = IngestionHarness::new(IngestionOptions {
        server_settings: Some(json!({"responseStreamingMode": "turn"})),
        ..Default::default()
    })
    .await;
    let (turn, item) = ("turn-wait-mode", "item-wait-mode");
    h.emit_and_drain(vec![json!({"type": "turn.started", "eventId": "evt-wait-started", "turnId": turn})])
        .await;
    h.advance_clock(1_000);
    h.emit_and_drain(vec![delta("evt-wait-delta", turn, item, "First paragraph.\n\nSecond paragraph.\n\n")])
        .await;
    let id = format!("assistant:{item}");
    assert!(message(&h.thread().await, &id).is_none());
    h.emit_and_drain(vec![completed("evt-wait-completed", turn, item)]).await;
    assert_eq!(message(&h.thread().await, &id).unwrap()["text"], "First paragraph.\n\nSecond paragraph.\n\n");
}

#[tokio::test]
async fn holds_paragraphs_that_finish_inside_the_pacing_window_and_lands_them_together() {
    let h = IngestionHarness::new(Default::default()).await;
    let (turn, item) = ("turn-paced", "item-paced");
    started(&h, turn).await;
    let id = format!("assistant:{item}");
    let mut clock = 0;
    for (event, text, offset) in [
        ("evt-paced-1", "One.\n\n", 0),
        ("evt-paced-2", "Two.\n\n", 100),
        ("evt-paced-3", "Three.\n\n", 200),
    ] {
        h.advance_clock(offset - clock);
        clock = offset;
        h.emit_and_drain(vec![delta(event, turn, item, text)]).await;
    }
    assert_eq!(message(&h.thread().await, &id).unwrap()["text"], "One.\n\n");
    h.advance_clock(500 - clock);
    h.emit_and_drain(vec![delta("evt-paced-4", turn, item, "Four.\n\n")]).await;
    assert_eq!(message(&h.thread().await, &id).unwrap()["text"], "One.\n\nTwo.\n\nThree.\n\nFour.\n\n");
}

#[tokio::test]
async fn spills_oversized_buffered_deltas_and_still_finalizes_full_assistant_text() {
    let h = IngestionHarness::new(Default::default()).await;
    started(&h, "turn-buffer-spill").await;
    let oversized = "x".repeat(40_000);
    h.emit(delta("evt-message-delta-buffer-spill", "turn-buffer-spill", "item-buffer-spill", &oversized));
    h.emit(completed("evt-message-completed-buffer-spill", "turn-buffer-spill", "item-buffer-spill"));
    let thread = h
        .wait_for_thread(|t| message(t, "assistant:item-buffer-spill").is_some_and(|m| m["streaming"] == false))
        .await;
    assert_eq!(message(&thread, "assistant:item-buffer-spill").unwrap()["text"], oversized);
}

#[tokio::test]
async fn does_not_duplicate_assistant_completion_when_item_completed_is_followed_by_turn_completed() {
    let h = IngestionHarness::new(Default::default()).await;
    let turn = "turn-complete-dedup";
    started(&h, turn).await;
    h.emit(delta("evt-message-delta-for-complete-dedup", turn, "item-complete-dedup", "done"));
    h.emit(completed("evt-message-completed-for-complete-dedup", turn, "item-complete-dedup"));
    h.emit(json!({"type": "turn.completed", "eventId": "evt-turn-completed-for-complete-dedup", "turnId": turn, "payload": {"state": "completed"}}));
    h.wait_for_thread(|t| {
        session_status(t) == "ready"
            && t["session"]["activeTurnId"].is_null()
            && message(t, "assistant:item-complete-dedup").is_some_and(|m| m["streaming"] == false)
    })
    .await;
    let completions = h
        .events()
        .await
        .into_iter()
        .filter(|event| {
            event["type"] == "thread.message-sent" && event["payload"]["messageId"] == "assistant:item-complete-dedup" && event["payload"]["streaming"] == false
        })
        .count();
    assert_eq!(completions, 1);
}

#[tokio::test]
async fn maps_canonical_request_events_into_approval_activities_with_request_kind() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit(json!({"type": "request.opened", "eventId": "evt-request-opened", "requestId": "req-open", "payload": {"requestType": "command_execution_approval", "detail": "pwd"}}));
    h.emit(json!({"type": "request.resolved", "eventId": "evt-request-resolved", "requestId": "req-open", "payload": {"requestType": "command_execution_approval", "decision": "accept"}}));
    let thread = h
        .wait_for_thread(|t| has_kind(t, "approval.requested") && has_kind(t, "approval.resolved"))
        .await;
    for id in ["evt-request-opened", "evt-request-resolved"] {
        let payload = &activity(&thread, id).unwrap()["payload"];
        assert_eq!(payload["requestKind"], "command");
        assert_eq!(payload["requestType"], "command_execution_approval");
    }
}

#[tokio::test]
async fn maps_runtime_error_into_errored_session_state() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit(json!({"type": "runtime.error", "eventId": "evt-runtime-error", "turnId": "turn-3", "payload": {"message": "runtime exploded"}}));
    let thread = h
        .wait_for_thread(|t| session_status(t) == "error" && t["session"]["activeTurnId"] == "turn-3" && t["session"]["lastError"] == "runtime exploded")
        .await;
    assert_eq!(thread["session"]["lastError"], "runtime exploded");
}

#[tokio::test]
async fn records_runtime_error_activities_from_the_typed_payload_message() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit(json!({
        "type": "runtime.error", "eventId": "evt-runtime-error-activity", "turnId": "turn-runtime-error-activity",
        "payload": {"message": "runtime activity exploded", "code": "subscription_sharing_usage_limit_exceeded"},
    }));
    let thread = h.wait_for_thread(|t| activity(t, "evt-runtime-error-activity").is_some()).await;
    let activity = activity(&thread, "evt-runtime-error-activity").unwrap();
    assert_eq!(activity["kind"], "runtime.error");
    assert_eq!(activity["payload"]["message"], "runtime activity exploded");
    assert_eq!(activity["payload"]["code"], "subscription_sharing_usage_limit_exceeded");
}

#[tokio::test]
async fn keeps_the_session_running_when_a_runtime_warning_arrives_during_an_active_turn() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit(json!({"type": "turn.started", "eventId": "evt-warning-turn-started", "turnId": "turn-warning", "payload": {}}));
    h.emit(json!({"type": "runtime.warning", "eventId": "evt-warning-runtime", "turnId": "turn-warning", "payload": {"message": "Reconnecting... 2/5", "detail": {"willRetry": true}}}));
    let thread = h
        .wait_for_thread(|t| {
            session_status(t) == "running"
                && t["session"]["activeTurnId"] == "turn-warning"
                && activity(t, "evt-warning-runtime").is_some_and(|a| a["kind"] == "runtime.warning")
        })
        .await;
    assert!(thread["session"]["lastError"].is_null());
}

#[tokio::test]
async fn maps_session_thread_lifecycle_and_item_started_into_session_activity_projections() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit(json!({"type": "session.started", "eventId": "evt-session-started", "message": "session started"}));
    h.emit(json!({"type": "thread.started", "eventId": "evt-thread-started"}));
    let payload = json!({
        "itemType": "command_execution", "status": "inProgress", "title": "Command run", "toolSurface": "computer",
        "toolIcon": {"_tag": "native-app", "app": {"_tag": "app-id", "appId": "com.apple.Terminal"}},
        "toolSource": {"key": "native-app:com.apple.terminal", "name": "Terminal", "kind": "computer"},
        "detail": "Bash: vp test run", "data": {"toolName": "Bash", "input": {"command": "vp test run"}},
    });
    h.emit(json!({"type": "item.started", "eventId": "evt-tool-started", "turnId": "turn-9", "itemId": "tool-call-9", "payload": payload}));
    let thread = h
        .wait_for_thread(|t| session_status(t) == "ready" && t["session"]["activeTurnId"].is_null() && has_kind(t, "tool.started"))
        .await;
    let started = find(&thread["activities"], |a| s(a, "kind") == "tool.started").unwrap();
    let mut expected = payload;
    expected["toolCallId"] = json!("tool-call-9");
    assert_match(&started["payload"], expected);
}

/// A repository probe that blocks until released.
struct BlockingProbe {
    started: Notify,
    release: watch::Sender<bool>,
}

#[async_trait]
impl RepositoryProbe for BlockingProbe {
    async fn is_git_repository(&self, _cwd: &str) -> Result<bool, TaggedError> {
        self.started.notify_one();
        let mut receiver = self.release.subscribe();
        let _ = receiver.wait_for(|released| *released).await;
        Ok(true)
    }
}

#[tokio::test]
async fn settles_the_turn_while_repository_detection_for_a_diff_is_blocked() {
    let probe = Arc::new(BlockingProbe {
        started: Notify::new(),
        release: watch::channel(false).0,
    });
    let h = IngestionHarness::new(IngestionOptions {
        repositories: Some(probe.clone()),
        ..Default::default()
    })
    .await;
    let turn = "blocked-diff-turn";
    h.emit_and_drain(vec![json!({"type": "turn.started", "eventId": "evt-blocked-turn-start", "turnId": turn})])
        .await;
    h.emit(json!({"type": "turn.diff.updated", "eventId": "evt-blocked-diff", "turnId": turn, "payload": {"unifiedDiff": "diff --git a/file.ts b/file.ts\n+new\n"}}));
    probe.started.notified().await;

    let mut events = zc_ports::OrchestrationDispatch::subscribe_domain_events(&*h.engine);
    h.emit(json!({
        "type": "item.completed", "eventId": "evt-blocked-final-reply", "turnId": turn, "itemId": "blocked-final-reply",
        "payload": {"itemType": "assistant_message", "status": "completed", "detail": "Work finished."},
    }));
    h.emit(json!({"type": "turn.completed", "eventId": "evt-blocked-turn-completed", "turnId": turn, "payload": {"state": "failed"}}));
    // Resolves only if turn.completed is processed while detection is still blocked.
    loop {
        let event = serde_json::to_value(events.next().await.unwrap()).unwrap();
        if event["type"] == "thread.session-set" && event["payload"]["session"]["status"] == "error" {
            break;
        }
    }
    let blocked = h.thread().await;
    assert_match(&blocked["session"], json!({"status": "error", "activeTurnId": null}));
    assert_contains(&blocked["messages"], json!({"text": "Work finished."}));
    assert_eq!(blocked["checkpoints"], json!([]));

    // A newer turn starts before detection returns: the late placeholder must not move the
    // latest turn back or settle the failed turn as completed.
    h.emit(json!({"type": "turn.started", "eventId": "evt-next-turn-start", "turnId": "next-turn"}));
    loop {
        let event = serde_json::to_value(events.next().await.unwrap()).unwrap();
        if event["type"] == "thread.session-set" && event["payload"]["session"]["activeTurnId"] == "next-turn" {
            break;
        }
    }
    probe.release.send_replace(true);
    h.drain().await;
    let released = h.thread().await;
    assert_eq!(released["checkpoints"], json!([]));
    assert_match(&released["latestTurn"], json!({"turnId": "next-turn", "state": "running"}));
    assert_match(&h.read_turn(turn).await.unwrap(), json!({"state": "error", "checkpointRef": null}));
}

#[tokio::test]
async fn ignores_a_diff_for_a_missing_turn_without_moving_the_latest_turn() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit_and_drain(vec![
        json!({"type": "turn.started", "eventId": "evt-existing-turn", "turnId": "current-turn"}),
        json!({"type": "turn.diff.updated", "eventId": "evt-missing-turn-diff", "turnId": "missing-turn", "payload": {"unifiedDiff": "diff --git a/file.ts b/file.ts\n+late\n"}}),
    ])
    .await;
    let thread = h.thread().await;
    assert_eq!(thread["checkpoints"], json!([]));
    assert_match(&thread["latestTurn"], json!({"turnId": "current-turn", "state": "running"}));
    assert!(h.read_turn("missing-turn").await.is_none());
}

#[tokio::test]
async fn tracks_provider_diff_updates_from_a_nested_git_workspace() {
    let h = IngestionHarness::new(IngestionOptions {
        workspace_subdirectory: Some("apps/server".into()),
        ..Default::default()
    })
    .await;
    h.emit_and_drain(vec![
        json!({"type": "turn.started", "eventId": "evt-nested-turn-started", "turnId": "nested-turn"}),
        json!({"type": "turn.diff.updated", "eventId": "evt-nested-diff", "turnId": "nested-turn", "payload": {"unifiedDiff": "diff --git a/apps/server/file.ts b/apps/server/file.ts\n+new\n"}}),
    ])
    .await;
    assert_match(&h.thread().await["checkpoints"], json!([{"turnId": "nested-turn", "status": "missing"}]));
}

#[tokio::test]
async fn consumes_p1_runtime_events_into_thread_metadata_diff_checkpoints_and_activities() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit_and_drain(vec![json!({"type": "turn.started", "eventId": "evt-p1-turn-started", "turnId": "turn-p1"})])
        .await;
    h.emit(json!({"type": "thread.metadata.updated", "eventId": "evt-thread-metadata-updated", "payload": {"name": "Renamed by provider", "metadata": {"source": "provider"}}}));
    h.emit(json!({
        "type": "turn.plan.updated", "eventId": "evt-turn-plan-updated", "turnId": "turn-p1",
        "payload": {"explanation": "Working through the plan", "plan": [{"step": "Inspect files", "status": "completed"}, {"step": "Apply patch", "status": "in_progress"}]},
    }));
    h.emit(json!({
        "type": "item.updated", "eventId": "evt-item-updated", "turnId": "turn-p1", "itemId": "item-p1-tool",
        "payload": {"itemType": "command_execution", "status": "in_progress", "title": "Run tests", "detail": "bun test", "data": {"pid": 123}},
    }));
    h.emit(json!({"type": "runtime.warning", "eventId": "evt-runtime-warning", "turnId": "turn-p1", "payload": {"message": "Provider got slow", "detail": {"latencyMs": 1500}}}));
    h.emit(json!({"type": "turn.diff.updated", "eventId": "evt-turn-diff-updated", "turnId": "turn-p1", "itemId": "item-p1-assistant", "payload": {"unifiedDiff": "diff --git a/file.txt b/file.txt\n+hello\n"}}));
    let thread = h
        .wait_for_thread(|t| {
            t["title"] == "Thread"
                && has_kind(t, "turn.plan.updated")
                && has_kind(t, "tool.updated")
                && has_kind(t, "runtime.warning")
                && find(&t["checkpoints"], |c| s(c, "turnId") == "turn-p1").is_some()
        })
        .await;
    assert_eq!(thread["title"], "Thread");
    let plan = activity(&thread, "evt-turn-plan-updated").unwrap();
    assert_eq!(plan["kind"], "turn.plan.updated");
    assert!(plan["payload"]["plan"].is_array());
    let tool = activity(&thread, "evt-item-updated").unwrap();
    assert_eq!(tool["kind"], "tool.updated");
    assert_eq!(tool["payload"]["itemType"], "command_execution");
    assert_eq!(tool["payload"]["status"], "in_progress");
    assert_eq!(tool["payload"]["toolCallId"], "item-p1-tool");
    let warning = activity(&thread, "evt-runtime-warning").unwrap();
    assert_eq!(warning["kind"], "runtime.warning");
    assert_eq!(warning["payload"]["message"], "Provider got slow");
    let checkpoint = find(&thread["checkpoints"], |c| s(c, "turnId") == "turn-p1").unwrap();
    assert_eq!(checkpoint["status"], "missing");
    assert_eq!(checkpoint["assistantMessageId"], "assistant:item-p1-assistant");
    assert_eq!(checkpoint["checkpointRef"], "provider-diff:evt-turn-diff-updated");
}

#[tokio::test]
async fn mirrors_a_provider_title_only_while_the_thread_still_has_the_default_title() {
    let h = IngestionHarness::new(IngestionOptions {
        thread_title: Some("New thread".into()),
        ..Default::default()
    })
    .await;
    h.emit(json!({"type": "thread.metadata.updated", "eventId": "evt-thread-metadata-default", "payload": {"name": "Renamed by provider", "metadata": {"source": "provider"}}}));
    h.wait_for_thread(|t| t["title"] == "Renamed by provider").await;
}

#[tokio::test]
async fn rejects_a_provider_title_once_the_thread_has_a_real_title() {
    let h = IngestionHarness::new(IngestionOptions {
        thread_title: Some("User-set title".into()),
        ..Default::default()
    })
    .await;
    h.emit(json!({"type": "thread.metadata.updated", "eventId": "evt-thread-metadata-real", "payload": {"name": "Renamed by provider", "metadata": {"source": "provider"}}}));
    h.drain().await;
    assert_eq!(h.thread().await["title"], "User-set title");
}

async fn usage_activity(h: &IngestionHarness) -> Value {
    let thread = h.wait_for_thread(|t| has_kind(t, "context-window.updated")).await;
    find(&thread["activities"], |a| s(a, "kind") == "context-window.updated").unwrap().clone()
}

#[tokio::test]
async fn projects_context_window_updates_into_normalized_thread_activities() {
    let h = IngestionHarness::new(Default::default()).await;
    let usage = json!({
        "usedTokens": 1075, "totalProcessedTokens": 10_200, "maxTokens": 128_000, "inputTokens": 1000, "cachedInputTokens": 500,
        "outputTokens": 50, "reasoningOutputTokens": 25, "lastUsedTokens": 1075, "lastInputTokens": 1000, "lastCachedInputTokens": 500,
        "lastOutputTokens": 50, "lastReasoningOutputTokens": 25, "compactsAutomatically": true,
    });
    h.emit(json!({"type": "thread.token-usage.updated", "eventId": "evt-thread-token-usage-updated", "payload": {"usage": usage}}));
    assert_match(
        &usage_activity(&h).await["payload"],
        json!({
            "usedTokens": 1075, "totalProcessedTokens": 10_200, "maxTokens": 128_000, "inputTokens": 1000, "cachedInputTokens": 500,
            "outputTokens": 50, "reasoningOutputTokens": 25, "lastUsedTokens": 1075, "compactsAutomatically": true,
        }),
    );
}

#[tokio::test]
async fn projects_codex_camel_case_token_usage_payloads_into_normalized_thread_activities() {
    let h = IngestionHarness::new(Default::default()).await;
    let usage = json!({
        "usedTokens": 126, "totalProcessedTokens": 11_839, "maxTokens": 258_400, "inputTokens": 120, "cachedInputTokens": 0, "outputTokens": 6,
        "reasoningOutputTokens": 0, "lastUsedTokens": 126, "lastInputTokens": 120, "lastCachedInputTokens": 0, "lastOutputTokens": 6,
        "lastReasoningOutputTokens": 0, "compactsAutomatically": true,
    });
    h.emit(json!({"type": "thread.token-usage.updated", "eventId": "evt-thread-token-usage-updated-camel", "payload": {"usage": usage}}));
    assert_match(
        &usage_activity(&h).await["payload"],
        json!({
            "usedTokens": 126, "totalProcessedTokens": 11_839, "maxTokens": 258_400, "inputTokens": 120, "cachedInputTokens": 0, "outputTokens": 6,
            "reasoningOutputTokens": 0, "lastUsedTokens": 126, "lastInputTokens": 120, "lastOutputTokens": 6, "compactsAutomatically": true,
        }),
    );
}

#[tokio::test]
async fn projects_claude_usage_snapshots_with_context_window_into_normalized_thread_activities() {
    let h = IngestionHarness::new(Default::default()).await;
    let usage = json!({"usedTokens": 31_251, "lastUsedTokens": 31_251, "maxTokens": 200_000, "toolUses": 25, "durationMs": 43_567});
    h.emit(json!({
        "type": "thread.token-usage.updated", "eventId": "evt-thread-token-usage-updated-claude-window", "provider": "claudeAgent",
        "payload": {"usage": usage}, "raw": {"source": "claude.sdk.message", "method": "claude/result/success", "payload": {}},
    }));
    assert_match(&usage_activity(&h).await["payload"], usage);
}

#[tokio::test]
async fn projects_compacted_thread_state_into_context_compaction_activities() {
    let h = IngestionHarness::new(Default::default()).await;
    h.dispatch(turn_start("cmd-thread-compact", "message-compact", "/compact", NOW)).await;
    h.emit(
        json!({"type": "session.state.changed", "eventId": "evt-session-starting-compact", "providerInstanceId": "codex", "payload": {"state": "starting"}}),
    );
    h.wait_for_thread(|t| session_status(t) == "starting").await;
    for (index, used) in [899_000, 0].iter().enumerate() {
        h.emit(json!({"type": "thread.token-usage.updated", "eventId": format!("evt-thread-token-usage-{index}"), "payload": {"usage": {"usedTokens": used}}}));
    }
    h.wait_for_thread(|t| filter(&t["activities"], |a| s(a, "kind") == "context-window.updated").len() == 2)
        .await;
    h.emit(json!({
        "type": "thread.state.changed", "eventId": "evt-thread-compacted", "providerInstanceId": "codex", "turnId": "turn-1",
        "payload": {"state": "compacted", "detail": {"source": "provider"}},
    }));
    let thread = h.wait_for_thread(|t| activity(t, "evt-thread-compacted").is_some()).await;
    let compaction = activity(&thread, "evt-thread-compacted").unwrap();
    assert_eq!(compaction["summary"], "Compacted context 899K → 0 tokens");
    assert_eq!(compaction["tone"], "info");
    assert_match(&compaction["payload"], json!({"requestId": "message-compact"}));
}

#[tokio::test]
async fn projects_codex_task_lifecycle_chunks_into_thread_activities() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit(json!({"type": "task.started", "eventId": "evt-task-started", "turnId": "turn-task-1", "payload": {"taskId": "turn-task-1", "taskType": "plan"}}));
    h.emit(json!({
        "type": "task.progress", "eventId": "evt-task-progress", "turnId": "turn-task-1",
        "payload": {"taskId": "turn-task-1", "description": "Comparing the desktop rollout chunks to the app-server stream.", "summary": "Code reviewer is validating the desktop rollout chunks."},
    }));
    h.emit(json!({
        "type": "task.completed", "eventId": "evt-task-completed", "turnId": "turn-task-1",
        "payload": {"taskId": "turn-task-1", "status": "completed", "summary": "<proposed_plan>\n# Plan title\n</proposed_plan>"},
    }));
    h.emit(json!({"type": "turn.proposed.completed", "eventId": "evt-task-proposed-plan-completed", "turnId": "turn-task-1", "payload": {"planMarkdown": "# Plan title"}}));
    let thread = h
        .wait_for_thread(|t| has_kind(t, "task.completed") && find(&t["proposedPlans"], |p| s(p, "id") == "plan:thread-1:turn:turn-task-1").is_some())
        .await;
    let started = activity(&thread, "evt-task-started").unwrap();
    assert_eq!(started["kind"], "task.started");
    assert_eq!(started["summary"], "Plan task started");
    let progress = activity(&thread, "task-progress:thread-1:turn-task-1").unwrap();
    assert_eq!(progress["kind"], "task.progress");
    assert_eq!(progress["payload"]["detail"], "Code reviewer is validating the desktop rollout chunks.");
    assert_eq!(progress["payload"]["summary"], "Code reviewer is validating the desktop rollout chunks.");
    let completed = activity(&thread, "evt-task-completed").unwrap();
    assert_eq!(completed["kind"], "task.completed");
    assert_eq!(completed["payload"]["detail"], "<proposed_plan>\n# Plan title\n</proposed_plan>");
    let plan = find(&thread["proposedPlans"], |p| s(p, "id") == "plan:thread-1:turn:turn-task-1").unwrap();
    assert_eq!(plan["planMarkdown"], "# Plan title");
}

#[tokio::test]
async fn titles_task_activities_with_the_task_description_including_on_completion() {
    let h = IngestionHarness::new(Default::default()).await;
    let base =
        |kind: &str, id: &str, payload: Value| json!({"type": kind, "eventId": id, "provider": "claudeAgent", "turnId": "turn-named-task", "payload": payload});
    h.emit(base(
        "task.started",
        "evt-named-task-started",
        json!({"taskId": "named-task-1", "description": "Typecheck mobile app", "taskType": "local_bash"}),
    ));
    h.emit(base(
        "task.progress",
        "evt-named-task-progress",
        json!({"taskId": "named-task-1", "description": "Typecheck mobile app", "summary": "Running tsc across the mobile workspace."}),
    ));
    h.emit(base(
        "task.completed",
        "evt-named-task-completed",
        json!({"taskId": "named-task-1", "status": "completed", "summary": "Typecheck finished without errors."}),
    ));
    let thread = h.wait_for_thread(|t| activity(t, "evt-named-task-completed").is_some()).await;
    let progress = activity(&thread, "task-progress:thread-1:named-task-1").unwrap();
    assert_eq!(progress["summary"], "Typecheck mobile app");
    assert_eq!(progress["payload"]["title"], "Typecheck mobile app");
    let completed = activity(&thread, "evt-named-task-completed").unwrap();
    assert_eq!(completed["summary"], "Task completed");
    assert_eq!(completed["payload"]["title"], "Typecheck mobile app");
    assert_eq!(completed["payload"]["summary"], "Typecheck finished without errors.");
    assert_eq!(completed["payload"]["detail"], "Typecheck finished without errors.");
}

#[tokio::test]
async fn titles_task_completion_from_task_started_when_no_progress_event_carried_the_name() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit(json!({
        "type": "task.started", "eventId": "evt-fast-task-started", "provider": "claudeAgent", "turnId": "turn-fast-task",
        "payload": {"taskId": "fast-task-1", "description": "wait for codex review to finish", "taskType": "local_bash"},
    }));
    h.emit(json!({"type": "task.completed", "eventId": "evt-fast-task-completed", "provider": "claudeAgent", "turnId": "turn-fast-task", "payload": {"taskId": "fast-task-1", "status": "completed"}}));
    let thread = h.wait_for_thread(|t| activity(t, "evt-fast-task-completed").is_some()).await;
    assert_eq!(
        activity(&thread, "evt-fast-task-completed").unwrap()["payload"]["title"],
        "wait for codex review to finish"
    );
}

#[tokio::test]
async fn recovers_a_task_title_past_untitled_progress_after_the_cache_is_swept() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit_and_drain(vec![json!({
        "type": "task.started", "eventId": "evt-swept-task-started", "provider": "claudeAgent", "turnId": "turn-swept-task",
        "payload": {"taskId": "swept-task-1", "description": "Watch round-3 CI and bots"},
    })])
    .await;
    h.dispatch(json!({
        "type": "thread.activity.append", "commandId": "cmd-swept-task-progress", "threadId": "thread-1",
        "activity": {
            "id": "evt-swept-task-progress", "kind": "task.progress", "tone": "info", "summary": "Polling CI checks.",
            "payload": {"taskId": "swept-task-1"}, "turnId": "turn-swept-task", "createdAt": "2026-01-01T00:00:01.000Z",
        },
        "createdAt": "2026-01-01T00:00:01.000Z",
    }))
    .await;
    h.emit_and_drain(vec![
        json!({"type": "session.exited", "eventId": "evt-swept-task-session-exited", "provider": "claudeAgent", "createdAt": "2026-01-01T00:00:02.000Z", "payload": {}}),
        json!({
            "type": "task.completed", "eventId": "evt-swept-task-completed", "provider": "claudeAgent", "createdAt": "2026-01-01T00:00:03.000Z",
            "turnId": "turn-swept-task", "payload": {"taskId": "swept-task-1", "status": "completed", "summary": "CI is green."},
        }),
    ])
    .await;
    let thread = h.thread().await;
    assert_match(
        &activity(&thread, "evt-swept-task-completed").unwrap()["payload"],
        json!({"title": "Watch round-3 CI and bots"}),
    );
}

#[tokio::test]
async fn projects_structured_user_input_request_and_resolution_as_thread_activities() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit(json!({
        "type": "user-input.requested", "eventId": "evt-user-input-requested", "turnId": "turn-user-input", "requestId": "req-user-input-1",
        "payload": {"questions": [{"id": "sandbox_mode", "header": "Sandbox", "question": "Which mode should be used?", "options": [{"label": "workspace-write", "description": "Allow workspace writes only"}]}]},
    }));
    h.emit(json!({
        "type": "user-input.resolved", "eventId": "evt-user-input-resolved", "turnId": "turn-user-input", "requestId": "req-user-input-1",
        "payload": {"answers": {"sandbox_mode": "workspace-write"}},
    }));
    let thread = h
        .wait_for_thread(|t| has_kind(t, "user-input.requested") && has_kind(t, "user-input.resolved"))
        .await;
    assert_eq!(activity(&thread, "evt-user-input-requested").unwrap()["kind"], "user-input.requested");
    let resolved = activity(&thread, "evt-user-input-resolved").unwrap();
    assert_eq!(resolved["kind"], "user-input.resolved");
    assert_eq!(resolved["payload"]["answers"], json!({"sandbox_mode": "workspace-write"}));
}

#[tokio::test]
async fn continues_processing_runtime_events_after_a_single_event_handler_failure() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit(json!({"type": "content.delta", "eventId": "evt-invalid-delta", "turnId": "turn-invalid", "itemId": "item-invalid", "payload": {"streamKind": "assistant_text", "delta": null}}));
    h.emit(json!({"type": "runtime.error", "eventId": "evt-runtime-error-after-failure", "turnId": "turn-after-failure", "payload": {"message": "runtime still processed"}}));
    let thread = h
        .wait_for_thread(|t| {
            session_status(t) == "error" && t["session"]["activeTurnId"] == "turn-after-failure" && t["session"]["lastError"] == "runtime still processed"
        })
        .await;
    assert_eq!(thread["session"]["lastError"], "runtime still processed");
}

// ---------------------------------------------------------------------------------------------
// ProviderRuntimeIngestion.activity.test.ts

fn base_event(kind: &str, id: &str, payload: Value) -> Value {
    json!({"provider": "codex", "createdAt": "2026-08-06T00:00:00.000Z", "threadId": "thread-1", "type": kind, "eventId": id, "payload": payload})
}

fn ids(activities: &[Value]) -> Vec<&str> {
    activities.iter().map(|activity| s(activity, "id")).collect()
}

#[test]
fn persists_usage_independently_from_replaceable_activity() {
    let usage_only = base_event(
        "task.progress",
        "evt-usage",
        json!({"taskId": "agent-1", "description": "Agent one", "typedUsage": {"totalTokens": 73_700_000}}),
    );
    let command = base_event(
        "task.progress",
        "evt-command",
        json!({"taskId": "agent-1", "description": "Agent one", "summary": "Running tests", "lastToolName": "exec_command"}),
    );
    let usage_activities = runtime_event_to_activities(&usage_only, None);
    let command_activities = runtime_event_to_activities(&command, None);
    assert_eq!(ids(&usage_activities), ["task-usage:thread-1:agent-1"]);
    assert_eq!(ids(&command_activities), ["task-progress:thread-1:agent-1"]);
    assert_eq!(usage_activities[0]["payload"]["typedUsage"], json!({"totalTokens": 73_700_000}));
    assert_eq!(usage_activities[0]["payload"]["usageSnapshot"], true);
}

#[test]
fn splits_combined_progress_and_usage_into_their_independent_snapshots() {
    let event = base_event(
        "task.progress",
        "evt-combined",
        json!({"taskId": "agent-2", "description": "Agent two", "summary": "Inspecting the panel", "typedUsage": {"totalTokens": 4_200, "toolUses": 7}, "status": "running"}),
    );
    let activities = runtime_event_to_activities(&event, None);
    assert_eq!(ids(&activities), ["task-progress:thread-1:agent-2", "task-usage:thread-1:agent-2"]);
    let progress = &activities[0]["payload"];
    let usage = &activities[1]["payload"];
    assert_eq!(progress["summary"], "Inspecting the panel");
    assert_eq!(progress["status"], "running");
    assert!(progress.get("typedUsage").is_none());
    assert_eq!(usage["typedUsage"], json!({"totalTokens": 4_200, "toolUses": 7}));
    assert_eq!(usage["usageSnapshot"], true);
    assert!(usage.get("status").is_none());
}

fn streaming_data() -> (String, Value) {
    let mut lines = vec!["first line of output".to_owned()];
    lines.extend((0..500).map(|index| format!("Capturing frame {index}/9028")));
    let stdout = lines.join("\n");
    let data = json!({
        "toolCallId": "tool-call-1", "kind": "execute", "command": "blender --render", "rawOutput": {"stdout": stdout},
        "content": [{"type": "content", "content": {"type": "text", "text": stdout}}],
    });
    (stdout, data)
}

#[test]
fn persists_tool_updated_with_the_wire_projection_of_data_not_the_accumulated_stream() {
    let (stdout, data) = streaming_data();
    let event = base_event(
        "item.updated",
        "evt-tool-streaming-updated",
        json!({"itemType": "command_execution", "status": "inProgress", "title": "Render", "detail": stdout, "data": data}),
    );
    let activities = runtime_event_to_activities(&event, None);
    assert_eq!(activities.len(), 1);
    let payload = &activities[0]["payload"];
    assert_eq!(payload["status"], "inProgress");
    assert_eq!(payload["data"]["toolCallId"], "tool-call-1");
    assert_eq!(payload["data"]["command"], "blender --render");
    assert_eq!(payload["data"]["rawOutput"], json!({"content": "first line of output"}));
    assert!(payload["data"].get("content").is_none());
    assert!(payload["data"].to_string().len() < 1_000);
}

#[test]
fn persists_the_full_terminal_payload_on_tool_completed() {
    let (_, data) = streaming_data();
    let event = base_event(
        "item.completed",
        "evt-tool-streaming-completed",
        json!({"itemType": "command_execution", "status": "completed", "title": "Render", "data": data}),
    );
    let activities = runtime_event_to_activities(&event, None);
    assert_eq!(activities.len(), 1);
    assert_eq!(activities[0]["payload"]["data"], data);
}

// ---------------------------------------------------------------------------------------------
// ProviderRuntimeIngestion.approval.test.ts

#[test]
fn preserves_complete_multiline_command_details() {
    let detail = format!("bun run release -- {}\nsecond line", "long-argument ".repeat(20));
    let mut event = base_event(
        "request.opened",
        "evt-request-opened",
        json!({"requestType": "command_execution_approval", "detail": detail}),
    );
    event["requestId"] = json!("approval-1");
    let activities = runtime_event_to_activities(&event, None);
    assert_eq!(activities[0]["kind"], "approval.requested");
    assert_eq!(activities[0]["payload"]["detail"], detail);
}

#[test]
fn keeps_app_details_and_approval_options_available_to_remote_clients() {
    let options = json!([
        {"decision": "decline", "label": "Decline"},
        {"decision": "acceptAlways", "label": "Always allow Safari"},
        {"decision": "accept", "label": "Approve"},
    ]);
    let mut event = base_event(
        "request.opened",
        "evt-mcp-elicitation",
        json!({"requestType": "mcp_elicitation_approval", "detail": "Allow ChatGPT to use Safari?", "appName": "Safari", "options": options}),
    );
    event["requestId"] = json!("approval-safari");
    let activities = runtime_event_to_activities(&event, None);
    assert_match(
        &activities[0],
        json!({
            "kind": "approval.requested",
            "summary": "App access approval requested",
            "payload": {
                "requestId": "approval-safari", "requestKind": "mcp-elicitation", "requestType": "mcp_elicitation_approval",
                "detail": "Allow ChatGPT to use Safari?", "appName": "Safari", "options": options,
            },
        }),
    );
}
