//! Ports of `Layers/EventNdjsonLogger.test.ts`, plus the parity gate: the same record sequence
//! written by the TS logger (run from source with Node) and by this one must give byte-identical
//! files.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use zc_providers::logger::{
    write_batched_messages, EventNdjsonLogStore, EventNdjsonLogStoreOptions, EventNdjsonStream, LogAttribution, PendingRecord, RotatingFileSink,
};

const CLOCK_MS: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00.000Z

fn options(batch_window_ms: u64) -> EventNdjsonLogStoreOptions {
    EventNdjsonLogStoreOptions {
        batch_window_ms,
        clock: Arc::new(|| CLOCK_MS),
        ..Default::default()
    }
}

fn read_lines(path: &Path) -> Vec<(String, String, String)> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| {
            let rest = line.strip_prefix('[').unwrap();
            let (stamp, rest) = rest.split_once("] ").unwrap();
            let (stream, payload) = rest.split_once(": ").unwrap();
            (stamp.to_owned(), stream.to_owned(), payload.to_owned())
        })
        .collect()
}

#[tokio::test]
async fn writes_lines_to_thread_scoped_files() {
    let dir = tempfile::tempdir().unwrap();
    let store = EventNdjsonLogStore::open(dir.path().join("provider-native.ndjson"), options(1_000)).unwrap();
    let native = store.logger(EventNdjsonStream::Native);
    native.write(&json!({"threadId": "provider-thread-1", "id": "evt-1"}), Some("thread-1"));
    native.write(
        &json!({"type": "turn.completed", "threadId": "provider-thread-2", "id": "evt-2"}),
        Some("thread-2"),
    );
    store.close();
    let first = read_lines(&dir.path().join("provider-native.thread-1.log"));
    assert_eq!(
        first,
        vec![(
            "2026-01-01T00:00:00.000Z".into(),
            "NTIVE".into(),
            r#"{"threadId":"provider-thread-1","id":"evt-1"}"#.into()
        )]
    );
    let second = read_lines(&dir.path().join("provider-native.thread-2.log"));
    assert_eq!(second[0].2, r#"{"type":"turn.completed","threadId":"provider-thread-2","id":"evt-2"}"#);
}

#[tokio::test]
async fn shares_one_thread_writer_across_streams_and_flushes_on_its_batch_timer() {
    let dir = tempfile::tempdir().unwrap();
    let store = EventNdjsonLogStore::open(dir.path().join("events.log"), options(20)).unwrap();
    store.logger(EventNdjsonStream::Native).write(&json!({"id": "native"}), Some("shared"));
    store.logger(EventNdjsonStream::Canonical).write(&json!({"id": "canonical"}), Some("shared"));
    let path = dir.path().join("events.shared.log");
    assert!(!path.exists(), "batched until the window closes");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let streams: Vec<String> = read_lines(&path).into_iter().map(|(_, stream, _)| stream).collect();
    assert_eq!(streams, vec!["NTIVE", "CANON"]);
    // A later batch is not stranded.
    store.logger(EventNdjsonStream::Native).write(&json!({"id": "later"}), Some("shared"));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(read_lines(&path).len(), 3);
    store.close();
    store.logger(EventNdjsonStream::Native).write(&json!({"id": "after-close"}), Some("shared"));
    assert_eq!(read_lines(&path).len(), 3);
}

#[tokio::test]
async fn drops_transient_provider_events() {
    let dir = tempfile::tempdir().unwrap();
    let store = EventNdjsonLogStore::open(dir.path().join("events.log"), options(0)).unwrap();
    let canonical = store.logger(EventNdjsonStream::Canonical);
    let native = store.logger(EventNdjsonStream::Native);
    let thread = Some("thread-filtered");
    canonical.write(&json!({"type": "content.delta"}), thread);
    canonical.write(&json!({"type": "item.completed", "id": "final"}), thread);
    native.write(&json!({"type": "content.delta", "id": "native-delta"}), thread);
    for method in [
        "item/agentMessage/delta",
        "thread/realtime/outputAudio/delta",
        "thread/realtime/transcript/delta",
        "turn/diff/updated",
    ] {
        native.write(&json!({"method": method, "payload": {}}), thread);
    }
    native.write(
        &json!({"event": {"direction": "incoming", "stage": "decoded", "payload": {"method": "turn/diff/updated", "params": {}}}}),
        thread,
    );
    native.write(
        &json!({"event": {"method": "claude/stream_event/content_block_delta/text_delta", "payload": {}}}),
        thread,
    );
    native.write(
        &json!({"event": {"method": "session/update", "payload": {"update": {"sessionUpdate": "agent_message_chunk"}}}}),
        thread,
    );
    native.write(
        &json!({"event": {"direction": "incoming", "stage": "decoded", "payload": {"method": "item/commandExecution/outputDelta", "params": {}}}}),
        thread,
    );
    native.write(
        &json!({"event": {"direction": "incoming", "stage": "decoded", "payload": {"type": "stream_event", "event": {"type": "content_block_delta"}}}}),
        thread,
    );
    native.write(&json!({"event": {"direction": "incoming", "stage": "raw", "payload": "{}"}}), thread);
    native.write(
        &json!({"event": {"type": "message.part.updated", "payload": {"properties": {"part": {"type": "text"}}}}}),
        thread,
    );
    native.write(&json!({"event": {"type": "message.part.updated", "payload": {"properties": {"part": {"type": "tool", "state": {"status": "running", "output": "progress"}}}}}}), thread);
    native.write(&json!({"type": "turn.completed", "id": "native-final"}), thread);
    store.close();
    let lines: Vec<(String, String)> = read_lines(&dir.path().join("events.thread-filtered.log"))
        .into_iter()
        .map(|(_, stream, payload)| (stream, payload))
        .collect();
    assert_eq!(
        lines,
        vec![
            ("CANON".into(), r#"{"type":"item.completed","id":"final"}"#.into()),
            ("NTIVE".into(), r#"{"type":"turn.completed","id":"native-final"}"#.into())
        ]
    );
}

#[tokio::test]
async fn bounds_large_records_but_keeps_routing_and_error_fields() {
    let dir = tempfile::tempdir().unwrap();
    let store = EventNdjsonLogStore::open(dir.path().join("events.log"), options(1_000)).unwrap();
    let native = store.logger(EventNdjsonStream::Native);
    let turns: Vec<Value> = (0..10_000).map(|_| Value::Null).collect();
    native.write(&json!({"provider": "codex", "event": {"direction": "incoming", "stage": "decoded", "payload": {"id": 42, "result": {"thread": {"id": "native-thread", "turns": turns}}}}}), Some("large-history"));
    native.write(
        &json!({"method": "error", "params": {"threadId": "native-thread", "turnId": "native-turn", "error": {"message": "The provider is unavailable.", "code": "overloaded"}, "output": "x".repeat(128 * 1024)}}),
        Some("large-error"),
    );
    native.write(&json!({"id": "escaped", "output": "\u{0}".repeat(20_000)}), Some("large-error"));
    let diff = "diff-payload".repeat(128 * 1024);
    store.logger(EventNdjsonStream::Canonical).write(
        &json!({"type": "turn.diff.updated", "threadId": "large-diff", "turnId": "native-turn", "raw": {"method": "turn/diff/updated", "payload": {"diff": diff}}, "payload": {"unifiedDiff": diff}}),
        Some("large-diff"),
    );
    store.close();

    let history = std::fs::read_to_string(dir.path().join("events.large-history.log")).unwrap();
    assert!(history.len() < 2_048);
    let record: Value = serde_json::from_str(&read_lines(&dir.path().join("events.large-history.log"))[0].2).unwrap();
    assert_eq!(record["event"]["payload"]["id"], json!(42));
    assert_eq!(record["event"]["payload"]["result"]["thread"]["id"], json!("native-thread"));
    assert_eq!(record["event"]["payload"]["result"]["thread"]["turns"]["itemCount"], json!(10_000));

    let errors = read_lines(&dir.path().join("events.large-error.log"));
    assert!(std::fs::read_to_string(dir.path().join("events.large-error.log")).unwrap().len() < 64 * 1024);
    let failure: Value = serde_json::from_str(&errors[0].2).unwrap();
    assert_eq!(failure["params"]["error"]["message"], json!("The provider is unavailable."));
    assert_eq!(failure["params"]["error"]["code"], json!("overloaded"));
    assert_eq!(failure["params"]["turnId"], json!("native-turn"));
    let escaped: Value = serde_json::from_str(&errors[1].2).unwrap();
    assert_eq!(escaped["id"], json!("escaped"));

    let diff_record: Value = serde_json::from_str(&read_lines(&dir.path().join("events.large-diff.log"))[0].2).unwrap();
    assert_eq!(diff_record["type"], json!("turn.diff.updated"));
    assert_eq!(diff_record["turnId"], json!("native-turn"));
}

#[tokio::test]
async fn rotates_per_thread_files() {
    let dir = tempfile::tempdir().unwrap();
    let store = EventNdjsonLogStore::open(
        dir.path().join("provider-native.ndjson"),
        EventNdjsonLogStoreOptions {
            max_bytes: 120,
            max_files: 2,
            ..options(0)
        },
    )
    .unwrap();
    for index in 0..10 {
        let stream = if index % 2 == 0 {
            EventNdjsonStream::Native
        } else {
            EventNdjsonStream::Canonical
        };
        store.logger(stream).write(
            &json!({"type": "session.started", "threadId": "provider-thread-rotate", "id": format!("evt-{index}"), "payload": "x".repeat(40)}),
            Some("thread-rotate"),
        );
    }
    store.close();
    let names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"provider-native.thread-rotate.log.1".to_owned()));
    assert!(names.contains(&"provider-native.thread-rotate.log".to_owned()) || names.contains(&"provider-native.thread-rotate.log.2".to_owned()));
    assert!(!names.contains(&"provider-native.thread-rotate.log.3".to_owned()));
}

fn set_mtime(path: &Path, millis: i64) {
    let time = std::time::UNIX_EPOCH + std::time::Duration::from_millis(millis as u64);
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_modified(time).unwrap();
}

#[tokio::test]
async fn enforces_age_and_size_retention_on_startup_and_spares_active_sinks() {
    let dir = tempfile::tempdir().unwrap();
    let now = 1_800_000_000_000;
    let path = |name: &str| dir.path().join(name);
    for name in ["events.expired.log", "events.old.log", "events.new.log", "unrelated.log", "ignored.txt"] {
        std::fs::write(path(name), "x".repeat(40)).unwrap();
    }
    std::fs::write(path("legacy-thread.log"), "[2026-01-01T00:00:00.000Z] CANON: legacy provider event\n").unwrap();
    set_mtime(&path("events.expired.log"), now - 20_000);
    set_mtime(&path("legacy-thread.log"), now - 20_000);
    set_mtime(&path("events.old.log"), now - 5_000);
    set_mtime(&path("events.new.log"), now);
    set_mtime(&path("unrelated.log"), now);
    let store = EventNdjsonLogStore::open(
        path("events.log"),
        EventNdjsonLogStoreOptions {
            max_age_ms: 10_000,
            max_total_bytes: 60,
            clock: Arc::new(move || now),
            ..Default::default()
        },
    )
    .unwrap();
    store.close();
    assert!(!path("events.expired.log").exists());
    assert!(!path("legacy-thread.log").exists());
    assert!(!path("events.old.log").exists());
    assert!(path("events.new.log").exists());
    assert!(path("unrelated.log").exists());
    assert!(path("ignored.txt").exists());

    // An active thread sink survives a retention pass triggered by another thread's flush.
    let clock = Arc::new(Mutex::new(now));
    let store = EventNdjsonLogStore::open(
        path("events.log"),
        EventNdjsonLogStoreOptions {
            batch_window_ms: 0,
            max_age_ms: 1,
            retention_check_interval_ms: 1,
            clock: {
                let clock = clock.clone();
                Arc::new(move || *clock.lock().unwrap())
            },
            ..Default::default()
        },
    )
    .unwrap();
    let native = store.logger(EventNdjsonStream::Native);
    native.write(&json!({"id": "active-before-retention"}), Some("active"));
    assert!(path("events.active.log").exists());
    *clock.lock().unwrap() += 2;
    native.write(&json!({"id": "retention-trigger"}), Some("other"));
    assert!(path("events.active.log").exists());
    store.close();
}

#[test]
fn batched_writes_report_what_was_written_before_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    let mut sink = RotatingFileSink::new(dir.path().join("x.log"), 5, 1).unwrap();
    let records = vec![
        PendingRecord {
            stream: EventNdjsonStream::Native,
            thread_segment: "thread".into(),
            line: "first".into(),
            bytes: 5,
        },
        PendingRecord {
            stream: EventNdjsonStream::Canonical,
            thread_segment: "thread".into(),
            line: "second".into(),
            bytes: 6,
        },
    ];
    let mut written = Vec::new();
    write_batched_messages(&mut sink, &records, 5, |batch| written.extend(batch.iter().map(|record| record.line.clone()))).unwrap();
    assert_eq!(written, vec!["first", "second"]);
    // Rotation kept one backup.
    assert_eq!(std::fs::read_to_string(dir.path().join("x.log")).unwrap(), "second");
    assert_eq!(std::fs::read_to_string(dir.path().join("x.log.1")).unwrap(), "first");
}

