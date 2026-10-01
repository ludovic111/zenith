//! Port of `ProviderCommandReactor.test.ts`: title generation, regeneration and refinement,
//! worktree branch names and worktree recreation.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use common::command::*;
use common::*;
use serde_json::{json, Value};
use zc_ports::text_generation::ThreadTitleGenerationResult;

const QUOTE: &str = "Retain the reconnect backoff.";

fn citation(text: &str) -> Value {
    json!({
        "version": 1, "environmentId": "source-environment", "threadId": "source-thread", "messageId": "source-message",
        "text": text, "start": 0, "end": text.encode_utf16().count(), "prefix": "", "suffix": "",
    })
}

fn serialize(citation: &Value) -> String {
    zc_providers::citations::serialize_assistant_citation(citation)
}

async fn rename(h: &CommandHarness, command_id: &str, title: &str) {
    h.dispatch(json!({"type": "thread.meta.update", "commandId": command_id, "threadId": "thread-1", "title": title}))
        .await;
}

async fn regenerate(h: &CommandHarness, command_id: &str) {
    h.dispatch(json!({"type": "thread.meta.update", "commandId": command_id, "threadId": "thread-1", "regenerateTitle": true}))
        .await;
}

async fn assistant(h: &CommandHarness, id: &str, delta: &str) {
    h.dispatch(json!({"type": "thread.message.assistant.delta", "commandId": format!("cmd-{id}"), "threadId": "thread-1", "messageId": id, "delta": delta, "createdAt": "2026-01-01T00:00:01.000Z"}))
        .await;
    h.dispatch(json!({"type": "thread.message.assistant.complete", "commandId": format!("cmd-{id}-complete"), "threadId": "thread-1", "messageId": id, "createdAt": "2026-01-01T00:00:02.000Z"}))
        .await;
}

fn image(id: &str, name: &str) -> Value {
    json!({"type": "image", "id": id, "name": name, "mimeType": "image/png", "sizeBytes": 5})
}

#[tokio::test]
async fn retries_thread_title_generation_after_a_transient_failure() {
    let seeded = "Please investigate reconnect failures after restar...";
    let h = CommandHarness::new(CommandOptions {
        initial_title: Some(seeded.into()),
        ..Default::default()
    })
    .await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    *h.text.title_hook.lock().unwrap() = Some(hook(move |_| {
        let counter = counter.clone();
        async move {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(zc_ports::TaggedError::new("TextGenerationError", "Claude CLI request timed out.").with("detail", "Claude CLI request timed out."))
            } else {
                Ok(ThreadTitleGenerationResult {
                    title: "Generated title".into(),
                    needs_refinement: None,
                })
            }
        }
    }));
    let mut command = turn_start_for(
        "thread-1",
        "cmd-turn-start-title",
        "user-message-title",
        "Please investigate reconnect failures after restarting the session.",
        NOW,
    );
    command["titleSeed"] = json!(seeded);
    h.dispatch(command).await;
    wait_for(|| async { h.text.title_calls.count() >= 1 }).await;
    assert_match(
        &h.text.title_calls.call(0),
        json!({"message": "Please investigate reconnect failures after restarting the session."}),
    );
    wait_for(|| async { h.thread("thread-1").await["title"] == "Generated title" }).await;
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn regenerates_a_thread_title_from_the_current_conversation() {
    let h = CommandHarness::new(Default::default()).await;
    h.text.titles("Resolve stale reconnect state");
    rename(&h, "cmd-thread-title-existing", "Investigate reconnect regressions").await;
    h.turn(
        "cmd-turn-start-before-title-regeneration",
        "user-message-before-title-regeneration",
        "Please investigate reconnect regressions after restarting the session.",
        NOW,
    )
    .await;
    assistant(
        &h,
        "assistant-message-before-title-regeneration",
        "The remaining issue is stale reconnect state.",
    )
    .await;
    regenerate(&h, "cmd-thread-title-regenerate").await;
    h.drain().await;
    assert_eq!(h.text.title_calls.count(), 1);
    assert_match(
        &h.text.title_calls.call(0),
        json!({
            "cwd": "/tmp/provider-project",
            "previousTitle": "Investigate reconnect regressions",
            "message": "USER:\nPlease investigate reconnect regressions after restarting the session.\n\nASSISTANT:\nThe remaining issue is stale reconnect state.",
        }),
    );
    let thread = h.thread("thread-1").await;
    assert_eq!(thread["title"], "Resolve stale reconnect state");
    assert!(thread["titleRegeneration"].is_null());
}

