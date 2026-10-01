//! Port of `ProviderCommandReactor.test.ts`: provider error attribution, sign-out commands,
//! turn starts, compaction queueing, startup failures, first-turn titles.
//!
//! The TS "unreadableHistory" option corrupts a SQL projection row to prove the reactor never
//! decodes unrelated message bodies; the event-log reads used here have no such rows, so those
//! tests run without it.

mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use common::command::*;
use common::*;
use serde_json::{json, Value};
use zc_ports::text_generation::ThreadTitleGenerationResult;
use zc_reactors::command_reactor::provider_error_label_from_instance_hint;

fn session(thread: &Value) -> &Value {
    &thread["session"]
}

fn kinds<'a>(activities: &'a Value, kind: &str) -> Vec<&'a Value> {
    filter(activities, |a| s(a, "kind") == kind)
}

#[test]
fn uses_the_current_provider_instance_slug_when_current_instance_lookup_fails() {
    assert_eq!(
        provider_error_label_from_instance_hint(Some("codex_personal"), Some("codex"), Some("codex")),
        "codex_personal"
    );
}

#[test]
fn uses_the_desired_provider_instance_slug_when_desired_instance_lookup_fails() {
    assert_eq!(
        provider_error_label_from_instance_hint(Some("claude_openrouter"), None, None),
        "claude_openrouter"
    );
}

async fn handles_sign_out(status: &str) {
    let instance_id = "antigravity-personal";
    let handled = Latch::new();
    let h = CommandHarness::new(CommandOptions {
        thread_model_selection: (status != "new").then(|| json!({"instanceId": instance_id, "model": "gemini-3.1-pro"})),
        ..Default::default()
    })
    .await;
    let signal = handled.clone();
    *h.auth.hook.lock().unwrap() = Some(hook(move |_| {
        let signal = signal.clone();
        async move {
            signal.open();
            Ok(true)
        }
    }));
    if status != "new" {
        h.dispatch(json!({
            "type": "thread.session.set", "commandId": "cmd-sign-out-bound-session", "threadId": "thread-1",
            "session": {"threadId": "thread-1", "providerInstanceId": instance_id, "providerName": "antigravity", "status": status,
                "runtimeMode": "approval-required", "activeTurnId": null, "lastError": null, "updatedAt": NOW},
            "createdAt": NOW,
        }))
        .await;
    }
    let missing = h.state_dir.path().join("missing-worktree").to_string_lossy().into_owned();
    h.dispatch(json!({"type": "thread.meta.update", "commandId": "cmd-sign-out-worktree", "threadId": "thread-1", "title": "New thread", "branch": "t3code/1234abcd", "worktreePath": missing}))
        .await;
    let mut command = turn_start_for("thread-1", "cmd-provider-sign-out", "message-provider-sign-out", "/logout", NOW);
    command["modelSelection"] = json!({"instanceId": if status == "new" { instance_id } else { "antigravity-other" }, "model": "gemini-3.1-pro"});
    h.dispatch(command).await;
    handled.wait().await;
    h.drain().await;

    let thread = h.thread("thread-1").await;
    assert_match(
        session(&thread),
        json!({"status": "stopped", "providerName": "antigravity", "providerInstanceId": instance_id, "activeTurnId": null, "lastError": null}),
    );
    let texts: Vec<&str> = thread["messages"].as_array().unwrap().iter().map(|m| s(m, "text")).collect();
    assert_eq!(texts, ["/logout"]);
    assert_contains(
        &thread["activities"],
        json!({"kind": "provider.auth.signed-out", "tone": "info", "turnId": null}),
    );
    assert!(h.pending_turn_starts().await.is_empty());
    assert_eq!(
        h.auth.calls.calls(),
        vec![json!({"instanceId": instance_id, "text": "/logout", "hasAttachments": false})]
    );
    assert_eq!(h.git.prune_worktrees.count(), 0);
    assert_eq!(h.git.create_worktree.count(), 0);
    assert_eq!(h.text.title_calls.count(), 0);
    assert_eq!(h.text.branch_calls.count(), 0);
    assert_eq!(h.providers.start_session.count(), 0);
    assert_eq!(h.providers.send_turn.count(), 0);
}

