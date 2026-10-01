//! `usageTranscripts.test.ts`.

use serde_json::{json, Value};

use super::*;

fn claude_line(message_id: &str, content_type: &str, output_tokens: u64, speed: Option<&str>) -> Vec<u8> {
    let mut usage = json!({
        "input_tokens": 2,
        "cache_creation_input_tokens": 66818,
        "cache_read_input_tokens": 1000,
        "output_tokens": output_tokens,
    });
    if let Some(speed) = speed {
        usage["speed"] = json!(speed);
    }
    json!({
        "type": "assistant",
        "timestamp": "2026-08-07T04:05:13.944Z",
        "sessionId": "5a128faa-8253-489e-b935-6c08e8e670c0",
        "cwd": "/home/dev/project",
        "message": {
            "id": message_id,
            "role": "assistant",
            "model": "claude-fable-5",
            "content": [{"type": content_type}],
            "usage": usage,
        },
    })
    .to_string()
    .into_bytes()
}

#[test]
fn claude_extracts_token_totals_and_a_dedupe_key() {
    let record = parse_claude_line(&claude_line("msg_1", "text", 286, None)).unwrap();
    assert_eq!(record.provider, UsageProviderKind::Claude);
    assert_eq!(&*record.model, "claude-fable-5");
    assert_eq!(
        record.totals,
        Totals {
            uncached_input_tokens: 2.0,
            cached_input_tokens: 1000.0,
            cache_creation_tokens: 66818.0,
            output_tokens: 286.0,
            reasoning_tokens: 0.0
        }
    );
    assert_eq!(record.dedupe_key.as_deref(), Some("msg_1:"));
    assert!(!record.fast);
}

#[test]
fn claude_marks_fast_mode_requests() {
    assert!(parse_claude_line(&claude_line("msg_1", "text", 286, Some("fast"))).unwrap().fast);
    assert!(!parse_claude_line(&claude_line("msg_1", "text", 286, Some("standard"))).unwrap().fast);
}

#[test]
fn claude_content_blocks_share_one_dedupe_key() {
    let text = parse_claude_line(&claude_line("msg_2", "text", 286, None)).unwrap();
    let tool_use = parse_claude_line(&claude_line("msg_2", "tool_use", 286, None)).unwrap();
    assert_eq!(text.dedupe_key, tool_use.dedupe_key);
    assert_eq!(text.totals, tool_use.totals);
}

#[test]
fn claude_ignores_non_assistant_records() {
    assert!(parse_claude_line(json!({"type": "user", "message": {}}).to_string().as_bytes()).is_none());
    assert!(parse_claude_line(b"not json").is_none());
}

const SESSION_META: &str =
    r#"{"type":"session_meta","timestamp":"2026-08-01T05:17:41.289Z","payload":{"type":"session_meta","id":"019fbbc1-b12c-7360-a685-28c181f0025f"}}"#;
const TURN_CONTEXT: &str = r#"{"type":"turn_context","timestamp":"2026-08-01T05:17:42.694Z","payload":{"type":"turn_context","model":"gpt-5.6-sol"}}"#;

fn token_count(input: u64, cached: u64, output: u64, reasoning: u64) -> String {
    json!({
        "type": "event_msg",
        "timestamp": "2026-08-01T05:17:49.919Z",
        "payload": {
            "type": "token_count",
            "info": {"last_token_usage": {
                "input_tokens": input,
                "cached_input_tokens": cached,
                "cache_write_input_tokens": 0,
                "output_tokens": output,
                "reasoning_output_tokens": reasoning,
            }},
        },
    })
    .to_string()
}

fn codex(line: &str, state: &mut CodexScanState) -> Option<UsageRecord> {
    parse_codex_line(line.as_bytes(), state)
}

#[test]
fn codex_attributes_usage_to_the_preceding_turn_context() {
    let mut state = CodexScanState::default();
    codex(SESSION_META, &mut state);
    codex(TURN_CONTEXT, &mut state);
    let record = codex(&token_count(19239, 11008, 299, 116), &mut state).unwrap();
    assert_eq!(record.provider, UsageProviderKind::Codex);
    assert_eq!(&*record.model, "gpt-5.6-sol");
    assert_eq!(&*record.session_id, "019fbbc1-b12c-7360-a685-28c181f0025f");
    assert_eq!(record.totals.uncached_input_tokens, (19239 - 11008) as f64);
    assert_eq!(record.totals.cached_input_tokens, 11008.0);
    assert_eq!(record.totals.reasoning_tokens, 116.0);
}

