//! `usageScanCache.test.ts`.

use serde_json::{json, Value};

use super::*;
use crate::json;

fn record() -> UsageRecord {
    UsageRecord {
        provider: UsageProviderKind::Claude,
        timestamp_ms: 1_786_000_000_000.0,
        model: "claude-fable-5".into(),
        rate_model: None,
        session_id: "session-a".into(),
        totals: Totals {
            uncached_input_tokens: 2.0,
            cached_input_tokens: 1000.0,
            cache_creation_tokens: 10.0,
            output_tokens: 50.0,
            reasoning_tokens: 0.0,
        },
        reported_cost_usd: None,
        fast: false,
        dedupe_key: Some("msg_1:".into()),
    }
}

fn position() -> ParsePosition {
    ParsePosition {
        resume_offset: 120,
        guard_length: 64,
        guard_hash: f64::from(0xdead_beef_u32),
        codex_state: None,
    }
}

fn cache_with(entries: Vec<(&str, f64, Vec<UsageRecord>)>) -> ScanCache {
    let mut cache = ScanCache::new();
    for (path, mtime_ms, records) in entries {
        cache.insert(
            path.to_owned(),
            CachedFile {
                size: records.len() as f64 * 10.0,
                mtime_ms,
                provider: UsageProviderKind::Claude,
                records,
                tail_records: Vec::new(),
                position: position(),
            },
        );
    }
    cache
}

/// `decodeScanCache(JSON.parse(JSON.stringify(document)))`.
fn round_trip(document: &Value) -> ScanCache {
    decode_scan_cache(&json::parse(document.to_string().as_bytes()).unwrap())
}

fn encoded(cache: &ScanCache) -> Value {
    Value::Object(encode_scan_cache(cache))
}

#[test]
fn restores_records_unchanged() {
    let mut original = cache_with(vec![
        (
            "/a.jsonl",
            100.0,
            vec![
                record(),
                UsageRecord {
                    dedupe_key: Some("msg_2:".into()),
                    model: "claude-opus-5-5".into(),
                    fast: true,
                    ..record()
                },
            ],
        ),
        (
            "/b.jsonl",
            200.0,
            vec![UsageRecord {
                session_id: "session-b".into(),
                reported_cost_usd: Some(1.5),
                ..record()
            }],
        ),
    ]);
    let grok = UsageRecord {
        provider: UsageProviderKind::Grok,
        model: "grok-4.5-build".into(),
        dedupe_key: Some("s:p:grok-4.5-build".into()),
        ..record()
    };
    original.insert(
        "/grok.jsonl".into(),
        CachedFile {
            size: 40.0,
            mtime_ms: 300.0,
            provider: UsageProviderKind::Grok,
            records: vec![grok.clone()],
            tail_records: vec![UsageRecord { dedupe_key: None, ..grok }],
            position: ParsePosition {
                resume_offset: 30,
                guard_length: 30,
                guard_hash: 123.0,
                codex_state: None,
            },
        },
    );
    original.insert(
        "/codex.jsonl".into(),
        CachedFile {
            size: 80.0,
            mtime_ms: 400.0,
            provider: UsageProviderKind::Codex,
            records: vec![UsageRecord {
                provider: UsageProviderKind::Codex,
                model: "gpt-5.2-codex".into(),
                dedupe_key: None,
                ..record()
            }],
            tail_records: Vec::new(),
            position: ParsePosition {
                codex_state: Some(CodexScanState {
                    model: "gpt-5.2-codex".into(),
                    session_id: "session-c".into(),
                    last_usage_signature: Some("{\"input_tokens\":1}".into()),
                    saw_session_meta: true,
                    suppressing_fork_copies: false,
                    fork_copy_anchor_ms: 0.0,
                }),
                ..position()
            },
        },
    );
    let restored = round_trip(&encoded(&original));
    assert_eq!(restored.len(), 4);
    for path in ["/a.jsonl", "/b.jsonl", "/grok.jsonl", "/codex.jsonl"] {
        assert_eq!(restored.get(path), original.get(path), "{path}");
    }
}

#[test]
fn drops_an_entry_with_a_corrupt_parse_state() {
    let mut document = encoded(&cache_with(vec![("/a.jsonl", 100.0, vec![record()])]));
    document["files"]["/a.jsonl"]["cs"] = json!({"model": 42});
    assert!(!round_trip(&document).contains_key("/a.jsonl"));
}

#[test]
fn drops_an_entry_with_an_unsupported_guard_length() {
    let mut document = encoded(&cache_with(vec![("/a.jsonl", 100.0, vec![record()])]));
    document["files"]["/a.jsonl"]["gl"] = json!(1e20);
    assert!(!round_trip(&document).contains_key("/a.jsonl"));
}