#[tokio::test]
async fn handles_sign_out_for_a_new_thread_before_worktree_repair_text_helpers_or_startup() {
    handles_sign_out("new").await;
}

#[tokio::test]
async fn handles_sign_out_for_a_ready_thread_before_worktree_repair_text_helpers_or_startup() {
    handles_sign_out("ready").await;
}

#[tokio::test]
async fn handles_sign_out_for_a_stopped_thread_before_worktree_repair_text_helpers_or_startup() {
    handles_sign_out("stopped").await;
}

#[tokio::test]
async fn clears_a_failed_sign_out_request_without_sending_it_as_a_prompt() {
    let instance_id = "antigravity-personal";
    let h = CommandHarness::new(CommandOptions {
        thread_model_selection: Some(json!({"instanceId": instance_id, "model": "gemini-3.1-pro"})),
        ..Default::default()
    })
    .await;
    let handled = Latch::new();
    let signal = handled.clone();
    *h.auth.hook.lock().unwrap() = Some(hook(move |_| {
        let signal = signal.clone();
        async move {
            signal.open();
            Err(zc_ports::TaggedError::new("ProviderSetupError", "The provider could not sign out. Try again.")
                .with("instanceId", "antigravity-personal")
                .with("operation", "logout")
                .with("detail", "The provider could not sign out. Try again."))
        }
    }));
    h.turn("cmd-provider-sign-out-failed", "message-provider-sign-out-failed", "/logout", NOW).await;
    handled.wait().await;
    h.drain().await;
    let thread = h.thread("thread-1").await;
    assert_match(session(&thread), json!({"status": "error", "activeTurnId": null}));
    assert!(s(session(&thread), "lastError").contains("The provider could not sign out. Try again."));
    assert_contains(&thread["activities"], json!({"kind": "provider.turn.start.failed", "tone": "error"}));
    assert!(kinds(&thread["activities"], "provider.auth.signed-out").is_empty());
    assert!(h.pending_turn_starts().await.is_empty());
    assert_eq!(h.providers.start_session.count(), 0);
    assert_eq!(h.providers.send_turn.count(), 0);
}

async fn sends_unhandled(text: &str, attachments: Value) {
    let h = CommandHarness::new(Default::default()).await;
    let started = Latch::new();
    let signal = started.clone();
    *h.providers.start_hook.lock().unwrap() = Some(hook(move |session: Value| {
        let signal = signal.clone();
        async move {
            signal.open();
            Ok(session)
        }
    }));
    let mut command = turn_start_for("thread-1", "cmd-provider-command-unhandled", "message-provider-command-unhandled", text, NOW);
    command["message"]["attachments"] = attachments.clone();
    h.dispatch(command).await;
    started.wait().await;
    h.drain().await;
    let has_attachments = attachments.as_array().is_some_and(|items| !items.is_empty());
    assert_eq!(
        h.auth.calls.calls(),
        vec![json!({"instanceId": "codex", "text": text, "hasAttachments": has_attachments})]
    );
    wait_for(|| async { h.providers.send_turn.count() == 1 }).await;
    let mut expected = json!({"input": text});
    if has_attachments {
        expected["attachments"] = attachments;
    }
    assert_match(&h.providers.send_turn.call(0), expected);
}

#[tokio::test]
async fn sends_a_command_mention_when_the_provider_auth_handler_leaves_it_unhandled() {
    sends_unhandled("What does /logout do?", json!([])).await;
}

#[tokio::test]
async fn sends_a_command_with_an_attachment_when_the_provider_auth_handler_leaves_it_unhandled() {
    sends_unhandled(
        "/logout",
        json!([{"type": "file", "id": "attached-notes", "name": "notes.txt", "mimeType": "text/plain", "sizeBytes": 8}]),
    )
    .await;
}

#[tokio::test]
async fn sends_another_providers_command_when_the_provider_auth_handler_leaves_it_unhandled() {
    sends_unhandled("/logout", json!([])).await;
}

