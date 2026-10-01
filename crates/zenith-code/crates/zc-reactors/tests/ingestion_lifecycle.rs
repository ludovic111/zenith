//! Port of `ProviderRuntimeIngestion.test.ts`: session and turn lifecycle.
//!
//! The TS tests read the SQL projections (`getSnapshot()`); these read the engine's command
//! read model and the event-log fold (`EventLogReactorReads`) until WP-09 lands.

mod common;

use common::*;
use serde_json::{json, Value};

fn session_status(thread: &Value) -> &str {
    thread["session"]["status"].as_str().unwrap_or("")
}

fn active_turn(thread: &Value) -> &Value {
    &thread["session"]["activeTurnId"]
}

#[tokio::test]
async fn maps_turn_started_completed_events_into_thread_session_updates() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit(json!({"type": "turn.started", "eventId": "evt-turn-started", "turnId": "turn-1"}));
    h.wait_for_thread(|t| session_status(t) == "running" && active_turn(t) == "turn-1").await;
    h.emit(json!({
        "type": "turn.completed", "eventId": "evt-turn-completed", "turnId": "turn-1",
        "payload": {"state": "failed", "errorMessage": "turn failed"},
    }));
    let thread = h
        .wait_for_thread(|t| session_status(t) == "error" && active_turn(t).is_null() && t["session"]["lastError"] == "turn failed")
        .await;
    assert_eq!(thread["session"]["lastError"], "turn failed");
}

async fn settles_open_code_aborted_turns(mode: &str) {
    let h = IngestionHarness::new(IngestionOptions {
        server_settings: Some(json!({"responseStreamingMode": mode})),
        ..Default::default()
    })
    .await;
    let base = json!({"provider": "opencode", "threadId": "thread-1", "turnId": "opencode-aborted-turn", "createdAt": "2026-01-01T00:00:01.000Z"});
    let with = |extra: Value| {
        let mut event = base.clone();
        for (key, value) in extra.as_object().unwrap() {
            event[key] = value.clone();
        }
        event
    };
    h.emit(with(json!({"type": "turn.started", "eventId": "opencode-started"})));
    h.emit(with(json!({
        "type": "content.delta", "eventId": "opencode-partial-text", "itemId": "opencode-text-part",
        "payload": {"streamKind": "assistant_text", "delta": "Work before the stop."},
    })));
    h.emit(with(json!({
        "type": "turn.aborted", "eventId": "opencode-aborted", "createdAt": "2026-01-01T00:00:02.000Z",
        "payload": {"reason": "Interrupted by user."},
    })));
    h.drain().await;
    let thread = h.thread().await;
    assert_match(&thread["session"], json!({"status": "interrupted", "activeTurnId": null, "lastError": null}));
    assert_match(
        &thread["latestTurn"],
        json!({"turnId": "opencode-aborted-turn", "state": "interrupted", "completedAt": "2026-01-01T00:00:02.000Z"}),
    );
    assert_match(
        &thread["messages"],
        json!([{"role": "assistant", "turnId": "opencode-aborted-turn", "text": "Work before the stop.", "streaming": false}]),
    );
}

#[tokio::test]
async fn settles_open_code_aborted_turns_and_saves_buffered_assistant_text() {
    settles_open_code_aborted_turns("paragraph").await;
}

#[tokio::test]
async fn settles_open_code_aborted_turns_and_saves_streamed_assistant_text() {
    settles_open_code_aborted_turns("token").await;
}

