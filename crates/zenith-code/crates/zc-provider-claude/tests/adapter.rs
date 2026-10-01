//! The port of `provider/Layers/ClaudeAdapter.test.ts`: the adapter driven by an in-memory
//! fake query (`FakeClaudeQuery`). Process-level fakes live in `tests/process.rs`.

mod support;

use serde_json::{json, Value};
use support::*;
use zc_ports::adapter::AdapterError;
use zc_provider_claude::adapter::build_runtime_instructions;
use zc_provider_claude::options::{SystemPrompt, ThinkingConfig};

#[tokio::test]
async fn returns_validation_error_for_non_claude_provider_on_start_session() {
    let harness = Harness::default();
    let input = serde_json::from_value(json!({"threadId": THREAD_ID, "provider": "codex", "runtimeMode": "full-access"})).unwrap();
    let error = harness.adapter.start(input).await.unwrap_err();
    assert_eq!(
        err_json(&error),
        err_json(&AdapterError::Validation {
            provider: "claudeAgent".into(),
            operation: "startSession".into(),
            issue: "Expected provider 'claudeAgent' but received 'codex'.".into()
        })
    );
}

#[tokio::test]
async fn retains_claude_session_startup_causes_without_exposing_their_messages() {
    let harness = Harness::default();
    *harness.factory.create_error.lock().unwrap() = Some("credential material that must remain in the cause chain".into());
    let input = serde_json::from_value(json!({"threadId": THREAD_ID, "provider": "claudeAgent", "runtimeMode": "full-access"})).unwrap();
    let error = harness.adapter.start(input).await.unwrap_err();
    match &error {
        AdapterError::Process { detail, .. } => assert_eq!(detail, "Failed to start Claude runtime session."),
        other => panic!("unexpected {other:?}"),
    }
    assert!(!error.to_string().contains("credential material"));
}

#[tokio::test]
async fn derives_bypass_permission_mode_from_full_access_runtime_policy() {
    let harness = Harness::default();
    harness.start(json!({"runtimeMode": "full-access"})).await;
    let options = harness.factory.last().options.clone();
    assert_eq!(options.setting_sources, Some(vec!["user".to_string(), "project".into(), "local".into()]));
    assert_eq!(
        options.system_prompt,
        Some(SystemPrompt::Preset {
            append: Some(build_runtime_instructions("Claude Code"))
        })
    );
    assert_eq!(options.permission_mode.as_deref(), Some("bypassPermissions"));
    assert!(options.allow_dangerously_skip_permissions);
}

#[tokio::test]
async fn derives_auto_permission_mode_from_auto_runtime_policy_without_skip_flag() {
    let harness = Harness::default();
    harness.start(json!({"runtimeMode": "auto"})).await;
    let options = harness.factory.last().options.clone();
    assert_eq!(options.permission_mode.as_deref(), Some("auto"));
    assert!(!options.allow_dangerously_skip_permissions);
}

#[tokio::test]
async fn lets_a_launch_arg_permission_flag_win_over_the_thread_runtime_mode() {
    let harness = Harness::new(HarnessConfig {
        claude_config: Some(json!({"launchArgs": "--dangerously-skip-permissions --verbose"})),
        ..HarnessConfig::default()
    });
    harness.start(json!({"runtimeMode": "auto-accept-edits"})).await;
    let options = harness.factory.last().options.clone();
    assert_eq!(options.permission_mode.as_deref(), Some("bypassPermissions"));
    assert!(options.allow_dangerously_skip_permissions);
    assert_eq!(Value::Object(options.extra_args), json!({"verbose": null, "thinking-display": "summarized"}));
}

#[tokio::test]
async fn loads_claude_filesystem_settings_sources_for_sdk_sessions() {
    let harness = Harness::default();
    harness.start(json!({"runtimeMode": "approval-required"})).await;
    let options = harness.factory.last().options.clone();
    assert_eq!(options.setting_sources, Some(vec!["user".to_string(), "project".into(), "local".into()]));
    assert_eq!(options.permission_mode, None);
    assert!(!options.allow_dangerously_skip_permissions);
}

