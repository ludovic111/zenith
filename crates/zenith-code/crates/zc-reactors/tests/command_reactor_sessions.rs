//! Port of `ProviderCommandReactor.test.ts`: model options, session reuse and restarts,
//! provider switching, interrupts, approvals, user input, session stop and settlement.

mod common;

use common::command::*;
use common::*;
use serde_json::{json, Value};

fn session(thread: &Value) -> &Value {
    &thread["session"]
}

fn with_model(command_id: &str, message_id: &str, text: &str, selection: Value) -> Value {
    let mut command = turn_start_for("thread-1", command_id, message_id, text, NOW);
    command["modelSelection"] = selection;
    command
}

fn selection(instance: &str, model: &str, options: Value) -> Value {
    let mut value = json!({"instanceId": instance, "model": model});
    if options.as_array().is_some_and(|options| !options.is_empty()) {
        value["options"] = options;
    }
    value
}

async fn first_failure(h: &CommandHarness) -> Value {
    wait_for(|| async { find(&h.activities("thread-1").await, |a| s(a, "kind") == "provider.turn.start.failed").is_some() }).await;
    find(&h.activities("thread-1").await, |a| s(a, "kind") == "provider.turn.start.failed")
        .unwrap()
        .clone()
}

async fn forwards_options(instance: &str, model: &str, options: Value) {
    let thread_selection = (instance != "codex").then(|| json!({"instanceId": instance, "model": model}));
    let h = CommandHarness::new(CommandOptions {
        thread_model_selection: thread_selection,
        ..Default::default()
    })
    .await;
    let expected = selection(instance, model, options);
    h.dispatch(with_model(
        "cmd-turn-start-options",
        "user-message-options",
        "hello with options",
        expected.clone(),
    ))
    .await;
    wait_for(|| async { h.providers.start_session.count() == 1 }).await;
    wait_for(|| async { h.providers.send_turn.count() == 1 }).await;
    assert_match(&h.providers.start_session.call(0)[1], json!({"modelSelection": expected}));
    assert_match(&h.providers.send_turn.call(0), json!({"threadId": "thread-1", "modelSelection": expected}));
}

#[tokio::test]
async fn forwards_codex_model_options_through_session_start_and_turn_send() {
    forwards_options(
        "codex",
        "gpt-5.3-codex",
        json!([{"id": "reasoningEffort", "value": "high"}, {"id": "fastMode", "value": true}]),
    )
    .await;
}

#[tokio::test]
async fn forwards_claude_effort_options_through_session_start_and_turn_send() {
    forwards_options("claudeAgent", "claude-sonnet-4-6", json!([{"id": "effort", "value": "max"}])).await;
}

#[tokio::test]
async fn forwards_claude_fast_mode_options_through_session_start_and_turn_send() {
    forwards_options("claudeAgent", "claude-opus-4-6", json!([{"id": "fastMode", "value": true}])).await;
}

#[tokio::test]
async fn forwards_plan_interaction_mode_to_the_provider_turn_request() {
    let h = CommandHarness::new(Default::default()).await;
    h.dispatch(json!({"type": "thread.interaction-mode.set", "commandId": "cmd-interaction-mode-set-plan", "threadId": "thread-1", "interactionMode": "plan", "createdAt": NOW}))
        .await;
    let mut command = turn_start_for("thread-1", "cmd-turn-start-plan", "user-message-plan", "plan this change", NOW);
    command["interactionMode"] = json!("plan");
    h.dispatch(command).await;
    wait_for(|| async { h.providers.send_turn.count() == 1 }).await;
    assert_match(&h.providers.send_turn.call(0), json!({"threadId": "thread-1", "interactionMode": "plan"}));
}

#[tokio::test]
async fn preserves_the_active_session_model_when_in_session_model_switching_is_unsupported() {
    let h = CommandHarness::new(CommandOptions {
        unsupported_model_switch: true,
        ..Default::default()
    })
    .await;
    h.turn("cmd-turn-start-unsupported-1", "user-message-unsupported-1", "first", NOW).await;
    wait_for(|| async { h.providers.send_turn.count() == 1 }).await;
    h.turn("cmd-turn-start-unsupported-2", "user-message-unsupported-2", "second", NOW).await;
    wait_for(|| async { h.providers.send_turn.count() == 2 }).await;
    assert_match(
        &h.providers.send_turn.call(1),
        json!({"threadId": "thread-1", "modelSelection": {"instanceId": "codex", "model": "gpt-5-codex"}}),
    );
}