#[test]
fn drops_an_entry_whose_fast_flag_is_not_0_or_1() {
    let mut document = encoded(&cache_with(vec![("/a.jsonl", 100.0, vec![UsageRecord { fast: true, ..record() }])]));
    document["files"]["/a.jsonl"]["r"][0][10] = json!(true);
    assert!(!round_trip(&document).contains_key("/a.jsonl"));
}

#[test]
fn rejects_a_previous_cache_version() {
    let mut document = encoded(&cache_with(vec![("/a.jsonl", 100.0, vec![record()])]));
    document["version"] = json!(3);
    assert!(round_trip(&document).is_empty());
}

#[test]
fn interns_repeated_models_and_sessions() {
    let document = encoded(&cache_with(vec![(
        "/a.jsonl",
        100.0,
        vec![
            record(),
            UsageRecord {
                dedupe_key: Some("msg_2:".into()),
                ..record()
            },
            record(),
        ],
    )]));
    assert_eq!(document["models"], json!(["claude-fable-5"]));
    assert_eq!(document["sessions"], json!(["session-a"]));
}

#[test]
fn corrupt_or_foreign_documents_are_empty_caches() {
    assert!(round_trip(&Value::Null).is_empty());
    assert!(round_trip(&json!("nonsense")).is_empty());
    assert!(round_trip(&json!({"version": 999, "models": [], "sessions": [], "files": {}})).is_empty());
}

#[test]
fn skips_malformed_entries_but_keeps_good_ones() {
    let mut document = encoded(&cache_with(vec![("/good.jsonl", 100.0, vec![record()])]));
    document["files"]["/bad.jsonl"] = json!({"s": "nope", "m": 1, "p": "claude", "r": []});
    let restored = round_trip(&document);
    assert_eq!(restored.keys().collect::<Vec<_>>(), ["/good.jsonl"]);
}

#[test]
fn rejects_the_cache_when_an_intern_table_holds_a_non_string() {
    let mut document = encoded(&cache_with(vec![("/a.jsonl", 100.0, vec![record()])]));
    document["models"] = json!([1]);
    assert!(round_trip(&document).is_empty());
}

#[test]
fn drops_the_whole_entry_when_any_row_is_corrupt() {
    let mut document = encoded(&cache_with(vec![(
        "/a.jsonl",
        100.0,
        vec![
            record(),
            UsageRecord {
                dedupe_key: Some("msg_2:".into()),
                ..record()
            },
        ],
    )]));
    document["files"]["/a.jsonl"]["r"][1][3] = json!("not-a-number");
    assert!(!round_trip(&document).contains_key("/a.jsonl"));
}

#[test]
fn reads_a_ts_written_document() {
    // The TS encoder's exact output for one cached Claude file.
    let document = r#"{"version":4,"models":["claude-fable-5"],"sessions":["session-a"],"files":{"/a.jsonl":{"s":120,"m":1790867552033.4062,"p":"claude","r":[[1786000000000,0,0,2,1000,10,50,0,"msg_1:",null,0]],"t":[],"o":120,"gl":64,"gh":3735928559,"cs":null}},"sources":{"claude\u0000/x":{"dir":"/x","volumeId":"1:2"}}}"#;
    let restored = decode_scan_cache(&json::parse(document.as_bytes()).unwrap());
    let entry = restored.get("/a.jsonl").unwrap();
    assert_eq!(entry.mtime_ms, 1_790_867_552_033.406_2);
    assert_eq!(entry.records, [record()]);
    assert_eq!(entry.position, position());
}

#[test]
fn prune_drops_entries_older_than_retention() {
    let mut cache = cache_with(vec![("/old.jsonl", 500.0, vec![record()])]);
    assert_eq!(prune_scan_cache(&mut cache, 1000.0), 1);
    assert!(cache.is_empty());
}

#[test]
fn prune_keeps_entries_whose_file_disappeared() {
    let mut cache = cache_with(vec![("/gone.jsonl", 5000.0, vec![record()])]);
    prune_scan_cache(&mut cache, 1000.0);
    assert_eq!(cache.len(), 1);
}

#[test]
fn dedupe_keeps_the_first_record_per_key() {
    let mut seen = HashSet::new();
    let kept = dedupe_within_file(
        vec![
            UsageRecord {
                totals: Totals {
                    output_tokens: 1.0,
                    ..record().totals
                },
                ..record()
            },
            UsageRecord {
                totals: Totals {
                    output_tokens: 999.0,
                    ..record().totals
                },
                ..record()
            },
            UsageRecord {
                dedupe_key: Some("msg_2:".into()),
                ..record()
            },
        ],
        &mut seen,
    );
    assert_eq!(kept.len(), 2);
    assert_eq!(kept[0].totals.output_tokens, 1.0);
}

#[test]
fn dedupe_keeps_records_without_a_key() {
    let mut seen = HashSet::new();
    assert_eq!(
        dedupe_within_file(
            vec![UsageRecord { dedupe_key: None, ..record() }, UsageRecord { dedupe_key: None, ..record() }],
            &mut seen
        )
        .len(),
        2
    );
}
