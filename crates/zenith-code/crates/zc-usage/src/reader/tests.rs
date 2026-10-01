//! `usageTranscriptReader.test.ts` (resume) and `usageTranscriptStreaming.test.ts` (large
//! records), plus the SQLite readers of the former.

use std::io::Write as _;
use std::path::Path;

use serde_json::{json, Value};

use super::*;
use crate::records::Totals;

fn claude_line(id: u32, output_tokens: u64) -> String {
    format!(
        "{}\n",
        json!({
            "type": "assistant",
            "timestamp": "2026-08-01T10:00:00Z",
            "requestId": format!("req_{id}"),
            "sessionId": "session-1",
            "message": {"id": format!("msg_{id}"), "model": "claude-fable-5", "usage": {"input_tokens": 10, "output_tokens": output_tokens}},
        })
    )
}

fn codex_meta_line() -> String {
    format!(
        "{}\n",
        json!({"type": "session_meta", "timestamp": "2026-08-01T10:00:00Z", "payload": {"type": "session_meta", "id": "codex-session-1"}})
    )
}

fn codex_model_line(model: &str) -> String {
    format!(
        "{}\n",
        json!({"type": "turn_context", "timestamp": "2026-08-01T10:00:01Z", "payload": {"type": "turn_context", "model": model}})
    )
}

fn codex_usage_line(output_tokens: u64, seconds: u32) -> String {
    format!(
        "{}\n",
        json!({
            "type": "event_msg",
            "timestamp": format!("2026-08-01T10:00:{seconds:02}Z"),
            "payload": {"type": "token_count", "info": {"last_token_usage": {"input_tokens": 100, "output_tokens": output_tokens}}},
        })
    )
}

fn append(path: &Path, text: &[u8]) {
    std::fs::OpenOptions::new().append(true).open(path).unwrap().write_all(text).unwrap();
}

fn outputs(records: &[UsageRecord]) -> Vec<f64> {
    records.iter().map(|record| record.totals.output_tokens).collect()
}

fn read(path: &Path, provider: UsageProviderKind, from: Option<&ParsePosition>) -> ParseResult {
    read_transcript_records(path, provider, from).expect("readable")
}

#[test]
fn resume_parses_only_appended_lines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("claude.jsonl");
    std::fs::write(&path, claude_line(1, 5) + &claude_line(2, 7)).unwrap();
    let first = read(&path, UsageProviderKind::Claude, None);
    assert_eq!(first.records.len(), 2);
    assert!(!first.resumed);
    append(&path, claude_line(3, 11).as_bytes());
    let second = read(&path, UsageProviderKind::Claude, Some(&first.position));
    assert!(second.resumed);
    assert_eq!(outputs(&second.records), [11.0]);
    let full = read(&path, UsageProviderKind::Claude, None);
    assert_eq!([first.records, second.records].concat(), full.records);
}

#[test]
fn resume_carries_the_codex_reducer_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    std::fs::write(&path, codex_meta_line() + &codex_model_line("gpt-5.2-codex")).unwrap();
    let first = read(&path, UsageProviderKind::Codex, None);
    assert!(first.records.is_empty());
    append(&path, codex_usage_line(9, 5).as_bytes());
    let second = read(&path, UsageProviderKind::Codex, Some(&first.position));
    assert!(second.resumed);
    assert_eq!(second.records.len(), 1);
    assert_eq!(&*second.records[0].model, "gpt-5.2-codex");
    assert_eq!(&*second.records[0].session_id, "codex-session-1");
}

#[test]
fn resume_suppresses_a_codex_duplicate_across_the_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    std::fs::write(&path, codex_meta_line() + &codex_model_line("gpt-5.2-codex") + &codex_usage_line(9, 5)).unwrap();
    let first = read(&path, UsageProviderKind::Codex, None);
    assert_eq!(first.records.len(), 1);
    append(&path, (codex_usage_line(9, 5) + &codex_usage_line(21, 8)).as_bytes());
    let second = read(&path, UsageProviderKind::Codex, Some(&first.position));
    assert!(second.resumed);
    assert_eq!(outputs(&second.records), [21.0]);
}

