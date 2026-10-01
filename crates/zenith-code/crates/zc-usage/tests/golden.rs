//! Golden comparison with the TypeScript `UsageService` (`code/apps/server/scripts/usage-oracle.ts`).
//!
//!   cargo test --release -p zc-usage --test golden -- --ignored --nocapture
//!
//! Both services read the same inputs, read-only: the real `~/.claude` and `~/.codex` (or
//! `ZC_USAGE_CLAUDE_HOME` / `ZC_USAGE_CODEX_HOME`), plus synthetic Grok, OpenCode and
//! Antigravity fixtures built here. Pricing is pinned to a copy of a rates file
//! (`ZC_USAGE_RATES`, default `~/.zenith/code/userdata/usage-model-rates.json`) and the rate
//! fetch fails on both sides. Each side gets its own state directory; then each reads the
//! scan cache the other wrote, which must give the same summaries again.
//!
//! Needs Node and the server's `node_modules` (symlinked from the main checkout).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::{json, Value};
use zc_contracts::UsageSummaryInput;
use zc_usage::cursor::{CursorHttp, HttpReply, KeychainError, KeychainToken};
use zc_usage::service::{Platform, RatesFetcher};
use zc_usage::{UsageService, UsageServiceOptions, UsageSettings};

struct StaticSettings(Value);

#[async_trait]
impl UsageSettings for StaticSettings {
    async fn get(&self) -> Result<Value, String> {
        Ok(self.0.clone())
    }
    fn changes(&self) -> BoxStream<'static, Value> {
        Box::pin(futures::stream::pending())
    }
}

struct Offline;

#[async_trait]
impl RatesFetcher for Offline {
    async fn fetch(&self, _: Duration) -> Result<Vec<u8>, String> {
        Err("offline".into())
    }
}

#[async_trait]
impl CursorHttp for Offline {
    async fn post(&self, _: &str, _: &[(&str, String)], _: String, _: Duration) -> Result<HttpReply, String> {
        Err("offline".into())
    }
}

#[async_trait]
impl KeychainToken for Offline {
    async fn read(&self) -> Result<Option<String>, KeychainError> {
        Err(KeychainError::Failed)
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).unwrap().to_path_buf()
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap())
}

fn hostname() -> String {
    let output = Command::new("hostname").output().unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
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
    let mut bytes = proto_varint(field * 8);
    bytes.extend(proto_varint(value));
    bytes
}

fn proto_bytes(field: u64, payload: &[u8]) -> Vec<u8> {
    let mut bytes = proto_varint(field * 8 + 2);
    bytes.extend(proto_varint(payload.len() as u64));
    bytes.extend_from_slice(payload);
    bytes
}

fn proto_text(field: u64, value: &str) -> Vec<u8> {
    proto_bytes(field, value.as_bytes())
}

fn concat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.concat()
}