#[test]
fn codex_skips_a_repeated_token_count() {
    let mut state = CodexScanState::default();
    codex(TURN_CONTEXT, &mut state);
    assert!(codex(&token_count(100, 0, 10, 0), &mut state).is_some());
    assert!(codex(&token_count(100, 0, 10, 0), &mut state).is_none());
}

#[test]
fn codex_drops_usage_before_any_model() {
    let mut state = CodexScanState::default();
    assert!(codex(&token_count(100, 0, 10, 0), &mut state).is_none());
}

#[test]
fn codex_pre_model_event_does_not_poison_the_signature() {
    let mut state = CodexScanState::default();
    assert!(codex(&token_count(100, 0, 10, 0), &mut state).is_none());
    codex(TURN_CONTEXT, &mut state);
    assert!(codex(&token_count(100, 0, 10, 0), &mut state).is_some());
}

fn meta(id: &str, timestamp: &str, forked_from_id: Option<&str>, spawn_parent_id: Option<&str>) -> String {
    let mut payload = json!({"type": "session_meta", "id": id});
    if let Some(parent) = forked_from_id {
        payload["forked_from_id"] = json!(parent);
    }
    if let Some(parent) = spawn_parent_id {
        payload["source"] = json!({"subagent": {"thread_spawn": {"parent_thread_id": parent}}});
    }
    json!({"type": "session_meta", "timestamp": timestamp, "payload": payload}).to_string()
}

fn stamped(timestamp: &str, line: &str) -> String {
    let mut parsed: Value = serde_json::from_str(line).unwrap();
    parsed["timestamp"] = json!(timestamp);
    parsed.to_string()
}

#[test]
fn codex_fork_keeps_the_child_session_id() {
    let mut state = CodexScanState::default();
    codex(&meta("child", "2026-08-01T05:00:00.000Z", None, None), &mut state);
    codex(&meta("parent", "2026-08-01T05:00:00.000Z", None, None), &mut state);
    codex(TURN_CONTEXT, &mut state);
    assert_eq!(&*codex(&token_count(100, 0, 10, 0), &mut state).unwrap().session_id, "child");
}

#[test]
fn codex_fork_drops_the_copied_burst_and_keeps_the_first_real_event() {
    let mut state = CodexScanState::default();
    let fork = "2026-08-01T05:00:00.000Z";
    codex(&meta("child", fork, Some("parent"), None), &mut state);
    codex(&meta("parent", fork, None, None), &mut state);
    codex(&stamped(fork, TURN_CONTEXT), &mut state);
    assert!(codex(&stamped("2026-08-01T05:00:00.001Z", &token_count(100, 0, 10, 0)), &mut state).is_none());
    assert!(codex(&stamped("2026-08-01T05:00:00.002Z", &token_count(200, 0, 20, 0)), &mut state).is_none());
    let real = codex(&stamped("2026-08-01T05:00:06.000Z", &token_count(300, 0, 30, 0)), &mut state).unwrap();
    assert_eq!(real.totals.output_tokens, 30.0);
    assert!(codex(&stamped("2026-08-01T05:00:06.100Z", &token_count(400, 0, 40, 0)), &mut state).is_some());
}

#[test]
fn codex_recognizes_subagent_spawns() {
    let mut state = CodexScanState::default();
    let spawn = "2026-08-01T05:00:00.000Z";
    codex(&meta("child", spawn, None, Some("parent")), &mut state);
    codex(&stamped(spawn, TURN_CONTEXT), &mut state);
    assert!(codex(&stamped("2026-08-01T05:00:00.001Z", &token_count(100, 0, 10, 0)), &mut state).is_none());
}

#[test]
fn codex_does_not_suppress_a_rollout_that_is_not_a_fork() {
    let mut state = CodexScanState::default();
    codex(&meta("root", "2026-08-01T05:00:00.000Z", None, None), &mut state);
    codex(&stamped("2026-08-01T05:00:00.100Z", TURN_CONTEXT), &mut state);
    assert!(codex(&stamped("2026-08-01T05:00:00.200Z", &token_count(100, 0, 10, 0)), &mut state).is_some());
}

#[test]
fn total_tokens_does_not_add_reasoning() {
    let totals = Totals {
        uncached_input_tokens: 10.0,
        cached_input_tokens: 20.0,
        cache_creation_tokens: 30.0,
        output_tokens: 40.0,
        reasoning_tokens: 25.0,
    };
    assert_eq!(totals.total(), 100.0);
}