async fn finalizes_old_buffered_text_on_late_terminal(terminal: &str) {
    let h = IngestionHarness::new(IngestionOptions {
        server_settings: Some(json!({"responseStreamingMode": "paragraph"})),
        ..Default::default()
    })
    .await;
    let at = "2026-01-01T00:00:01.000Z";
    h.emit_and_drain(vec![
        json!({"provider": "opencode", "createdAt": at, "type": "turn.started", "eventId": "old-buffered-started", "turnId": "old-buffered-turn"}),
        json!({
            "provider": "opencode", "createdAt": at, "type": "content.delta", "eventId": "old-buffered-delta",
            "turnId": "old-buffered-turn", "itemId": "old-buffered-message",
            "payload": {"streamKind": "assistant_text", "delta": "Keep the old answer."},
        }),
    ])
    .await;
    h.dispatch(turn_start("start-new-while-old-finishes", "new-turn-prompt", "Continue", at)).await;
    h.set_provider_session(json!({
        "provider": "opencode", "status": "running", "runtimeMode": "approval-required", "threadId": "thread-1",
        "createdAt": at, "updatedAt": at, "activeTurnId": "new-active-turn",
    }));
    let payload = if terminal == "turn.completed" {
        json!({"state": "completed"})
    } else {
        json!({"reason": "Interrupted by user."})
    };
    h.emit_and_drain(vec![
        json!({"provider": "opencode", "createdAt": at, "type": "turn.started", "eventId": "new-active-started", "turnId": "new-active-turn"}),
        json!({"provider": "opencode", "createdAt": at, "type": terminal, "eventId": "old-buffered-terminal", "turnId": "old-buffered-turn", "payload": payload}),
    ])
    .await;
    let thread = h.thread().await;
    assert_match(&thread["session"], json!({"activeTurnId": "new-active-turn", "status": "running"}));
    assert_contains(
        &thread["messages"],
        json!({"turnId": "old-buffered-turn", "text": "Keep the old answer.", "streaming": false}),
    );
}

#[tokio::test]
async fn finalizes_old_buffered_text_on_late_turn_completed_without_stopping_the_newer_turn() {
    finalizes_old_buffered_text_on_late_terminal("turn.completed").await;
}

#[tokio::test]
async fn finalizes_old_buffered_text_on_late_turn_aborted_without_stopping_the_newer_turn() {
    finalizes_old_buffered_text_on_late_terminal("turn.aborted").await;
}

async fn ignores_late_open_code_aborts(late_turn_id: Option<&str>) {
    let h = IngestionHarness::new(IngestionOptions {
        server_settings: Some(json!({"responseStreamingMode": "token"})),
        ..Default::default()
    })
    .await;
    let at = "2026-01-01T00:00:01.000Z";
    let event = |extra: Value| {
        let mut event = json!({"provider": "opencode", "threadId": "thread-1", "createdAt": at});
        for (key, value) in extra.as_object().unwrap() {
            event[key] = value.clone();
        }
        event
    };
    let late = |id: &str, created_at: &str| {
        let mut abort = event(json!({"type": "turn.aborted", "eventId": id, "createdAt": created_at, "payload": {"reason": "Interrupted by user."}}));
        if let Some(turn_id) = late_turn_id {
            abort["turnId"] = json!(turn_id);
        }
        abort
    };
    h.emit(event(
        json!({"type": "turn.started", "eventId": "opencode-first-started", "turnId": "opencode-stopped-turn"}),
    ));
    h.emit(event(
        json!({"type": "turn.aborted", "eventId": "opencode-first-aborted", "turnId": "opencode-stopped-turn", "payload": {"reason": "Interrupted by user."}}),
    ));
    h.emit(event(
        json!({"type": "turn.started", "eventId": "opencode-next-started", "turnId": "opencode-next-turn"}),
    ));
    h.emit(event(json!({
        "type": "content.delta", "eventId": "opencode-next-partial-text", "turnId": "opencode-next-turn",
        "itemId": "opencode-next-text-part", "payload": {"streamKind": "assistant_text", "delta": "The next turn is running."},
    })));
    h.drain().await;
    h.emit(late("opencode-late-abort", at));
    h.drain().await;

    let thread = h.thread().await;
    assert_match(&thread["session"], json!({"status": "running", "activeTurnId": "opencode-next-turn"}));
    assert_match(&thread["latestTurn"], json!({"turnId": "opencode-next-turn", "state": "running"}));
    assert_match(
        &thread["messages"],
        json!([{"turnId": "opencode-next-turn", "text": "The next turn is running.", "streaming": true}]),
    );

    h.emit(event(json!({
        "type": "turn.completed", "eventId": "opencode-next-completed", "turnId": "opencode-next-turn",
        "createdAt": "2026-01-01T00:00:02.000Z", "payload": {"state": "completed"},
    })));
    h.drain().await;

    let pending_at = "2026-01-01T00:00:03.000Z";
    for has_pending_start in [false, true] {
        if has_pending_start {
            h.dispatch(turn_start(
                "opencode-pending-start",
                "opencode-pending-message",
                "Start another turn.",
                pending_at,
            ))
            .await;
            h.emit(event(
                json!({"type": "session.state.changed", "eventId": "opencode-pending-starting", "createdAt": pending_at, "payload": {"state": "starting"}}),
            ));
        }
        h.emit(late(
            &format!("opencode-late-abort-after-completion-{has_pending_start}"),
            "2026-01-01T00:00:04.000Z",
        ));
        h.drain().await;
        let thread = h.thread().await;
        assert_match(
            &thread["session"],
            json!({"status": if has_pending_start { "starting" } else { "ready" }, "activeTurnId": null}),
        );
        assert_match(&thread["latestTurn"], json!({"turnId": "opencode-next-turn", "state": "completed"}));
    }

    h.emit(event(
        json!({"type": "turn.started", "eventId": "opencode-pending-started", "turnId": "opencode-pending-turn", "createdAt": "2026-01-01T00:00:05.000Z"}),
    ));
    h.drain().await;
    let thread = h.thread().await;
    assert_match(
        &thread["latestTurn"],
        json!({"turnId": "opencode-pending-turn", "state": "running", "requestedAt": pending_at}),
    );
}