/// Synthetic Grok, OpenCode and Antigravity history with made-up sessions.
fn build_fixtures(root: &Path) {
    // Grok: two sessions, one multi-model turn, one plain turn, one duplicate prompt.
    for (session, lines) in [
        (
            "grok-session-a",
            vec![
                json!({"timestamp": 1_790_000_000, "params": {"sessionId": "grok-session-a", "_meta": {"agentTimestampMs": 1_790_000_000_123u64},
                    "update": {"sessionUpdate": "turn_completed", "prompt_id": "p1", "usage": {"inputTokens": 1200, "outputTokens": 300, "cachedReadTokens": 200, "costUsdTicks": 30_000_000,
                    "modelUsage": {"grok-4.5": {"inputTokens": 1000, "outputTokens": 250, "cachedReadTokens": 200, "reasoningTokens": 40}, "grok-composer-2.5-fast": {"inputTokens": 200, "outputTokens": 50}}}}}}),
                json!({"timestamp": 1_790_100_000, "params": {"sessionId": "grok-session-a", "update": {"sessionUpdate": "turn_completed", "prompt_id": "p2", "usage": {"inputTokens": 500, "outputTokens": 20}}}}),
                json!({"timestamp": 1_790_100_000, "params": {"sessionId": "grok-session-a", "update": {"sessionUpdate": "turn_completed", "prompt_id": "p2", "usage": {"inputTokens": 500, "outputTokens": 20}}}}),
                json!({"method": "session/update", "params": {"update": {"sessionUpdate": "agent_message_chunk"}}}),
            ],
        ),
        (
            "grok-session-b",
            vec![
                json!({"timestamp": 1_791_000_000, "params": {"sessionId": "grok-session-b", "update": {"sessionUpdate": "turn_completed", "prompt_id": "p1",
                "usage": {"inputTokens": 9000, "outputTokens": 700, "cachedReadTokens": 4000, "cacheCreationTokens": 100, "reasoningTokens": 300, "costUsdTicks": 120_000_000}}}}),
            ],
        ),
    ] {
        let dir = root.join("grok").join("sessions").join(session);
        std::fs::create_dir_all(&dir).unwrap();
        let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
        std::fs::write(dir.join("updates.jsonl"), text).unwrap();
        std::fs::write(dir.join("events.jsonl"), "{\"usage\": 1}\n").unwrap();
    }

    // OpenCode: a store with a migrated legacy copy, and a second store.
    let opencode = root.join("opencode");
    std::fs::create_dir_all(opencode.join("storage").join("message").join("oc-session-1")).unwrap();
    let message = |id: &str, session: &str, created: u64, model: &str, input: u64, cost: f64| {
        json!({"id": id, "sessionID": session, "role": "assistant", "modelID": model, "time": {"created": created}, "cost": cost,
            "tokens": {"input": input, "output": 40, "reasoning": 7, "cache": {"read": 300, "write": 12}}})
    };
    {
        let db = rusqlite::Connection::open(opencode.join("opencode.db")).unwrap();
        db.execute_batch("CREATE TABLE message (id TEXT, session_id TEXT, data TEXT, time_created INTEGER)")
            .unwrap();
        for (index, (model, cost)) in [("claude-sonnet-4-5", 0.0), ("example-open-model", 0.5), ("gpt-5", 0.0)].iter().enumerate() {
            let id = format!("oc-msg-{index}");
            let created = 1_790_200_000_000u64 + index as u64 * 3_600_000;
            let data = message(&id, "oc-session-1", created, model, 100 + index as u64, *cost).to_string();
            db.execute(
                "INSERT INTO message VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![id, "oc-session-1", data, created as i64],
            )
            .unwrap();
        }
        db.execute(
            "INSERT INTO message VALUES ('oc-user', 'oc-session-1', '{\"role\":\"user\"}', 1790200000000)",
            [],
        )
        .unwrap();
    }
    std::fs::write(
        opencode.join("storage").join("message").join("oc-session-1").join("oc-msg-0.json"),
        message("oc-msg-0", "oc-session-1", 1_790_200_000_000, "claude-sonnet-4-5", 100, 0.0).to_string(),
    )
    .unwrap();
    std::fs::write(
        opencode.join("storage").join("message").join("oc-session-1").join("oc-legacy.json"),
        message("oc-legacy", "oc-session-1", 1_790_300_000_000, "claude-sonnet-4-5", 55, 0.0).to_string(),
    )
    .unwrap();
    {
        let db = rusqlite::Connection::open(opencode.join("opencode-work.db")).unwrap();
        db.execute_batch("CREATE TABLE session_message (id TEXT, session_id TEXT, type TEXT, data TEXT, time_created INTEGER)")
            .unwrap();
        let data = message("oc-work-1", "oc-session-2", 1_790_400_000_000, "gpt-5-mini", 900, 0.0).to_string();
        db.execute(
            "INSERT INTO session_message VALUES ('oc-work-1', 'oc-session-2', 'assistant', ?1, 1790400000000)",
            [data],
        )
        .unwrap();
    }

    // Antigravity: one conversation with generation + step + retry metadata, one step-only.
    let conversations = root.join("antigravity").join("conversations");
    std::fs::create_dir_all(&conversations).unwrap();
    {
        let db = rusqlite::Connection::open(conversations.join("ag-conversation-1.db")).unwrap();
        db.execute_batch("CREATE TABLE gen_metadata (idx INTEGER, data BLOB); CREATE TABLE steps (idx INTEGER, metadata BLOB)")
            .unwrap();
        let stamp = proto_number(1, 1_790_500_000);
        let usage = concat(&[
            proto_number(2, 1000),
            proto_number(3, 400),
            proto_number(4, 50),
            proto_number(5, 2000),
            proto_number(9, 100),
            proto_text(11, "ag-response-1"),
        ]);
        let retry = concat(&[proto_number(1, 1026), proto_number(2, 120), proto_number(3, 30), proto_text(11, "ag-retry-1")]);
        let generation = proto_bytes(
            1,
            &concat(&[proto_bytes(4, &usage), proto_text(19, "Gemini 3 Pro"), proto_bytes(9, &proto_bytes(4, &stamp))]),
        );
        let step = concat(&[proto_bytes(9, &usage), proto_bytes(8, &stamp), proto_bytes(28, &proto_bytes(2, &retry))]);
        db.execute("INSERT INTO gen_metadata VALUES (0, ?1)", [generation]).unwrap();
        db.execute("INSERT INTO steps VALUES (0, ?1)", [step]).unwrap();
    }
    {
        let db = rusqlite::Connection::open(conversations.join("ag-conversation-2.db")).unwrap();
        db.execute_batch("CREATE TABLE steps (idx INTEGER, metadata BLOB)").unwrap();
        let usage = concat(&[proto_number(1, 246), proto_number(2, 700), proto_number(3, 90)]);
        let step = concat(&[proto_bytes(9, &usage), proto_bytes(8, &proto_number(1, 1_790_600_000))]);
        db.execute("INSERT INTO steps VALUES (0, ?1)", [step]).unwrap();
    }
}