fn turn_completed(model_usage: Option<Value>, usage_overrides: Value) -> Vec<u8> {
    let mut usage = json!({
        "inputTokens": 20_272,
        "outputTokens": 272,
        "totalTokens": 20_544,
        "cachedReadTokens": 11_264,
        "cacheCreationTokens": 0,
        "reasoningTokens": 180,
        "costUsdTicks": 230_272_000,
    });
    if let Some(model_usage) = model_usage {
        usage["modelUsage"] = model_usage;
    }
    for (key, value) in usage_overrides.as_object().unwrap() {
        usage[key] = value.clone();
    }
    json!({
        "timestamp": 1_786_372_566,
        "method": "_x.ai/session/update",
        "params": {
            "sessionId": "019fec1a-12f7-72f2-9b1f-7778a00aea3c",
            "update": {"sessionUpdate": "turn_completed", "prompt_id": "prompt-1", "stop_reason": "end_turn", "usage": usage},
            "_meta": {"eventId": "event-1", "agentTimestampMs": 1_786_372_566_485u64},
        },
    })
    .to_string()
    .into_bytes()
}

fn default_model_usage() -> Value {
    json!({"grok-4.5-build": {
        "inputTokens": 20_272, "outputTokens": 272, "totalTokens": 20_544, "cachedReadTokens": 11_264,
        "cacheCreationTokens": 0, "reasoningTokens": 180, "costUsdTicks": 230_272_000,
    }})
}

fn close(actual: Option<f64>, expected: f64) {
    let actual = actual.expect("a cost");
    assert!((actual - expected).abs() < 1e-12, "{actual} != {expected}");
}

#[test]
fn grok_extracts_per_model_totals_and_cost_ticks() {
    let records = parse_grok_line(&turn_completed(Some(default_model_usage()), json!({})));
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.provider, UsageProviderKind::Grok);
    assert_eq!(&*record.model, "grok-4.5-build");
    assert_eq!(&*record.session_id, "019fec1a-12f7-72f2-9b1f-7778a00aea3c");
    assert_eq!(record.timestamp_ms, 1_786_372_566_485.0);
    assert_eq!(
        record.totals,
        Totals {
            uncached_input_tokens: 9008.0,
            cached_input_tokens: 11_264.0,
            cache_creation_tokens: 0.0,
            output_tokens: 272.0,
            reasoning_tokens: 180.0
        }
    );
    close(record.reported_cost_usd, 230_272_000.0 / GROK_COST_USD_TICKS_PER_DOLLAR);
    assert_eq!(
        record.dedupe_key.as_deref(),
        Some("019fec1a-12f7-72f2-9b1f-7778a00aea3c:prompt-1:grok-4.5-build")
    );
}

#[test]
fn grok_one_record_per_model() {
    let records = parse_grok_line(&turn_completed(
        Some(json!({
            "grok-4.5": {"inputTokens": 1000, "outputTokens": 50, "cachedReadTokens": 400, "reasoningTokens": 20, "costUsdTicks": 50_000_000},
            "grok-composer-2.5-fast": {"inputTokens": 200, "outputTokens": 30, "cachedReadTokens": 100, "reasoningTokens": 0, "costUsdTicks": 10_000_000},
        })),
        json!({}),
    ));
    let mut models: Vec<&str> = records.iter().map(|record| &*record.model).collect();
    models.sort_unstable();
    assert_eq!(models, ["grok-4.5", "grok-composer-2.5-fast"]);
    close(records.iter().find(|record| &*record.model == "grok-4.5").unwrap().reported_cost_usd, 0.005);
}

#[test]
fn grok_single_model_inherits_top_level_ticks() {
    let records = parse_grok_line(&turn_completed(
        Some(json!({"grok-4.5-build": {"inputTokens": 1000, "outputTokens": 10, "cachedReadTokens": 0, "reasoningTokens": 0}})),
        json!({"costUsdTicks": GROK_COST_USD_TICKS_PER_DOLLAR}),
    ));
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].reported_cost_usd, Some(1.0));
}

#[test]
fn grok_generic_model_without_model_usage() {
    let records = parse_grok_line(&turn_completed(None, json!({})));
    assert_eq!(records.len(), 1);
    assert_eq!(&*records[0].model, "grok");
    assert_eq!(records[0].totals.uncached_input_tokens, 9008.0);
    close(records[0].reported_cost_usd, 230_272_000.0 / GROK_COST_USD_TICKS_PER_DOLLAR);
    assert_eq!(records[0].dedupe_key.as_deref(), Some("019fec1a-12f7-72f2-9b1f-7778a00aea3c:prompt-1:grok"));
}