#[tokio::test]
async fn pins_the_first_user_message_when_regeneration_context_is_truncated() {
    let h = CommandHarness::new(Default::default()).await;
    let quote_text = "界".repeat(1_000);
    let first = format!(
        "Review subagent monitoring risks. {} {}",
        serialize(&citation(&quote_text)),
        "Opening context. ".repeat(200)
    );
    let recent = format!("LATEST FINDING: {}", "implementation detail ".repeat(320));
    h.text.titles("Review subagent monitoring risks");
    rename(&h, "cmd-thread-title-existing-long", "Generic PR review").await;
    for (command, message, text, image_id, at) in [
        (
            "cmd-turn-start-before-long-title-regeneration",
            "user-message-before-long-title-regeneration",
            first.as_str(),
            "opening-context-image",
            NOW,
        ),
        (
            "cmd-middle-turn-before-long-title-regeneration",
            "middle-message-before-long-title-regeneration",
            "Temporary handoff details.",
            "middle-context-image",
            "2026-01-01T00:00:01.000Z",
        ),
        (
            "cmd-recent-turn-before-long-title-regeneration",
            "recent-message-before-long-title-regeneration",
            recent.as_str(),
            "recent-context-image",
            "2026-01-01T00:00:02.000Z",
        ),
    ] {
        let mut start = turn_start_for("thread-1", command, message, text, at);
        start["message"]["attachments"] = json!([image(image_id, "image.png")]);
        h.dispatch(start).await;
    }
    regenerate(&h, "cmd-thread-title-regenerate-long").await;
    h.drain().await;
    assert_eq!(h.text.title_calls.count(), 1);
    let input = h.text.title_calls.call(0);
    let message = s(&input, "message");
    let head: String = quote_text.chars().take(100).collect();
    assert!(message.contains(&format!("USER:\nReview subagent monitoring risks. {head}")));
    assert!(!message.contains("t3-citation://"));
    assert!(message.contains("[Content truncated]"));
    assert!(message.contains("[Earlier content truncated]"));
    assert!(message.contains("image.png"));
    assert!(message.encode_utf16().count() <= 8_000);
    let ids: Vec<&str> = input["attachments"].as_array().unwrap().iter().map(|a| s(a, "id")).collect();
    assert_eq!(ids, ["opening-context-image", "middle-context-image", "recent-context-image"]);
    let thread = h.thread("thread-1").await;
    let stored = find(&thread["messages"], |m| s(m, "id") == "user-message-before-long-title-regeneration").unwrap();
    assert_eq!(stored["text"], first);
}

#[tokio::test]
async fn clears_title_regeneration_state_left_pending_across_reactor_startup() {
    let h = CommandHarness::new(CommandOptions {
        title_regeneration_before_start: 1,
        ..Default::default()
    })
    .await;
    assert_eq!(h.text.title_calls.count(), 0);
    assert_eq!(h.engine.completion_attempts.load(Ordering::SeqCst), 1);
    let thread = h.thread("thread-1").await;
    assert_eq!(thread["title"], "Thread");
    assert!(thread["titleRegeneration"].is_null());
}

#[tokio::test]
async fn continues_clearing_startup_title_regeneration_state_after_one_completion_fails() {
    let h = CommandHarness::new(CommandOptions {
        title_regeneration_before_start: 2,
        completion_failures: 1,
        ..Default::default()
    })
    .await;
    assert_eq!(h.text.title_calls.count(), 0);
    assert_eq!(h.engine.completion_attempts.load(Ordering::SeqCst), 2);
    assert!(!h.thread("thread-1").await["titleRegeneration"].is_null());
    assert!(h.thread("thread-2").await["titleRegeneration"].is_null());
}