#[test]
fn unterminated_tail_is_deferred_then_consumed_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("claude.jsonl");
    let unterminated = claude_line(2, 7);
    std::fs::write(&path, claude_line(1, 5) + unterminated.trim_end()).unwrap();
    let first = read(&path, UsageProviderKind::Claude, None);
    assert_eq!(first.records.len(), 1);
    assert_eq!(outputs(&first.tail_records), [7.0]);
    append(&path, format!("\n{}", claude_line(3, 11)).as_bytes());
    let second = read(&path, UsageProviderKind::Claude, Some(&first.position));
    assert!(second.resumed);
    assert_eq!(outputs(&second.records), [7.0, 11.0]);
    assert!(second.tail_records.is_empty());
}

#[test]
fn reparses_when_the_guard_bytes_changed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("claude.jsonl");
    std::fs::write(&path, claude_line(1, 5)).unwrap();
    let first = read(&path, UsageProviderKind::Claude, None);
    std::fs::write(&path, claude_line(4, 13) + &claude_line(5, 17)).unwrap();
    let second = read(&path, UsageProviderKind::Claude, Some(&first.position));
    assert!(!second.resumed);
    assert_eq!(outputs(&second.records), [13.0, 17.0]);
}

#[test]
fn reparses_when_the_file_shrank() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("claude.jsonl");
    std::fs::write(&path, claude_line(1, 5) + &claude_line(2, 7)).unwrap();
    let first = read(&path, UsageProviderKind::Claude, None);
    std::fs::write(&path, claude_line(3, 11)).unwrap();
    let second = read(&path, UsageProviderKind::Claude, Some(&first.position));
    assert!(!second.resumed);
    assert_eq!(outputs(&second.records), [11.0]);
}

#[test]
fn parses_a_line_larger_than_one_chunk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("claude.jsonl");
    let big = json!({
        "type": "assistant",
        "timestamp": "2026-08-01T10:00:00Z",
        "requestId": "req_big",
        "sessionId": "session-1",
        "padding": "x".repeat(3 * 1024 * 1024),
        "message": {"id": "msg_big", "model": "claude-fable-5", "usage": {"input_tokens": 10, "output_tokens": 42}},
    });
    std::fs::write(&path, format!("{big}\n") + &claude_line(2, 7)).unwrap();
    assert_eq!(outputs(&read(&path, UsageProviderKind::Claude, None).records), [42.0, 7.0]);
}

#[test]
fn unreadable_file_is_none() {
    let dir = tempfile::tempdir().unwrap();
    assert!(read_transcript_records(&dir.path().join("missing.jsonl"), UsageProviderKind::Claude, None).is_none());
}

/* ---------------------------------------------------------------------------------------- */
/* Large records (`usageTranscriptStreaming.test.ts`)                                       */
/* ---------------------------------------------------------------------------------------- */

const TIMESTAMP: &str = "2026-08-01T10:00:00Z";

fn content() -> String {
    "工具 output \\\" usage token_count ".repeat(40_000)
}

fn claude_record(id: &str, output: u64, content: &str) -> Value {
    json!({
        "type": "assistant",
        "timestamp": TIMESTAMP,
        "sessionId": "s1",
        "requestId": format!("r-{id}"),
        "costUSD": 0.25,
        "message": {
            "content": [{"type": "tool_use", "input": {"text": content}}],
            "id": id,
            "model": "claude-fable-5",
            "usage": {"input_tokens": 100, "output_tokens": output, "cache_read_input_tokens": 20, "cache_creation_input_tokens": 5, "speed": "fast"},
        },
    })
}

fn codex_records() -> Vec<Value> {
    vec![
        json!({"type": "session_meta", "timestamp": TIMESTAMP, "payload": {"id": "s1"}}),
        json!({"type": "turn_context", "timestamp": TIMESTAMP, "payload": {"model": "gpt-5.6-sol"}}),
        json!({"type": "event_msg", "timestamp": TIMESTAMP, "payload": {"type": "token_count", "info": {"last_token_usage": {
            "input_tokens": 100, "output_tokens": 99, "cached_input_tokens": 20, "cache_write_input_tokens": 5, "reasoning_output_tokens": 10,
        }}}}),
    ]
}