#[tokio::test]
async fn passes_the_configured_auto_compaction_window_to_claude() {
    let harness = Harness::new(HarnessConfig {
        claude_config: Some(json!({"autoCompactWindow": "300000"})),
        ..HarnessConfig::default()
    });
    harness.start(json!({})).await;
    let options = harness.factory.last().options.clone();
    assert_eq!(
        Value::Object(options.settings.unwrap()),
        json!({"showThinkingSummaries": true, "autoCompactWindow": 300000})
    );
    assert_eq!(options.supported_dialog_kinds, Some(vec!["resume_return".to_string()]));
}

#[tokio::test]
async fn forwards_claude_effort_levels_into_query_options() {
    let harness = Harness::default();
    harness
        .start(json!({"modelSelection": {"instanceId": "claudeAgent", "model": CAPABLE, "options": [{"id": "effort", "value": "max"}]}}))
        .await;
    assert_eq!(harness.factory.last().options.effort.as_deref(), Some("max"));
}

#[tokio::test]
async fn runs_claude_sessions_with_the_configured_claude_config_dir() {
    let harness = Harness::new(HarnessConfig {
        claude_config: Some(json!({"homePath": "~/.claude-work"})),
        ..HarnessConfig::default()
    });
    harness.start(json!({"modelSelection": {"instanceId": "claudeAgent", "model": CAPABLE}})).await;
    let expected = zc_core::paths::home_dir().join(".claude-work").to_string_lossy().into_owned();
    assert_eq!(harness.factory.last().options.env.get("CLAUDE_CONFIG_DIR"), Some(&expected));
}

#[tokio::test]
async fn forwards_claude_thinking_toggle_for_models_that_support_it() {
    let harness = Harness::default();
    harness
        .start(json!({"modelSelection": {"instanceId": "claudeAgent", "model": THINKING, "options": [{"id": "thinking", "value": false}]}}))
        .await;
    let options = harness.factory.last().options.clone();
    assert_eq!(Value::Object(options.settings.unwrap()), json!({"alwaysThinkingEnabled": false}));
    assert_eq!(options.thinking, None);
    assert_eq!(options.extra_args.get("thinking-display"), None);
}

#[tokio::test]
async fn requests_claude_thinking_summaries_unless_thinking_is_off() {
    let harness = Harness::default();
    harness
        .start(json!({"modelSelection": {"instanceId": "claudeAgent", "model": THINKING, "options": [{"id": "thinking", "value": true}]}}))
        .await;
    let options = harness.factory.last().options.clone();
    assert_eq!(
        Value::Object(options.settings.unwrap()),
        json!({"alwaysThinkingEnabled": true, "showThinkingSummaries": true})
    );
    assert_eq!(
        options.thinking,
        Some(ThinkingConfig::Adaptive {
            display: Some("summarized".into())
        })
    );
    assert_eq!(options.extra_args.get("thinking-display"), Some(&json!("summarized")));
}

#[tokio::test]
async fn honors_a_launch_arg_that_omits_claude_thinking_display() {
    let harness = Harness::new(HarnessConfig {
        claude_config: Some(json!({"launchArgs": "--thinking-display omitted"})),
        ..HarnessConfig::default()
    });
    harness.start(json!({})).await;
    let options = harness.factory.last().options.clone();
    assert_eq!(options.settings, None);
    assert_eq!(options.thinking, None);
    assert_eq!(options.extra_args.get("thinking-display"), Some(&json!("omitted")));
}

#[tokio::test]
async fn ignores_claude_thinking_toggle_for_models_without_it() {
    let harness = Harness::default();
    harness
        .start(json!({"modelSelection": {"instanceId": "claudeAgent", "model": STANDARD, "options": [{"id": "thinking", "value": false}]}}))
        .await;
    let options = harness.factory.last().options.clone();
    assert_eq!(Value::Object(options.settings.unwrap()), json!({"showThinkingSummaries": true}));
    assert_eq!(
        options.thinking,
        Some(ThinkingConfig::Adaptive {
            display: Some("summarized".into())
        })
    );
}