struct Recorder(Mutex<Vec<(String, String, u64, u64)>>);

impl LogAttribution for Recorder {
    fn record(&self, component: &str, operation: &str, logical_write_bytes: u64, count: u64, _duration_ms: i64) {
        self.0.lock().unwrap().push((component.into(), operation.into(), logical_write_bytes, count));
    }
}

#[tokio::test]
async fn reports_logical_writes_to_resource_attribution() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder(Mutex::new(Vec::new())));
    let store = EventNdjsonLogStore::open(
        dir.path().join("provider-native.ndjson"),
        EventNdjsonLogStoreOptions {
            attribution: Some(recorder.clone()),
            ..options(0)
        },
    )
    .unwrap();
    store
        .logger(EventNdjsonStream::Native)
        .write(&json!({"id": "attributed-event"}), Some("thread-attribution"));
    store.close();
    let entries = recorder.0.lock().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].0, "provider-event-log");
    assert_eq!(entries[0].1, "native.append");
    assert_eq!(entries[0].3, 1);
    assert!(entries[0].2 > 0);
}

#[test]
fn rejects_invalid_options() {
    let dir = tempfile::tempdir().unwrap();
    let error = EventNdjsonLogStore::open(
        dir.path().join("events.log"),
        EventNdjsonLogStoreOptions {
            max_files: 0,
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(error
        .to_string()
        .starts_with("Provider event log option 'maxFiles' must be an integer >= 1; received 0 for '"));
}

// ---------------------------------------------------------------------------------------------
// Parity with the TypeScript logger
// ---------------------------------------------------------------------------------------------

fn code_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../code").canonicalize().unwrap()
}

/// A record sequence exercising every shaping rule: transient drops, bounding, summaries,
/// escaping, JS number printing and key order, segment names, rotation and batching.
fn parity_records() -> Vec<Value> {
    let mut records = Vec::new();
    let mut push = |stream: &str, thread_id: Value, event: Value| records.push(json!({"stream": stream, "threadId": thread_id, "event": event}));
    let canonical = |kind: &str, id: usize, extra: Value| {
        let mut event = json!({"eventId": format!("evt-{id}"), "provider": "codex", "providerInstanceId": "codex", "threadId": "thread-1", "createdAt": "2026-01-01T00:00:00.000Z", "type": kind});
        for (key, value) in extra.as_object().unwrap() {
            event[key] = value.clone();
        }
        event
    };
    push(
        "canonical",
        json!("thread-1"),
        canonical("turn.started", 1, json!({"turnId": "turn-1", "payload": {"model": "gpt-6-astra"}})),
    );
    push(
        "canonical",
        json!("thread-1"),
        canonical("content.delta", 2, json!({"payload": {"streamKind": "assistant_text", "delta": "dropped"}})),
    );
    push(
        "canonical",
        json!("thread-1"),
        canonical(
            "item.completed",
            3,
            json!({"payload": {"itemType": "assistant_message", "detail": "Résumé — naïve café 😀 \u{1}\u{1f}\t\n\r\"quoted\" \\ back</script>\u{2028}"}}),
        ),
    );
    push(
        "canonical",
        json!("thread-1"),
        canonical(
            "turn.completed",
            4,
            json!({"turnId": "turn-1", "payload": {"state": "completed", "totalCostUsd": 0.012_345_678_9, "usage": {"big": 1e21, "tiny": 1.5e-7, "neg": -0.0, "frac": 123_456_789.125, "huge": 12_345_678_901_234_567_890_u64, "int": 42, "float_int": 3.0}, "modelUsage": {"b": 1, "2": 2, "10": 3, "a": 4, "01": 5}}}),
        ),
    );
    push(
        "canonical",
        json!("Thread Segment/1"),
        canonical(
            "request.opened",
            5,
            json!({"requestId": "req-1", "payload": {"requestType": "command_execution_approval", "detail": "ls -la"}}),
        ),
    );
    push(
        "canonical",
        json!(null),
        canonical("runtime.warning", 6, json!({"payload": {"message": "global"}})),
    );
    push("canonical", json!("pending"), canonical("session.started", 7, json!({"payload": {}})));
    push(
        "canonical",
        json!("---"),
        canonical("session.exited", 8, json!({"payload": {"exitKind": "graceful"}})),
    );
    // Native frames.
    push(
        "native",
        json!("thread-1"),
        json!({"provider": "codex", "kind": "notification", "event": {"direction": "incoming", "stage": "raw", "payload": "{}"}}),
    );
    push(
        "native",
        json!("thread-1"),
        json!({"provider": "codex", "event": {"direction": "incoming", "stage": "decoded", "payload": {"method": "item/agentMessage/delta", "params": {"delta": "x"}}}}),
    );
    push(
        "native",
        json!("thread-1"),
        json!({"provider": "codex", "event": {"direction": "incoming", "stage": "decoded", "payload": {"method": "turn/completed", "params": {"turn": {"id": "turn-1", "items": []}}}}}),
    );
    push(
        "native",
        json!("thread-1"),
        json!({"provider": "claudeAgent", "event": {"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"text": "x"}}}}),
    );
    push(
        "native",
        json!("thread-1"),
        json!({"provider": "claudeAgent", "event": {"type": "assistant", "message": {"content": [{"type": "text", "text": "hello"}]}}}),
    );
    push(
        "native",
        json!("thread-1"),
        json!({"provider": "cursor", "event": {"method": "session/update", "payload": {"update": {"sessionUpdate": "agent_thought_chunk"}}}}),
    );
    push(
        "native",
        json!("thread-1"),
        json!({"provider": "cursor", "event": {"method": "session/update", "payload": {"update": {"sessionUpdate": "tool_call", "title": "Read file"}}}}),
    );
    push(
        "native",
        json!("thread-1"),
        json!({"provider": "opencode", "event": {"type": "message.part.updated", "payload": {"properties": {"part": {"type": "tool", "state": {"status": "completed", "output": "done"}}}}}}),
    );
    // Bounding and summaries.
    push(
        "native",
        json!("thread-2"),
        json!({"provider": "codex", "event": {"direction": "incoming", "stage": "decoded", "payload": {"id": 7, "result": {"thread": {"id": "t", "turns": (0..2_000).collect::<Vec<_>>()}}}}}),
    );
    push(
        "native",
        json!("thread-2"),
        json!({"method": "error", "params": {"threadId": "t", "error": {"message": "overloaded", "code": 529}, "output": "y".repeat(70_000)}}),
    );
    push("native", json!("thread-2"), json!({"id": "escaped", "output": "\u{0}".repeat(20_000)}));
    let mut deep = json!("bottom");
    for _ in 0..20 {
        deep = json!({"payload": deep});
    }
    push("native", json!("thread-2"), json!({"type": "deep", "event": deep}));
    push(
        "canonical",
        json!("thread-2"),
        canonical("turn.diff.updated", 9, json!({"payload": {"unifiedDiff": "d".repeat(66_000)}})),
    );
    push(
        "native",
        json!("thread-2"),
        json!({"message": "m".repeat(2_000), "status": "s".repeat(500), "code": "c", "id": "😀".repeat(300), "payload": {"x": 1}}),
    );
    // Orchestration records are never filtered.
    push("orchestration", json!("thread-1"), json!({"type": "content.delta", "sequence": 12}));
    // Enough volume to rotate and to cross the buffered-record limit.
    for index in 0..40 {
        push(
            "canonical",
            json!("thread-rotate"),
            canonical("item.updated", 100 + index, json!({"payload": {"n": index}})),
        );
        push(
            "canonical",
            json!("thread-rotate"),
            canonical("item.completed", 200 + index, json!({"payload": {"n": index, "text": "z".repeat(index * 7)}})),
        );
    }
    records
}

fn run_rust(fixture: &Value, out_dir: &Path) {
    let options = fixture["options"].as_object().unwrap();
    let number = |key: &str| options.get(key).and_then(Value::as_u64);
    let clock_ms = fixture["clockMs"].as_i64().unwrap();
    let defaults = EventNdjsonLogStoreOptions::default();
    let store = EventNdjsonLogStore::open(
        out_dir.join("events.log"),
        EventNdjsonLogStoreOptions {
            max_bytes: number("maxBytes").unwrap_or(defaults.max_bytes),
            max_files: number("maxFiles").unwrap_or(defaults.max_files),
            batch_window_ms: number("batchWindowMs").unwrap_or(defaults.batch_window_ms),
            max_buffered_records: number("maxBufferedRecords").unwrap_or(defaults.max_buffered_records),
            max_buffered_bytes: number("maxBufferedBytes").unwrap_or(defaults.max_buffered_bytes),
            clock: Arc::new(move || clock_ms),
            ..defaults
        },
    )
    .unwrap();
    for record in fixture["records"].as_array().unwrap() {
        let stream = match record["stream"].as_str().unwrap() {
            "native" => EventNdjsonStream::Native,
            "canonical" => EventNdjsonStream::Canonical,
            _ => EventNdjsonStream::Orchestration,
        };
        store.logger(stream).write(&record["event"], record["threadId"].as_str());
    }
    store.close();
}

fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name().to_string_lossy().into_owned(), std::fs::read(entry.path()).unwrap())
        })
        .collect()
}