#[tokio::test]
async fn rejects_changing_models_after_start_when_the_provider_requires_a_new_thread() {
    let h = CommandHarness::new(CommandOptions {
        requires_new_thread_for_model_change: true,
        ..Default::default()
    })
    .await;
    h.turn("cmd-turn-start-restricted-1", "user-message-restricted-1", "first", NOW).await;
    wait_for(|| async { h.providers.send_turn.count() == 1 }).await;
    h.dispatch(with_model(
        "cmd-turn-start-restricted-2",
        "user-message-restricted-2",
        "second",
        json!({"instanceId": "codex", "model": "gpt-5.1-codex"}),
    ))
    .await;
    let failure = first_failure(&h).await;
    assert_eq!(h.providers.send_turn.count(), 1);
    assert!(s(&failure["payload"], "detail").contains("cannot switch models after the conversation has started"));
}

#[tokio::test]
async fn starts_a_first_turn_on_the_requested_provider_instance_even_when_it_differs_from_the_thread_model() {
    let h = CommandHarness::new(Default::default()).await;
    h.dispatch(with_model(
        "cmd-turn-start-provider-first",
        "user-message-provider-first",
        "hello claude",
        json!({"instanceId": "claudeAgent", "model": "claude-opus-4-6"}),
    ))
    .await;
    wait_for(|| async { h.providers.send_turn.count() == 1 }).await;
    assert_eq!(h.providers.start_session.count(), 1);
    assert_match(
        &h.providers.start_session.call(0)[1],
        json!({"provider": "claudeAgent", "providerInstanceId": "claudeAgent", "modelSelection": {"instanceId": "claudeAgent", "model": "claude-opus-4-6"}}),
    );
    let thread = h.thread("thread-1").await;
    assert_eq!(session(&thread)["providerName"], "claudeAgent");
    assert_eq!(session(&thread)["providerInstanceId"], "claudeAgent");
    assert!(find(&thread["activities"], |a| s(a, "kind") == "provider.turn.start.failed").is_none());
}

#[tokio::test]
async fn reuses_the_same_provider_session_when_runtime_mode_is_unchanged() {
    let h = CommandHarness::new(Default::default()).await;
    h.turn("cmd-turn-start-unchanged-1", "user-message-unchanged-1", "first", NOW).await;
    wait_for(|| async { h.providers.start_session.count() == 1 }).await;
    wait_for(|| async { h.providers.send_turn.count() == 1 }).await;
    h.turn("cmd-turn-start-unchanged-2", "user-message-unchanged-2", "second", NOW).await;
    wait_for(|| async { h.providers.send_turn.count() == 2 }).await;
    assert_eq!(h.providers.start_session.count(), 1);
    assert_eq!(h.providers.stop_session.count(), 0);
}

#[tokio::test]
async fn restarts_an_existing_codex_thread_on_a_compatible_requested_instance() {
    let h = CommandHarness::new(Default::default()).await;
    h.dispatch(with_model(
        "cmd-turn-start-compatible-codex-1",
        "user-message-compatible-codex-1",
        "first",
        json!({"instanceId": "codex", "model": "gpt-5-codex"}),
    ))
    .await;
    wait_for(|| async { h.providers.send_turn.count() == 1 }).await;
    h.dispatch(with_model(
        "cmd-turn-start-compatible-codex-2",
        "user-message-compatible-codex-2",
        "second",
        json!({"instanceId": "codex_work", "model": "gpt-5-codex"}),
    ))
    .await;
    wait_for(|| async { h.providers.send_turn.count() == 2 }).await;
    assert_eq!(h.providers.start_session.count(), 2);
    assert_match(
        &h.providers.start_session.call(1)[1],
        json!({"provider": "codex", "providerInstanceId": "codex_work", "resumeCursor": {"opaque": "resume-1"}}),
    );
    assert_eq!(session(&h.thread("thread-1").await)["providerInstanceId"], "codex_work");
}