fn grok_record() -> Value {
    json!({"timestamp": 1_785_578_400u64, "params": {"sessionId": "s1", "_meta": {"agentTimestampMs": 1_785_578_400_123u64}, "update": {
        "sessionUpdate": "turn_completed", "prompt_id": "p1", "usage": {"inputTokens": 100, "outputTokens": 99, "costUsdTicks": 2_500_000_000u64,
        "modelUsage": {"grok-4.5-build": {"inputTokens": 100, "outputTokens": 99, "cachedReadTokens": 20, "cacheCreationTokens": 5, "reasoningTokens": 10}}}}}})
}

fn scan(dir: &Path, lines: &[Value], provider: UsageProviderKind, name: &str) -> ParseResult {
    let path = dir.join(format!("{name}.jsonl"));
    let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
    std::fs::write(&path, text).unwrap();
    read(&path, provider, None)
}

/// `{padding, ...record}` or `{...record, padding}`.
fn padded(record: &Value, first: bool, padding: &str) -> Value {
    let mut out = serde_json::Map::new();
    if first {
        out.insert("padding".into(), json!(padding));
    }
    for (key, value) in record.as_object().unwrap() {
        out.insert(key.clone(), value.clone());
    }
    if !first {
        out.insert("padding".into(), json!(padding));
    }
    Value::Object(out)
}

#[test]
fn large_claude_record_keeps_fast_cost_and_dedupe_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let result = scan(dir.path(), &[claude_record("m1", 99, &content())], UsageProviderKind::Claude, "history");
    assert_eq!(
        result.records,
        [UsageRecord {
            provider: UsageProviderKind::Claude,
            timestamp_ms: 1_785_578_400_000.0,
            model: "claude-fable-5".into(),
            rate_model: None,
            session_id: "s1".into(),
            totals: Totals {
                uncached_input_tokens: 100.0,
                cached_input_tokens: 20.0,
                cache_creation_tokens: 5.0,
                output_tokens: 99.0,
                reasoning_tokens: 0.0
            },
            reported_cost_usd: Some(0.25),
            fast: true,
            dedupe_key: Some("m1:r-m1".into()),
        }]
    );
}

#[test]
fn large_irrelevant_fields_in_either_order_match_small_records() {
    let dir = tempfile::tempdir().unwrap();
    let padding = content();
    for provider in [UsageProviderKind::Claude, UsageProviderKind::Codex, UsageProviderKind::Grok] {
        let small: Vec<Value> = match provider {
            UsageProviderKind::Codex => codex_records(),
            UsageProviderKind::Grok => vec![grok_record()],
            _ => vec![claude_record("m1", 99, "")],
        };
        let expected = scan(dir.path(), &small, provider, "small");
        for first in [true, false] {
            let large: Vec<Value> = small.iter().map(|record| padded(record, first, &padding)).collect();
            let actual = scan(dir.path(), &large, provider, "history");
            assert_eq!(actual.records, expected.records);
            assert_eq!(actual.position.codex_state, expected.position.codex_state);
        }
    }
}

#[test]
fn large_grok_record_keeps_allocation_timestamp_and_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let result = scan(dir.path(), &[padded(&grok_record(), true, &content())], UsageProviderKind::Grok, "history");
    let record = &result.records[0];
    assert_eq!(record.timestamp_ms, 1_785_578_400_123.0);
    assert_eq!(&*record.model, "grok-4.5-build");
    assert_eq!(
        record.totals,
        Totals {
            uncached_input_tokens: 75.0,
            cached_input_tokens: 20.0,
            cache_creation_tokens: 5.0,
            output_tokens: 99.0,
            reasoning_tokens: 10.0
        }
    );
    assert_eq!(record.dedupe_key.as_deref(), Some("s1:p1:grok-4.5-build"));
    assert_eq!(record.reported_cost_usd, Some(0.25));
}

#[test]
fn usage_looking_text_inside_tool_output_does_not_count() {
    let dir = tempfile::tempdir().unwrap();
    let padding = content();
    let result = scan(
        dir.path(),
        &[
            json!({"type": "user", "padding": padding, "toolOutput": claude_record("m1", 99, "")}),
            json!({"padding": padding, "message": {"content": claude_record("m1", 99, "").to_string()}}),
            json!({"type": "assistant", "timestamp": TIMESTAMP, "message": {"model": "claude-fable-5", "content": [claude_record("m1", 99, "")]}}),
        ],
        UsageProviderKind::Claude,
        "history",
    );
    assert!(result.records.is_empty());
}

