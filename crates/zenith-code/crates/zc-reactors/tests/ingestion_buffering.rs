//! Port of `ProviderRuntimeIngestion.test.ts`: source proposed plans, plan buffering, assistant
//! text buffering around approvals and questions, native question settlement.

mod common;

use common::*;
use serde_json::{json, Value};

fn session_status(thread: &Value) -> &str {
    thread["session"]["status"].as_str().unwrap_or("")
}

fn finished(message: &Value) -> bool {
    message["streaming"] == json!(false)
}

fn message<'a>(thread: &'a Value, id: &str) -> Option<&'a Value> {
    find(&thread["messages"], |m| s(m, "id") == id)
}

fn plan<'a>(thread: &'a Value, id: &str) -> Option<&'a Value> {
    find(&thread["proposedPlans"], |p| s(p, "id") == id)
}

async fn create_thread(h: &IngestionHarness, thread_id: &str, command: &str, title: &str, mode: &str) {
    h.dispatch(json!({
        "type": "thread.create", "commandId": format!("cmd-thread-create-{command}"), "threadId": thread_id, "projectId": "project-1",
        "title": title, "modelSelection": {"instanceId": "codex", "model": "gpt-5-codex"}, "interactionMode": mode,
        "runtimeMode": "approval-required", "branch": null, "worktreePath": null, "createdAt": NOW,
    }))
    .await;
    h.dispatch(json!({
        "type": "thread.session.set", "commandId": format!("cmd-session-set-{command}"), "threadId": thread_id,
        "session": {"threadId": thread_id, "status": "ready", "providerName": "codex", "runtimeMode": "approval-required", "activeTurnId": null, "updatedAt": NOW, "lastError": null},
        "createdAt": NOW,
    }))
    .await;
}

fn plan_turn_start(command_id: &str, thread_id: &str, message_id: &str, source_thread: &str, plan_id: &str) -> Value {
    json!({
        "type": "thread.turn.start", "commandId": command_id, "threadId": thread_id,
        "message": {"messageId": message_id, "role": "user", "text": "PLEASE IMPLEMENT THIS PLAN:\n# Source plan", "attachments": []},
        "sourceProposedPlan": {"threadId": source_thread, "planId": plan_id},
        "interactionMode": "default", "runtimeMode": "approval-required", "createdAt": NOW,
    })
}

const SOURCE_PLAN: &str = "plan:thread-plan:turn:turn-plan-source";

#[tokio::test]
async fn marks_the_source_proposed_plan_implemented_only_after_the_target_turn_starts() {
    let h = IngestionHarness::new(Default::default()).await;
    create_thread(&h, "thread-plan", "plan-source", "Plan Source", "plan").await;
    create_thread(&h, "thread-implement", "plan-target", "Plan Target", "default").await;
    h.set_provider_session(json!({
        "provider": "codex", "status": "ready", "runtimeMode": "approval-required", "threadId": "thread-implement",
        "createdAt": NOW, "updatedAt": NOW, "activeTurnId": "turn-plan-implement",
    }));
    h.emit(json!({"type": "turn.proposed.completed", "eventId": "evt-plan-source-completed", "threadId": "thread-plan", "turnId": "turn-plan-source", "payload": {"planMarkdown": "# Source plan"}}));
    let source = h
        .wait_for_thread_by_id("thread-plan", |t| plan(t, SOURCE_PLAN).is_some_and(|p| p["implementedAt"].is_null()))
        .await;
    let source_plan = plan(&source, SOURCE_PLAN).unwrap().clone();

    h.dispatch(plan_turn_start(
        "cmd-turn-start-plan-target",
        "thread-implement",
        "msg-plan-target",
        "thread-plan",
        SOURCE_PLAN,
    ))
    .await;
    let before = h
        .wait_for_thread_by_id("thread-plan", |t| plan(t, SOURCE_PLAN).is_some_and(|p| p["implementedAt"].is_null()))
        .await;
    assert_match(
        plan(&before, SOURCE_PLAN).unwrap(),
        json!({"implementedAt": null, "implementationThreadId": null}),
    );

    h.emit(json!({"type": "turn.started", "eventId": "evt-plan-target-started", "threadId": "thread-implement", "turnId": "turn-plan-implement"}));
    let after = h
        .wait_for_thread_by_id("thread-plan", |t| {
            plan(t, SOURCE_PLAN).is_some_and(|p| !p["implementedAt"].is_null() && p["implementationThreadId"] == "thread-implement")
        })
        .await;
    let implemented = plan(&after, SOURCE_PLAN).unwrap().clone();

    h.emit_and_drain(vec![json!({
        "type": "turn.proposed.completed", "eventId": "evt-plan-source-late-completion", "createdAt": "2026-01-01T00:01:00.000Z",
        "threadId": "thread-plan", "turnId": "turn-plan-source", "payload": {"planMarkdown": "# Source plan with late details"},
    })])
    .await;
    let late = h.thread_by_id("thread-plan").await;
    assert_match(
        plan(&late, SOURCE_PLAN).unwrap(),
        json!({
            "planMarkdown": "# Source plan with late details",
            "createdAt": source_plan["createdAt"],
            "implementedAt": implemented["implementedAt"],
            "implementationThreadId": "thread-implement",
        }),
    );
}