#[tokio::test]
async fn reacts_to_thread_turn_start_by_ensuring_session_and_sending_provider_turn() {
    let h = CommandHarness::new(Default::default()).await;
    h.turn("cmd-turn-start-1", "user-message-1", "hello reactor", NOW).await;
    wait_for(|| async { h.providers.start_session.count() == 1 }).await;
    wait_for(|| async { h.providers.send_turn.count() == 1 }).await;
    let start = h.providers.start_session.call(0);
    assert_eq!(start[0], "thread-1");
    assert_match(
        &start[1],
        json!({"cwd": "/tmp/provider-project", "modelSelection": {"instanceId": "codex", "model": "gpt-5-codex"}, "runtimeMode": "approval-required"}),
    );
    let thread = h.thread("thread-1").await;
    assert_eq!(session(&thread)["threadId"], "thread-1");
    assert_eq!(session(&thread)["status"], "starting");
    assert_eq!(session(&thread)["runtimeMode"], "approval-required");
    assert!(start[1].get("title").is_none());
}

#[tokio::test]
async fn forwards_only_a_user_renamed_title_when_starting_a_provider_session() {
    let h = CommandHarness::new(CommandOptions {
        initial_title: Some("Add a progressive blur as you scroll".into()),
        ..Default::default()
    })
    .await;
    let start_turn = |thread_id: &str, text: &str, seed: &str| {
        let mut command = turn_start_for(thread_id, &format!("cmd-title-{thread_id}"), &format!("message-{thread_id}"), text, NOW);
        command["titleSeed"] = json!(seed);
        command
    };
    let create = |thread_id: &str, command: &str| {
        json!({
            "type": "thread.create", "commandId": command, "threadId": thread_id, "projectId": "project-1", "title": "New thread",
            "modelSelection": {"instanceId": "codex", "model": "gpt-5-codex"}, "interactionMode": "default", "runtimeMode": "approval-required",
            "branch": null, "worktreePath": null, "createdAt": NOW,
        })
    };
    h.dispatch(start_turn(
        "thread-1",
        "Add a progressive blur as you scroll",
        "Add a progressive blur as you scroll",
    ))
    .await;
    wait_for(|| async { h.providers.start_session.count() == 1 }).await;
    assert!(h.providers.start_session.call(0)[1].get("title").is_none());

    h.dispatch(create("thread-renamed", "cmd-thread-create-renamed")).await;
    h.dispatch(json!({"type": "thread.meta.update", "commandId": "cmd-thread-rename", "threadId": "thread-renamed", "title": "Keep this name"}))
        .await;
    h.dispatch(start_turn("thread-renamed", "hello there", "hello there")).await;
    wait_for(|| async { h.providers.start_session.count() == 2 }).await;
    assert_match(&h.providers.start_session.call(1)[1], json!({"title": "Keep this name"}));

    h.dispatch(create("thread-seeded", "cmd-thread-create-seeded")).await;
    h.dispatch(json!({"type": "thread.meta.update", "commandId": "cmd-thread-autotitle", "threadId": "thread-seeded", "title": "hello there"}))
        .await;
    h.dispatch(start_turn("thread-seeded", "hello there", "hello there")).await;
    wait_for(|| async { h.providers.start_session.count() == 3 }).await;
    assert!(h.providers.start_session.call(2)[1].get("title").is_none());
}

#[tokio::test]
async fn projects_inline_context_before_sending_the_provider_turn() {
    let h = CommandHarness::new(Default::default()).await;
    let mut command = turn_start_for(
        "thread-1",
        "cmd-turn-start-with-context",
        "user-message-with-context",
        "Inspect [build](t3-context://v1/terminal/terminal-1)",
        NOW,
    );
    command["message"]["context"] = json!({
        "version": 1,
        "records": [{"version": 1, "kind": "terminal", "contextId": "terminal-1", "label": "build", "terminalId": "terminal-1",
            "terminalLabel": "Build", "lineStart": 7, "lineEnd": 7, "text": "compiled successfully"}],
    });
    h.dispatch(command).await;
    wait_for(|| async { h.providers.send_turn.count() == 1 }).await;
    let input = s(&h.providers.send_turn.call(0), "input").to_owned();
    assert!(input.contains("[Terminal: build; ref=terminal-1]"), "{input}");
    assert!(input.contains("<context kind=\"terminal\" id=\"terminal-1\">"), "{input}");
}