#[tokio::test]
async fn keeps_the_current_title_when_regeneration_returns_the_fallback() {
    let h = CommandHarness::new(Default::default()).await;
    h.text.titles("New thread");
    rename(&h, "cmd-thread-title-before-fallback-regeneration", "Keep meaningful title").await;
    h.turn(
        "cmd-turn-start-before-fallback-regeneration",
        "user-message-before-fallback-regeneration",
        "Investigate the reconnect state.",
        NOW,
    )
    .await;
    regenerate(&h, "cmd-thread-title-fallback-regeneration").await;
    h.drain().await;
    let thread = h.thread("thread-1").await;
    assert_eq!(thread["title"], "Keep meaningful title");
    assert!(thread["titleRegeneration"].is_null());
}

#[tokio::test]
async fn clears_title_regeneration_state_when_generation_fails() {
    let h = CommandHarness::new(Default::default()).await;
    rename(&h, "cmd-thread-title-before-failed-regeneration", "Keep title after failure").await;
    h.turn(
        "cmd-turn-start-before-failed-regeneration",
        "user-message-before-failed-regeneration",
        "Investigate the reconnect state.",
        NOW,
    )
    .await;
    regenerate(&h, "cmd-thread-title-failed-regeneration").await;
    h.drain().await;
    let thread = h.thread("thread-1").await;
    assert_eq!(thread["title"], "Keep title after failure");
    assert!(thread["titleRegeneration"].is_null());
}

#[tokio::test]
async fn retries_a_failed_completion_and_continues_regenerating() {
    let h = CommandHarness::new(CommandOptions {
        completion_failures: 1,
        ..Default::default()
    })
    .await;
    h.text.title_once("Title lost to completion failure");
    h.text.title_once("Recovered regeneration worker");
    rename(&h, "cmd-thread-title-before-completion-failure", "Existing title").await;
    h.turn(
        "cmd-turn-start-before-completion-failure",
        "user-message-before-completion-failure",
        "Investigate the reconnect state.",
        NOW,
    )
    .await;
    regenerate(&h, "cmd-thread-title-regeneration-completion-failure").await;
    h.drain().await;
    let thread = h.thread("thread-1").await;
    assert_eq!(thread["title"], "Title lost to completion failure");
    assert!(thread["titleRegeneration"].is_null());
    regenerate(&h, "cmd-thread-title-regeneration-after-completion-failure").await;
    h.drain().await;
    assert_eq!(h.text.title_calls.count(), 2);
    assert_eq!(h.engine.completion_attempts.load(Ordering::SeqCst), 3);
    let thread = h.thread("thread-1").await;
    assert_eq!(thread["title"], "Recovered regeneration worker");
    assert!(thread["titleRegeneration"].is_null());
}

#[tokio::test]
async fn pins_the_first_user_context_and_attachment_before_the_retained_tail() {
    let h = CommandHarness::new(Default::default()).await;
    rename(&h, "cmd-thread-title-before-truncated-regeneration", "Existing title").await;
    let mut start = turn_start_for(
        "thread-1",
        "cmd-turn-start-before-truncated-regeneration",
        "user-message-before-truncated-regeneration",
        "Old visual issue",
        NOW,
    );
    start["message"]["attachments"] = json!([image("old-title-context-image", "old-issue.png")]);
    h.dispatch(start).await;
    assistant(
        &h,
        "assistant-truncated-regeneration-context",
        &format!("content before retained tail{}", "x".repeat(8_100)),
    )
    .await;
    regenerate(&h, "cmd-thread-title-regenerate-truncated-context").await;
    h.drain().await;
    let input = h.text.title_calls.call(0);
    let context = s(&input, "message");
    assert!(context.contains("USER:\nOld visual issue\n[Attachments: old-issue.png]"));
    assert!(context.contains("ASSISTANT:\ncontent before retained tail"));
    assert!(context.encode_utf16().count() <= 8_000);
    assert_match(&input["attachments"], json!([{"id": "old-title-context-image", "name": "old-issue.png"}]));
}