#[tokio::test]
async fn does_not_mark_the_source_proposed_plan_implemented_for_a_rejected_turn_started_event() {
    let h = IngestionHarness::new(Default::default()).await;
    create_thread(&h, "thread-plan", "plan-source-guarded", "Plan Source", "plan").await;
    h.set_provider_session(json!({
        "provider": "codex", "status": "running", "runtimeMode": "approval-required", "threadId": "thread-1",
        "createdAt": NOW, "updatedAt": NOW, "activeTurnId": "turn-already-running",
    }));
    h.emit(json!({"type": "turn.started", "eventId": "evt-turn-started-already-running", "turnId": "turn-already-running"}));
    h.wait_for_thread(|t| session_status(t) == "running" && t["session"]["activeTurnId"] == "turn-already-running")
        .await;
    h.emit(json!({"type": "turn.proposed.completed", "eventId": "evt-plan-source-completed-guarded", "threadId": "thread-plan", "turnId": "turn-plan-source", "payload": {"planMarkdown": "# Source plan"}}));
    h.wait_for_thread_by_id("thread-plan", |t| plan(t, SOURCE_PLAN).is_some_and(|p| p["implementedAt"].is_null()))
        .await;
    h.dispatch(plan_turn_start(
        "cmd-turn-start-plan-target-guarded",
        "thread-1",
        "msg-plan-target-guarded",
        "thread-plan",
        SOURCE_PLAN,
    ))
    .await;
    h.emit(json!({"type": "turn.started", "eventId": "evt-turn-started-stale-plan-implementation", "turnId": "turn-stale-start"}));
    h.drain().await;
    let source = h.thread_by_id("thread-plan").await;
    assert_match(
        plan(&source, SOURCE_PLAN).unwrap(),
        json!({"implementedAt": null, "implementationThreadId": null}),
    );
    let target = h.thread().await;
    assert_eq!(session_status(&target), "running");
    assert_eq!(target["session"]["activeTurnId"], "turn-already-running");
}

