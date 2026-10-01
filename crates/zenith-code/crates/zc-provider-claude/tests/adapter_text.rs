//! `ClaudeAdapter.test.ts`, part 4: token usage snapshots and assistant text segmentation.

mod support;

use serde_json::{json, Value};
use support::*;

#[tokio::test]
async fn emits_thread_token_usage_updates_from_claude_task_progress() {
    let mut harness = Harness::default();
    harness.start(json!({})).await;
    harness.emit(
        json!({"type": "system", "subtype": "task_progress", "task_id": "task-usage-1", "description": "Thinking through the patch",
        "usage": {"total_tokens": 321, "tool_uses": 2, "duration_ms": 654}, "session_id": "sdk-session-task-usage", "uuid": "task-usage-progress-1"}),
    );
    let events = harness.take(6).await;
    let usage = first_of(&events, "thread.token-usage.updated");
    let progress = first_of(&events, "task.progress");
    assert_eq!(
        usage["payload"],
        json!({"usage": {"usedTokens": 321, "lastUsedTokens": 321, "toolUses": 2, "durationMs": 654}})
    );
    assert_ne!(usage["eventId"], progress["eventId"]);
}

async fn result_usage(result: Value, before: Vec<Value>) -> Vec<Value> {
    let mut harness = Harness::default();
    harness.start(json!({})).await;
    harness.send(json!({"input": "hello"})).await;
    for message in before {
        harness.emit(message);
    }
    harness.emit(result);
    harness.query().finish();
    harness.take_until("session.exited").await
}

#[tokio::test]
async fn emits_claude_context_window_on_result_completion_usage_snapshots() {
    let events = result_usage(
        json!({"type": "result", "subtype": "success", "is_error": false, "num_turns": 1, "result": "done", "stop_reason": "end_turn", "session_id": "s",
            "usage": {"input_tokens": 4, "cache_creation_input_tokens": 2715, "cache_read_input_tokens": 21144, "output_tokens": 679},
            "modelUsage": {CAPABLE: {"contextWindow": 200000, "maxOutputTokens": 64000}}}),
        vec![],
    )
    .await;
    assert_eq!(
        first_of(&events, "thread.token-usage.updated")["payload"],
        json!({"usage": {"usedTokens": 24542, "lastUsedTokens": 24542, "inputTokens": 23863, "outputTokens": 679, "maxTokens": 200000}})
    );
}

#[tokio::test]
async fn clamps_oversized_claude_usage_to_the_reported_context_window() {
    let events = result_usage(
        json!({"type": "result", "subtype": "success", "is_error": false, "num_turns": 1, "result": "done", "stop_reason": "end_turn", "session_id": "s",
            "usage": {"total_tokens": 535000}, "modelUsage": {CAPABLE: {"contextWindow": 200000, "maxOutputTokens": 64000}}}),
        vec![],
    )
    .await;
    assert_eq!(
        first_of(&events, "thread.token-usage.updated")["payload"],
        json!({"usage": {"usedTokens": 200000, "lastUsedTokens": 200000, "totalProcessedTokens": 535000, "maxTokens": 200000}})
    );
}

#[tokio::test]
async fn preserves_oversized_claude_result_totals_after_task_progress_snapshots_are_recorded() {
    let events = result_usage(
        json!({"type": "result", "subtype": "success", "is_error": false, "num_turns": 1, "result": "done", "stop_reason": "end_turn", "session_id": "s",
            "usage": {"total_tokens": 535000}, "modelUsage": {CAPABLE: {"contextWindow": 200000, "maxOutputTokens": 64000}}}),
        vec![json!({"type": "system", "subtype": "task_progress", "task_id": "task-usage-clamped", "description": "Thinking through the patch", "usage": {"total_tokens": 190000}, "session_id": "s", "uuid": "p"})],
    )
    .await;
    let last = events.iter().rev().find(|e| e["type"] == "thread.token-usage.updated").unwrap();
    assert_eq!(
        last["payload"],
        json!({"usage": {"usedTokens": 190000, "lastUsedTokens": 190000, "totalProcessedTokens": 535000, "maxTokens": 200000}})
    );
}

#[tokio::test]
async fn emits_completion_only_after_turn_result_when_assistant_frames_arrive_before_deltas() {
    let mut harness = Harness::default();
    harness.start(json!({})).await;
    let turn = harness.send(json!({"input": "hello"})).await;
    let s = "sdk-session-early-assistant";
    harness.emit(assistant(
        s,
        "assistant-early",
        "assistant-message-early",
        json!([{"type": "tool_use", "id": "tool-early", "name": "Read", "input": {"path": "a.ts"}}]),
    ));
    harness.emit(stream_event(
        s,
        "stream-early",
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Late text"}}),
    ));
    harness.emit(result_success(s, "result-early"));
    let events = harness.take(8).await;
    assert_eq!(
        types(&events),
        vec![
            "session.started",
            "session.configured",
            "session.state.changed",
            "turn.started",
            "thread.started",
            "content.delta",
            "item.completed",
            "turn.completed"
        ]
    );
    assert_eq!(events[5]["payload"]["delta"], json!("Late text"));
    assert_eq!(events[5]["turnId"], json!(turn.turn_id.as_str()));
}