#[tokio::test]
async fn restarts_the_provider_session_when_the_thread_workspace_changes() {
    let h = CommandHarness::new(CommandOptions {
        thread_model_selection: Some(json!({"instanceId": "claudeAgent", "model": "claude-sonnet-4-6"})),
        ..Default::default()
    })
    .await;
    h.turn("cmd-turn-start-workspace-1", "user-message-workspace-1", "first in project root", NOW)
        .await;
    wait_for(|| async { h.providers.start_session.count() == 1 }).await;
    wait_for(|| async { h.providers.send_turn.count() == 1 }).await;
    assert_match(&h.providers.start_session.call(0)[1], json!({"cwd": "/tmp/provider-project"}));
    h.dispatch(json!({"type": "thread.meta.update", "commandId": "cmd-thread-worktree-change", "threadId": "thread-1", "worktreePath": "/tmp/provider-project-worktree"}))
        .await;
    h.turn("cmd-turn-start-workspace-2", "user-message-workspace-2", "second in worktree", NOW)
        .await;
    wait_for(|| async { h.providers.start_session.count() == 2 }).await;
    wait_for(|| async { h.providers.send_turn.count() == 2 }).await;
    assert_eq!(h.providers.stop_session.count(), 0);
    assert_match(
        &h.providers.start_session.call(1)[1],
        json!({
            "threadId": "thread-1", "cwd": "/tmp/provider-project-worktree", "resumeCursor": {"opaque": "resume-1"},
            "modelSelection": {"instanceId": "claudeAgent", "model": "claude-sonnet-4-6"}, "runtimeMode": "approval-required",
        }),
    );
}

#[tokio::test]
async fn restarts_claude_sessions_when_claude_effort_changes() {
    let h = CommandHarness::new(CommandOptions {
        thread_model_selection: Some(json!({"instanceId": "claudeAgent", "model": "claude-sonnet-4-6"})),
        ..Default::default()
    })
    .await;
    h.dispatch(with_model(
        "cmd-turn-start-claude-effort-1",
        "user-message-claude-effort-1",
        "first claude turn",
        selection("claudeAgent", "claude-sonnet-4-6", json!([{"id": "effort", "value": "medium"}])),
    ))
    .await;
    wait_for(|| async { h.providers.start_session.count() == 1 }).await;
    wait_for(|| async { h.providers.send_turn.count() == 1 }).await;
    let max = selection("claudeAgent", "claude-sonnet-4-6", json!([{"id": "effort", "value": "max"}]));
    h.dispatch(with_model(
        "cmd-turn-start-claude-effort-2",
        "user-message-claude-effort-2",
        "second claude turn",
        max.clone(),
    ))
    .await;
    wait_for(|| async { h.providers.start_session.count() == 2 }).await;
    wait_for(|| async { h.providers.send_turn.count() == 2 }).await;
    assert_match(
        &h.providers.start_session.call(1)[1],
        json!({"resumeCursor": {"opaque": "resume-1"}, "modelSelection": max}),
    );
}

fn runtime_mode(command_id: &str, mode: &str) -> Value {
    json!({"type": "thread.runtime-mode.set", "commandId": command_id, "threadId": "thread-1", "runtimeMode": mode, "createdAt": NOW})
}

#[tokio::test]
async fn restarts_the_provider_session_when_runtime_mode_is_updated_on_the_thread() {
    let h = CommandHarness::new(Default::default()).await;
    h.dispatch(runtime_mode("cmd-runtime-mode-set-initial-full-access", "full-access")).await;
    let mut first = turn_start_for("thread-1", "cmd-turn-start-runtime-mode-1", "user-message-runtime-mode-1", "first", NOW);
    first["runtimeMode"] = json!("full-access");
    h.dispatch(first).await;
    wait_for(|| async { h.providers.start_session.count() == 1 }).await;
    wait_for(|| async { h.providers.send_turn.count() == 1 }).await;
    h.dispatch(runtime_mode("cmd-runtime-mode-set-1", "approval-required")).await;
    wait_for(|| async { h.thread("thread-1").await["runtimeMode"] == "approval-required" }).await;
    wait_for(|| async { h.providers.start_session.count() == 2 }).await;
    let mut second = turn_start_for("thread-1", "cmd-turn-start-runtime-mode-2", "user-message-runtime-mode-2", "second", NOW);
    second["runtimeMode"] = json!("full-access");
    h.dispatch(second).await;
    wait_for(|| async { h.providers.send_turn.count() == 2 }).await;
    assert_eq!(h.providers.stop_session.count(), 0);
    assert_match(
        &h.providers.start_session.call(1)[1],
        json!({"threadId": "thread-1", "resumeCursor": {"opaque": "resume-1"}, "runtimeMode": "approval-required"}),
    );
    assert_match(&h.providers.send_turn.call(1), json!({"threadId": "thread-1"}));
    let thread = h.thread("thread-1").await;
    assert_eq!(session(&thread)["threadId"], "thread-1");
    assert_eq!(session(&thread)["runtimeMode"], "approval-required");
}