#[tokio::test]
async fn forwards_claude_fast_mode_into_sdk_settings_only_for_models_with_it() {
    let harness = Harness::default();
    harness
        .start(json!({"modelSelection": {"instanceId": "claudeAgent", "model": CAPABLE, "options": [{"id": "fastMode", "value": true}]}}))
        .await;
    assert_eq!(
        Value::Object(harness.factory.last().options.settings.clone().unwrap()),
        json!({"showThinkingSummaries": true, "fastMode": true})
    );
    let harness = Harness::default();
    harness
        .start(json!({"modelSelection": {"instanceId": "claudeAgent", "model": STANDARD, "options": [{"id": "fastMode", "value": true}]}}))
        .await;
    assert_eq!(
        Value::Object(harness.factory.last().options.settings.clone().unwrap()),
        json!({"showThinkingSummaries": true})
    );
}

#[tokio::test]
async fn keeps_a_configured_custom_alias_opaque_without_disabling_the_canonical_built_in() {
    let options = json!([{"id": "effort", "value": "max"}, {"id": "fastMode", "value": true}, {"id": "contextWindow", "value": "expanded"}]);
    let custom = Harness::new(HarnessConfig {
        claude_config: Some(json!({"customModels": [COLLIDING_ALIAS]})),
        ..HarnessConfig::default()
    });
    custom
        .start(json!({"modelSelection": {"instanceId": "claudeAgent", "model": COLLIDING_ALIAS, "options": options}}))
        .await;
    let created = custom.factory.last();
    custom.send(json!({"input": "use the built-in model", "modelSelection": {"instanceId": "claudeAgent", "model": CAPABLE, "options": [{"id": "contextWindow", "value": "expanded"}]}})).await;
    created.next_prompt().await;
    custom.send(json!({"input": "keep this prompt literal", "modelSelection": {"instanceId": "claudeAgent", "model": COLLIDING_ALIAS, "options": [{"id": "effort", "value": "ultrathink"}]}})).await;
    let prompt = created.next_prompt().await;
    assert_eq!(created.options.model.as_deref(), Some(COLLIDING_ALIAS));
    assert_eq!(created.options.effort, None);
    assert_eq!(Value::Object(created.options.settings.clone().unwrap()), json!({"showThinkingSummaries": true}));
    assert_eq!(
        *created.query.set_model_calls.lock().unwrap(),
        vec![Some(format!("{CAPABLE}[expanded]")), Some(COLLIDING_ALIAS.to_string())]
    );
    assert_eq!(prompt["message"]["content"][0]["text"], json!("keep this prompt literal"));

    let built_in = Harness::new(HarnessConfig {
        claude_config: Some(json!({"customModels": [COLLIDING_ALIAS]})),
        ..HarnessConfig::default()
    });
    built_in
        .start(json!({"modelSelection": {"instanceId": "claudeAgent", "model": CAPABLE, "options": options}}))
        .await;
    let options = built_in.factory.last().options.clone();
    assert_eq!(options.model, Some(format!("{CAPABLE}[expanded]")));
    assert_eq!(options.effort.as_deref(), Some("max"));
    assert_eq!(
        Value::Object(options.settings.unwrap()),
        json!({"showThinkingSummaries": true, "fastMode": true})
    );
}

#[tokio::test]
async fn treats_ultrathink_as_a_prompt_keyword_instead_of_a_session_effort() {
    let harness = Harness::default();
    let selection = json!({"instanceId": "claudeAgent", "model": STANDARD, "options": [{"id": "effort", "value": "ultrathink"}]});
    harness.start(json!({"modelSelection": selection})).await;
    harness.send(json!({"input": "Investigate the edge cases", "modelSelection": selection})).await;
    let created = harness.factory.last();
    assert_eq!(created.options.effort.as_deref(), Some("high"));
    assert_eq!(
        created.next_prompt().await["message"]["content"][0]["text"],
        json!("Ultrathink:\nInvestigate the edge cases")
    );
}

#[tokio::test]
async fn keeps_compact_commands_intact_when_ultrathink_is_selected() {
    let harness = Harness::default();
    let selection = json!({"instanceId": "claudeAgent", "model": STANDARD, "options": [{"id": "effort", "value": "ultrathink"}]});
    harness.start(json!({"modelSelection": selection})).await;
    harness.send(json!({"input": "/compact", "modelSelection": selection})).await;
    assert_eq!(harness.factory.last().next_prompt().await["message"]["content"][0]["text"], json!("/compact"));
}