#[tokio::test]
async fn does_not_overwrite_a_manual_rename_while_title_regeneration_is_running() {
    let h = CommandHarness::new(Default::default()).await;
    let release = Latch::new();
    let gate = release.clone();
    *h.text.title_hook.lock().unwrap() = Some(hook(move |_| {
        let gate = gate.clone();
        async move {
            gate.wait().await;
            Ok(ThreadTitleGenerationResult {
                title: "Generated title should not win".into(),
                needs_refinement: None,
            })
        }
    }));
    rename(&h, "cmd-thread-title-before-regeneration-race", "Existing thread title").await;
    h.turn(
        "cmd-turn-start-before-regeneration-race",
        "user-message-before-regeneration-race",
        "Investigate the reconnect state.",
        NOW,
    )
    .await;
    regenerate(&h, "cmd-thread-title-regeneration-race").await;
    wait_for(|| async { h.text.title_calls.count() == 1 }).await;
    assert_eq!(
        h.thread("thread-1").await["titleRegeneration"]["requestId"],
        "cmd-thread-title-regeneration-race"
    );
    rename(&h, "cmd-thread-manual-rename-during-regeneration", "Keep manual rename").await;
    release.open();
    h.drain().await;
    let thread = h.thread("thread-1").await;
    assert_eq!(thread["title"], "Keep manual rename");
    assert!(thread["titleRegeneration"].is_null());
}

fn gate_starts(h: &CommandHarness) -> Latch {
    let release = Latch::new();
    let gate = release.clone();
    *h.providers.start_hook.lock().unwrap() = Some(hook(move |session: Value| {
        let gate = gate.clone();
        async move {
            gate.wait().await;
            Ok(session)
        }
    }));
    release
}

#[tokio::test]
async fn does_not_overwrite_a_manual_rename_while_title_regeneration_is_queued() {
    let h = CommandHarness::new(Default::default()).await;
    let release = gate_starts(&h);
    h.text.titles("Generated title should not win");
    rename(&h, "cmd-thread-title-before-queued-regeneration", "Existing thread title").await;
    h.turn(
        "cmd-turn-start-before-queued-regeneration",
        "user-message-before-queued-regeneration",
        "Investigate the reconnect state.",
        NOW,
    )
    .await;
    wait_for(|| async { h.providers.start_session.count() == 1 }).await;
    regenerate(&h, "cmd-thread-title-queued-regeneration").await;
    rename(&h, "cmd-thread-manual-rename-before-regeneration-starts", "Keep queued manual rename").await;
    release.open();
    h.drain().await;
    assert_eq!(h.text.title_calls.count(), 0);
    assert_eq!(h.thread("thread-1").await["title"], "Keep queued manual rename");
}

#[tokio::test]
async fn skips_superseded_title_regeneration_before_generation_starts() {
    let h = CommandHarness::new(Default::default()).await;
    let release = gate_starts(&h);
    h.text.titles("Latest regenerated title");
    h.turn(
        "cmd-turn-start-before-superseded-regeneration",
        "user-message-before-superseded-regeneration",
        "Investigate the reconnect state.",
        NOW,
    )
    .await;
    wait_for(|| async { h.providers.start_session.count() == 1 }).await;
    regenerate(&h, "cmd-thread-title-superseded-regeneration").await;
    regenerate(&h, "cmd-thread-title-latest-regeneration").await;
    release.open();
    h.drain().await;
    assert_eq!(h.text.title_calls.count(), 1);
    assert_eq!(h.engine.completion_attempts.load(Ordering::SeqCst), 1);
    let thread = h.thread("thread-1").await;
    assert_eq!(thread["title"], "Latest regenerated title");
    assert!(thread["titleRegeneration"].is_null());
}

#[tokio::test]
async fn does_not_overwrite_an_existing_custom_thread_title_on_the_first_turn() {
    let h = CommandHarness::new(Default::default()).await;
    rename(&h, "cmd-thread-title-custom", "Keep this custom title").await;
    let mut command = turn_start_for(
        "thread-1",
        "cmd-turn-start-title-preserve",
        "user-message-title-preserve",
        "Please investigate reconnect failures after restarting the session.",
        NOW,
    );
    command["titleSeed"] = json!("Please investigate reconnect failures after restar...");
    h.dispatch(command).await;
    wait_for(|| async { h.providers.send_turn.count() == 1 }).await;
    assert_eq!(h.text.title_calls.count(), 0);
    assert_eq!(h.thread("thread-1").await["title"], "Keep this custom title");
}