#[tokio::test]
async fn ignores_late_open_code_aborts_for_the_previous_turn_across_newer_turns() {
    ignores_late_open_code_aborts(Some("opencode-stopped-turn")).await;
}

#[tokio::test]
async fn ignores_late_open_code_aborts_for_an_unspecified_turn_across_newer_turns() {
    ignores_late_open_code_aborts(None).await;
}

#[tokio::test]
async fn applies_provider_session_state_changed_transitions_directly() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit(json!({"type": "session.state.changed", "eventId": "evt-session-state-waiting", "payload": {"state": "waiting", "reason": "awaiting approval"}}));
    let thread = h.wait_for_thread(|t| session_status(t) == "running" && active_turn(t).is_null()).await;
    assert!(thread["session"]["lastError"].is_null());

    h.emit(json!({"type": "session.state.changed", "eventId": "evt-session-state-error", "payload": {"state": "error", "reason": "provider crashed"}}));
    let thread = h
        .wait_for_thread(|t| session_status(t) == "error" && active_turn(t).is_null() && t["session"]["lastError"] == "provider crashed")
        .await;
    assert_eq!(thread["session"]["lastError"], "provider crashed");

    h.emit(json!({"type": "session.state.changed", "eventId": "evt-session-state-stopped", "payload": {"state": "stopped"}}));
    h.wait_for_thread(|t| session_status(t) == "stopped" && active_turn(t).is_null() && t["session"]["lastError"] == "provider crashed")
        .await;

    h.emit(json!({"type": "session.state.changed", "eventId": "evt-session-state-ready", "payload": {"state": "ready"}}));
    let thread = h
        .wait_for_thread(|t| session_status(t) == "ready" && active_turn(t).is_null() && t["session"]["lastError"].is_null())
        .await;
    assert!(thread["session"]["lastError"].is_null());
}

#[tokio::test]
async fn clears_active_turn_when_provider_session_becomes_ready() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit(json!({"type": "turn.started", "eventId": "evt-turn-started-session-ready", "turnId": "turn-session-ready"}));
    h.wait_for_thread(|t| session_status(t) == "running" && active_turn(t) == "turn-session-ready")
        .await;
    h.emit(json!({"type": "session.state.changed", "eventId": "evt-session-state-ready-with-active-turn", "createdAt": "2026-01-01T00:00:01.000Z", "payload": {"state": "ready"}}));
    let thread = h
        .wait_for_thread(|t| session_status(t) == "ready" && active_turn(t).is_null() && t["session"]["lastError"].is_null())
        .await;
    assert!(active_turn(&thread).is_null());
}