fn by_model(records: &[UsageRecord], model: &str) -> Option<f64> {
    records
        .iter()
        .find(|record| &*record.model == model)
        .and_then(|record| record.reported_cost_usd)
}

#[test]
fn grok_pro_rates_top_level_ticks() {
    let records = parse_grok_line(&turn_completed(
        Some(json!({
            "grok-4.5": {"inputTokens": 300, "outputTokens": 0, "cachedReadTokens": 0, "reasoningTokens": 0},
            "grok-composer-2.5-fast": {"inputTokens": 100, "outputTokens": 0, "cachedReadTokens": 0, "reasoningTokens": 0},
        })),
        json!({"costUsdTicks": GROK_COST_USD_TICKS_PER_DOLLAR}),
    ));
    assert_eq!(records.len(), 2);
    close(by_model(&records, "grok-4.5"), 0.75);
    close(by_model(&records, "grok-composer-2.5-fast"), 0.25);
}

#[test]
fn grok_zero_token_sibling_with_zero_ticks() {
    let records = parse_grok_line(&turn_completed(
        Some(json!({
            "grok-4.5": {"inputTokens": 300, "outputTokens": 0, "cachedReadTokens": 0, "reasoningTokens": 0},
            "grok-composer-2.5-fast": {"inputTokens": 100, "outputTokens": 0, "cachedReadTokens": 0, "reasoningTokens": 0},
            "empty-sibling": {"inputTokens": 0, "outputTokens": 0, "cachedReadTokens": 0, "reasoningTokens": 0, "costUsdTicks": 0},
        })),
        json!({"costUsdTicks": GROK_COST_USD_TICKS_PER_DOLLAR}),
    ));
    assert_eq!(records.len(), 2);
    assert!(records.iter().all(|record| &*record.model != "empty-sibling"));
    close(by_model(&records, "grok-4.5"), 0.75);
    close(by_model(&records, "grok-composer-2.5-fast"), 0.25);
}

#[test]
fn grok_allocates_leftover_ticks() {
    let records = parse_grok_line(&turn_completed(
        Some(json!({
            "grok-4.5": {"inputTokens": 300, "outputTokens": 0, "cachedReadTokens": 0, "reasoningTokens": 0, "costUsdTicks": 0.4 * GROK_COST_USD_TICKS_PER_DOLLAR},
            "grok-composer-2.5-fast": {"inputTokens": 100, "outputTokens": 0, "cachedReadTokens": 0, "reasoningTokens": 0},
        })),
        json!({"costUsdTicks": GROK_COST_USD_TICKS_PER_DOLLAR}),
    ));
    close(by_model(&records, "grok-4.5"), 0.4);
    close(by_model(&records, "grok-composer-2.5-fast"), 0.6);
}

#[test]
fn grok_no_dedupe_key_without_prompt_id() {
    let line = json!({
        "timestamp": 1_786_372_566,
        "params": {"sessionId": "s1", "update": {"sessionUpdate": "turn_completed", "usage": {
            "inputTokens": 10, "outputTokens": 2, "modelUsage": {"grok-4.5": {"inputTokens": 10, "outputTokens": 2}},
        }}},
    });
    assert_eq!(parse_grok_line(line.to_string().as_bytes())[0].dedupe_key, None);
}

#[test]
fn grok_ignores_non_turn_lines_and_empty_usage() {
    assert!(parse_grok_line(br#"{"method":"session/update","params":{}}"#).is_empty());
    assert!(parse_grok_line(b"not json").is_empty());
    assert!(parse_grok_line(&turn_completed(
        Some(json!({"grok-4.5-build": {"inputTokens": 0, "outputTokens": 0, "cachedReadTokens": 0, "reasoningTokens": 0, "costUsdTicks": 0}})),
        json!({}),
    ))
    .is_empty());
}

#[test]
fn grok_falls_back_to_unix_seconds() {
    let line = json!({
        "timestamp": 1_786_372_566,
        "params": {"sessionId": "s1", "update": {"sessionUpdate": "turn_completed", "prompt_id": "p1", "usage": {
            "inputTokens": 10, "outputTokens": 2, "modelUsage": {"grok-4.5": {"inputTokens": 10, "outputTokens": 2}},
        }}},
    });
    assert_eq!(parse_grok_line(line.to_string().as_bytes())[0].timestamp_ms, 1_786_372_566_000.0);
}