#[tokio::test]
async fn retains_a_turn_dispatched_immediately_after_start_until_activation() {
    let activation = Latch::new();
    let h = CommandHarness::new(CommandOptions {
        activation: Some(activation.clone()),
        ..Default::default()
    })
    .await;
    let started = Latch::new();
    let signal = started.clone();
    *h.providers.start_hook.lock().unwrap() = Some(hook(move |session: Value| {
        let signal = signal.clone();
        async move {
            signal.open();
            Ok(session)
        }
    }));
    h.turn("cmd-turn-start-before-activation", "message-before-activation", "Start after activation", NOW)
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert!(!started.is_open());
    activation.open();
    started.wait().await;
    h.drain().await;
    assert_eq!(h.providers.start_session.call(0)[0], "thread-1");
    wait_for(|| async { h.providers.send_turn.count() == 1 }).await;
    assert_match(
        &h.providers.send_turn.call(0),
        json!({"threadId": "thread-1", "input": "Start after activation"}),
    );
}

#[tokio::test]
async fn starts_a_turn_and_generates_its_title() {
    let h = CommandHarness::new(Default::default()).await;
    let generated = Latch::new();
    let signal = generated.clone();
    *h.text.title_hook.lock().unwrap() = Some(hook(move |_| {
        let signal = signal.clone();
        async move {
            signal.open();
            Ok(ThreadTitleGenerationResult {
                title: "Generated title".into(),
                needs_refinement: None,
            })
        }
    }));
    let mut command = turn_start_for(
        "thread-1",
        "cmd-turn-start-with-old-history",
        "message-turn-start-with-old-history",
        "Use the current message",
        "2026-01-01T00:00:01.000Z",
    );
    command["titleSeed"] = json!("Thread");
    h.dispatch(command).await;
    generated.wait().await;
    wait_for(|| async { h.providers.send_turn.count() == 1 }).await;
    assert_match(&h.providers.send_turn.call(0), json!({"input": "Use the current message"}));
    assert_match(&h.text.title_calls.call(0), json!({"message": "Use the current message"}));
}

#[tokio::test]
async fn rejects_compact_without_conversation_context() {
    let h = CommandHarness::new(Default::default()).await;
    h.turn("cmd-empty-compact", "user-message-empty-compact", "/compact", NOW).await;
    h.drain().await;
    assert_eq!(h.providers.compact_thread.count(), 0);
}