#[tokio::test]
async fn matches_the_client_seeded_title_even_when_the_outgoing_prompt_is_reformatted() {
    let seeded = "Fix reconnect spinner on resume";
    let h = CommandHarness::new(CommandOptions {
        initial_title: Some(seeded.into()),
        ..Default::default()
    })
    .await;
    let prompt = format!("[effort:high]\\n\\nFix reconnect spinner on resume {}", serialize(&citation(QUOTE)));
    h.text.titles("Reconnect spinner resume bug");
    let mut command = turn_start_for("thread-1", "cmd-turn-start-title-formatted", "user-message-title-formatted", &prompt, NOW);
    command["titleSeed"] = json!(seeded);
    h.dispatch(command).await;
    wait_for(|| async { h.thread("thread-1").await["title"] == "Reconnect spinner resume bug" }).await;
    h.drain().await;
    let message = s(&h.text.title_calls.call(0), "message").to_owned();
    assert_eq!(message, format!("[effort:high]\\n\\nFix reconnect spinner on resume {QUOTE}"));
    assert!(!message.contains("t3-citation://"));
    let thread = h.thread("thread-1").await;
    assert_eq!(
        find(&thread["messages"], |m| s(m, "id") == "user-message-title-formatted").unwrap()["text"],
        prompt
    );
    wait_for(|| async { h.providers.send_turn.count() == 1 }).await;
    assert_match(&h.providers.send_turn.call(0), json!({"input": prompt}));
}

#[tokio::test]
async fn generates_a_worktree_branch_name_for_the_first_turn() {
    let h = CommandHarness::new(Default::default()).await;
    let prompt = format!("Add a safer reconnect backoff. {}", serialize(&citation(QUOTE)));
    h.dispatch(json!({"type": "thread.meta.update", "commandId": "cmd-thread-branch", "threadId": "thread-1", "branch": "t3code/1234abcd", "worktreePath": "/tmp/provider-project-worktree"}))
        .await;
    *h.text.branch_hook.lock().unwrap() = Some(hook(|input: Value| async move {
        Ok(match input["modelSelection"]["model"].as_str() {
            Some(model) => format!("feature/{model}"),
            None => "feature/generated".into(),
        })
    }));
    h.turn("cmd-turn-start-branch-model", "user-message-branch-model", &prompt, NOW).await;
    h.vcs.refreshed.wait().await;
    h.drain().await;
    let message = s(&h.text.branch_calls.call(0), "message").to_owned();
    assert_eq!(message, format!("Add a safer reconnect backoff. {QUOTE}"));
    assert!(!message.contains("t3-citation://"));
    assert_eq!(h.vcs.refresh_status.call(0), "/tmp/provider-project-worktree");
    let thread = h.thread("thread-1").await;
    assert_eq!(
        find(&thread["messages"], |m| s(m, "id") == "user-message-branch-model").unwrap()["text"],
        prompt
    );
    assert!(s(&thread, "branch").starts_with("t3code/feature/"));
}

#[tokio::test]
async fn recreates_a_missing_worktree_from_the_thread_branch_before_starting_a_turn() {
    let h = CommandHarness::new(Default::default()).await;
    let worktree = h.state_dir.path().join("missing-worktree").to_string_lossy().into_owned();
    h.dispatch(json!({"type": "thread.meta.update", "commandId": "cmd-thread-missing-worktree", "threadId": "thread-1", "branch": "feature/restore", "worktreePath": worktree}))
        .await;
    h.turn("cmd-turn-start-missing-worktree", "user-message-missing-worktree", "continue", NOW)
        .await;
    wait_for(|| async { h.providers.start_session.count() == 1 }).await;
    assert_eq!(h.git.prune_worktrees.calls(), vec![json!({"cwd": "/tmp/provider-project"})]);
    assert_eq!(
        h.git.create_worktree.calls(),
        vec![json!([{"cwd": "/tmp/provider-project", "refName": "feature/restore", "path": worktree}, {"submodules": null}])]
    );
    assert!(h.git.create_worktree.first_order().unwrap() < h.providers.start_session.first_order().unwrap());
}