#[tokio::test]
async fn creates_a_fresh_assistant_message_when_claude_reuses_a_text_block_index() {
    let mut harness = Harness::default();
    harness.start(json!({})).await;
    harness.send(json!({"input": "hello"})).await;
    let s = "sdk-session-reused-text-index";
    for (n, text) in [(1, "First"), (2, "Second")] {
        harness.emit(stream_event(
            s,
            &format!("start-{n}"),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        ));
        harness.emit(stream_event(
            s,
            &format!("delta-{n}"),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
        ));
        harness.emit(stream_event(s, &format!("stop-{n}"), json!({"type": "content_block_stop", "index": 0})));
    }
    harness.emit(result_success(s, "result-reused-text-index"));
    let events = harness.take(9).await;
    assert_eq!(
        types(&events),
        vec![
            "session.started",
            "session.configured",
            "session.state.changed",
            "turn.started",
            "thread.started",
            "content.delta",
            "item.completed",
            "content.delta",
            "item.completed"
        ]
    );
    assert_eq!(events[5]["payload"]["delta"], json!("First"));
    assert_eq!(events[7]["payload"]["delta"], json!("Second"));
    assert_ne!(events[5]["itemId"], events[7]["itemId"]);
    assert_eq!(events[6]["itemId"], events[5]["itemId"]);
    assert_eq!(events[8]["itemId"], events[7]["itemId"]);
}

#[tokio::test]
async fn falls_back_to_assistant_payload_text_when_stream_deltas_are_absent() {
    let mut harness = Harness::default();
    harness.start(json!({})).await;
    let turn = harness.send(json!({"input": "hello"})).await;
    harness.emit(assistant(
        "sdk-session-fallback-text",
        "assistant-fallback",
        "assistant-message-fallback",
        json!([{"type": "text", "text": "Fallback hello"}]),
    ));
    harness.emit(result_success("sdk-session-fallback-text", "result-fallback"));
    let events = harness.take(8).await;
    assert_eq!(
        types(&events),
        vec![
            "session.started",
            "session.configured",
            "session.state.changed",
            "turn.started",
            "thread.started",
            "content.delta",
            "item.completed",
            "turn.completed"
        ]
    );
    assert_eq!(events[5]["payload"]["delta"], json!("Fallback hello"));
    assert_eq!(events[5]["turnId"], json!(turn.turn_id.as_str()));
    assert_eq!(events[6]["payload"]["detail"], json!("Fallback hello"));
}

#[tokio::test]
async fn segments_claude_assistant_text_blocks_around_tool_calls() {
    let mut harness = Harness::default();
    harness.start(json!({})).await;
    harness.send(json!({"input": "hello"})).await;
    let s = "sdk-session-interleaved";
    harness.emit(stream_event(
        s,
        "t1-start",
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
    ));
    harness.emit(stream_event(
        s,
        "t1-delta",
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "First message."}}),
    ));
    harness.emit(stream_event(s, "t1-stop", json!({"type": "content_block_stop", "index": 0})));
    harness.emit(stream_event(s, "tool-start", json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "tool-interleaved-1", "name": "Grep", "input": {"pattern": "assistant", "path": "src"}}})));
    harness.emit(stream_event(s, "tool-stop", json!({"type": "content_block_stop", "index": 1})));
    harness.emit(tool_result(
        s,
        "user-tool-result-interleaved",
        "tool-interleaved-1",
        "src/example.ts:1:assistant",
    ));
    harness.emit(stream_event(
        s,
        "t2-start",
        json!({"type": "content_block_start", "index": 2, "content_block": {"type": "text", "text": ""}}),
    ));
    harness.emit(stream_event(
        s,
        "t2-delta",
        json!({"type": "content_block_delta", "index": 2, "delta": {"type": "text_delta", "text": "Second message."}}),
    ));
    harness.emit(stream_event(s, "t2-stop", json!({"type": "content_block_stop", "index": 2})));
    harness.emit(result_success(s, "result-interleaved"));
    let events = harness.take(13).await;
    assert_eq!(
        types(&events),
        vec![
            "session.started",
            "session.configured",
            "session.state.changed",
            "turn.started",
            "thread.started",
            "content.delta",
            "item.completed",
            "item.started",
            "item.updated",
            "item.completed",
            "content.delta",
            "item.completed",
            "turn.completed"
        ]
    );
    assert_ne!(events[5]["itemId"], events[10]["itemId"]);
    assert_eq!(events[6]["itemId"], events[5]["itemId"]);
    assert_eq!(events[7]["payload"]["detail"], json!(r#"Grep: {"pattern":"assistant","path":"src"}"#));
}

#[tokio::test]
async fn does_not_fabricate_provider_thread_ids_before_first_sdk_session_id() {
    let mut harness = Harness::default();
    let session = harness.start(json!({})).await;
    assert_eq!(session.thread_id.as_str(), THREAD_ID);
    let turn = harness.send(json!({"input": "hello"})).await;
    assert_eq!(turn.thread_id.as_str(), THREAD_ID);
    harness.emit(stream_event(
        "sdk-thread-real",
        "stream-thread-real",
        json!({"type": "message_start", "message": {"id": "msg-thread-real"}}),
    ));
    harness.emit(result_success("sdk-thread-real", "result-thread-real"));
    let events = harness.take(5).await;
    assert_eq!(
        types(&events),
        vec![
            "session.started",
            "session.configured",
            "session.state.changed",
            "turn.started",
            "thread.started"
        ]
    );
    assert_eq!(events[0]["threadId"], json!(THREAD_ID));
    assert_eq!(events[4]["threadId"], json!(THREAD_ID));
    assert_eq!(events[4]["payload"], json!({"providerThreadId": "sdk-thread-real"}));
    assert_eq!(
        events[4]["raw"],
        json!({"source": "claude.sdk.message", "method": "claude/thread/started", "payload": {"session_id": "sdk-thread-real"}})
    );
}