#[tokio::test]
async fn keeps_a_reconnecting_pending_turn_starting_while_ready_clears_stale_active_state() {
    let h = IngestionHarness::new(Default::default()).await;
    h.dispatch(turn_start(
        "cmd-turn-start-pending-reconnect",
        "message-pending-reconnect",
        "resume after reconnect",
        "2026-01-01T00:00:01.000Z",
    ))
    .await;
    h.dispatch(session_set(
        "cmd-session-starting-pending-reconnect",
        "starting",
        "codex",
        json!("turn-stale-before-reconnect"),
        "2026-01-01T00:00:01.000Z",
    ))
    .await;
    h.emit(json!({"type": "session.state.changed", "eventId": "evt-session-ready-pending-reconnect", "createdAt": "2026-01-01T00:00:02.000Z", "payload": {"state": "ready"}}));
    h.wait_for_thread(|t| session_status(t) == "starting" && active_turn(t).is_null()).await;

    h.emit(json!({"type": "session.started", "eventId": "evt-session-started-pending-reconnect", "createdAt": "2026-01-01T00:00:03.000Z"}));
    h.drain().await;
    let thread = h.thread().await;
    assert_eq!(session_status(&thread), "starting");
    assert!(active_turn(&thread).is_null());

    h.emit(json!({"type": "turn.started", "eventId": "evt-turn-started-pending-reconnect", "turnId": "turn-after-reconnect", "createdAt": "2026-01-01T00:00:04.000Z"}));
    h.wait_for_thread(|t| session_status(t) == "running" && active_turn(t) == "turn-after-reconnect")
        .await;

    h.emit(json!({"type": "session.started", "eventId": "evt-session-started-duplicate-midturn", "createdAt": "2026-01-01T00:00:05.000Z"}));
    h.drain().await;
    let thread = h.thread().await;
    assert_eq!(session_status(&thread), "running");
    assert_eq!(active_turn(&thread), "turn-after-reconnect");
}

#[tokio::test]
async fn keeps_an_aborted_pending_start_stopped_across_duplicate_exit_events() {
    let h = IngestionHarness::new(Default::default()).await;
    h.dispatch(turn_start(
        "cmd-turn-start-before-stop",
        "message-before-stop",
        "stop this startup",
        "2026-01-01T00:00:01.000Z",
    ))
    .await;
    h.dispatch(session_set(
        "cmd-session-starting-before-stop",
        "starting",
        "codex",
        Value::Null,
        "2026-01-01T00:00:01.000Z",
    ))
    .await;
    h.dispatch(session_set(
        "cmd-session-stop-pending-start",
        "stopped",
        "codex",
        Value::Null,
        "2026-01-01T00:00:02.000Z",
    ))
    .await;
    h.emit(json!({"type": "session.exited", "eventId": "evt-session-exited-after-stop", "createdAt": "2026-01-01T00:00:03.000Z"}));
    h.emit(json!({"type": "session.exited", "eventId": "evt-duplicate-session-exited-after-stop", "createdAt": "2026-01-01T00:00:04.000Z"}));
    h.drain().await;
    let thread = h.thread().await;
    assert_eq!(session_status(&thread), "stopped");
    assert!(active_turn(&thread).is_null());
}

#[tokio::test]
async fn does_not_clear_active_turn_when_session_thread_started_arrives_mid_turn() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit(json!({"type": "turn.started", "eventId": "evt-turn-started-midturn-lifecycle", "turnId": "turn-midturn-lifecycle"}));
    h.wait_for_thread(|t| session_status(t) == "running" && active_turn(t) == "turn-midturn-lifecycle")
        .await;
    h.emit(json!({"type": "thread.started", "eventId": "evt-thread-started-midturn-lifecycle"}));
    h.emit(json!({"type": "session.started", "eventId": "evt-session-started-midturn-lifecycle"}));
    h.drain().await;
    let thread = h.thread().await;
    assert_eq!(session_status(&thread), "running");
    assert_eq!(active_turn(&thread), "turn-midturn-lifecycle");
    h.emit(json!({"type": "turn.completed", "eventId": "evt-turn-completed-midturn-lifecycle", "turnId": "turn-midturn-lifecycle", "status": "completed"}));
    h.wait_for_thread(|t| session_status(t) == "ready" && active_turn(t).is_null()).await;
}

#[tokio::test]
async fn accepts_claude_turn_lifecycle_when_seeded_thread_id_is_a_synthetic_placeholder() {
    let h = IngestionHarness::new(Default::default()).await;
    h.dispatch(session_set("cmd-session-seed-claude-placeholder", "ready", "claudeAgent", Value::Null, NOW))
        .await;
    h.emit(json!({"type": "turn.started", "eventId": "evt-turn-started-claude-placeholder", "provider": "claudeAgent", "turnId": "turn-claude-placeholder"}));
    h.wait_for_thread(|t| session_status(t) == "running" && active_turn(t) == "turn-claude-placeholder")
        .await;
    h.emit(json!({
        "type": "turn.completed", "eventId": "evt-turn-completed-claude-placeholder", "provider": "claudeAgent",
        "turnId": "turn-claude-placeholder", "status": "completed",
    }));
    h.wait_for_thread(|t| session_status(t) == "ready" && active_turn(t).is_null()).await;
}