#[test]
fn partial_utf8_and_json_tails_replay_with_exact_crlf_offsets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tail.jsonl");
    let first = format!("{}\r\n", claude_record("first", 5, &content()));
    let second = claude_record("second", 7, &content()).to_string().into_bytes();
    let marker = "工具".as_bytes();
    let split = second.windows(marker.len()).rposition(|window| window == marker).unwrap() + 1;
    std::fs::write(&path, [first.as_bytes(), &second[..split]].concat()).unwrap();
    let partial = read(&path, UsageProviderKind::Claude, None);
    assert_eq!(outputs(&partial.records), [5.0]);
    assert!(partial.tail_records.is_empty());
    assert_eq!(partial.position.resume_offset, first.len() as u64);
    append(&path, &second[split..]);
    let complete = read(&path, UsageProviderKind::Claude, Some(&partial.position));
    assert!(complete.resumed);
    assert!(complete.records.is_empty());
    assert_eq!(outputs(&complete.tail_records), [7.0]);
    assert_eq!(complete.position.resume_offset, first.len() as u64);
    append(&path, format!("\r\n{}\n", claude_record("third", 11, &content())).as_bytes());
    let appended = read(&path, UsageProviderKind::Claude, Some(&complete.position));
    assert_eq!(outputs(&appended.records), [7.0, 11.0]);
    assert!(appended.tail_records.is_empty());
    assert_eq!(appended.position.resume_offset, std::fs::metadata(&path).unwrap().len());
    let full = read(&path, UsageProviderKind::Claude, None);
    assert_eq!([partial.records, appended.records].concat(), full.records);
}

#[test]
fn codex_model_switches_and_duplicates_survive_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let padding = content();
    let first = scan(
        dir.path(),
        &codex_records().iter().map(|record| padded(record, true, &padding)).collect::<Vec<_>>(),
        UsageProviderKind::Codex,
        "history",
    );
    let path = dir.path().join("history.jsonl");
    let appended = [
        padded(&codex_records()[2], true, &padding),
        json!({"type": "turn_context", "payload": {"padding": padding, "model": "gpt-6"}}),
        json!({"type": "event_msg", "timestamp": TIMESTAMP, "payload": {"type": "token_count", "padding": padding, "info": {"last_token_usage": {"input_tokens": 200, "output_tokens": 101}}}}),
    ];
    append(&path, appended.iter().map(|line| format!("{line}\n")).collect::<String>().as_bytes());
    let result = read(&path, UsageProviderKind::Codex, Some(&first.position));
    assert!(result.resumed);
    assert_eq!(result.records.len(), 1);
    assert_eq!(&*result.records[0].model, "gpt-6");
    assert_eq!(&*result.records[0].session_id, "s1");
    assert_eq!(result.records[0].totals.output_tokens, 101.0);
    let full = read(&path, UsageProviderKind::Codex, None);
    assert_eq!([first.records, result.records].concat(), full.records);
}

#[test]
fn codex_fork_copy_suppression_from_either_marker() {
    let dir = tempfile::tempdir().unwrap();
    for fork in [
        json!({"forked_from_id": "parent"}),
        json!({"source": {"subagent": {"thread_spawn": {"parent_thread_id": "parent"}}}}),
    ] {
        let mut payload = json!({"padding": content(), "id": "child"});
        for (key, value) in fork.as_object().unwrap() {
            payload[key] = value.clone();
        }
        let records = codex_records();
        let lines = [
            json!({"type": "session_meta", "timestamp": TIMESTAMP, "payload": payload}),
            records[1].clone(),
            records[2].clone(),
            json!({"type": "event_msg", "timestamp": "2026-08-01T10:00:05Z", "payload": {"type": "token_count", "info": {"last_token_usage": {"input_tokens": 100, "output_tokens": 101}}}}),
        ];
        let result = scan(dir.path(), &lines, UsageProviderKind::Codex, "history");
        assert_eq!(result.records.len(), 1);
        assert_eq!(&*result.records[0].session_id, "child");
        assert_eq!(result.records[0].totals.output_tokens, 101.0);
    }
}