fn write_attachment(harness: &Harness, id: &str, extension: &str) {
    std::fs::create_dir_all(&harness.attachments_dir).unwrap();
    std::fs::write(harness.attachments_dir.join(format!("{id}{extension}")), [1u8, 2, 3, 4]).unwrap();
}

#[tokio::test]
async fn embeds_image_attachments_in_claude_user_messages() {
    let harness = Harness::default();
    let id = "thread-claude-attachment-12345678-1234-1234-1234-123456789abc";
    write_attachment(&harness, id, ".png");
    harness.start(json!({})).await;
    harness.send(json!({"input": "What's in this image?", "attachments": [{"type": "image", "id": id, "name": "diagram.png", "mimeType": "image/png", "sizeBytes": 4}]})).await;
    let prompt = harness.factory.last().next_prompt().await;
    assert_eq!(
        prompt["message"]["content"],
        json!([
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AQIDBA=="}},
            {"type": "text", "text": "What's in this image?"}
        ])
    );
    assert_eq!(prompt["type"], json!("user"));
    assert_eq!(prompt["session_id"], json!(""));
    assert_eq!(prompt["parent_tool_use_id"], Value::Null);
}

#[tokio::test]
async fn puts_the_command_text_last_so_attachments_do_not_suppress_expansion() {
    let harness = Harness::default();
    let image_id = "thread-claude-attachment-22345678-1234-1234-1234-123456789abc";
    let file_id = "thread-claude-attachment-32345678-1234-1234-1234-123456789abc";
    write_attachment(&harness, image_id, ".png");
    write_attachment(&harness, file_id, ".pdf");
    harness.start(json!({})).await;
    harness.send(json!({"input": "/flow-patterns hello"})).await;
    harness.send(json!({"input": "/flow-patterns hello", "attachments": [{"type": "image", "id": image_id, "name": "screenshot.png", "mimeType": "image/png", "sizeBytes": 4}]})).await;
    harness.send(json!({"input": "/flow-patterns hello", "attachments": [{"type": "file", "id": file_id, "name": "notes.pdf", "mimeType": "application/pdf", "sizeBytes": 4}]})).await;
    let created = harness.factory.last();
    let command = json!({"type": "text", "text": "/flow-patterns hello"});
    assert_eq!(created.next_prompt().await["message"]["content"], json!([command]));
    assert_eq!(
        created.next_prompt().await["message"]["content"],
        json!([{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AQIDBA=="}}, command])
    );
    assert_eq!(created.next_prompt().await["message"]["content"], json!([command]));
}

fn write_skill(home: &std::path::Path, name: &str, body: &str) {
    let dir = home.join("skills").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), body).unwrap();
}

#[tokio::test]
async fn dispatches_a_skill_mention_as_a_trailing_slash_command_block() {
    let home = tempfile::tempdir().unwrap();
    write_skill(home.path(), "implement", "---\ndescription: Implement the tickets.\n---\n# Body\n");
    let harness = Harness::new(HarnessConfig {
        claude_config: Some(json!({"homePath": home.path().to_string_lossy()})),
        ..HarnessConfig::default()
    });
    harness.start(json!({})).await;
    harness.send(json!({"input": "ok, now $implement all the tickets\nstart with auth"})).await;
    assert_eq!(
        harness.factory.last().next_prompt().await["message"]["content"],
        json!([{"type": "text", "text": "ok, now"}, {"type": "text", "text": "/implement all the tickets\nstart with auth"}])
    );
}

#[tokio::test]
async fn keeps_the_skill_command_block_after_image_attachments() {
    let home = tempfile::tempdir().unwrap();
    write_skill(home.path(), "review", "---\ndescription: Review.\n---\n# Body\n");
    let harness = Harness::new(HarnessConfig {
        claude_config: Some(json!({"homePath": home.path().to_string_lossy()})),
        ..HarnessConfig::default()
    });
    let id = "thread-claude-attachment-12345678-1234-1234-1234-123456789abc";
    write_attachment(&harness, id, ".png");
    harness.start(json!({})).await;
    harness.send(json!({"input": "$review this screenshot", "attachments": [{"type": "image", "id": id, "name": "diagram.png", "mimeType": "image/png", "sizeBytes": 4}]})).await;
    let content = harness.factory.last().next_prompt().await["message"]["content"].clone();
    let blocks: Vec<String> = content
        .as_array()
        .unwrap()
        .iter()
        .map(|b| {
            if b["type"] == "text" {
                b["text"].as_str().unwrap().to_string()
            } else {
                b["type"].as_str().unwrap().to_string()
            }
        })
        .collect();
    assert_eq!(blocks, vec!["image", "/review this screenshot"]);
}

#[tokio::test]
async fn leaves_a_mention_of_an_unknown_or_disabled_skill_as_prose() {
    let home = tempfile::tempdir().unwrap();
    write_skill(home.path(), "deploy", "---\ndescription: Deploy.\n---\n# Body\n");
    std::fs::write(home.path().join("settings.json"), r#"{"skillOverrides":{"deploy":"off"}}"#).unwrap();
    let harness = Harness::new(HarnessConfig {
        claude_config: Some(json!({"homePath": home.path().to_string_lossy()})),
        ..HarnessConfig::default()
    });
    harness.start(json!({})).await;
    harness.send(json!({"input": "run $deploy and echo $HOME"})).await;
    assert_eq!(
        harness.factory.last().next_prompt().await["message"]["content"][0]["text"],
        json!("run $deploy and echo $HOME")
    );
}

#[tokio::test]
async fn rejects_unsupported_image_types() {
    let harness = Harness::default();
    harness.start(json!({})).await;
    let error = harness
        .try_send(json!({"input": "x", "attachments": [{"type": "image", "id": "a", "name": "a.bmp", "mimeType": "image/bmp", "sizeBytes": 4}]}))
        .await
        .unwrap_err();
    assert_eq!(
        err_json(&error),
        err_json(&AdapterError::Request {
            provider: "claudeAgent".into(),
            method: "turn/start".into(),
            detail: "Unsupported Claude image attachment type 'image/bmp'.".into()
        })
    );
}

#[tokio::test]
async fn maps_claude_stream_runtime_messages_to_canonical_provider_runtime_events() {
    let mut harness = Harness::default();
    harness.start(json!({"modelSelection": {"instanceId": "claudeAgent", "model": STANDARD}})).await;
    let turn = harness.send(json!({"input": "hello"})).await;
    let s = "sdk-session-1";
    harness.emit(stream_event(
        s,
        "stream-0",
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
    ));
    harness.emit(stream_event(
        s,
        "stream-1",
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hi"}}),
    ));
    harness.emit(stream_event(s, "stream-2", json!({"type": "content_block_stop", "index": 0})));
    harness.emit(stream_event(
        s,
        "stream-3",
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "tool-1", "name": "Bash", "input": {"command": "ls"}}}),
    ));
    harness.emit(stream_event(s, "stream-4", json!({"type": "content_block_stop", "index": 1})));
    harness.emit(assistant(s, "assistant-1", "assistant-message-1", json!([{"type": "text", "text": "Hi"}])));
    harness.emit(result_success(s, "result-1"));
    let events = harness.take(10).await;
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
            "item.completed",
            "turn.completed"
        ]
    );
    assert_eq!(events[3]["turnId"], json!(turn.turn_id.as_str()));
    let delta = first_of(&events, "content.delta");
    assert_eq!(delta["payload"]["delta"], json!("Hi"));
    assert_eq!(delta["turnId"], json!(turn.turn_id.as_str()));
    assert_eq!(first_of(&events, "item.started")["payload"]["itemType"], json!("command_execution"));
    assert_eq!(events[6]["payload"]["itemType"], json!("assistant_message"));
    assert_eq!(events[9]["turnId"], json!(turn.turn_id.as_str()));
    assert_eq!(events[9]["payload"]["state"], json!("completed"));
}