#[tokio::test]
async fn accepts_a_conflicting_turn_started_for_a_pending_turn_start_when_the_provider_expects_that_turn() {
    let h = IngestionHarness::new(Default::default()).await;
    let session = |turn: &str| json!({"provider": "codex", "status": "running", "runtimeMode": "approval-required", "threadId": "thread-1", "createdAt": NOW, "updatedAt": NOW, "activeTurnId": turn});
    h.set_provider_session(session("turn-steered-over"));
    h.emit(json!({"type": "turn.started", "eventId": "evt-turn-started-steered-over", "turnId": "turn-steered-over"}));
    h.wait_for_thread(|t| session_status(t) == "running" && t["session"]["activeTurnId"] == "turn-steered-over")
        .await;
    h.dispatch(turn_start("cmd-turn-start-steer", "msg-steer", "actually, do 15 instead", NOW))
        .await;
    h.set_provider_session(session("turn-from-steer"));
    h.emit(json!({"type": "turn.started", "eventId": "evt-turn-started-from-steer", "turnId": "turn-from-steer"}));
    let thread = h
        .wait_for_thread(|t| session_status(t) == "running" && t["session"]["activeTurnId"] == "turn-from-steer")
        .await;
    assert_eq!(thread["latestTurn"]["turnId"], "turn-from-steer");
    assert_eq!(thread["latestTurn"]["state"], "running");
}

#[tokio::test]
async fn does_not_mark_the_source_plan_implemented_for_an_unrelated_turn_started_when_no_active_turn_is_tracked() {
    let h = IngestionHarness::new(Default::default()).await;
    create_thread(&h, "thread-plan", "plan-source-unrelated", "Plan Source", "plan").await;
    create_thread(&h, "thread-implement", "plan-target-unrelated", "Plan Target", "default").await;
    h.emit(json!({"type": "turn.proposed.completed", "eventId": "evt-plan-source-completed-unrelated", "threadId": "thread-plan", "turnId": "turn-plan-source", "payload": {"planMarkdown": "# Source plan"}}));
    h.wait_for_thread_by_id("thread-plan", |t| plan(t, SOURCE_PLAN).is_some_and(|p| p["implementedAt"].is_null()))
        .await;
    h.dispatch(plan_turn_start(
        "cmd-turn-start-plan-target-unrelated",
        "thread-implement",
        "msg-plan-target-unrelated",
        "thread-plan",
        SOURCE_PLAN,
    ))
    .await;
    h.set_provider_session(json!({
        "provider": "codex", "status": "running", "runtimeMode": "approval-required", "threadId": "thread-implement",
        "createdAt": NOW, "updatedAt": NOW, "activeTurnId": "turn-plan-implement",
    }));
    h.emit(
        json!({"type": "turn.started", "eventId": "evt-turn-started-unrelated-plan-implementation", "threadId": "thread-implement", "turnId": "turn-replayed"}),
    );
    h.drain().await;
    let source = h.thread_by_id("thread-plan").await;
    assert_match(
        plan(&source, SOURCE_PLAN).unwrap(),
        json!({"implementedAt": null, "implementationThreadId": null}),
    );
}

#[tokio::test]
async fn finalizes_buffered_proposed_plan_deltas_into_a_first_class_proposed_plan_on_turn_completion() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit(json!({"type": "turn.started", "eventId": "evt-turn-started-plan-buffer", "turnId": "turn-plan-buffer"}));
    h.wait_for_thread(|t| session_status(t) == "running" && t["session"]["activeTurnId"] == "turn-plan-buffer")
        .await;
    h.emit(json!({"type": "turn.proposed.delta", "eventId": "evt-plan-delta-1", "createdAt": "", "turnId": "turn-plan-buffer", "payload": {"delta": "## Buffered plan\n\n- first"}}));
    h.emit(json!({"type": "turn.proposed.delta", "eventId": "evt-plan-delta-2", "createdAt": "", "turnId": "turn-plan-buffer", "payload": {"delta": "\n- second"}}));
    h.emit(json!({"type": "turn.completed", "eventId": "evt-turn-completed-plan-buffer", "turnId": "turn-plan-buffer", "payload": {"state": "completed"}}));
    let thread = h.wait_for_thread(|t| plan(t, "plan:thread-1:turn:turn-plan-buffer").is_some()).await;
    let plan = plan(&thread, "plan:thread-1:turn:turn-plan-buffer").unwrap();
    assert_eq!(plan["planMarkdown"], "## Buffered plan\n\n- first\n- second");
    assert_eq!(plan["createdAt"], NOW);
}