async fn queues_messages_until_compaction_restores(scenario: &str) {
    let stop_before_resume = scenario == "stop before resume";
    let h = CommandHarness::new(Default::default()).await;
    let ready_started = Latch::new();
    let release_ready = Latch::new();
    let first_sent = Latch::new();
    let queued_sent = Latch::new();
    let resume_started = Latch::new();
    let release_resume = Latch::new();
    let resume_dispatched = Latch::new();
    let queued_send_started = Latch::new();
    let release_queued_send = Latch::new();
    let block_ready = Arc::new(AtomicBool::new(false));
    if stop_before_resume {
        let (started, release) = (resume_started.clone(), release_resume.clone());
        *h.engine.before_replay.lock().unwrap() = Some(hook(move |_| {
            let (started, release) = (started.clone(), release.clone());
            async move {
                started.open();
                release.wait().await;
            }
        }));
    }
    let dispatched = resume_dispatched.clone();
    *h.engine.after_replay.lock().unwrap() = Some(hook(move |_| {
        let dispatched = dispatched.clone();
        async move { dispatched.open() }
    }));
    {
        let (block, started, release) = (block_ready.clone(), ready_started.clone(), release_ready.clone());
        *h.engine.before_ready.lock().unwrap() = Some(hook(move |_| {
            let (block, started, release) = (block.clone(), started.clone(), release.clone());
            async move {
                if block.load(Ordering::SeqCst) {
                    started.open();
                    release.wait().await;
                }
            }
        }));
    }
    let sent_count = Arc::new(AtomicUsize::new(0));
    {
        let (count, first, queued, send_started, release_send) = (
            sent_count.clone(),
            first_sent.clone(),
            queued_sent.clone(),
            queued_send_started.clone(),
            release_queued_send.clone(),
        );
        let stop_after_send = scenario == "stop after send";
        *h.providers.send_hook.lock().unwrap() = Some(hook(move |_| {
            let (count, first, queued, send_started, release_send) = (count.clone(), first.clone(), queued.clone(), send_started.clone(), release_send.clone());
            async move {
                let n = count.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 {
                    first.open();
                } else if n == 2 && stop_after_send {
                    send_started.open();
                    release_send.wait().await;
                } else if n == 3 {
                    queued.open();
                }
                Ok(())
            }
        }));
    }
    h.turn("cmd-before-blocked-compact", "user-message-before-blocked-compact", "hello", NOW).await;
    first_sent.wait().await;
    h.dispatch(json!({
        "type": "thread.session.set", "commandId": "cmd-session-ready-before-blocked-compact", "threadId": "thread-1",
        "session": {"threadId": "thread-1", "status": "ready", "providerName": "codex", "providerInstanceId": "codex", "runtimeMode": "approval-required",
            "activeTurnId": null, "lastError": null, "updatedAt": NOW},
        "createdAt": NOW,
    }))
    .await;
    block_ready.store(true, Ordering::SeqCst);
    h.turn("cmd-blocked-compact", "user-message-blocked-compact", "/compact", "2026-01-01T00:00:01.000Z")
        .await;
    ready_started.wait().await;
    h.dispatch(json!({"type": "thread.interaction-mode.set", "commandId": "cmd-queued-mode-plan", "threadId": "thread-1", "interactionMode": "plan", "createdAt": NOW}))
        .await;
    h.turn(
        "cmd-during-compact-recovery",
        "user-message-during-compact-recovery",
        "first queued",
        "2026-01-01T00:00:02.000Z",
    )
    .await;
    h.dispatch(json!({"type": "thread.interaction-mode.set", "commandId": "cmd-queued-mode-default", "threadId": "thread-1", "interactionMode": "default", "createdAt": NOW}))
        .await;
    h.turn(
        "cmd-during-compact-recovery-2",
        "user-message-during-compact-recovery-2",
        "second queued",
        "2026-01-01T00:00:03.000Z",
    )
    .await;
    h.drain().await;
    assert_eq!(h.providers.send_turn.count(), 1);
    assert!(kinds(&h.activities("thread-1").await, "provider.turn.start.failed").is_empty());
    assert_eq!(h.pending_turn_starts().await, vec!["thread-1".to_owned()]);

    release_ready.open();
    let not_sent = |activities: &Value| -> Vec<Value> {
        filter(activities, |a| s(a, "summary") == "Queued message was not sent")
            .into_iter()
            .cloned()
            .collect()
    };
    if scenario == "stop after send" {
        queued_send_started.wait().await;
        h.dispatch(
            json!({"type": "thread.session.stop", "commandId": "cmd-stop-after-queued-send", "threadId": "thread-1", "createdAt": "2026-01-01T00:00:04.000Z"}),
        )
        .await;
        h.drain().await;
        let thread = h.thread("thread-1").await;
        assert_eq!(session(&thread)["status"], "stopped");
        let failures = not_sent(&thread["activities"]);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0]["payload"]["requestId"], "user-message-during-compact-recovery-2");
        assert!(failures[0]["payload"]["detail"].is_string());
        assert_eq!(failures[0]["payload"].as_object().unwrap().len(), 2);
        assert_eq!(h.providers.send_turn.count(), 2);
        release_queued_send.open();
        return;
    }
    if stop_before_resume {
        resume_started.wait().await;
        h.turn(
            "cmd-compact-during-resume",
            "user-message-compact-during-resume",
            "/compact",
            "2026-01-01T00:00:04.000Z",
        )
        .await;
        h.drain().await;
        assert_eq!(h.providers.compact_thread.count(), 1);
        h.dispatch(json!({"type": "thread.session.stop", "commandId": "cmd-stop-before-queued-resume", "threadId": "thread-1", "createdAt": "2026-01-01T00:00:04.000Z"}))
            .await;
        h.drain().await;
        release_resume.open();
        resume_dispatched.wait().await;
        h.drain().await;
        assert_eq!(h.providers.send_turn.count(), 1);
        let thread = h.thread("thread-1").await;
        assert_eq!(session(&thread)["status"], "stopped");
        assert!(h.pending_turn_starts().await.is_empty());
        assert_eq!(not_sent(&thread["activities"]).len(), 2);
        return;
    }
    queued_sent.wait().await;
    let sends: Vec<Value> = h.providers.send_turn.calls().into_iter().skip(1).collect();
    assert_match(&sends[0], json!({"input": "first queued", "interactionMode": "plan"}));
    assert_match(&sends[1], json!({"input": "second queued", "interactionMode": "default"}));
    assert_eq!(sends.len(), 2);
    let thread = h.thread("thread-1").await;
    for text in ["first queued", "second queued"] {
        assert_eq!(filter(&thread["messages"], |m| s(m, "text") == text).len(), 1);
    }
}