#[tokio::test]
async fn places_overage_included_rate_limit_events_on_the_bucket_the_probe_named() {
    let names = zc_provider_claude::usage_limits::make_scoped_limit_names();
    let mut harness = Harness::new(HarnessConfig {
        scoped_limit_names: Some(names.clone()),
        ..HarnessConfig::default()
    });
    let rate_limit = |utilization: f64| json!({"type": "rate_limit_event", "rate_limit_info": {"status": "allowed", "rateLimitType": "seven_day_overage_included", "utilization": utilization}, "uuid": format!("rl-{utilization}"), "session_id": "sdk-session-1"});
    let result = |uuid: &str| json!({"type": "result", "subtype": "success", "is_error": false, "errors": [], "num_turns": 1, "session_id": "sdk-session-1", "uuid": uuid});
    harness.start(json!({})).await;
    harness.send(json!({"input": "hello"})).await;
    harness.emit(rate_limit(0.2));
    harness.emit(result("result-1"));
    let first = harness.take_until("turn.completed").await;
    assert!(of_type(&first, "account.rate-limits.updated").is_empty());
    names.write().unwrap().overage_included = Some("Fable".into());
    harness.send(json!({"input": "again"})).await;
    harness.emit(rate_limit(0.4));
    harness.emit(result("result-2"));
    let second = harness.take_until("turn.completed").await;
    let updates: Vec<&Value> = of_type(&second, "account.rate-limits.updated")
        .into_iter()
        .map(|e| &e["payload"]["limits"])
        .collect();
    assert_eq!(
        updates,
        vec![&json!({"windows": [{"id": "seven_day_fable", "kind": "weekly", "label": "Weekly · Fable", "usedPercent": 40, "windowDurationMins": 10080}]})]
    );
}