#[tokio::test]
async fn releases_a_blank_completed_plan_before_a_late_replacement() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit_and_drain(vec![
        json!({"type": "turn.proposed.delta", "eventId": "blank-plan-delta", "turnId": "blank-plan-turn", "createdAt": "2026-01-01T00:00:00.000Z", "payload": {"delta": " \n "}}),
        json!({"type": "turn.completed", "eventId": "blank-plan-completed", "turnId": "blank-plan-turn", "createdAt": "2026-01-01T00:00:01.000Z", "payload": {"state": "completed"}}),
        json!({"type": "turn.proposed.completed", "eventId": "late-plan-completed", "turnId": "blank-plan-turn", "createdAt": "2026-01-01T00:00:02.000Z", "payload": {"planMarkdown": "# Replacement plan"}}),
    ])
    .await;
    let thread = h.thread().await;
    assert_match(
        &thread["proposedPlans"],
        json!([{"planMarkdown": "# Replacement plan", "createdAt": "2026-01-01T00:00:02.000Z"}]),
    );
}

/// The TS test counts SQL statements: one lifecycle query per buffered delta. Here every read
/// the ingestion makes is counted.
#[tokio::test]
async fn buffers_assistant_deltas_with_one_lifecycle_query_per_event_until_completion() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit_and_drain(vec![
        json!({"type": "turn.started", "eventId": "evt-turn-started-buffered", "turnId": "turn-buffered"}),
    ])
    .await;
    let event_count = 1_000;
    let before = h.counting.count();
    h.emit_and_drain(
        (0..event_count)
            .map(|index| {
                json!({
                    "type": "content.delta", "eventId": format!("evt-message-delta-buffered-{index}"), "turnId": "turn-buffered",
                    "itemId": "item-buffered", "payload": {"streamKind": "assistant_text", "delta": "a"},
                })
            })
            .collect(),
    )
    .await;
    assert_eq!(h.counting.count() - before, event_count);
    assert!(message(&h.thread().await, "assistant:item-buffered").is_none());
    h.emit_and_drain(vec![json!({
        "type": "item.completed", "eventId": "evt-message-completed-buffered", "turnId": "turn-buffered", "itemId": "item-buffered",
        "payload": {"itemType": "assistant_message", "status": "completed"},
    })])
    .await;
    let thread = h.thread().await;
    let message = message(&thread, "assistant:item-buffered").unwrap();
    assert_eq!(message["text"], "a".repeat(event_count));
    assert_eq!(message["streaming"], false);
}

async fn flushes_on_pause(pause: Value, turn: &str, item: &str, text: &str) {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit(json!({"type": "turn.started", "eventId": format!("evt-turn-started-{turn}"), "turnId": turn}));
    h.wait_for_thread(|t| session_status(t) == "running" && t["session"]["activeTurnId"] == turn)
        .await;
    h.emit(json!({"type": "content.delta", "eventId": format!("evt-message-delta-{turn}"), "turnId": turn, "itemId": item, "payload": {"streamKind": "assistant_text", "delta": text}}));
    h.emit(pause);
    let id = format!("assistant:{item}");
    let thread = h.wait_for_thread(|t| message(t, &id).is_some_and(|m| finished(m) && m["text"] == text)).await;
    assert_eq!(message(&thread, &id).unwrap()["streaming"], false);
}

#[tokio::test]
async fn flushes_and_completes_buffered_assistant_text_when_an_approval_request_opens() {
    flushes_on_pause(
        json!({
            "type": "request.opened", "eventId": "evt-request-opened-buffered-request-flush", "turnId": "turn-buffered-request-flush",
            "requestId": "req-buffered-request-flush", "payload": {"requestType": "command_execution_approval", "detail": "pwd"},
        }),
        "turn-buffered-request-flush",
        "item-buffered-request-flush",
        "visible before approval",
    )
    .await;
}