fn input(time_zone: &str, since: &str, until: &str, hourly: Option<(&str, &str)>) -> Value {
    let mut value = json!({"timeZone": time_zone, "sinceDay": since, "untilDay": until});
    if let Some((since_time, until_time)) = hourly {
        value["resolution"] = json!("hour");
        value["sinceTime"] = json!(since_time);
        value["untilTime"] = json!(until_time);
    }
    value
}

fn prepare_state(base: &Path, rates: &Path) {
    let state = base.join("userdata");
    std::fs::create_dir_all(&state).unwrap();
    if rates.exists() {
        std::fs::copy(rates, state.join("usage-model-rates.json")).unwrap();
    }
}

fn run_ts(config: &Value, scratch: &Path) -> Vec<(Value, f64)> {
    let config_path = scratch.join(format!("oracle-{}.json", std::process::id()));
    std::fs::write(&config_path, config.to_string()).unwrap();
    let output = Command::new("node")
        .arg("apps/server/scripts/usage-oracle.ts")
        .arg(&config_path)
        .current_dir(repo_root().join("code"))
        .output()
        .expect("node runs");
    assert!(output.status.success(), "oracle failed: {}", String::from_utf8_lossy(&output.stderr));
    // The exact reader (serde_json's default float parsing can be one ulp off).
    let parsed: Value = zc_usage::json::to_value(&zc_usage::json::parse(&output.stdout).expect("oracle JSON"));
    parsed["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|result| (result["summary"].clone(), result["elapsedMs"].as_f64().unwrap()))
        .collect()
}

async fn run_rust(base: &Path, settings: &Value, environment: &HashMap<String, String>, fake_home: &Path, inputs: &[Value]) -> Vec<(Value, f64)> {
    let service = UsageService::new(UsageServiceOptions {
        state_dir: base.join("userdata"),
        settings: Arc::new(StaticSettings(settings.clone())),
        environment: environment.clone(),
        platform: Platform::Linux,
        home_dir: fake_home.to_path_buf(),
        hostname: hostname(),
        rates: Arc::new(Offline),
        cursor_http: Arc::new(Offline),
        keychain: Arc::new(Offline),
        now_ms: Arc::new(|| zc_core::time::now_millis() as f64),
    });
    let mut results = Vec::new();
    for input in inputs {
        let input: UsageSummaryInput = serde_json::from_value(input.clone()).unwrap();
        let started = Instant::now();
        let summary = service.read_summary(input).await.expect("summary");
        results.push((summary, started.elapsed().as_secs_f64() * 1000.0));
    }
    results
}

/// Differences between two encoded summaries, ignoring `readAt` and `scanDurationMs`.
fn diff(path: &str, ts: &Value, rust: &Value, out: &mut Vec<String>) {
    if out.len() > 20 {
        return;
    }
    match (ts, rust) {
        (Value::Object(a), Value::Object(b)) => {
            let keys_a: Vec<&String> = a.keys().collect();
            let keys_b: Vec<&String> = b.keys().collect();
            if keys_a != keys_b {
                out.push(format!("{path}: keys {keys_a:?} != {keys_b:?}"));
            }
            for (key, value) in a {
                if key == "readAt" || key == "scanDurationMs" {
                    continue;
                }
                if let Some(other) = b.get(key) {
                    diff(&format!("{path}.{key}"), value, other, out);
                }
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                out.push(format!("{path}: length {} != {}", a.len(), b.len()));
            }
            for (index, (x, y)) in a.iter().zip(b).enumerate() {
                diff(&format!("{path}[{index}]"), x, y, out);
            }
        }
        (Value::Number(a), Value::Number(b)) => {
            if a.as_f64() != b.as_f64() {
                out.push(format!("{path}: {a} != {b}"));
            }
        }
        (a, b) => {
            if a != b {
                out.push(format!("{path}: {a} != {b}"));
            }
        }
    }
}

fn compare(label: &str, ts: &[(Value, f64)], rust: &[(Value, f64)], inputs: &[Value]) -> usize {
    if let Ok(dump) = std::env::var("ZC_USAGE_GOLDEN_DUMP") {
        let slug: String = label.chars().filter(char::is_ascii_alphanumeric).collect();
        let summaries = |side: &[(Value, f64)]| Value::Array(side.iter().map(|(summary, _)| summary.clone()).collect());
        std::fs::create_dir_all(&dump).unwrap();
        std::fs::write(Path::new(&dump).join(format!("{slug}-ts.json")), summaries(ts).to_string()).unwrap();
        std::fs::write(Path::new(&dump).join(format!("{slug}-rust.json")), summaries(rust).to_string()).unwrap();
    }
    let mut failures = 0;
    for (index, ((ts_summary, ts_ms), (rust_summary, rust_ms))) in ts.iter().zip(rust).enumerate() {
        let mut differences = Vec::new();
        diff("summary", ts_summary, rust_summary, &mut differences);
        let buckets = ts_summary["buckets"].as_array().map_or(0, Vec::len);
        let sources = ts_summary["sources"].as_array().map_or(0, Vec::len);
        println!(
            "[{label}] {} → {} buckets, {} sources; TS {:.0} ms, Rust {:.0} ms: {}",
            inputs[index],
            buckets,
            sources,
            ts_ms,
            rust_ms,
            if differences.is_empty() {
                "identical".to_owned()
            } else {
                format!("{} differences", differences.len())
            }
        );
        for difference in &differences {
            println!("    {difference}");
        }
        failures += usize::from(!differences.is_empty());
    }
    failures
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Node, the server's node_modules and real transcripts; run explicitly"]
async fn golden_against_the_typescript_usage_service() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().canonicalize().unwrap();
    let fixtures = root.join("fixtures");
    build_fixtures(&fixtures);
    let fake_home = root.join("home");
    std::fs::create_dir_all(&fake_home).unwrap();
    let claude_home = std::env::var("ZC_USAGE_CLAUDE_HOME").map_or_else(|_| home().join(".claude"), PathBuf::from);
    let codex_home = std::env::var("ZC_USAGE_CODEX_HOME").map_or_else(|_| home().join(".codex"), PathBuf::from);
    let rates = std::env::var("ZC_USAGE_RATES").map_or_else(|_| home().join(".zenith/code/userdata/usage-model-rates.json"), PathBuf::from);

    let environment: HashMap<String, String> = [
        ("HOME", fake_home.clone()),
        ("GROK_HOME", fixtures.join("grok")),
        ("OPENCODE_DATA_DIR", fixtures.join("opencode")),
        ("ANTIGRAVITY_DATA_DIR", fixtures.join("antigravity")),
        ("XDG_CONFIG_HOME", fixtures.join("config")),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_string_lossy().into_owned()))
    .chain([("AGENT_CLI_CREDENTIAL_STORE".to_owned(), "file".to_owned())])
    .collect();
    let settings = json!({
        "providers": {
            "claudeAgent": {"homePath": claude_home.to_string_lossy()},
            "codex": {"homePath": codex_home.to_string_lossy()},
        },
        "usagePriceOverrides": {"example-open-model": {"inputCostPerMillionTokens": 2, "outputCostPerMillionTokens": 8, "cacheReadCostPerMillionTokens": 0.5}},
    });
    let inputs = vec![
        input("Europe/Paris", "2026-09-02", "2026-10-01", None),
        input("UTC", "2026-09-24", "2026-10-01", None),
        input("America/Los_Angeles", "2026-09-01", "2026-09-30", None),
        input("Asia/Tokyo", "2026-07-04", "2026-10-01", None),
        input(
            "Europe/Paris",
            "2026-09-30",
            "2026-10-01",
            Some(("2026-09-30T12:00:00.000Z", "2026-10-01T12:00:00.000Z")),
        ),
        input("Australia/Lord_Howe", "2026-09-20", "2026-09-27", None),
    ];

    let ts_base = root.join("ts-state");
    let rust_base = root.join("rust-state");
    prepare_state(&ts_base, &rates);
    prepare_state(&rust_base, &rates);
    let config = |base: &Path| {
        json!({
            "baseDir": base.to_string_lossy(),
            "cwd": root.to_string_lossy(),
            "settings": settings,
            "environment": environment,
            "platform": "linux",
            "inputs": inputs,
        })
    };

    // Cold: each side with its own empty scan cache.
    let ts_cold = run_ts(&config(&ts_base), &root);
    let rust_cold = run_rust(&rust_base, &settings, &environment, &fake_home, &inputs).await;
    let mut failures = compare("cold", &ts_cold, &rust_cold, &inputs);

    // Warm, crossed: each side reads the scan cache the other one wrote.
    let ts_cache = ts_base.join("userdata/usage-scan-cache.json");
    let rust_cache = rust_base.join("userdata/usage-scan-cache.json");
    let crossed_ts = root.join("crossed-ts");
    let crossed_rust = root.join("crossed-rust");
    prepare_state(&crossed_ts, &rates);
    prepare_state(&crossed_rust, &rates);
    std::fs::copy(&rust_cache, crossed_ts.join("userdata/usage-scan-cache.json")).unwrap();
    std::fs::copy(&ts_cache, crossed_rust.join("userdata/usage-scan-cache.json")).unwrap();
    let ts_warm = run_ts(&config(&crossed_ts), &root);
    let rust_warm = run_rust(&crossed_rust, &settings, &environment, &fake_home, &inputs).await;
    failures += compare("warm, TS reads the Rust cache vs Rust reads the TS cache", &ts_warm, &rust_warm, &inputs);
    failures += compare("cold TS vs warm Rust", &ts_cold, &rust_warm, &inputs);

    assert_eq!(failures, 0, "summaries differ");
}