#[tokio::test]
async fn queues_messages_until_compaction_restores_the_session_resume() {
    queues_messages_until_compaction_restores("resume").await;
}

#[tokio::test]
async fn queues_messages_until_compaction_restores_the_session_stop_before_resume() {
    queues_messages_until_compaction_restores("stop before resume").await;
}

#[tokio::test]
async fn queues_messages_until_compaction_restores_the_session_stop_after_send() {
    queues_messages_until_compaction_restores("stop after send").await;
}

#[tokio::test]
async fn does_not_overwrite_concurrent_session_state_after_compaction_failure() {
    let h = CommandHarness::new(Default::default()).await;
    let release_compaction = Latch::new();
    let release_running_compaction = Latch::new();
    let release_failed_stop = Latch::new();
    let compactions = Arc::new(AtomicUsize::new(0));
    {
        let (count, first, second) = (compactions.clone(), release_compaction.clone(), release_running_compaction.clone());
        *h.providers.compact_hook.lock().unwrap() = Some(hook(move |_| {
            let (count, first, second) = (count.clone(), first.clone(), second.clone());
            async move {
                if count.fetch_add(1, Ordering::SeqCst) == 0 {
                    first.wait().await;
                } else {
                    second.wait().await;
                }
                Err(zc_ports::TaggedError::new("Defect", "Compaction stopped"))
            }
        }));
        let release = release_failed_stop.clone();
        *h.providers.stop_hook.lock().unwrap() = Some(hook(move |_| {
            let release = release.clone();
            async move {
                release.wait().await;
                Err(request_error("codex", "session.stop", "provider stop failed"))
            }
        }));
    }
    let compact = |suffix: &str, at: &str| {
        turn_start_for(
            "thread-1",
            &format!("cmd-compact-{suffix}"),
            &format!("user-message-compact-{suffix}"),
            "/compact",
            at,
        )
    };
    h.turn("cmd-message-before-compact", "user-message-before-compact", "hello", NOW).await;
    wait_for(|| async { h.providers.send_turn.count() == 1 }).await;
    h.dispatch(json!({
        "type": "thread.session.set", "commandId": "cmd-session-ready-before-compact", "threadId": "thread-1",
        "session": {"threadId": "thread-1", "status": "ready", "providerName": "codex", "providerInstanceId": "codex", "runtimeMode": "approval-required",
            "activeTurnId": null, "lastError": null, "updatedAt": NOW},
        "createdAt": NOW,
    }))
    .await;
    h.dispatch(compact("before-stop", NOW)).await;
    wait_for(|| async { h.providers.compact_thread.count() == 1 }).await;
    assert_eq!(session(&h.thread("thread-1").await)["status"], "starting");
    h.turn(
        "cmd-queued-before-stop",
        "user-message-queued-before-stop",
        "do not restart after stopping",
        NOW,
    )
    .await;
    h.drain().await;
    h.dispatch(json!({"type": "thread.session.stop", "commandId": "cmd-stop-during-compact", "threadId": "thread-1", "createdAt": "2026-01-01T00:00:01.000Z"}))
        .await;
    wait_for(|| async { h.providers.stop_session.count() == 1 }).await;
    release_compaction.open();
    wait_for(|| async { find(&h.activities("thread-1").await, |a| s(a, "summary") == "Context compaction failed").is_some() }).await;
    assert_eq!(session(&h.thread("thread-1").await)["status"], "starting");
    release_failed_stop.open();
    h.drain().await;
    wait_for(|| async { find(&h.activities("thread-1").await, |a| s(a, "kind") == "provider.session.stop.failed").is_some() }).await;

    let recovered = h.thread("thread-1").await;
    assert_eq!(session(&recovered)["status"], "ready");
    assert_eq!(h.providers.send_turn.count(), 1);
    assert_match(
        find(&recovered["activities"], |a| s(a, "summary") == "Queued message was not sent").unwrap(),
        json!({"payload": {"requestId": "user-message-queued-before-stop"}}),
    );
    assert_match(
        find(&recovered["activities"], |a| s(a, "kind") == "provider.session.stop.failed").unwrap(),
        json!({"summary": "Provider session stop failed", "payload": {"detail": "provider stop failed"}}),
    );

    h.dispatch(compact("before-running", "2026-01-01T00:00:02.000Z")).await;
    wait_for(|| async { h.providers.compact_thread.count() == 2 }).await;
    h.dispatch(json!({"type": "thread.session.stop", "commandId": "cmd-failed-stop-before-compaction-settles", "threadId": "thread-1", "createdAt": "2026-01-01T00:00:02.500Z"}))
        .await;
    wait_for(|| async { kinds(&h.activities("thread-1").await, "provider.session.stop.failed").len() == 2 }).await;
    let restarted = h.thread("thread-1").await;
    assert_eq!(session(&restarted)["status"], "starting");
    let mut running = session(&restarted).clone();
    running["status"] = json!("running");
    running["activeTurnId"] = json!("compaction-turn");
    running["updatedAt"] = json!("2026-01-01T00:00:03.000Z");
    h.dispatch(json!({"type": "thread.session.set", "commandId": "cmd-running-during-compact", "threadId": "thread-1", "session": running, "createdAt": "2026-01-01T00:00:03.000Z"}))
        .await;
    release_running_compaction.open();
    h.drain().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    h.drain().await;
    assert_eq!(session(&h.thread("thread-1").await)["status"], "running");
}