#[tokio::test]
async fn does_not_emit_turn_completed_for_a_result_with_no_active_turn() {
    let mut harness = Harness::default();
    harness.start(json!({})).await;
    let turn = harness.send(json!({"input": "hello"})).await;
    harness.emit(
        json!({"type": "result", "subtype": "success", "is_error": false, "errors": [], "num_turns": 1, "session_id": "sdk-session-1", "uuid": "result-real"}),
    );
    harness.emit(json!({"type": "result", "subtype": "success", "is_error": false, "errors": [], "num_turns": 0, "usage": {"input_tokens": 0, "output_tokens": 0}, "session_id": "sdk-session-1", "uuid": "result-handshake"}));
    harness.query().finish();
    let events = harness.take_until("session.exited").await;
    let completions = of_type(&events, "turn.completed");
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0]["turnId"], json!(turn.turn_id.as_str()));
    assert_eq!(completions[0]["payload"]["state"], json!("completed"));
}

#[tokio::test]
async fn steers_a_running_turn_instead_of_opening_a_new_one_on_mid_turn_send_turn() {
    let mut harness = Harness::default();
    harness.start(json!({})).await;
    let turn = harness.send(json!({"input": "run 5 commands"})).await;
    let steered = harness.send(json!({"input": "actually run 15"})).await;
    assert_eq!(steered.turn_id, turn.turn_id);
    harness.emit(assistant(
        "sdk-session-steer",
        "assistant-steer-1",
        "assistant-message-steer-1",
        json!([{"type": "text", "text": "Adjusting to 15."}]),
    ));
    harness.emit(result_success("sdk-session-steer", "result-steer-1"));
    let events = harness.take_until("turn.completed").await;
    let started = of_type(&events, "turn.started");
    let completed = of_type(&events, "turn.completed");
    assert_eq!(started.len(), 1);
    assert_eq!(started[0]["turnId"], json!(turn.turn_id.as_str()));
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0]["turnId"], json!(turn.turn_id.as_str()));
    let created = harness.factory.last();
    let first = created.next_prompt().await;
    let second = created.next_prompt().await;
    assert_eq!(first["uuid"], json!(turn.turn_id.as_str()));
    assert!(second.get("uuid").is_none(), "a steer carries no turn uuid");
}