#[test]
fn malformed_large_lines_are_rejected_without_losing_later_lines() {
    let dir = tempfile::tempdir().unwrap();
    let valid = claude_record("m1", 99, &content()).to_string();
    let path = dir.path().join("broken.jsonl");
    let lines = [
        format!("{valid} junk"),
        valid[..valid.len() - 1].to_owned(),
        format!("{valid}{valid}"),
        claude_record("good", 7, "").to_string(),
    ];
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    assert_eq!(outputs(&read(&path, UsageProviderKind::Claude, None).records), [7.0]);
}

#[test]
fn odd_keys_match_json_parse() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("odd.jsonl");
    let mut small = claude_record("m1", 99, "");
    small["message"]["content"] = json!([]);
    let small = small.to_string();
    let variants = [
        small.replace("\"message\":", "\"mess\\u0061ge\":"),
        small.replace("\"costUSD\":0.25", "\"costUSD\":5,\"costUSD\":0.25"),
        small.replace("\"type\":\"assistant\"", "\"type\":\"user\",\"type\":\"assistant\""),
        small.replace("\"message\":", "\"__proto__\":{\"type\":\"user\"},\"message\":"),
        small.replace("\"message\":", "\"message.usage\":{\"output_tokens\":1234},\"message\":"),
    ];
    let padding = json!(content()).to_string();
    for line in variants {
        std::fs::write(&path, format!("{line}\n")).unwrap();
        let expected = read(&path, UsageProviderKind::Claude, None);
        assert_eq!(expected.records.len(), 1, "{line}");
        std::fs::write(&path, format!("{{\"padding\":{padding},{}\n", &line[1..])).unwrap();
        assert_eq!(read(&path, UsageProviderKind::Claude, None).records, expected.records);
    }
}

#[test]
fn deeply_nested_discarded_content() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("deep.jsonl");
    let record = claude_record("m1", 99, &content()).to_string();
    let nested = format!("{}0{}", "[".repeat(300), "]".repeat(300));
    std::fs::write(&path, format!("{{\"toolOutput\":{nested},{}\n", &record[1..])).unwrap();
    assert_eq!(read(&path, UsageProviderKind::Claude, None).records[0].totals.output_tokens, 99.0);
}

#[test]
fn non_finite_numbers_are_not_costs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("numbers.jsonl");
    let record = claude_record("m1", 99, &content()).to_string().replace("\"costUSD\":0.25", "\"costUSD\":1e400");
    std::fs::write(&path, format!("{record}\n")).unwrap();
    let records = read(&path, UsageProviderKind::Claude, None).records;
    assert_eq!(records[0].reported_cost_usd, None);
    assert_eq!(records[0].totals.output_tokens, 99.0);
}

/* ---------------------------------------------------------------------------------------- */
/* SQLite readers                                                                           */
/* ---------------------------------------------------------------------------------------- */

#[test]
fn opencode_counts_migrated_messages_once_and_sees_wal_writes() {
    let dir = tempfile::tempdir().unwrap();
    let db = rusqlite::Connection::open(dir.path().join("opencode.db")).unwrap();
    db.execute_batch("PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0; CREATE TABLE message (id TEXT, session_id TEXT, data TEXT)")
        .unwrap();
    let message = json!({
        "id": "msg-1", "sessionID": "session-1", "role": "assistant", "modelID": "claude-sonnet-4-5", "time": {"created": 1_780_000_000_000u64},
        "cost": 0.25, "tokens": {"input": 100, "output": 20, "reasoning": 5, "cache": {"read": 30, "write": 10}},
    });
    db.execute(
        "INSERT INTO message VALUES (?1, ?2, ?3)",
        rusqlite::params!["msg-1", "session-1", message.to_string()],
    )
    .unwrap();
    let legacy = dir.path().join("storage").join("message").join("session-1");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("msg-1.json"), message.to_string()).unwrap();
    let first = crate::opencode::read_opencode_usage(dir.path(), 0.0);
    assert!(!first.error);
    let records: Vec<UsageRecord> = first.files.into_iter().flat_map(|file| file.records).collect();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].totals,
        Totals {
            uncached_input_tokens: 100.0,
            cached_input_tokens: 30.0,
            cache_creation_tokens: 10.0,
            output_tokens: 25.0,
            reasoning_tokens: 5.0
        }
    );
    assert_eq!(records[0].reported_cost_usd, Some(0.25));
    let mut second = message.clone();
    second["id"] = json!("msg-2");
    second["time"] = json!({"created": 1_780_000_001_000u64});
    db.execute(
        "INSERT INTO message VALUES (?1, ?2, ?3)",
        rusqlite::params!["msg-2", "session-1", second.to_string()],
    )
    .unwrap();
    let next = crate::opencode::read_opencode_usage(dir.path(), 1_780_000_001_000.0);
    assert!(!next.error);
    let keys: Vec<Option<String>> = next.files.into_iter().flat_map(|file| file.records).map(|record| record.dedupe_key).collect();
    assert_eq!(keys, [Some("opencode:msg-2".to_owned())]);
    assert!(std::fs::metadata(dir.path().join("opencode.db-wal")).unwrap().len() > 0);
}