#[tokio::test]
async fn does_not_inject_derived_model_options_when_restarting_claude_on_runtime_mode_changes() {
    let h = CommandHarness::new(CommandOptions {
        thread_model_selection: Some(json!({"instanceId": "claudeAgent", "model": "claude-opus-4-6"})),
        ..Default::default()
    })
    .await;
    h.dispatch(json!({
        "type": "thread.session.set", "commandId": "cmd-session-set-runtime-mode-claude", "threadId": "thread-1",
        "session": {"threadId": "thread-1", "status": "ready", "providerName": "claudeAgent", "runtimeMode": "full-access", "activeTurnId": null, "lastError": null, "updatedAt": NOW},
        "createdAt": NOW,
    }))
    .await;
    h.dispatch(runtime_mode("cmd-runtime-mode-set-claude-no-options", "approval-required")).await;
    wait_for(|| async { h.providers.start_session.count() == 1 }).await;
    assert_match(
        &h.providers.start_session.call(0)[1],
        json!({"modelSelection": {"instanceId": "claudeAgent", "model": "claude-opus-4-6"}, "runtimeMode": "approval-required"}),
    );
    assert!(h.providers.start_session.call(0)[1]["modelSelection"].get("options").is_none());
}

#[tokio::test]
async fn does_not_stop_the_active_session_when_restart_fails_before_rebind() {
    let h = CommandHarness::new(Default::default()).await;
    h.dispatch(runtime_mode("cmd-runtime-mode-set-initial-full-access-2", "full-access")).await;
    let mut first = turn_start_for("thread-1", "cmd-turn-start-restart-failure-1", "user-message-restart-failure-1", "first", NOW);
    first["runtimeMode"] = json!("full-access");
    h.dispatch(first).await;
    wait_for(|| async { h.providers.start_session.count() == 1 }).await;
    wait_for(|| async { h.providers.send_turn.count() == 1 }).await;
    h.providers
        .start_failures_once
        .lock()
        .unwrap()
        .push(zc_ports::TaggedError::new("UnknownError", "simulated restart failure"));
    h.dispatch(runtime_mode("cmd-runtime-mode-set-restart-failure", "approval-required")).await;
    wait_for(|| async { h.thread("thread-1").await["runtimeMode"] == "approval-required" }).await;
    wait_for(|| async { h.providers.start_session.count() == 2 }).await;
    h.drain().await;
    assert_eq!(h.providers.stop_session.count(), 0);
    assert_eq!(h.providers.send_turn.count(), 1);
    let thread = h.thread("thread-1").await;
    assert_eq!(session(&thread)["threadId"], "thread-1");
    assert_eq!(session(&thread)["runtimeMode"], "full-access");
}

#[tokio::test]
async fn rejects_provider_changes_after_a_thread_is_already_bound_to_a_session_provider() {
    let h = CommandHarness::new(Default::default()).await;
    h.turn("cmd-turn-start-provider-switch-1", "user-message-provider-switch-1", "first", NOW).await;
    wait_for(|| async { h.providers.start_session.count() == 1 }).await;
    wait_for(|| async { h.providers.send_turn.count() == 1 }).await;
    h.dispatch(with_model(
        "cmd-turn-start-provider-switch-2",
        "user-message-provider-switch-2",
        "second",
        json!({"instanceId": "claudeAgent", "model": "claude-opus-4-6"}),
    ))
    .await;
    let failure = first_failure(&h).await;
    assert_eq!(h.providers.start_session.count(), 1);
    assert_eq!(h.providers.send_turn.count(), 1);
    assert_eq!(h.providers.stop_session.count(), 0);
    let thread = h.thread("thread-1").await;
    assert_eq!(session(&thread)["threadId"], "thread-1");
    assert_eq!(session(&thread)["providerName"], "codex");
    assert_eq!(session(&thread)["runtimeMode"], "approval-required");
    assert!(s(&failure["payload"], "detail").contains("cannot switch to 'claudeAgent'"));
}

