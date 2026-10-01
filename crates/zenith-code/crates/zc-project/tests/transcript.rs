//! Port of the `parseAgentSessionTranscript` part of `project/AgentSessionScanner.test.ts`.

use serde_json::{json, Value};
use zc_contracts::{AgentSessionSource, ProviderInstanceId};
use zc_project::sessions::transcript::{parse_agent_session_transcript, AgentSessionThread, TranscriptMetadata};

const NOW_MS: i64 = 1_787_572_800_000; // 2026-08-24T12:00:00.000Z
const LATER_MS: i64 = 1_787_644_800_000; // 2026-08-25T08:00:00.000Z

fn line(value: Value) -> String {
    value.to_string()
}

fn parse(contents: &str, source: AgentSessionSource, fallback: &str, now: i64) -> Option<AgentSessionThread> {
    parse_agent_session_transcript(
        &TranscriptMetadata {
            source,
            provider_instance_id: ProviderInstanceId::new(source.as_str()),
            fallback_session_id: fallback.into(),
            last_active_at_ms: now,
        },
        contents,
    )
}

fn codex(records: &[Value]) -> Option<AgentSessionThread> {
    let contents = records.iter().map(|r| r.to_string()).collect::<Vec<_>>().join("\n");
    parse(&contents, AgentSessionSource::Codex, "fallback", NOW_MS)
}

fn texts(thread: &Option<AgentSessionThread>) -> Vec<String> {
    thread.as_ref().unwrap().messages.iter().map(|m| m.text.clone()).collect()
}

fn session_meta(id: &str) -> Value {
    json!({"type": "session_meta", "payload": {"id": id}})
}

fn user_event(message: &str) -> Value {
    json!({"type": "event_msg", "payload": {"type": "user_message", "message": message}})
}

fn response_user(text: &str, turn: Option<Value>) -> Value {
    let mut payload = json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]});
    if let Some(turn) = turn {
        payload["internal_chat_message_metadata_passthrough"] = turn;
    }
    json!({"type": "response_item", "payload": payload})
}