#[tokio::test]
async fn projects_starting_before_a_slow_provider_session_finishes() {
    let h = CommandHarness::new(Default::default()).await;
    let release = Latch::new();
    let gate = release.clone();
    *h.providers.start_hook.lock().unwrap() = Some(hook(move |session: Value| {
        let gate = gate.clone();
        async move {
            gate.wait().await;
            Ok(session)
        }
    }));
    h.turn("cmd-turn-start-slow-provider", "user-message-slow-provider", "start slowly", NOW).await;
    wait_for(|| async { h.providers.start_session.count() == 1 }).await;
    assert_eq!(session(&h.thread("thread-1").await)["status"], "starting");
    assert_eq!(h.providers.send_turn.count(), 0);
    release.open();
    wait_for(|| async { h.providers.send_turn.count() == 1 }).await;
}

#[tokio::test]
async fn shows_the_missing_workspace_message_without_a_provider_stack_trace() {
    let h = CommandHarness::new(Default::default()).await;
    let message = "Provider workspace is missing for thread 'thread-1': /missing/project/worktree";
    let attempted = Latch::new();
    let signal = attempted.clone();
    *h.providers.start_hook.lock().unwrap() = Some(hook(move |_| {
        let signal = signal.clone();
        async move {
            signal.open();
            Err(zc_ports::TaggedError::new("ProviderWorkspaceMissingError", message)
                .with("threadId", "thread-1")
                .with("cwd", "/missing/project/worktree"))
        }
    }));
    h.turn("cmd-turn-start-missing-workspace", "user-message-missing-workspace", "continue", NOW)
        .await;
    attempted.wait().await;
    h.drain().await;
    let thread = h.thread("thread-1").await;
    assert_match(session(&thread), json!({"status": "error", "activeTurnId": null, "lastError": message}));
    let failure = find(&thread["activities"], |a| s(a, "kind") == "provider.turn.start.failed").unwrap();
    assert_match(&failure["payload"], json!({"detail": message}));
    assert!(h.providers.sessions.lock().unwrap().is_empty());
    assert_eq!(h.providers.send_turn.count(), 0);
    assert!(h.pending_turn_starts().await.is_empty());
}