#[tokio::test]
async fn rejects_cross_driver_provider_changes_after_the_existing_thread_session_has_stopped() {
    let h = CommandHarness::new(Default::default()).await;
    h.dispatch(json!({
        "type": "thread.session.set", "commandId": "cmd-session-set-stopped-provider-switch", "threadId": "thread-1",
        "session": {"threadId": "thread-1", "status": "stopped", "providerName": "codex", "providerInstanceId": "codex", "runtimeMode": "approval-required",
            "activeTurnId": null, "lastError": null, "updatedAt": NOW},
        "createdAt": NOW,
    }))
    .await;
    h.dispatch(with_model(
        "cmd-turn-start-stopped-provider-switch",
        "user-message-stopped-provider-switch",
        "continue with claude",
        json!({"instanceId": "claudeAgent", "model": "claude-opus-4-6"}),
    ))
    .await;
    let failure = first_failure(&h).await;
    assert_eq!(h.providers.start_session.count(), 0);
    assert_eq!(h.providers.send_turn.count(), 0);
    assert!(s(&failure["payload"], "detail").contains("cannot switch to 'claudeAgent'"));
}

async fn running(h: &CommandHarness, command_id: &str, status: &str, turn: Value) {
    h.dispatch(json!({
        "type": "thread.session.set", "commandId": command_id, "threadId": "thread-1",
        "session": {"threadId": "thread-1", "status": status, "providerName": "codex", "runtimeMode": "approval-required", "activeTurnId": turn, "lastError": null, "updatedAt": NOW},
        "createdAt": NOW,
    }))
    .await;
}

#[tokio::test]
async fn reacts_to_thread_turn_interrupt_requested_by_calling_provider_interrupt() {
    let h = CommandHarness::new(Default::default()).await;
    running(&h, "cmd-session-set", "running", json!("turn-1")).await;
    h.dispatch(json!({"type": "thread.turn.interrupt", "commandId": "cmd-turn-interrupt", "threadId": "thread-1", "turnId": "turn-1", "createdAt": NOW}))
        .await;
    wait_for(|| async { h.providers.interrupt_turn.count() == 1 }).await;
    assert_eq!(h.providers.interrupt_turn.call(0), json!({"threadId": "thread-1"}));
}

fn fail_with(detail: &'static str, method: &'static str) -> Hook<Value, Result<(), zc_ports::TaggedError>> {
    hook(move |_| async move { Err(request_error("codex", method, detail)) })
}

#[tokio::test]
async fn stops_a_running_session_and_records_the_failure_when_provider_interrupt_fails() {
    let h = CommandHarness::new(Default::default()).await;
    *h.providers.interrupt_hook.lock().unwrap() = Some(fail_with("provider session disappeared", "thread.interrupt"));
    *h.providers.stop_hook.lock().unwrap() = Some(fail_with("provider process already exited", "session.stop"));
    running(&h, "cmd-session-set-interrupt-failure", "running", json!("turn-1")).await;
    h.dispatch(json!({"type": "thread.turn.interrupt", "commandId": "cmd-turn-interrupt-provider-failure", "threadId": "thread-1", "turnId": "turn-1", "createdAt": NOW}))
        .await;
    wait_for(|| async { session(&h.thread("thread-1").await)["status"] == "stopped" }).await;
    h.drain().await;
    let thread = h.thread("thread-1").await;
    assert_match(
        session(&thread),
        json!({"status": "stopped", "activeTurnId": null, "lastError": "provider session disappeared"}),
    );
    assert_match(
        find(&thread["activities"], |a| s(a, "kind") == "provider.turn.interrupt.failed").unwrap(),
        json!({"summary": "Provider turn interrupt failed", "payload": {"detail": "provider session disappeared"}}),
    );
    assert_eq!(h.providers.stop_session.calls(), vec![json!({"threadId": "thread-1"})]);
}

#[tokio::test]
async fn stops_a_starting_session_without_a_bound_turn_when_interrupt_fails() {
    let h = CommandHarness::new(Default::default()).await;
    *h.providers.interrupt_hook.lock().unwrap() = Some(fail_with("provider session disappeared", "thread.interrupt"));
    running(&h, "cmd-session-set-interrupt-starting", "starting", Value::Null).await;
    h.dispatch(json!({"type": "thread.turn.interrupt", "commandId": "cmd-turn-interrupt-starting-provider-failure", "threadId": "thread-1", "createdAt": NOW}))
        .await;
    h.drain().await;
    let thread = h.thread("thread-1").await;
    assert_match(
        session(&thread),
        json!({"status": "stopped", "activeTurnId": null, "lastError": "provider session disappeared"}),
    );
    assert_eq!(h.providers.stop_session.calls(), vec![json!({"threadId": "thread-1"})]);
    assert_match(
        find(&thread["activities"], |a| s(a, "kind") == "provider.turn.interrupt.failed").unwrap(),
        json!({"payload": {"detail": "provider session disappeared"}}),
    );
}