fn proto_varint(mut value: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let byte = (value % 128) as u8;
        value /= 128;
        bytes.push(byte + if value > 0 { 128 } else { 0 });
        if value == 0 {
            return bytes;
        }
    }
}

fn proto_number(field: u64, value: u64) -> Vec<u8> {
    [proto_varint(field * 8), proto_varint(value)].concat()
}

fn proto_bytes(field: u64, payload: &[u8]) -> Vec<u8> {
    [proto_varint(field * 8 + 2), proto_varint(payload.len() as u64), payload.to_vec()].concat()
}

fn proto_text(field: u64, value: &str) -> Vec<u8> {
    proto_bytes(field, value.as_bytes())
}

fn antigravity_records(roots: &[std::path::PathBuf], since: f64) -> (Vec<UsageRecord>, crate::antigravity::AntigravityUsage) {
    let result = crate::antigravity::read_antigravity_usage(roots, since);
    (result.files.iter().flat_map(|file| file.records.clone()).collect(), result)
}

#[test]
fn antigravity_dedupes_generation_and_step_usage_keeping_retry_model() {
    let dir = tempfile::tempdir().unwrap();
    let db = rusqlite::Connection::open(dir.path().join("session-1.db")).unwrap();
    let stamp = proto_number(1, 1_780_000_000);
    let usage = [
        proto_number(2, 100),
        proto_number(3, 40),
        proto_number(4, 5),
        proto_number(5, 20),
        proto_number(9, 10),
        proto_text(11, "response-1"),
    ]
    .concat();
    let retry = [proto_number(1, 1026), proto_number(2, 12), proto_number(3, 3), proto_text(11, "retry-1")].concat();
    let generation = proto_bytes(
        1,
        &[proto_bytes(4, &usage), proto_text(19, "Gemini 3 Pro"), proto_bytes(9, &proto_bytes(4, &stamp))].concat(),
    );
    let step = [proto_bytes(9, &usage), proto_bytes(8, &stamp), proto_bytes(28, &proto_bytes(2, &retry))].concat();
    db.execute_batch("CREATE TABLE gen_metadata (idx INTEGER, data BLOB); CREATE TABLE steps (idx INTEGER, metadata BLOB)")
        .unwrap();
    db.execute("INSERT INTO gen_metadata VALUES (0, ?1)", [generation]).unwrap();
    db.execute("INSERT INTO steps VALUES (0, ?1)", [step]).unwrap();
    drop(db);
    let roots = [dir.path().to_path_buf()];
    let (records, result) = antigravity_records(&roots, 0.0);
    assert!(result.errors.is_empty());
    assert_eq!(records.len(), 2);
    let main = records.iter().find(|record| &*record.model == "gemini-3-pro").unwrap();
    assert_eq!(main.timestamp_ms, 1_780_000_000_000.0);
    assert_eq!(&*main.session_id, "session-1");
    assert_eq!(
        main.totals,
        Totals {
            uncached_input_tokens: 100.0,
            cached_input_tokens: 20.0,
            cache_creation_tokens: 5.0,
            output_tokens: 40.0,
            reasoning_tokens: 10.0
        }
    );
    assert_eq!(
        records
            .iter()
            .find(|record| &*record.model == "claude-opus-4-6")
            .unwrap()
            .totals
            .uncached_input_tokens,
        12.0
    );
    assert!(antigravity_records(&roots, 1_780_000_000_001.0).0.is_empty());
}