#[tokio::test]
async fn settles_a_failed_provider_startup_and_allows_a_clean_retry() {
    let h = CommandHarness::new(Default::default()).await;
    let fail = Arc::new(AtomicBool::new(true));
    let failing = fail.clone();
    *h.providers.start_hook.lock().unwrap() = Some(hook(move |session: Value| {
        let failing = failing.clone();
        async move {
            if failing.load(Ordering::SeqCst) {
                Err(request_error("codex", "thread.start", "deterministic startup failure"))
            } else {
                Ok(session)
            }
        }
    }));
    h.turn("cmd-turn-start-provider-failure", "user-message-provider-failure", "fail once", NOW)
        .await;
    wait_for(|| async { session(&h.thread("thread-1").await)["status"] == "error" }).await;
    assert!(s(session(&h.thread("thread-1").await), "lastError").contains("deterministic startup failure"));
    assert_eq!(h.providers.send_turn.count(), 0);
    fail.store(false, Ordering::SeqCst);
    h.turn(
        "cmd-turn-start-provider-retry",
        "user-message-provider-retry",
        "retry",
        "2026-01-01T00:00:01.000Z",
    )
    .await;
    wait_for(|| async { h.providers.send_turn.count() == 1 }).await;
    let thread = h.thread("thread-1").await;
    assert_eq!(session(&thread)["status"], "starting");
    assert!(session(&thread)["lastError"].is_null());
}

async fn refines_a_vague_title(timing: &str) {
    let h = CommandHarness::new(CommandOptions {
        defer_start: timing == "before startup",
        ..Default::default()
    })
    .await;
    let created_at = "2026-01-01T00:00:01.000Z";
    h.text.titles("Fix QR pairing expiry");
    h.turn("title-turn", "title-user", "Fix this", created_at).await;
    h.drain().await;
    let generate = json!({
        "type": "thread.title.generate.complete", "commandId": "initial-title", "threadId": "thread-1", "expectedTitle": "Thread",
        "expectedVersion": null, "title": "Investigate issue", "needsRefinement": true,
    });
    if timing != "after completion" {
        h.dispatch(generate.clone()).await;
    }
    let set = |command_id: &str, status: &str, turn: Value| {
        json!({
            "type": "thread.session.set", "commandId": command_id, "threadId": "thread-1", "createdAt": created_at,
            "session": {"threadId": "thread-1", "status": status, "providerName": "codex", "runtimeMode": "approval-required", "activeTurnId": turn, "lastError": null, "updatedAt": created_at},
        })
    };
    h.dispatch(set("title-running", "running", json!("title-first-turn"))).await;
    h.dispatch(json!({
        "type": "thread.message.assistant.delta", "commandId": "title-answer", "threadId": "thread-1", "messageId": "title-assistant",
        "turnId": "title-first-turn", "delta": "The QR pairing token expires before the phone redeems it.", "createdAt": created_at,
    }))
    .await;
    h.dispatch(set("title-ready", "ready", Value::Null)).await;
    if timing == "after completion" {
        h.dispatch(generate).await;
    }
    if timing == "before startup" {
        h.start().await;
    }
    h.drain().await;
    if timing == "before startup" {
        assert_eq!(h.text.title_calls.count(), 1);
    }
    h.dispatch(set("title-ready-again", "ready", Value::Null)).await;
    h.drain().await;
    assert_eq!(h.text.title_calls.count(), 1);
    assert!(s(&h.text.title_calls.call(0), "message").contains("QR pairing token"));
    let thread = h.thread("thread-1").await;
    assert_eq!(thread["title"], "Fix QR pairing expiry");
    assert_eq!(thread["titleState"]["needsRefinement"], false);
}

#[tokio::test]
async fn refines_a_vague_title_once_when_initial_generation_finishes_before_completion() {
    refines_a_vague_title("before completion").await;
}

#[tokio::test]
async fn refines_a_vague_title_once_when_initial_generation_finishes_after_completion() {
    refines_a_vague_title("after completion").await;
}

#[tokio::test]
async fn refines_a_vague_title_once_when_initial_generation_finishes_before_startup() {
    refines_a_vague_title("before startup").await;
}

#[tokio::test]
async fn does_not_replace_a_manual_title_matching_the_first_message_seed() {
    let h = CommandHarness::new(Default::default()).await;
    h.dispatch(json!({"type": "thread.meta.update", "commandId": "manual-title", "threadId": "thread-1", "title": "Thread"}))
        .await;
    let mut command = turn_start_for("thread-1", "manual-title-turn", "manual-title-user", "Fix this", "2026-01-01T00:00:01.000Z");
    command["titleSeed"] = json!("Thread");
    h.dispatch(command).await;
    h.drain().await;
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert_eq!(h.text.title_calls.count(), 0);
}