#[tokio::test]
async fn does_not_overwrite_a_session_that_became_ready_while_an_interrupt_failed() {
    let h = CommandHarness::new(Default::default()).await;
    let completed_at = "2026-01-01T00:00:01.000Z";
    running(&h, "cmd-session-set-interrupt-race", "running", json!("turn-1")).await;
    let engine = h.engine.clone();
    *h.providers.interrupt_hook.lock().unwrap() = Some(hook(move |_| {
        let engine = engine.clone();
        async move {
            dispatch(
                &*engine,
                json!({
                    "type": "thread.session.set", "commandId": "cmd-session-set-natural-completion", "threadId": "thread-1",
                    "session": {"threadId": "thread-1", "status": "ready", "providerName": "codex", "runtimeMode": "approval-required", "activeTurnId": null, "lastError": null, "updatedAt": completed_at},
                    "createdAt": completed_at,
                }),
            )
            .await
            .expect("natural completion");
            Err(request_error("codex", "thread.interrupt", "provider session disappeared"))
        }
    }));
    h.dispatch(json!({"type": "thread.turn.interrupt", "commandId": "cmd-turn-interrupt-race", "threadId": "thread-1", "turnId": "turn-1", "createdAt": NOW}))
        .await;
    h.drain().await;
    let thread = h.thread("thread-1").await;
    assert_match(
        session(&thread),
        json!({"status": "ready", "activeTurnId": null, "lastError": null, "updatedAt": completed_at}),
    );
    assert_eq!(h.providers.stop_session.count(), 0);
    assert!(find(&thread["activities"], |a| s(a, "kind") == "provider.turn.interrupt.failed").is_none());
}

#[tokio::test]
async fn starts_a_fresh_session_when_only_projected_session_state_exists() {
    let h = CommandHarness::new(Default::default()).await;
    running(&h, "cmd-session-set-stale", "ready", Value::Null).await;
    h.turn("cmd-turn-start-stale", "user-message-stale", "resume codex", NOW).await;
    wait_for(|| async { h.providers.start_session.count() == 1 }).await;
    wait_for(|| async { h.providers.send_turn.count() == 1 }).await;
    assert_match(
        &h.providers.start_session.call(0)[1],
        json!({"threadId": "thread-1", "modelSelection": {"instanceId": "codex", "model": "gpt-5-codex"}, "runtimeMode": "approval-required"}),
    );
    assert_match(&h.providers.send_turn.call(0), json!({"threadId": "thread-1"}));
}

#[tokio::test]
async fn rejects_active_runtime_sessions_that_are_missing_provider_instance_ids() {
    let h = CommandHarness::new(Default::default()).await;
    running(&h, "cmd-session-set-missing-instance", "ready", Value::Null).await;
    h.providers.sessions.lock().unwrap().push(json!({
        "provider": "codex", "status": "ready", "runtimeMode": "approval-required", "threadId": "thread-1", "cwd": "/tmp/provider-project",
        "resumeCursor": {"opaque": "resume-without-instance"}, "createdAt": NOW, "updatedAt": NOW,
    }));
    h.turn("cmd-turn-start-missing-instance", "user-message-missing-instance", "resume codex", NOW)
        .await;
    let failure = first_failure(&h).await;
    assert_eq!(h.providers.start_session.count(), 0);
    assert_eq!(h.providers.send_turn.count(), 0);
    assert!(s(&failure["payload"], "detail").contains("without a provider instance id"));
}

#[tokio::test]
async fn forwards_approval_responses() {
    let h = CommandHarness::new(Default::default()).await;
    running(&h, "cmd-session-set-for-approval", "running", Value::Null).await;
    h.dispatch(json!({"type": "thread.approval.respond", "commandId": "cmd-approval-respond", "threadId": "thread-1", "requestId": "approval-request-1", "decision": "accept", "createdAt": NOW}))
        .await;
    h.drain().await;
    assert_eq!(
        h.providers.respond_to_request.call(0),
        json!({"threadId": "thread-1", "requestId": "approval-request-1", "decision": "accept"})
    );
}