#[test]
fn antigravity_model_less_steps_use_their_generation_model() {
    let dir = tempfile::tempdir().unwrap();
    let db = rusqlite::Connection::open(dir.path().join("model-switch.db")).unwrap();
    db.execute_batch("CREATE TABLE gen_metadata (idx INTEGER, data BLOB); CREATE TABLE steps (idx INTEGER, metadata BLOB)")
        .unwrap();
    for (idx, name) in ["Gemini 3 Pro", "Claude Opus 4.6"].iter().enumerate() {
        db.execute(
            "INSERT INTO gen_metadata VALUES (?1, ?2)",
            rusqlite::params![idx as i64, proto_bytes(1, &proto_text(19, name))],
        )
        .unwrap();
        db.execute(
            "INSERT INTO steps VALUES (?1, ?2)",
            rusqlite::params![idx as i64, proto_bytes(9, &proto_number(2, 10 + idx as u64))],
        )
        .unwrap();
    }
    drop(db);
    let (records, result) = antigravity_records(&[dir.path().to_path_buf()], 0.0);
    assert!(result.errors.is_empty());
    let models: Vec<&str> = records.iter().map(|record| &*record.model).collect();
    assert_eq!(models, ["gemini-3-pro", "claude-opus-4-6"]);
}

#[test]
fn antigravity_aliases_bridge_separate_steps() {
    let dir = tempfile::tempdir().unwrap();
    let db = rusqlite::Connection::open(dir.path().join("bridge.db")).unwrap();
    db.execute_batch("CREATE TABLE gen_metadata (idx INTEGER, data BLOB); CREATE TABLE steps (idx INTEGER, metadata BLOB)")
        .unwrap();
    db.execute(
        "INSERT INTO steps VALUES (0, ?1)",
        [proto_bytes(9, &[proto_number(2, 100), proto_text(11, "response")].concat())],
    )
    .unwrap();
    db.execute(
        "INSERT INTO steps VALUES (1, ?1)",
        [proto_bytes(9, &[proto_number(3, 40), proto_text(12, "provider")].concat())],
    )
    .unwrap();
    let generation = proto_bytes(
        1,
        &[
            proto_text(19, "Gemini 3 Pro"),
            proto_bytes(
                4,
                &[proto_number(2, 50), proto_number(5, 20), proto_text(11, "response"), proto_text(12, "provider")].concat(),
            ),
        ]
        .concat(),
    );
    db.execute("INSERT INTO gen_metadata VALUES (0, ?1)", [generation]).unwrap();
    drop(db);
    let (records, result) = antigravity_records(&[dir.path().to_path_buf()], 0.0);
    assert!(result.errors.is_empty());
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].totals,
        Totals {
            uncached_input_tokens: 100.0,
            cached_input_tokens: 20.0,
            cache_creation_tokens: 0.0,
            output_tokens: 40.0,
            reasoning_tokens: 0.0
        }
    );
}

#[test]
fn antigravity_aliases_merge_across_roots_keeping_ownership() {
    let dir = tempfile::tempdir().unwrap();
    let roots = [dir.path().join("first"), dir.path().join("second")];
    for (index, root) in roots.iter().enumerate() {
        std::fs::create_dir(root).unwrap();
        let db = rusqlite::Connection::open(root.join(format!("session-{index}.db"))).unwrap();
        db.execute_batch("CREATE TABLE steps (idx INTEGER, metadata BLOB)").unwrap();
        for identity in [7u64, 12] {
            let usage = [
                proto_number(1, 246),
                proto_number(2, if index == 0 { 100 } else { 150 }),
                proto_text(11, &format!("response-{index}-{identity}")),
                proto_text(identity, &format!("shared-{identity}")),
            ]
            .concat();
            db.execute("INSERT INTO steps VALUES (?1, ?2)", rusqlite::params![identity as i64, proto_bytes(9, &usage)])
                .unwrap();
        }
    }
    let result = crate::antigravity::read_antigravity_usage(&roots, 0.0);
    assert!(result.errors.is_empty());
    assert_eq!(result.files.len(), 2);
    assert_eq!(result.files[0].root, roots[0]);
    assert_eq!(result.files[0].records.len(), 2);
    assert!(result.files[1].records.is_empty());
    assert_eq!(
        result.files[0]
            .records
            .iter()
            .map(|record| record.totals.uncached_input_tokens)
            .collect::<Vec<_>>(),
        [150.0, 150.0]
    );
    assert!(result.files[0].records.iter().all(|record| &*record.session_id == "session-0"));
}