/// The gate: `node code/apps/server/scripts/provider-event-log-parity.ts` (the TS logger, from
/// source) and this crate write byte-identical files for the same records. Needs Node and
/// `code/apps/server/node_modules`; skipped (with a message) when they are missing.
#[tokio::test]
async fn writes_the_same_bytes_as_the_typescript_logger() {
    let code = code_dir();
    if !code.join("apps/server/node_modules/effect").exists() || Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipping the TS parity check: node or code/apps/server/node_modules is missing");
        return;
    }
    let records = parity_records();
    let runs = [
        json!({"clockMs": CLOCK_MS, "options": {"batchWindowMs": 0, "maxBytes": 4096, "maxFiles": 3}, "records": records}),
        json!({"clockMs": CLOCK_MS, "options": {"batchWindowMs": 60000, "maxBytes": 8192, "maxFiles": 2, "maxBufferedRecords": 7}, "records": records}),
        json!({"clockMs": CLOCK_MS, "options": {"batchWindowMs": 60000, "maxBufferedBytes": 3000}, "records": records}),
    ];
    for (index, fixture) in runs.iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let fixture_path = dir.path().join("fixture.json");
        std::fs::write(&fixture_path, serde_json::to_string(fixture).unwrap()).unwrap();
        // Both sides read the same bytes.
        let parsed: Value = serde_json::from_str(&std::fs::read_to_string(&fixture_path).unwrap()).unwrap();
        let ts_out = dir.path().join("ts");
        let rust_out = dir.path().join("rust");
        std::fs::create_dir_all(&ts_out).unwrap();
        std::fs::create_dir_all(&rust_out).unwrap();
        let output = Command::new("node")
            .current_dir(&code)
            .arg("apps/server/scripts/provider-event-log-parity.ts")
            .arg(&fixture_path)
            .arg(&ts_out)
            .output()
            .unwrap();
        assert!(output.status.success(), "TS logger failed: {}", String::from_utf8_lossy(&output.stderr));
        run_rust(&parsed, &rust_out);
        let ts_files = files(&ts_out);
        let rust_files = files(&rust_out);
        assert_eq!(
            ts_files.keys().collect::<Vec<_>>(),
            rust_files.keys().collect::<Vec<_>>(),
            "run {index}: same files"
        );
        assert!(ts_files.len() >= 6, "run {index}: {:?}", ts_files.keys());
        for (name, ts_bytes) in &ts_files {
            let rust_bytes = &rust_files[name];
            if ts_bytes != rust_bytes {
                let ts_text = String::from_utf8_lossy(ts_bytes);
                let rust_text = String::from_utf8_lossy(rust_bytes);
                let first_diff = ts_text.lines().zip(rust_text.lines()).find(|(a, b)| a != b);
                panic!(
                    "run {index}: {name} differs ({} vs {} bytes); first differing line:\n{first_diff:?}",
                    ts_bytes.len(),
                    rust_bytes.len()
                );
            }
        }
    }
}