#[tokio::test]
async fn forwards_user_input_answers() {
    let h = CommandHarness::new(Default::default()).await;
    running(&h, "cmd-session-set-for-user-input", "running", Value::Null).await;
    h.dispatch(json!({
        "type": "thread.user-input.respond", "commandId": "cmd-user-input-respond", "threadId": "thread-1", "requestId": "user-input-request-1",
        "answers": {"sandbox_mode": "workspace-write"}, "createdAt": NOW,
    }))
    .await;
    h.drain().await;
    assert_eq!(
        h.providers.respond_to_user_input.call(0),
        json!({"threadId": "thread-1", "requestId": "user-input-request-1", "answers": {"sandbox_mode": "workspace-write"}})
    );
}

#[tokio::test]
async fn normalizes_stale_codex_approval_callbacks_without_faking_approval_resolution() {
    let h = CommandHarness::new(Default::default()).await;
    *h.providers.respond_hook.lock().unwrap() = Some(hook(|_| async {
        Err(request_error(
            "codex",
            "item/requestApproval/decision",
            "Unknown pending Codex approval request: approval-request-1",
        ))
    }));
    running(&h, "cmd-session-set-for-approval-error", "running", Value::Null).await;
    h.dispatch(json!({
        "type": "thread.activity.append", "commandId": "cmd-approval-requested", "threadId": "thread-1",
        "activity": {"id": "activity-approval-requested", "tone": "approval", "kind": "approval.requested", "summary": "Command approval requested",
            "payload": {"requestId": "approval-request-1", "requestKind": "command"}, "turnId": null, "createdAt": NOW},
        "createdAt": NOW,
    }))
    .await;
    h.dispatch(json!({"type": "thread.approval.respond", "commandId": "cmd-approval-respond-stale", "threadId": "thread-1", "requestId": "approval-request-1", "decision": "acceptForSession", "createdAt": NOW}))
        .await;
    wait_for(|| async { find(&h.activities("thread-1").await, |a| s(a, "kind") == "provider.approval.respond.failed").is_some() }).await;
    let activities = h.activities("thread-1").await;
    let failure = find(&activities, |a| s(a, "kind") == "provider.approval.respond.failed").unwrap();
    assert_eq!(failure["payload"]["requestId"], "approval-request-1");
    assert!(s(&failure["payload"], "detail").contains("Stale pending approval request: approval-request-1"));
    assert!(find(&activities, |a| s(a, "kind") == "approval.resolved"
        && a["payload"]["requestId"] == "approval-request-1")
    .is_none());
}

#[tokio::test]
async fn surfaces_non_resumable_provider_user_input_callbacks_as_stale_failures() {
    let h = CommandHarness::new(Default::default()).await;
    *h.providers.user_input_hook.lock().unwrap() = Some(hook(|_| async {
        Err(request_error(
            "claudeAgent",
            "item/tool/respondToUserInput",
            "Unknown pending Codex user input request: user-input-request-1",
        ))
    }));
    running(&h, "cmd-session-set-for-user-input-error", "running", Value::Null).await;
    h.dispatch(json!({
        "type": "thread.activity.append", "commandId": "cmd-user-input-requested", "threadId": "thread-1",
        "activity": {"id": "activity-user-input-requested", "tone": "info", "kind": "user-input.requested", "summary": "User input requested",
            "payload": {"requestId": "user-input-request-1", "questions": [{"id": "sandbox_mode", "header": "Sandbox", "question": "Which mode should be used?",
                "options": [{"label": "workspace-write", "description": "Allow workspace writes only"}]}]},
            "turnId": null, "createdAt": NOW},
        "createdAt": NOW,
    }))
    .await;
    h.dispatch(json!({
        "type": "thread.user-input.respond", "commandId": "cmd-user-input-respond-stale", "threadId": "thread-1", "requestId": "user-input-request-1",
        "answers": {"sandbox_mode": "workspace-write"}, "createdAt": NOW,
    }))
    .await;
    wait_for(|| async { find(&h.activities("thread-1").await, |a| s(a, "kind") == "provider.user-input.respond.failed").is_some() }).await;
    let activities = h.activities("thread-1").await;
    let failure = find(&activities, |a| s(a, "kind") == "provider.user-input.respond.failed").unwrap();
    assert_eq!(failure["payload"]["requestId"], "user-input-request-1");
    assert!(s(&failure["payload"], "detail").contains("Stale pending user-input request: user-input-request-1"));
    assert!(find(&activities, |a| s(a, "kind") == "user-input.resolved"
        && a["payload"]["requestId"] == "user-input-request-1")
    .is_none());
}