#[tokio::test]
async fn maps_claude_reasoning_deltas_streamed_tool_inputs_and_tool_results() {
    let mut harness = Harness::default();
    harness.start(json!({})).await;
    let turn = harness.send(json!({"input": "hello"})).await;
    let s = "sdk-session-tool-streams";
    harness.emit(stream_event(
        s,
        "stream-thinking",
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Let"}}),
    ));
    harness.emit(stream_event(
        s,
        "stream-tool-start",
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "tool-grep-1", "name": "Grep", "input": {}}}),
    ));
    harness.emit(stream_event(
        s,
        "stream-tool-input-1",
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"pattern\":\"foo\",\"path\":\"src\"}"}}),
    ));
    harness.emit(stream_event(s, "stream-tool-stop", json!({"type": "content_block_stop", "index": 1})));
    harness.emit(tool_result(s, "user-tool-result", "tool-grep-1", "src/example.ts:1:foo"));
    harness.emit(result_success(s, "result-tool-streams"));
    let events = harness.take(11).await;
    assert_eq!(
        types(&events),
        vec![
            "session.started",
            "session.configured",
            "session.state.changed",
            "turn.started",
            "thread.started",
            "content.delta",
            "item.started",
            "item.updated",
            "item.updated",
            "item.completed",
            "turn.completed"
        ]
    );
    let reasoning = first_of(&events, "content.delta");
    assert_eq!(reasoning["payload"]["streamKind"], json!("reasoning_summary_text"));
    assert_eq!(reasoning["payload"]["delta"], json!("Let"));
    assert_eq!(reasoning["turnId"], json!(turn.turn_id.as_str()));
    assert_eq!(first_of(&events, "item.started")["payload"]["itemType"], json!("dynamic_tool_call"));
    assert_eq!(
        events[7]["payload"]["data"],
        json!({"toolName": "Grep", "input": {"pattern": "foo", "path": "src"}})
    );
    assert_eq!(events[8]["payload"]["data"]["result"]["content"], json!("src/example.ts:1:foo"));
}

#[tokio::test]
async fn backfills_claude_thinking_summaries_from_assistant_snapshots() {
    let mut harness = Harness::default();
    harness.start(json!({})).await;
    let turn = harness.send(json!({"input": "hello"})).await;
    let s = "sdk-session-thinking-snapshot";
    let snapshot = assistant(
        s,
        "assistant-thinking-snapshot",
        "assistant-message-thinking",
        json!([{"type": "thinking", "thinking": "Use Euclidean algorithm."}, {"type": "text", "text": "The gcd is 21."}]),
    );
    harness.emit(snapshot.clone());
    harness.emit(snapshot);
    harness.emit(assistant(
        s,
        "assistant-thinking-snapshot-2",
        "assistant-message-thinking-2",
        json!([{"type": "thinking", "thinking": "Verify the result."}]),
    ));
    harness.emit(result_success(s, "result-thinking-snapshot"));
    let events = harness.take_until("turn.completed").await;
    let reasoning: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "content.delta" && e["payload"]["streamKind"] == "reasoning_summary_text")
        .collect();
    assert_eq!(reasoning.len(), 2);
    assert_eq!(reasoning[0]["payload"]["delta"], json!("Use Euclidean algorithm."));
    assert_eq!(reasoning[0]["turnId"], json!(turn.turn_id.as_str()));
    let deltas: Vec<&Value> = of_type(&events, "content.delta").into_iter().map(|e| &e["payload"]["delta"]).collect();
    assert_eq!(
        deltas,
        vec![&json!("Use Euclidean algorithm."), &json!("The gcd is 21."), &json!("Verify the result.")]
    );
}