fn response_assistant(text: &str) -> Value {
    json!({"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}})
}

/// `makeRecordLimitTranscript`.
pub fn record_limit_transcript(cwd: &str, overflow: bool) -> String {
    let mut records = [
        line(json!({"type": "session_meta", "payload": {"id": "record-limit-session", "cwd": cwd}})),
        line(json!({"type": "event_msg", "payload": {"type": "user_message", "message": "First prompt"}})),
    ]
    .join("\n");
    records.push('\n');
    records.push_str(&"{}\n".repeat(99_998));
    if overflow {
        records.push('\n');
        records.push_str(&line(
            json!({"type": "event_msg", "payload": {"type": "user_message", "message": "Overflow prompt"}}),
        ));
        records.push('\n');
    }
    records
}

#[test]
fn handles_the_exact_record_limit_and_an_interior_blank() {
    for overflow in [false, true] {
        let thread = parse(&record_limit_transcript("/project", overflow), AgentSessionSource::Codex, "unused", NOW_MS);
        if overflow {
            assert!(thread.is_none());
        } else {
            assert_eq!(texts(&thread), ["First prompt"]);
        }
    }
}

#[test]
fn keeps_claude_text_and_titles_while_dropping_malformed_and_tool_records() {
    let contents = [
        "not valid json".to_owned(),
        line(json!({"type": "ai-title", "aiTitle": "Fix authentication"})),
        line(json!({"type": "user", "sessionId": "claude-session", "isMeta": true, "message": {"role": "user", "content": "Injected skill instructions"}})),
        line(json!({"type": "user", "sessionId": "claude-session", "isCompactSummary": true, "message": {"role": "user", "content": "Injected compaction summary"}})),
        line(json!({"type": "user", "sessionId": "claude-session", "timestamp": "2026-08-24T10:00:00.000Z", "message": {"role": "user", "content": [{"type": "text", "text": "Fix authentication"}]}})),
        line(json!({"type": "user", "sessionId": "claude-session", "message": {"role": "user", "content": [{"type": "tool_result", "text": "hidden"}]}})),
        line(json!({"type": "assistant", "sessionId": "claude-session", "message": {"role": "assistant", "model": "claude-sonnet-5", "content": [{"type": "text", "text": "Updated the login flow"}]}})),
        line(json!({"type": "assistant", "sessionId": "claude-session", "message": {"role": "assistant", "model": "<synthetic>", "content": [{"type": "text", "text": "The provider request failed"}]}})),
    ]
    .join("\n");
    let thread = parse(&contents, AgentSessionSource::ClaudeAgent, "fallback", NOW_MS).unwrap();
    assert_eq!(thread.provider_session_id, "claude-session");
    assert_eq!(thread.title, "Fix authentication");
    assert_eq!(thread.model.as_deref(), Some("claude-sonnet-5"));
    let messages: Vec<(&str, &str)> = thread.messages.iter().map(|m| (m.role.as_str(), m.text.as_str())).collect();
    assert_eq!(
        messages,
        [
            ("user", "Fix authentication"),
            ("assistant", "Updated the login flow"),
            ("assistant", "The provider request failed")
        ]
    );
    assert_eq!(thread.created_at, "2026-08-24T10:00:00.000Z");
    assert_eq!(thread.updated_at, "2026-08-24T12:00:00.000Z");
}

#[test]
fn drops_injected_codex_instructions_while_keeping_the_visible_user_event() {
    let thread = codex(&[
        json!({"type": "session_meta", "payload": {"id": "codex-session"}}),
        response_user(
            "<user_instructions>\nInternal setup instructions\n</user_instructions>",
            Some(json!({"turn_id": "turn-1"})),
        ),
        user_event("Fix the actual bug"),
        response_user("Fix the actual bug", Some(json!({"turn_id": "turn-1"}))),
        response_assistant("Fixed"),
    ]);
    assert_eq!(texts(&thread), ["Fix the actual bug", "Fixed"]);
}

#[test]
fn keeps_the_canonical_first_prompt_after_long_codex_transcripts_are_capped() {
    let canonical = "\n  Keep the canonical prompt  \n";
    let mut records = vec![
        session_meta("codex-session"),
        json!({"type": "response_item", "timestamp": "2026-08-24T10:00:00.000Z", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Keep the canonical prompt"}]}}),
        json!({"type": "event_msg", "timestamp": "2026-08-24T10:01:00.000Z", "payload": {"type": "user_message", "message": canonical}}),
    ];
    for index in 0..200 {
        records.push(json!({"type": "response_item", "timestamp": format!("2026-08-24T11:{:02}:00.000Z", index % 60), "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": format!("Assistant message {index}")}]}}));
    }
    let thread = codex(&records).unwrap();
    assert_eq!(thread.messages.len(), 200);
    assert_eq!(thread.messages[0].role, "user");
    assert_eq!(thread.messages[0].text, canonical);
    assert_eq!(thread.messages[0].created_at, "2026-08-24T10:01:00.000Z");
}

#[test]
fn restores_the_canonical_first_prompt_when_a_later_user_message_remains() {
    let canonical = "\n  Keep the canonical prompt  \n";
    let mut records = vec![
        session_meta("codex-session"),
        json!({"type": "response_item", "timestamp": "2026-08-24T10:00:00.000Z", "payload": {"type": "message", "role": "user", "internal_chat_message_metadata_passthrough": {"turn_id": "turn-1"}, "content": [{"type": "input_text", "text": "Keep the canonical prompt"}]}}),
        json!({"type": "event_msg", "timestamp": "2026-08-24T10:01:00.000Z", "payload": {"type": "user_message", "message": canonical}}),
    ];
    for index in 0..198 {
        records.push(json!({"type": "response_item", "timestamp": format!("2026-08-24T11:{:02}:00.000Z", index % 60), "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": format!("Assistant message {index}")}]}}));
    }
    records
        .push(json!({"type": "event_msg", "timestamp": "2026-08-24T11:58:30.000Z", "payload": {"type": "user_message", "message": "Keep this later prompt"}}));
    records.push(json!({"type": "response_item", "timestamp": "2026-08-24T11:59:00.000Z", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Keep this latest response"}]}}));
    let thread = codex(&records).unwrap();
    assert_eq!(thread.messages.len(), 200);
    assert_eq!(thread.messages[0].text, canonical);
    assert_eq!(thread.messages[0].created_at, "2026-08-24T10:01:00.000Z");
    assert_eq!(thread.messages.iter().filter(|m| m.text.trim() == canonical.trim()).count(), 1);
    assert!(thread.messages.iter().any(|m| m.text == "Keep this later prompt"));
    assert_eq!(thread.messages.last().unwrap().text, "Keep this latest response");
}

#[test]
fn keeps_mixed_format_response_users_when_turn_ids_repeat_after_an_assistant() {
    let thread = codex(&[
        session_meta("codex-session"),
        response_user("Keep this older prompt", Some(json!({"turn_id": "turn-older"}))),
        user_event("Keep this newer prompt"),
        response_user("Keep this newer prompt", Some(json!({"turn_id": "turn-newer"}))),
        response_assistant("Ask again when needed"),
        response_user("Keep this newer prompt", Some(json!({"turn_id": "turn-newer"}))),
    ]);
    assert_eq!(
        texts(&thread),
        [
            "Keep this older prompt",
            "Keep this newer prompt",
            "Ask again when needed",
            "Keep this newer prompt"
        ]
    );
}

#[test]
fn preserves_response_user_text_when_codex_turn_metadata_is_ambiguous() {
    let thread = codex(&[
        session_meta("codex-session"),
        response_user("Keep this legacy prompt", Some(json!(["unexpected"]))),
        response_user("Keep this prompt with a blank turn ID", Some(json!({"turn_id": "   "}))),
    ]);
    assert_eq!(texts(&thread), ["Keep this legacy prompt", "Keep this prompt with a blank turn ID"]);
}

#[test]
fn uses_the_first_valid_codex_session_id_when_a_fork_copies_ancestor_metadata() {
    let thread = codex(&[
        json!({"type": "session_meta", "payload": {"id": "fork-session", "forked_from_id": "parent-session"}}),
        session_meta("parent-session"),
        user_event("Continue in the fork"),
    ]);
    assert_eq!(thread.unwrap().provider_session_id, "fork-session");
}

#[test]
fn skips_codex_transcripts_without_a_resumable_session_id() {
    let contents = line(user_event("This transcript has no session metadata"));
    assert!(parse(&contents, AgentSessionSource::Codex, "rollout-2026-08-24T12-00-00-not-a-session-id", NOW_MS).is_none());
}

#[test]
fn uses_the_canonical_codex_event_when_its_turn_has_generated_response_context() {
    let contents = [
        session_meta("codex-session"),
        response_user(
            "<environment_context>\n<cwd>/tmp/project</cwd>\n<shell>zsh</shell>\n</environment_context>",
            Some(json!({"turn_id": "turn-1"})),
        ),
        response_user(
            "# AGENTS.md instructions for /tmp/project\n\n<INSTRUCTIONS>\nPrivate project rules\n</INSTRUCTIONS>",
            Some(json!({"turn_id": "turn-1"})),
        ),
        user_event("Do something here so it looks like a real project."),
        response_user("Do something here so it looks like a real project.", Some(json!({"turn_id": "turn-1"}))),
        response_assistant("Created the project."),
    ]
    .iter()
    .map(Value::to_string)
    .collect::<Vec<_>>()
    .join("\n");
    let thread = parse(&contents, AgentSessionSource::Codex, "fallback", LATER_MS);
    assert_eq!(thread.as_ref().unwrap().title, "Do something here so it looks like a real project.");
    assert_eq!(texts(&thread), ["Do something here so it looks like a real project.", "Created the project."]);
}

#[test]
fn preserves_context_markup_in_response_only_codex_messages() {
    let context = "<environment_context>\n<cwd>/tmp/project</cwd>\n</environment_context>";
    let thread = codex(&[
        session_meta("codex-session"),
        response_user(context, None),
        response_user("Initialize Git and add a README.", None),
    ]);
    assert_eq!(thread.as_ref().unwrap().title, "<environment_context>");
    assert_eq!(texts(&thread), [context, "Initialize Git and add a README."]);
}

#[test]
fn preserves_a_canonical_codex_event_that_starts_with_context_markup() {
    let prompt = "<environment_context>\n<cwd>/tmp/project</cwd>\n</environment_context>\n\nCreate a useful project.";
    let thread = codex(&[session_meta("codex-session"), user_event(prompt)]);
    assert_eq!(thread.as_ref().unwrap().title, "<environment_context>");
    assert_eq!(texts(&thread), [prompt]);
}

#[test]
fn preserves_a_codex_request_heading_in_a_canonical_event() {
    let prompt = "\n  ## My request for Codex:\n\nFix the visible bug";
    let thread = codex(&[session_meta("codex-session"), user_event(prompt)]);
    assert_eq!(thread.as_ref().unwrap().title, "## My request for Codex:");
    assert_eq!(texts(&thread), [prompt]);
}

#[test]
fn keeps_context_markup_quoted_inside_visible_codex_user_text() {
    let quoted = "Do not remove this example:\n<environment_context>\n<cwd>/tmp/example</cwd>\n</environment_context>";
    let thread = codex(&[session_meta("codex-session"), user_event(quoted)]);
    assert_eq!(texts(&thread), [quoted]);
}

#[test]
fn skips_sessions_without_a_visible_user_message() {
    let contents = line(json!({"type": "assistant", "message": {"role": "assistant", "content": "Done"}}));
    assert!(parse(&contents, AgentSessionSource::ClaudeAgent, "claude-session", NOW_MS).is_none());
}

#[test]
fn keeps_the_first_prompt_when_later_assistant_output_exceeds_the_message_limit() {
    let mut records = vec![line(
        json!({"type": "user", "sessionId": "claude-session", "message": {"role": "user", "content": "Keep this prompt"}}),
    )];
    for index in 0..250 {
        records.push(line(
            json!({"type": "assistant", "message": {"role": "assistant", "content": format!("Assistant update {index}")}}),
        ));
    }
    let thread = parse(&records.join("\n"), AgentSessionSource::ClaudeAgent, "fallback", NOW_MS).unwrap();
    assert_eq!(thread.messages.len(), 200);
    assert_eq!(thread.messages[0].text, "Keep this prompt");
    assert_eq!(thread.messages.last().unwrap().text, "Assistant update 249");
}

#[test]
fn selector_matches_the_record_schema() {
    use zc_project::sessions::json::PathSegment::{Index, Key};
    use zc_project::sessions::transcript::select_transcript_path as select;
    let key = |k: &str| Key(k.into());
    assert!(select(&[]));
    assert!(select(&[key("cwd")]));
    assert!(!select(&[key("cwd"), key("x")]));
    assert!(!select(&[key("toolUseResult")]));
    assert!(select(&[key("message"), key("content"), Index(3), key("text")]));
    assert!(!select(&[key("message"), key("content"), Index(3), key("input")]));
    assert!(select(&[key("payload"), key("internal_chat_message_metadata_passthrough"), key("turn_id")]));
    assert!(!select(&[key("payload"), key("arguments")]));
    assert!(!select(&[key("message"), key("usage")]));
}