#[tokio::test]
async fn ignores_auxiliary_turn_completions_from_a_different_provider_thread() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit(json!({"type": "turn.started", "eventId": "evt-turn-started-primary", "turnId": "turn-primary"}));
    h.wait_for_thread(|t| session_status(t) == "running" && active_turn(t) == "turn-primary").await;
    h.emit(json!({"type": "turn.completed", "eventId": "evt-turn-completed-aux", "turnId": "turn-aux", "status": "completed"}));
    h.drain().await;
    let thread = h.thread().await;
    assert_eq!(session_status(&thread), "running");
    assert_eq!(active_turn(&thread), "turn-primary");
    h.emit(json!({"type": "turn.completed", "eventId": "evt-turn-completed-primary", "turnId": "turn-primary", "status": "completed"}));
    h.wait_for_thread(|t| session_status(t) == "ready" && active_turn(t).is_null()).await;
}

#[tokio::test]
async fn rejects_an_untargeted_turn_completed_when_no_turn_is_active() {
    let h = IngestionHarness::new(Default::default()).await;
    h.dispatch(session_set(
        "cmd-session-seed-untargeted-completion",
        "starting",
        "claudeAgent",
        Value::Null,
        NOW,
    ))
    .await;
    h.emit(json!({"type": "turn.completed", "eventId": "evt-turn-completed-untargeted", "provider": "claudeAgent", "status": "completed"}));
    h.drain().await;
    let thread = h.thread().await;
    assert_eq!(session_status(&thread), "starting");
    assert!(active_turn(&thread).is_null());
}

#[tokio::test]
async fn accepts_a_targeted_turn_completed_when_no_turn_is_active() {
    let h = IngestionHarness::new(Default::default()).await;
    h.dispatch(session_set("cmd-session-seed-targeted-completion", "starting", "claudeAgent", Value::Null, NOW))
        .await;
    h.emit(json!({"type": "turn.completed", "eventId": "evt-turn-completed-targeted-late", "provider": "claudeAgent", "turnId": "turn-late", "status": "completed"}));
    h.wait_for_thread(|t| session_status(t) == "ready").await;
}

#[tokio::test]
async fn ignores_non_active_turn_completion_when_runtime_omits_thread_id() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit(json!({"type": "turn.started", "eventId": "evt-turn-started-guarded", "turnId": "turn-guarded-main"}));
    h.wait_for_thread(|t| session_status(t) == "running" && active_turn(t) == "turn-guarded-main")
        .await;
    h.emit(json!({"type": "turn.completed", "eventId": "evt-turn-completed-guarded-other", "turnId": "turn-guarded-other", "status": "completed"}));
    h.drain().await;
    let thread = h.thread().await;
    assert_eq!(session_status(&thread), "running");
    assert_eq!(active_turn(&thread), "turn-guarded-main");
    h.emit(json!({"type": "turn.completed", "eventId": "evt-turn-completed-guarded-main", "turnId": "turn-guarded-main", "status": "completed"}));
    h.wait_for_thread(|t| session_status(t) == "ready" && active_turn(t).is_null()).await;
}

#[tokio::test]
async fn ignores_provider_content_deltas_that_cannot_change_thread_state() {
    let h = IngestionHarness::new(Default::default()).await;
    let initial = h.read_model().await;
    for stream_kind in ["command_output", "file_change_output"] {
        h.emit(json!({
            "type": "content.delta", "eventId": format!("evt-ignored-{stream_kind}"), "turnId": "turn-ignored",
            "payload": {"streamKind": stream_kind, "delta": "ignored output"},
        }));
    }
    h.drain().await;
    assert_eq!(h.read_model().await, initial);
}

#[tokio::test]
async fn events_published_on_the_provider_stream_reach_the_ingestion() {
    let h = IngestionHarness::new(Default::default()).await;
    h.publish(json!({"type": "turn.started", "eventId": "evt-stream-turn-started", "turnId": "turn-stream"}));
    h.wait_for_thread(|t| session_status(t) == "running" && active_turn(t) == "turn-stream").await;
}