#[tokio::test]
async fn flushes_and_completes_buffered_assistant_text_when_user_input_is_requested() {
    flushes_on_pause(
        json!({
            "type": "user-input.requested", "eventId": "evt-user-input-requested-buffered-user-input-flush",
            "turnId": "turn-buffered-user-input-flush", "requestId": "req-buffered-user-input-flush",
            "payload": {"questions": [{"id": "choice", "header": "Choice", "question": "Pick one", "options": [{"label": "A", "description": "Option A"}]}]},
        }),
        "turn-buffered-user-input-flush",
        "item-buffered-user-input-flush",
        "visible before user input",
    )
    .await;
}

fn user_input_event(turn_id: &str, request_id: &str, message_mode: bool) -> Value {
    let questions: Vec<Value> = ["first", "second"]
        .iter()
        .map(|id| json!({"id": id, "header": id, "question": format!("Choose {id}"), "options": [{"label": "yes", "description": "Continue"}], "multiSelect": false}))
        .collect();
    let mut payload = json!({"questions": questions});
    if message_mode {
        payload["responseMode"] = json!("message");
    }
    json!({
        "type": "user-input.requested", "eventId": format!("requested:{request_id}"), "threadId": "thread-1", "turnId": turn_id,
        "requestId": request_id, "createdAt": "2026-01-01T00:00:01.000Z", "payload": payload,
    })
}

fn resolved(thread: &Value) -> Value {
    Value::Array(
        filter(&thread["activities"], |a| s(a, "kind") == "user-input.resolved")
            .into_iter()
            .cloned()
            .collect(),
    )
}