#[test]
fn antigravity_fallback_timestamps_upgrade_before_the_window() {
    let dir = tempfile::tempdir().unwrap();
    for fallback in ["mtime", "trajectory"] {
        let path = dir.path().join(format!("{fallback}.db"));
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE gen_metadata (idx INTEGER, data BLOB); CREATE TABLE steps (idx INTEGER, metadata BLOB)")
            .unwrap();
        if fallback == "trajectory" {
            db.execute_batch("CREATE TABLE trajectory_metadata_blob (data BLOB)").unwrap();
            db.execute(
                "INSERT INTO trajectory_metadata_blob VALUES (?1)",
                [proto_bytes(2, &proto_number(1, 1_780_000_200))],
            )
            .unwrap();
        }
        for (index, seconds) in [1_780_000_000u64, 1_780_000_200].iter().enumerate() {
            let usage = [proto_number(2, 10), proto_text(11, &format!("{fallback}-{index}"))].concat();
            db.execute("INSERT INTO steps VALUES (?1, ?2)", rusqlite::params![index as i64, proto_bytes(9, &usage)])
                .unwrap();
            let generation = proto_bytes(
                1,
                &[proto_bytes(4, &usage), proto_bytes(9, &proto_bytes(4, &proto_number(1, *seconds)))].concat(),
            );
            db.execute("INSERT INTO gen_metadata VALUES (?1, ?2)", rusqlite::params![index as i64, generation])
                .unwrap();
        }
        drop(db);
        let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_780_000_000);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(mtime).unwrap();
    }
    let (records, result) = antigravity_records(&[dir.path().to_path_buf()], 1_780_000_100_000.0);
    assert!(result.errors.is_empty());
    assert_eq!(
        records.iter().map(|record| record.timestamp_ms).collect::<Vec<_>>(),
        [1_780_000_200_000.0, 1_780_000_200_000.0]
    );
}

#[test]
fn antigravity_step_only_stores_and_malformed_databases() {
    let dir = tempfile::tempdir().unwrap();
    let db = rusqlite::Connection::open(dir.path().join("steps.db")).unwrap();
    db.execute_batch("CREATE TABLE steps (idx INTEGER, metadata BLOB)").unwrap();
    let usage = [proto_number(1, 246), proto_number(2, 10), proto_number(3, 5)].concat();
    db.execute(
        "INSERT INTO steps VALUES (0, ?1)",
        [[proto_bytes(9, &usage), proto_bytes(8, &proto_number(1, 1_780_000_000))].concat()],
    )
    .unwrap();
    drop(db);
    std::fs::write(dir.path().join("broken.db"), "not a sqlite database").unwrap();
    let (records, result) = antigravity_records(&[dir.path().to_path_buf()], 0.0);
    assert_eq!(result.errors.len(), 1);
    assert_eq!(&*records[0].model, "gemini-2.5-pro");
    assert_eq!(records[0].totals.output_tokens, 5.0);
}

#[test]
fn antigravity_ignores_large_values_in_unused_fields() {
    let dir = tempfile::tempdir().unwrap();
    let db = rusqlite::Connection::open(dir.path().join("large-varint.db")).unwrap();
    db.execute_batch("CREATE TABLE steps (idx INTEGER, metadata BLOB)").unwrap();
    let mut unused = proto_number(99, 0);
    unused.pop();
    unused.extend(std::iter::repeat_n(0xff, 9));
    unused.push(0x01);
    let usage = [proto_number(1, 246), proto_number(2, 10), unused].concat();
    db.execute("INSERT INTO steps VALUES (0, ?1)", [proto_bytes(9, &usage)]).unwrap();
    drop(db);
    let (records, result) = antigravity_records(&[dir.path().to_path_buf()], 0.0);
    assert!(result.errors.is_empty());
    assert_eq!(records[0].totals.uncached_input_tokens, 10.0);
}