#[tokio::test]
async fn stops_a_provider_session() {
    let h = CommandHarness::new(Default::default()).await;
    h.dispatch(json!({
        "type": "thread.session.set", "commandId": "cmd-session-set-for-stop", "threadId": "thread-1",
        "session": {"threadId": "thread-1", "status": "ready", "providerName": "codex", "providerInstanceId": "codex_work", "runtimeMode": "approval-required",
            "activeTurnId": null, "lastError": null, "updatedAt": NOW},
        "createdAt": NOW,
    }))
    .await;
    h.dispatch(json!({"type": "thread.session.stop", "commandId": "cmd-session-stop", "threadId": "thread-1", "createdAt": NOW}))
        .await;
    h.drain().await;
    assert_eq!(h.providers.stop_session.calls(), vec![json!({"threadId": "thread-1"})]);
    let shell = h.shell("thread-1").await;
    assert_match(
        session(&shell),
        json!({"status": "stopped", "threadId": "thread-1", "providerInstanceId": "codex_work", "activeTurnId": null}),
    );
}

#[tokio::test]
async fn stops_a_ready_provider_session_after_automatic_settlement() {
    let h = CommandHarness::new(Default::default()).await;
    let stopped = Latch::new();
    let signal = stopped.clone();
    *h.providers.stop_hook.lock().unwrap() = Some(hook(move |_| {
        let signal = signal.clone();
        async move {
            signal.open();
            Ok(())
        }
    }));
    h.dispatch(json!({
        "type": "thread.session.set", "commandId": "cmd-session-set-for-auto-settle", "threadId": "thread-1",
        "session": {"threadId": "thread-1", "status": "ready", "providerName": "codex", "providerInstanceId": "codex_work", "runtimeMode": "approval-required",
            "activeTurnId": null, "lastError": null, "updatedAt": NOW},
        "createdAt": NOW,
    }))
    .await;
    let before = h.read_model().await;
    h.dispatch(json!({"type": "thread.auto-settle", "commandId": "cmd-auto-settle-with-session", "threadId": "thread-1", "snapshotSequence": before["snapshotSequence"], "settledAt": NOW}))
        .await;
    stopped.wait().await;
    h.drain().await;
    wait_for(|| async { session(&h.thread("thread-1").await)["status"] == "stopped" }).await;
    let thread = h.thread("thread-1").await;
    assert_eq!(thread["settledOverride"], "settled");
    assert_eq!(session(&thread)["providerInstanceId"], "codex_work");
    assert_eq!(h.terminals.close_idle.calls(), vec![json!({"threadId": "thread-1"})]);
}

#[tokio::test]
async fn closes_idle_terminals_when_a_thread_without_a_session_settles() {
    let h = CommandHarness::new(Default::default()).await;
    let closed = Latch::new();
    let signal = closed.clone();
    *h.terminals.close_idle_hook.lock().unwrap() = Some(hook(move |_| {
        let signal = signal.clone();
        async move { signal.open() }
    }));
    h.dispatch(json!({"type": "thread.settle", "commandId": "cmd-settle-without-session", "threadId": "thread-1"}))
        .await;
    closed.wait().await;
    h.drain().await;
    assert_eq!(h.terminals.close_idle.calls(), vec![json!({"threadId": "thread-1"})]);
    assert_eq!(h.providers.stop_session.count(), 0);
}

#[tokio::test]
async fn keeps_terminals_when_the_thread_is_unsettled_before_its_settle_event_runs() {
    let h = CommandHarness::new(Default::default()).await;
    let started = Latch::new();
    let release = Latch::new();
    let (signal, gate) = (started.clone(), release.clone());
    h.terminals.close_idle_once.lock().unwrap().push(hook(move |_| {
        let (signal, gate) = (signal.clone(), gate.clone());
        async move {
            signal.open();
            gate.wait().await;
        }
    }));
    h.dispatch(json!({"type": "thread.settle", "commandId": "cmd-settle-first", "threadId": "thread-1"}))
        .await;
    started.wait().await;
    h.dispatch(json!({"type": "thread.unsettle", "commandId": "cmd-unsettle-first", "threadId": "thread-1", "reason": "user"}))
        .await;
    h.dispatch(json!({"type": "thread.settle", "commandId": "cmd-settle-second", "threadId": "thread-1"}))
        .await;
    h.dispatch(json!({"type": "thread.unsettle", "commandId": "cmd-unsettle-second", "threadId": "thread-1", "reason": "user"}))
        .await;
    release.open();
    h.drain().await;
    assert_eq!(h.terminals.close_idle.count(), 1);
}