async fn resolves_native_questions_when_their_turn_ends(state: &str) {
    let h = IngestionHarness::new(Default::default()).await;
    let request = user_input_event("question-turn", "question-request", false);
    h.emit_and_drain(vec![
        json!({"type": "turn.started", "eventId": "question-started", "turnId": "question-turn", "createdAt": "2026-01-01T00:00:01.000Z"}),
        request,
    ])
    .await;
    assert_eq!(h.thread_shell().await["hasPendingUserInput"], true);
    if state == "interrupted" {
        h.dispatch(
            json!({"type": "thread.turn.interrupt", "commandId": "question-interrupt", "threadId": "thread-1", "createdAt": "2026-01-01T00:00:02.000Z"}),
        )
        .await;
    }
    let completed = if state == "aborted" {
        json!({"type": "turn.aborted", "payload": {"reason": "Interrupted by user."}, "eventId": "question-completed", "turnId": "question-turn", "createdAt": "2026-01-01T00:00:03.000Z"})
    } else {
        json!({"type": "turn.completed", "payload": {"state": state}, "eventId": "question-completed", "turnId": "question-turn", "createdAt": "2026-01-01T00:00:03.000Z"})
    };
    h.emit_and_drain(vec![completed.clone()]).await;
    let thread = h.thread().await;
    assert!(thread["session"]["activeTurnId"].is_null());
    assert_eq!(h.thread_shell().await["hasPendingUserInput"], false);
    assert_match(
        &resolved(&thread),
        json!([{"turnId": "question-turn", "payload": {"requestId": "question-request"}}]),
    );
    h.emit_and_drain(vec![completed]).await;
    assert_eq!(resolved(&h.thread().await).as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn resolves_native_questions_when_their_turn_is_completed() {
    resolves_native_questions_when_their_turn_ends("completed").await;
}

#[tokio::test]
async fn resolves_native_questions_when_their_turn_is_interrupted() {
    resolves_native_questions_when_their_turn_ends("interrupted").await;
}

#[tokio::test]
async fn resolves_native_questions_when_their_turn_is_failed() {
    resolves_native_questions_when_their_turn_ends("failed").await;
}

#[tokio::test]
async fn resolves_native_questions_when_their_turn_is_aborted() {
    resolves_native_questions_when_their_turn_ends("aborted").await;
}

#[tokio::test]
async fn keeps_a_stale_request_dismissed_when_its_turn_later_completes() {
    let h = IngestionHarness::new(Default::default()).await;
    h.emit_and_drain(vec![user_input_event("stale-turn", "stale-question", false)]).await;
    h.dispatch(json!({
        "type": "thread.activity.append", "commandId": "stale-question-response", "threadId": "thread-1",
        "activity": {
            "id": "stale-question-failed", "kind": "provider.user-input.respond.failed", "tone": "error", "summary": "User input response failed",
            "turnId": "stale-turn", "payload": {"requestId": "stale-question", "detail": "Unknown pending user input request"},
            "createdAt": "2026-01-01T00:00:02.000Z",
        },
        "createdAt": "2026-01-01T00:00:02.000Z",
    }))
    .await;
    assert_eq!(h.thread_shell().await["hasPendingUserInput"], false);
    h.emit_and_drain(vec![json!({"type": "turn.completed", "eventId": "stale-turn-completed", "turnId": "stale-turn", "createdAt": "2026-01-01T00:00:03.000Z", "payload": {"state": "completed"}})])
        .await;
    assert_eq!(h.thread_shell().await["hasPendingUserInput"], false);
    assert_match(&resolved(&h.thread().await), json!([{"payload": {"requestId": "stale-question"}}]));
}

#[tokio::test]
async fn preserves_answered_questions_and_leaves_newer_child_and_async_questions_pending() {
    let h = IngestionHarness::new(Default::default()).await;
    let answer = json!({
        "type": "user-input.resolved", "eventId": "normal-answer", "threadId": "thread-1", "turnId": "old-turn", "requestId": "answered-question",
        "createdAt": "2026-01-01T00:00:02.000Z", "payload": {"answers": {"first": "yes", "second": "yes"}},
    });
    h.emit_and_drain(vec![
        user_input_event("old-turn", "answered-question", false),
        user_input_event("old-turn", "old-question", false),
        user_input_event("new-turn", "new-question", false),
        user_input_event("child-turn", "child-question", false),
        user_input_event("old-turn", "async-question", true),
        answer.clone(),
        json!({"type": "turn.started", "eventId": "new-turn-started", "turnId": "new-turn", "createdAt": "2026-01-01T00:00:03.000Z"}),
    ])
    .await;
    assert_eq!(h.thread_shell().await["hasPendingUserInput"], true);
    h.emit_and_drain(vec![json!({"type": "turn.completed", "eventId": "old-turn-completed", "turnId": "old-turn", "createdAt": "2026-01-01T00:00:04.000Z", "payload": {"state": "interrupted"}})])
        .await;
    let thread = h.thread().await;
    assert_eq!(thread["session"]["activeTurnId"], "new-turn");
    assert_eq!(h.thread_shell().await["hasPendingUserInput"], true);
    assert_match(
        &resolved(&thread),
        json!([
            {"id": "normal-answer", "payload": {"requestId": "answered-question", "answers": answer["payload"]["answers"]}},
            {"turnId": "old-turn", "payload": {"requestId": "old-question"}},
        ]),
    );
}

#[tokio::test]
async fn keeps_streaming_while_an_async_question_is_pending() {
    let h = IngestionHarness::new(IngestionOptions {
        server_settings: Some(json!({"responseStreamingMode": "token"})),
        ..Default::default()
    })
    .await;
    h.emit(json!({"type": "turn.started", "eventId": "async-start", "turnId": "turn-async"}));
    h.emit(json!({"type": "content.delta", "eventId": "async-before", "turnId": "turn-async", "itemId": "message-1", "payload": {"streamKind": "assistant_text", "delta": "Before. "}}));
    h.emit(json!({
        "type": "user-input.requested", "eventId": "async-request", "turnId": "turn-async", "requestId": "codex-async:question-1",
        "payload": {"responseMode": "message", "questions": [{"id": "0", "header": "Question", "question": "Which name?", "options": [], "allowCustomAnswer": true}]},
    }));
    h.emit(json!({"type": "content.delta", "eventId": "async-after", "turnId": "turn-async", "itemId": "message-1", "payload": {"streamKind": "assistant_text", "delta": "After."}}));
    h.drain().await;
    let thread = h.thread().await;
    assert_eq!(session_status(&thread), "running");
    assert_match(&thread["messages"], json!([{"text": "Before. After.", "streaming": true}]));
    let request = find(&thread["activities"], |a| s(a, "kind") == "user-input.requested").unwrap();
    assert_match(&request["payload"], json!({"responseMode": "message", "requestId": "codex-async:question-1"}));
}

#[tokio::test]
async fn does_not_create_assistant_segments_for_whitespace_only_buffered_text_at_approval_boundaries() {
    let h = IngestionHarness::new(Default::default()).await;
    let turn = "turn-buffered-whitespace-request";
    h.emit(json!({"type": "turn.started", "eventId": "evt-turn-started-ws", "createdAt": "2026-03-28T06:28:00.000Z", "turnId": turn}));
    h.wait_for_thread(|t| session_status(t) == "running" && t["session"]["activeTurnId"] == turn)
        .await;
    h.emit(json!({"type": "content.delta", "eventId": "evt-message-delta-ws", "createdAt": "2026-03-28T06:28:00.000Z", "turnId": turn, "itemId": "item-buffered-whitespace-request", "payload": {"streamKind": "assistant_text", "delta": "\n\n\n"}}));
    h.emit(json!({
        "type": "request.opened", "eventId": "evt-request-opened-ws", "createdAt": "2026-03-28T06:28:01.000Z", "turnId": turn,
        "requestId": "req-buffered-whitespace-request", "payload": {"requestType": "command_execution_approval", "detail": "pwd"},
    }));
    let thread = h
        .wait_for_thread(|t| find(&t["activities"], |a| s(a, "kind") == "approval.requested").is_some())
        .await;
    assert!(message(&thread, "assistant:item-buffered-whitespace-request").is_none());
}

async fn new_segment_after_approval(mode: Option<&str>, item: &str, first: &str, second: &str) -> IngestionHarness {
    let h = IngestionHarness::new(IngestionOptions {
        server_settings: mode.map(|mode| json!({"responseStreamingMode": mode})),
        ..Default::default()
    })
    .await;
    let turn = format!("turn-{item}");
    h.emit(json!({"type": "turn.started", "eventId": format!("evt-turn-started-{item}"), "createdAt": "2026-03-28T06:07:00.000Z", "turnId": turn}));
    h.wait_for_thread(|t| session_status(t) == "running" && t["session"]["activeTurnId"] == turn.as_str())
        .await;
    h.emit(json!({"type": "content.delta", "eventId": format!("evt-delta-initial-{item}"), "createdAt": "2026-03-28T06:07:00.000Z", "turnId": turn, "itemId": item, "payload": {"streamKind": "assistant_text", "delta": first}}));
    h.emit(json!({
        "type": "request.opened", "eventId": format!("evt-request-opened-{item}"), "createdAt": "2026-03-28T06:07:01.000Z", "turnId": turn,
        "requestId": format!("req-{item}"), "payload": {"requestType": "command_execution_approval", "detail": "pwd"},
    }));
    let id = format!("assistant:{item}");
    h.wait_for_thread(|t| message(t, &id).is_some_and(|m| finished(m) && m["text"] == first)).await;
    h.emit(json!({"type": "content.delta", "eventId": format!("evt-delta-followup-{item}"), "createdAt": "2026-03-28T06:07:02.000Z", "turnId": turn, "itemId": item, "payload": {"streamKind": "assistant_text", "delta": second}}));
    h.emit(json!({"type": "item.completed", "eventId": format!("evt-completed-{item}"), "createdAt": "2026-03-28T06:07:03.000Z", "turnId": turn, "itemId": item, "payload": {"itemType": "assistant_message", "status": "completed"}}));
    let segment = format!("assistant:{item}:segment:1");
    let thread = h
        .wait_for_thread(|t| message(t, &segment).is_some_and(|m| finished(m) && m["text"] == second))
        .await;
    assert_eq!(message(&thread, &id).unwrap()["text"], first);
    assert_eq!(message(&thread, &id).unwrap()["streaming"], false);
    assert_eq!(message(&thread, &segment).unwrap()["text"], second);
    h
}

#[tokio::test]
async fn starts_a_new_buffered_assistant_message_segment_after_approval_and_completes_without_duplication() {
    let item = "item-buffered-request-append";
    let h = new_segment_after_approval(None, item, "first half", " second half").await;
    let events = h.events().await;
    let assistant: Vec<&Value> = events
        .iter()
        .filter(|event| event["type"] == "thread.message-sent" && s(&event["payload"], "messageId").starts_with(&format!("assistant:{item}")))
        .collect();
    assert_eq!(assistant.len(), 4);
    assert_eq!(assistant[0]["payload"]["streaming"], true);
    assert_eq!(assistant[0]["payload"]["text"], "first half");
    assert_eq!(assistant[1]["payload"]["streaming"], false);
    assert_eq!(assistant[1]["payload"]["text"], "");
    assert_eq!(assistant[2]["payload"]["messageId"], format!("assistant:{item}:segment:1"));
    assert_eq!(assistant[2]["payload"]["streaming"], true);
    assert_eq!(assistant[2]["payload"]["text"], " second half");
    assert_eq!(assistant[3]["payload"]["messageId"], format!("assistant:{item}:segment:1"));
    assert_eq!(assistant[3]["payload"]["streaming"], false);
    assert_eq!(assistant[3]["payload"]["text"], "");
}

#[tokio::test]
async fn starts_a_new_streaming_assistant_message_segment_after_approval() {
    new_segment_after_approval(Some("token"), "item-streaming-request-segment", "before approval", " after approval").await;
}

#[tokio::test]
async fn streams_assistant_deltas_when_thread_turn_start_requests_streaming_mode() {
    let h = IngestionHarness::new(IngestionOptions {
        server_settings: Some(json!({"responseStreamingMode": "token"})),
        ..Default::default()
    })
    .await;
    h.dispatch(turn_start("cmd-turn-start-streaming-mode", "message-streaming-mode", "stream please", NOW))
        .await;
    h.drain().await;
    h.emit(json!({"type": "turn.started", "eventId": "evt-turn-started-streaming-mode", "turnId": "turn-streaming-mode"}));
    h.wait_for_thread(|t| session_status(t) == "running" && t["session"]["activeTurnId"] == "turn-streaming-mode")
        .await;
    h.emit(json!({"type": "content.delta", "eventId": "evt-message-delta-streaming-mode", "turnId": "turn-streaming-mode", "itemId": "item-streaming-mode", "payload": {"streamKind": "assistant_text", "delta": "hello live"}}));
    let live = h
        .wait_for_thread(|t| message(t, "assistant:item-streaming-mode").is_some_and(|m| m["streaming"] == true && m["text"] == "hello live"))
        .await;
    assert_eq!(message(&live, "assistant:item-streaming-mode").unwrap()["streaming"], true);
    h.emit(json!({
        "type": "item.completed", "eventId": "evt-message-completed-streaming-mode", "turnId": "turn-streaming-mode", "itemId": "item-streaming-mode",
        "payload": {"itemType": "assistant_message", "status": "completed", "detail": "hello live"},
    }));
    let done = h.wait_for_thread(|t| message(t, "assistant:item-streaming-mode").is_some_and(finished)).await;
    let message = message(&done, "assistant:item-streaming-mode").unwrap();
    assert_eq!(message["text"], "hello live");
    assert_eq!(message["streaming"], false);
}