#[tokio::test]
async fn classifies_only_streamed_read_image_inputs_as_image_views() {
    let mut harness = Harness::default();
    harness.start(json!({})).await;
    harness.send(json!({"input": "inspect both files"})).await;
    let s = "sdk-session-read-image";
    let image_path = format!("/workspace/{}reference image.webp", "nested folder/".repeat(16));
    harness.emit(stream_event(
        s,
        "read-image-start",
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "tool-read-image", "name": "Read", "input": {}}}),
    ));
    harness.emit(stream_event(
        s,
        "read-image-input",
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": json!({"file_path": image_path}).to_string()}}),
    ));
    harness.emit(tool_result(s, "read-image-result", "tool-read-image", "Image Size: 1280x720."));
    harness.emit(stream_event(s, "read-text-start", json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "tool-read-text", "name": "Read", "input": {"file_path": "/workspace/src/index.ts"}}})));
    harness.emit(tool_result(s, "read-text-result", "tool-read-text", "export {};"));
    harness.emit(result_success(s, "read-image-turn-result"));
    let events = harness.take_until("turn.completed").await;
    let item = |id: &str| -> Vec<&Value> {
        events
            .iter()
            .filter(|e| e["type"].as_str().unwrap().starts_with("item.") && e["itemId"] == id)
            .collect()
    };
    let image = item("tool-read-image");
    assert_eq!(
        image
            .iter()
            .map(|e| (e["type"].as_str().unwrap(), e["payload"]["itemType"].as_str().unwrap()))
            .collect::<Vec<_>>(),
        vec![
            ("item.started", "dynamic_tool_call"),
            ("item.updated", "image_view"),
            ("item.updated", "image_view"),
            ("item.completed", "image_view")
        ]
    );
    for event in &image[1..] {
        assert_eq!(event["payload"]["detail"], json!(image_path));
        assert_eq!(event["payload"]["data"]["input"]["file_path"], json!(image_path));
    }
    assert_eq!(
        item("tool-read-text")
            .iter()
            .map(|e| e["payload"]["itemType"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["dynamic_tool_call"; 3]
    );
}

#[tokio::test]
async fn falls_back_to_a_default_plan_step_label_for_blank_todo_write_content() {
    let mut harness = Harness::default();
    harness.start(json!({})).await;
    let turn = harness.send(json!({"input": "hello"})).await;
    let s = "sdk-session-todo-plan";
    harness.emit(stream_event(
        s,
        "stream-todo-start",
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "tool-todo-1", "name": "TodoWrite", "input": {}}}),
    ));
    harness.emit(stream_event(
        s,
        "stream-todo-input",
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"todos\":[{\"content\":\"   \",\"status\":\"in_progress\"},{\"content\":\"Ship it\",\"status\":\"completed\"}]}"}}),
    ));
    harness.emit(stream_event(s, "stream-todo-stop", json!({"type": "content_block_stop", "index": 1})));
    harness.emit(result_success(s, "result-todo-plan"));
    let events = harness.take_until("turn.completed").await;
    let plan = first_of(&events, "turn.plan.updated");
    assert_eq!(plan["turnId"], json!(turn.turn_id.as_str()));
    assert_eq!(
        plan["payload"]["plan"],
        json!([{"step": "Task", "status": "inProgress"}, {"step": "Ship it", "status": "completed"}])
    );
}

#[tokio::test]
async fn classifies_claude_task_tool_invocations_as_collaboration_agent_work() {
    let mut harness = Harness::default();
    harness.start(json!({})).await;
    harness.send(json!({"input": "delegate this"})).await;
    let s = "sdk-session-task";
    harness.emit(stream_event(
        s,
        "stream-task-1",
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "tool-task-1", "name": "Task",
            "input": {"description": "Review the database layer", "prompt": "Audit the SQL changes", "subagent_type": "code-reviewer"}}}),
    ));
    harness.emit(json!({"type": "system", "subtype": "task_started", "task_id": "task-agent-1", "description": "Review the database layer", "task_type": "local_agent", "tool_use_id": "tool-task-1", "uuid": "task-agent-1-uuid", "session_id": s}));
    harness.emit(assistant(
        s,
        "assistant-task-1",
        "assistant-message-task-1",
        json!([{"type": "text", "text": "Delegated"}]),
    ));
    harness.emit(json!({"type": "result", "subtype": "success", "is_error": false, "errors": [],
        "usage": {"input_tokens": 100, "cache_read_input_tokens": 40, "cache_creation_input_tokens": 10, "output_tokens": 20}, "session_id": s, "uuid": "result-task-1"}));
    let events = harness.take_until("turn.completed").await;
    let started = first_of(&events, "item.started");
    assert_eq!(started["payload"]["itemType"], json!("collab_agent_tool_call"));
    assert_eq!(started["payload"]["title"], json!("Subagent task"));
    assert_eq!(first_of(&events, "turn.completed")["payload"]["tokenUsage"]["hasSubagents"], json!(true));
}
