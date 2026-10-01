//! `UsageService.test.ts`.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering as AtomicOrdering};

use futures::stream::BoxStream;
use serde_json::{json, Value};

use super::*;
use crate::cursor::{HttpReply, KeychainError};

/// Settings the test can edit (`ServerSettings.layerTest` + `updateSettings`).
struct TestSettings(Mutex<Value>);

impl TestSettings {
    fn set(&self, path: &[&str], value: Value) {
        let mut settings = self.0.lock().unwrap();
        let mut target = &mut *settings;
        for key in &path[..path.len() - 1] {
            if target.get(*key).is_none() {
                target[*key] = json!({});
            }
            target = &mut target[*key];
        }
        target[path[path.len() - 1]] = value;
    }
}

#[async_trait]
impl UsageSettings for TestSettings {
    async fn get(&self) -> Result<Value, String> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn changes(&self) -> BoxStream<'static, Value> {
        Box::pin(futures::stream::pending())
    }
}

/// Serves `document` (default `{}`, which parses to no rates, so every scan refetches and
/// the fetch count observes how many scans ran), optionally gated.
struct TestRates {
    document: Value,
    fetches: AtomicUsize,
    gate: Option<tokio::sync::watch::Receiver<bool>>,
}

#[async_trait]
impl RatesFetcher for TestRates {
    async fn fetch(&self, _: Duration) -> Result<Vec<u8>, String> {
        self.fetches.fetch_add(1, AtomicOrdering::SeqCst);
        if let Some(gate) = &self.gate {
            let mut gate = gate.clone();
            let _ = gate.wait_for(|open| *open).await;
        }
        Ok(self.document.to_string().into_bytes())
    }
}

struct Offline;

#[async_trait]
impl CursorHttp for Offline {
    async fn post(&self, _: &str, _: &[(&str, String)], _: String, _: Duration) -> Result<HttpReply, String> {
        Err("offline".into())
    }
}

#[async_trait]
impl KeychainToken for Offline {
    async fn read(&self) -> Result<Option<String>, KeychainError> {
        panic!("the Keychain must not be read")
    }
}

struct Setup {
    _dir: tempfile::TempDir,
    home: PathBuf,
    transcript: PathBuf,
    settings: Value,
}

fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().canonicalize().unwrap();
    let transcript_dir = home.join("claude").join("projects").join("proj");
    std::fs::create_dir_all(&transcript_dir).unwrap();
    let settings = json!({"providers": {
        "claudeAgent": {"homePath": path_string(&home.join("claude"))},
        "codex": {"homePath": path_string(&home.join("codex"))},
    }});
    Setup {
        transcript: transcript_dir.join("session.jsonl"),
        home,
        settings,
        _dir: dir,
    }
}

struct Harness {
    service: UsageService,
    settings: Arc<TestSettings>,
    rates: Arc<TestRates>,
    clock: Arc<AtomicU64>,
    state_dir: PathBuf,
}

struct Options<'a> {
    settings: Value,
    environment: Vec<(&'a str, String)>,
    platform: Platform,
    rates: Value,
    state_dir: Option<PathBuf>,
    gate: Option<tokio::sync::watch::Receiver<bool>>,
}

impl Options<'_> {
    fn new(settings: &Value) -> Self {
        Self {
            settings: settings.clone(),
            environment: Vec::new(),
            platform: Platform::Linux,
            rates: json!({}),
            state_dir: None,
            gate: None,
        }
    }
}

fn now() -> u64 {
    zc_core::time::now_millis() as u64
}

fn harness(setup: &Setup, options: Options<'_>) -> Harness {
    let home = &setup.home;
    let mut environment: HashMap<String, String> = [
        ("HOME", home.clone()),
        ("GROK_HOME", home.join("grok")),
        ("OPENCODE_DATA_DIR", home.join("opencode")),
        ("ANTIGRAVITY_DATA_DIR", home.join("antigravity")),
        ("XDG_CONFIG_HOME", home.join("config")),
        ("APPDATA", home.join("config")),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), path_string(&value)))
    .collect();
    for (key, value) in options.environment {
        environment.insert(key.to_owned(), value);
    }
    let state_dir = options.state_dir.unwrap_or_else(|| {
        let dir = tempfile::Builder::new().prefix("usage-state-").tempdir_in(home).unwrap().keep();
        dir.join("userdata")
    });
    std::fs::create_dir_all(&state_dir).unwrap();
    let settings = Arc::new(TestSettings(Mutex::new(options.settings)));
    let rates = Arc::new(TestRates {
        document: options.rates,
        fetches: AtomicUsize::new(0),
        gate: options.gate,
    });
    let clock = Arc::new(AtomicU64::new(now()));
    let clock_reader = clock.clone();
    let service = UsageService::new(UsageServiceOptions {
        state_dir: state_dir.clone(),
        settings: settings.clone(),
        environment,
        platform: options.platform,
        home_dir: home.clone(),
        hostname: "test-host".into(),
        rates: rates.clone(),
        cursor_http: Arc::new(Offline),
        keychain: Arc::new(Offline),
        now_ms: Arc::new(move || clock_reader.load(AtomicOrdering::SeqCst) as f64),
    });
    Harness {
        service,
        settings,
        rates,
        clock,
        state_dir,
    }
}

fn claude_line(id: u32, output_tokens: u64, model: &str) -> String {
    format!(
        "{}\n",
        json!({
            "type": "assistant",
            "timestamp": "2026-08-01T10:00:00Z",
            "requestId": format!("req_{id}"),
            "sessionId": "session-1",
            "message": {"id": format!("msg_{id}"), "model": model, "usage": {"input_tokens": 10, "output_tokens": output_tokens}},
        })
    )
}

fn window() -> UsageSummaryInput {
    serde_json::from_value(json!({"timeZone": "UTC", "sinceDay": "2026-07-31", "untilDay": "2026-08-02"})).unwrap()
}

fn total_output(summary: &Value) -> f64 {
    summary["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|bucket| bucket["totals"]["outputTokens"].as_f64().unwrap())
        .sum()
}

fn sources_for<'a>(summary: &'a Value, provider: &str) -> Vec<&'a Value> {
    summary["sources"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|source| source["fingerprint"]["provider"] == provider)
        .collect()
}

fn codex_rollout(id: &str, outputs: &[u64]) -> String {
    let mut lines = vec![
        json!({"type": "session_meta", "payload": {"id": id}}),
        json!({"type": "turn_context", "payload": {"model": "gpt-5.6-sol"}}),
    ];
    for output in outputs {
        lines.push(json!({"type": "event_msg", "timestamp": "2026-08-01T10:00:00Z", "payload": {"type": "token_count", "info": {"last_token_usage": {"input_tokens": 10, "output_tokens": output}}}}));
    }
    lines.iter().map(|line| format!("{line}\n")).collect()
}

fn realpath(path: &Path) -> String {
    path_string(&std::fs::canonicalize(path).unwrap())
}

#[tokio::test]
async fn reads_managed_default_and_disabled_accounts_once() {
    for explicit_default in [true, false] {
        let setup = setup();
        let shared = setup.home.join("shared-codex");
        let sessions = shared.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        for (id, output) in [("codex", 17), ("codex-personal", 23)] {
            std::fs::write(sessions.join(format!("{id}-rollout.jsonl")), codex_rollout(id, &[output])).unwrap();
        }
        let mut settings = setup.settings.clone();
        settings["providers"]["codex"] = json!({"setupMode": "managed", "homePath": path_string(&shared)});
        let mut instances = serde_json::Map::new();
        if explicit_default {
            instances.insert(
                "codex".into(),
                json!({"driver": "codex", "config": {"setupMode": "managed", "homePath": path_string(&shared)}}),
            );
        }
        instances.insert(
            "codex-personal".into(),
            json!({
                "driver": "codex",
                "enabled": false,
                "config": {"setupMode": "managed", "homePath": path_string(&shared), "shadowHomePath": path_string(&setup.home.join("personal-shadow"))},
                "environment": [{"name": "CODEX_HOME", "value": path_string(&setup.home.join("ignored-environment")), "sensitive": false}],
            }),
        );
        settings["providerInstances"] = Value::Object(instances);
        let harness = harness(&setup, Options::new(&settings));
        let summary = harness.service.read_summary(window()).await.unwrap();
        assert_eq!(total_output(&summary), 40.0);
        assert_eq!(sources_for(&summary, "codex").iter().filter(|source| source["status"] == "ok").count(), 1);
    }
}

#[tokio::test]
async fn omits_cursor_without_a_saved_file_login() {
    let setup = setup();
    for platform in [Platform::Linux, Platform::Win32, Platform::Darwin] {
        let mut options = Options::new(&setup.settings);
        options.platform = platform;
        options.environment = vec![("AGENT_CLI_CREDENTIAL_STORE", "file".into())];
        let summary = harness(&setup, options).service.read_summary(window()).await.unwrap();
        assert!(sources_for(&summary, "cursor").is_empty());
    }
}

#[tokio::test]
async fn keeps_cursor_credential_errors_visible() {
    let setup = setup();
    let auth = setup.home.join("config").join("cursor").join("auth.json");
    std::fs::create_dir_all(auth.parent().unwrap()).unwrap();
    std::fs::write(&auth, "invalid json").unwrap();
    let summary = harness(&setup, Options::new(&setup.settings)).service.read_summary(window()).await.unwrap();
    assert_eq!(sources_for(&summary, "cursor")[0]["message"], "Cursor credentials could not be read.");
}

#[tokio::test]
async fn does_not_read_the_keychain_before_account_usage_is_enabled() {
    let setup = setup();
    let mut options = Options::new(&setup.settings);
    options.platform = Platform::Darwin;
    let summary = harness(&setup, options).service.read_summary(window()).await.unwrap();
    let cursor = sources_for(&summary, "cursor")[0];
    assert_eq!(cursor["status"], "missing");
    assert_eq!(cursor["action"], "enableCursorKeychain");
}

#[tokio::test]
async fn ignores_stale_file_logins_when_the_credential_store_differs() {
    let setup = setup();
    for (platform, environment, auth) in [
        (Platform::Darwin, ("AGENT_CLI_CREDENTIAL_STORE", "memory"), vec![".cursor", "auth.json"]),
        (Platform::Linux, ("AGENT_CLI_CREDENTIAL_STORE", "memory"), vec!["config", "cursor", "auth.json"]),
        (Platform::Linux, ("CURSOR_API_KEY", "different-account"), vec!["config", "cursor", "auth.json"]),
    ] {
        let path = auth.iter().fold(setup.home.clone(), |path, part| path.join(part));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, json!({"accessToken": "stale-token"}).to_string()).unwrap();
        let mut options = Options::new(&setup.settings);
        options.platform = platform;
        options.environment = vec![(environment.0, environment.1.into())];
        let summary = harness(&setup, options).service.read_summary(window()).await.unwrap();
        let cursor = sources_for(&summary, "cursor")[0];
        assert_eq!(cursor["status"], "missing");
        assert!(cursor["message"].as_str().unwrap().contains("Cursor CLI login"));
        assert!(!summary["buckets"].as_array().unwrap().iter().any(|bucket| bucket["provider"] == "cursor"));
    }
}

#[tokio::test]
async fn includes_opencode_history_without_a_cursor_substitute() {
    let setup = setup();
    let root = setup.home.join("opencode");
    let directory = root.join("storage").join("message").join("session-1");
    std::fs::create_dir_all(&directory).unwrap();
    let message = json!({
        "id": "msg_1", "sessionID": "session-1", "role": "assistant", "modelID": "example-model",
        "time": {"created": crate::time::date_parse("2026-08-01T10:00:00Z").unwrap()},
        "tokens": {"input": 10, "output": 5, "reasoning": 2, "cache": {"read": 20, "write": 3}},
    });
    std::fs::write(directory.join("msg_1.json"), message.to_string()).unwrap();
    let summary = harness(&setup, Options::new(&setup.settings)).service.read_summary(window()).await.unwrap();
    let bucket = &summary["buckets"][0];
    assert_eq!(bucket["provider"], "opencode");
    assert!(sources_for(&summary, "cursor").is_empty());
    assert_eq!(bucket["sourcePath"], realpath(&root).as_str());
    assert_eq!(bucket["totals"]["outputTokens"], 7);
    assert_eq!(sources_for(&summary, "opencode")[0]["distinctSessions"], 1);
}

#[tokio::test]
async fn counts_aliased_opencode_and_antigravity_directories_once() {
    let setup = setup();
    let home = &setup.home;
    let opencode = home.join("opencode-store");
    let opencode_alias = home.join("opencode-alias");
    let conversations = home.join("antigravity-conversations");
    let (antigravity_a, antigravity_b) = (home.join("antigravity-a"), home.join("antigravity-b"));
    for dir in [&opencode, &conversations, &antigravity_a, &antigravity_b] {
        std::fs::create_dir(dir).unwrap();
    }
    std::os::unix::fs::symlink(&opencode, &opencode_alias).unwrap();
    std::os::unix::fs::symlink(&conversations, antigravity_a.join("conversations")).unwrap();
    std::os::unix::fs::symlink(&conversations, antigravity_b.join("conversations")).unwrap();
    let mut options = Options::new(&setup.settings);
    options.environment = vec![
        ("OPENCODE_DATA_DIR", format!("{},{}", path_string(&opencode), path_string(&opencode_alias))),
        (
            "ANTIGRAVITY_DATA_DIR",
            format!("{},{}", path_string(&antigravity_a), path_string(&antigravity_b)),
        ),
    ];
    let summary = harness(&setup, options).service.read_summary(window()).await.unwrap();
    assert_eq!(sources_for(&summary, "opencode").len(), 1);
    assert_eq!(sources_for(&summary, "antigravity").len(), 1);
    assert_eq!(
        sources_for(&summary, "opencode")[0]["fingerprint"]["resolvedHomePath"],
        realpath(&opencode).as_str()
    );
    assert_eq!(
        sources_for(&summary, "antigravity")[0]["fingerprint"]["resolvedHomePath"],
        realpath(&conversations).as_str()
    );
}

#[tokio::test]
async fn reads_configured_and_disabled_accounts_once_across_shared_and_aliased_homes() {
    let setup = setup();
    let home = &setup.home;
    let codex_home = home.join("codex-account");
    let alias = home.join("codex-alias");
    let claude_home = home.join("claude-account");
    let grok_home = home.join("grok-account");
    std::fs::write(&setup.transcript, claude_line(1, 5, "claude-fable-5")).unwrap();
    std::fs::create_dir_all(claude_home.join("projects")).unwrap();
    std::fs::write(claude_home.join("projects").join("session.jsonl"), claude_line(2, 7, "claude-fable-5")).unwrap();
    std::fs::create_dir_all(codex_home.join("sessions")).unwrap();
    std::os::unix::fs::symlink(&codex_home, &alias).unwrap();
    // A-B-A at one timestamp must keep both equal A events.
    std::fs::write(
        codex_home.join("sessions").join("rollout.jsonl"),
        codex_rollout("codex-account-session", &[11, 12, 11]),
    )
    .unwrap();
    std::fs::create_dir_all(grok_home.join("sessions").join("session")).unwrap();
    let grok = json!({
        "timestamp": crate::time::date_parse("2026-08-01T10:00:00Z").unwrap() / 1000.0,
        "method": "_x.ai/session/update",
        "params": {"sessionId": "grok-account-session", "update": {"sessionUpdate": "turn_completed", "prompt_id": "prompt-1", "usage": {"inputTokens": 10, "outputTokens": 13}}},
    });
    std::fs::write(grok_home.join("sessions").join("session").join("updates.jsonl"), format!("{grok}\n")).unwrap();
    let mut settings = setup.settings.clone();
    settings["providerInstances"] = json!({
        "claude-work": {"driver": "claudeAgent", "enabled": false, "environment": [{"name": "CLAUDE_CONFIG_DIR", "value": path_string(&claude_home), "sensitive": false}]},
        "codex-work": {"driver": "codex", "environment": [{"name": "CODEX_HOME", "value": path_string(&codex_home), "sensitive": false}]},
        "codex-alias": {"driver": "codex", "config": {"homePath": path_string(&alias)}},
        "codex-shadow": {
            "driver": "codex",
            "config": {"homePath": path_string(&codex_home), "shadowHomePath": path_string(&home.join("shadow"))},
            "environment": [{"name": "CODEX_HOME", "value": path_string(&home.join("ignored")), "sensitive": false}],
        },
        "grok-work": {"driver": "grok", "environment": [{"name": "GROK_HOME", "value": path_string(&grok_home), "sensitive": false}]},
    });
    let harness = harness(&setup, Options::new(&settings));
    let summary = harness.service.read_summary(window()).await.unwrap();
    assert_eq!(total_output(&summary), 59.0);
    std::fs::rename(
        codex_home.join("sessions").join("rollout.jsonl"),
        codex_home.join("sessions").join("moved.jsonl"),
    )
    .unwrap();
    let moved = harness.service.read_summary(window()).await.unwrap();
    assert_eq!(moved["buckets"], summary["buckets"]);
    std::fs::remove_dir_all(codex_home.join("sessions")).unwrap();
    let removed = harness.service.read_summary(window()).await.unwrap();
    assert_eq!(removed["buckets"], summary["buckets"]);
    let ok: Vec<&Value> = summary["sources"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|source| source["status"] == "ok")
        .collect();
    assert_eq!(ok.len(), 4);
    assert_eq!(ok.iter().map(|source| source["scannedFiles"].as_u64().unwrap()).sum::<u64>(), 4);
    assert_eq!(ok.iter().filter(|source| source["fingerprint"]["provider"] == "codex").count(), 1);
}

#[tokio::test]
async fn explicit_account_settings_beat_environment_and_legacy_homes() {
    let setup = setup();
    let configured = setup.home.join("configured");
    let environment_home = setup.home.join("environment");
    std::fs::write(&setup.transcript, claude_line(1, 100, "claude-fable-5")).unwrap();
    for (index, root) in [&configured, &environment_home].iter().enumerate() {
        std::fs::create_dir_all(root.join("projects")).unwrap();
        std::fs::write(
            root.join("projects").join("session.jsonl"),
            claude_line(index as u32 + 2, index as u64 + 7, "claude-fable-5"),
        )
        .unwrap();
    }
    std::fs::create_dir_all(configured.join(".claude").join("projects")).unwrap();
    std::fs::write(
        configured.join(".claude").join("projects").join("wrong.jsonl"),
        claude_line(4, 1000, "claude-fable-5"),
    )
    .unwrap();
    let mut settings = setup.settings.clone();
    let environment_variable = json!([{"name": "CLAUDE_CONFIG_DIR", "value": path_string(&environment_home), "sensitive": false}]);
    settings["providerInstances"] =
        json!({"claudeAgent": {"driver": "claudeAgent", "config": {"homePath": path_string(&configured)}, "environment": environment_variable}});
    let mut options = Options::new(&settings);
    options.environment = vec![("CLAUDE_CONFIG_DIR", path_string(&setup.home.join("host-ignored")))];
    let harness = harness(&setup, options);
    let first = harness.service.read_summary(window()).await.unwrap();
    assert_eq!(total_output(&first), 7.0);
    let homes: Vec<&Value> = first["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|source| &source["fingerprint"]["resolvedHomePath"])
        .collect();
    assert!(homes.contains(&&json!(realpath(&configured.join("projects")))));
    harness.settings.set(
        &["providerInstances", "claudeAgent"],
        json!({"driver": "claudeAgent", "config": {"homePath": ""}, "environment": environment_variable}),
    );
    let second = harness.service.read_summary(window()).await.unwrap();
    assert_eq!(total_output(&second), 8.0);
    let homes: Vec<&Value> = second["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|source| &source["fingerprint"]["resolvedHomePath"])
        .collect();
    assert!(homes.contains(&&json!(realpath(&environment_home.join("projects")))));
}

#[tokio::test]
async fn inherited_home_variables_apply_to_explicit_defaults_without_homes() {
    let setup = setup();
    std::fs::write(&setup.transcript, claude_line(1, 5, "claude-fable-5")).unwrap();
    let mut settings = setup.settings.clone();
    settings["providerInstances"] = json!({"codex": {"driver": "codex", "config": {}}, "claudeAgent": {"driver": "claudeAgent", "config": {}}});
    let mut options = Options::new(&settings);
    options.environment = vec![
        ("CODEX_HOME", path_string(&setup.home.join("inherited-codex"))),
        ("CLAUDE_CONFIG_DIR", path_string(&setup.home.join("claude"))),
    ];
    let summary = harness(&setup, options).service.read_summary(window()).await.unwrap();
    assert_eq!(total_output(&summary), 5.0);
    assert_eq!(
        sources_for(&summary, "codex")[0]["fingerprint"]["resolvedHomePath"],
        path_string(&setup.home.join("inherited-codex").join("sessions")).as_str()
    );
    assert_eq!(
        sources_for(&summary, "grok")[0]["fingerprint"]["resolvedHomePath"],
        path_string(&setup.home.join("grok").join("sessions")).as_str()
    );
}

#[tokio::test]
async fn reprices_unchanged_transcripts_when_custom_prices_change() {
    let setup = setup();
    std::fs::write(&setup.transcript, claude_line(1, 5, "example-model")).unwrap();
    let harness = harness(&setup, Options::new(&setup.settings));
    let original = harness.service.read_summary(window()).await.unwrap();
    assert_eq!(original["buckets"][0]["costUsd"], 0);
    assert_eq!(original["buckets"][0]["unpricedRecords"], 1);
    harness.settings.set(
        &["usagePriceOverrides"],
        json!({"example-model": {"inputCostPerMillionTokens": 2, "outputCostPerMillionTokens": 8}}),
    );
    let overridden = harness.service.read_summary(window()).await.unwrap();
    assert!((overridden["buckets"][0]["costUsd"].as_f64().unwrap() - 0.00006).abs() < 1e-12);
    assert_eq!(overridden["buckets"][0]["costSource"], "modelPriced");
    assert_eq!(overridden["buckets"][0]["unpricedRecords"], 0);
    assert_eq!(overridden["buckets"][0]["totals"], original["buckets"][0]["totals"]);
    harness.settings.set(
        &["usagePriceOverrides"],
        json!({"example-model": {"inputCostPerMillionTokens": 4, "outputCostPerMillionTokens": 16}}),
    );
    let edited = harness.service.read_summary(window()).await.unwrap();
    assert!((edited["buckets"][0]["costUsd"].as_f64().unwrap() - 0.00012).abs() < 1e-12);
    harness.settings.set(&["usagePriceOverrides"], json!({}));
    let restored = harness.service.read_summary(window()).await.unwrap();
    assert_eq!(restored["buckets"], original["buckets"]);
}

#[tokio::test]
async fn counts_appended_usage_on_a_rescan() {
    let setup = setup();
    std::fs::write(&setup.transcript, claude_line(1, 5, "claude-fable-5")).unwrap();
    let harness = harness(&setup, Options::new(&setup.settings));
    assert_eq!(total_output(&harness.service.read_summary(window()).await.unwrap()), 5.0);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&setup.transcript)
        .unwrap()
        .write_all(claude_line(2, 7, "claude-fable-5").as_bytes())
        .unwrap();
    assert_eq!(total_output(&harness.service.read_summary(window()).await.unwrap()), 12.0);
}

use std::io::Write as _;

fn fable_rates() -> Value {
    json!({"claude-fable-5": {"input_cost_per_token": 1e-5, "output_cost_per_token": 5e-5}})
}

#[tokio::test]
async fn large_record_totals_stay_exact_through_append_dedupe_restart_and_cleanup() {
    let setup = setup();
    let padding = format!("\"padding\":{},\"message\":", json!("x".repeat(9 * 1024 * 1024)));
    let large = claude_line(1, 9900, "claude-fable-5").replace("\"message\":", &padding);
    std::fs::write(&setup.transcript, &large).unwrap();
    let mut options = Options::new(&setup.settings);
    options.rates = fable_rates();
    let first_harness = harness(&setup, options);
    let first = first_harness.service.read_summary(window()).await.unwrap();
    assert_eq!(total_output(&first), 9900.0);
    let cost: f64 = first["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|bucket| bucket["costUsd"].as_f64().unwrap())
        .sum();
    assert!((cost - 0.4951).abs() < 1e-12);
    let warm = first_harness.service.read_summary(window()).await.unwrap();
    assert_eq!(warm["buckets"], first["buckets"]);
    // The repeated content block has the same message/request identity.
    std::fs::OpenOptions::new()
        .append(true)
        .open(&setup.transcript)
        .unwrap()
        .write_all((large.clone() + &claude_line(2, 100, "claude-fable-5")).as_bytes())
        .unwrap();
    let appended = first_harness.service.read_summary(window()).await.unwrap();
    assert_eq!(total_output(&appended), 10_000.0);
    let uncached: f64 = appended["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|bucket| bucket["totals"]["uncachedInputTokens"].as_f64().unwrap())
        .sum();
    assert_eq!(uncached, 20.0);
    let restart = |state_dir: PathBuf| {
        let mut options = Options::new(&setup.settings);
        options.rates = fable_rates();
        options.state_dir = Some(state_dir);
        harness(&setup, options)
    };
    let restored = restart(first_harness.state_dir.clone()).service.read_summary(window()).await.unwrap();
    assert_eq!(restored["buckets"], appended["buckets"]);
    std::fs::remove_file(&setup.transcript).unwrap();
    let after_cleanup = restart(first_harness.state_dir.clone()).service.read_summary(window()).await.unwrap();
    assert_eq!(after_cleanup["buckets"], appended["buckets"]);
}

#[tokio::test]
async fn preserves_saved_usage_after_transcript_cleanup_and_restart() {
    let setup = setup();
    let alias = setup.home.join("claude-alias");
    std::os::unix::fs::symlink(setup.home.join("claude"), &alias).unwrap();
    let content = claude_line(1, 5, "claude-fable-5");
    std::fs::write(&setup.transcript, &content).unwrap();
    let mut settings = setup.settings.clone();
    settings["providers"]["claudeAgent"] = json!({"homePath": path_string(&alias)});
    let make = |state_dir: Option<PathBuf>| {
        let mut options = Options::new(&settings);
        options.rates = fable_rates();
        options.state_dir = state_dir;
        harness(&setup, options)
    };
    let first_harness = make(None);
    let first = first_harness.service.read_summary(window()).await.unwrap();
    assert_eq!(total_output(&first), 5.0);
    assert!(first["buckets"][0]["costUsd"].as_f64().unwrap() > 0.0);

    std::fs::remove_file(&setup.transcript).unwrap();
    let deleted = first_harness.service.read_summary(window()).await.unwrap();
    assert_eq!(deleted["buckets"], first["buckets"]);
    assert_eq!(deleted["sources"], first["sources"]);

    let restarted = make(Some(first_harness.state_dir.clone()));
    let restored = restarted.service.read_summary(window()).await.unwrap();
    assert_eq!(restored["buckets"], first["buckets"]);
    assert_eq!(restored["sources"], first["sources"]);

    // A moved transcript must not count the saved usage twice.
    std::fs::write(setup.transcript.with_extension("jsonl.jsonl"), &content).unwrap();
    let moved = restarted.service.read_summary(window()).await.unwrap();
    assert_eq!(moved["buckets"], first["buckets"]);
    assert_eq!(moved["sources"][0]["distinctSessions"], 1);

    let replacement = setup.home.join("replacement-projects");
    std::fs::create_dir(&replacement).unwrap();
    std::fs::remove_dir_all(setup.home.join("claude").join("projects")).unwrap();
    let after_root_cleanup = make(Some(first_harness.state_dir.clone()));
    let missing_root = after_root_cleanup.service.read_summary(window()).await.unwrap();
    assert_eq!(missing_root["buckets"], first["buckets"]);
    assert_eq!(missing_root["sources"][0]["distinctSessions"], 1);
    assert_eq!(missing_root["sources"][0]["status"], "ok");
    assert_eq!(missing_root["sources"][0]["fingerprint"], first["sources"][0]["fingerprint"]);

    let projects = setup.home.join("claude").join("projects");
    std::fs::rename(&replacement, &projects).unwrap();
    std::fs::write(projects.join("new.jsonl"), claude_line(2, 7, "claude-fable-5")).unwrap();
    let recreated = after_root_cleanup.service.read_summary(window()).await.unwrap();
    assert_eq!(total_output(&recreated), 12.0);
    assert_eq!(recreated["sources"][0]["fingerprint"], first["sources"][0]["fingerprint"]);

    let mut later = window();
    later.since_day = "2026-08-02".into();
    let outside = restarted.service.read_summary(later).await.unwrap();
    assert_eq!(outside["buckets"], json!([]));
    assert_eq!(outside["sources"][0]["distinctSessions"], 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn custom_price_changes_do_not_share_an_in_flight_scan() {
    let setup = setup();
    std::fs::write(&setup.transcript, claude_line(1, 5, "example-model")).unwrap();
    let (release, gate) = tokio::sync::watch::channel(false);
    let mut options = Options::new(&setup.settings);
    options.gate = Some(gate);
    let harness = harness(&setup, options);
    let first = tokio::spawn({
        let service = harness.service.clone();
        async move { service.read_summary(window()).await }
    });
    while harness.rates.fetches.load(AtomicOrdering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    harness.settings.set(
        &["usagePriceOverrides"],
        json!({"example-model": {"inputCostPerMillionTokens": 2, "outputCostPerMillionTokens": 8}}),
    );
    let second = tokio::spawn({
        let service = harness.service.clone();
        async move { service.read_summary(window()).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    release.send(true).unwrap();
    let original = first.await.unwrap().unwrap();
    let updated = second.await.unwrap().unwrap();
    assert_eq!(original["buckets"][0]["costUsd"], 0);
    assert!((updated["buckets"][0]["costUsd"].as_f64().unwrap() - 0.00006).abs() < 1e-12);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_identical_requests_share_one_scan() {
    let setup = setup();
    std::fs::write(&setup.transcript, claude_line(1, 5, "claude-fable-5")).unwrap();
    let harness = harness(&setup, Options::new(&setup.settings));
    let (first, second) = tokio::join!(harness.service.read_summary(window()), harness.service.read_summary(window()));
    assert_eq!(first.unwrap(), second.unwrap());
    assert_eq!(harness.rates.fetches.load(AtomicOrdering::SeqCst), 1);
    // A later request is fresh work again, not a stale cached answer.
    harness.service.read_summary(window()).await.unwrap();
    assert_eq!(harness.rates.fetches.load(AtomicOrdering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn refetches_rates_inside_the_ttl_only_when_asked() {
    let setup = setup();
    std::fs::write(&setup.transcript, claude_line(1, 5, "claude-fable-5")).unwrap();
    let mut options = Options::new(&setup.settings);
    options.rates = fable_rates();
    let harness = harness(&setup, options);
    let first = harness.service.read_summary(window()).await.unwrap();
    assert_eq!(harness.rates.fetches.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(first["pricing"]["status"], "fresh");
    // Inside the daily TTL a plain rescan keeps the table.
    harness.clock.fetch_add(2 * 60 * 1000, AtomicOrdering::SeqCst);
    harness.service.read_summary(window()).await.unwrap();
    assert_eq!(harness.rates.fetches.load(AtomicOrdering::SeqCst), 1);
    // An explicit refresh fetches again; a burst of refreshes shares that fetch.
    let (refreshed, _) = tokio::join!(harness.service.refresh_rates(), harness.service.refresh_rates());
    assert_eq!(harness.rates.fetches.load(AtomicOrdering::SeqCst), 2);
    assert_eq!(refreshed["status"], "fresh");
    assert_eq!(refreshed["knownModels"], 1);
    // The snapshot on disk keeps the TS format.
    let saved = crate::json::parse(&std::fs::read(harness.state_dir.join("usage-model-rates.json")).unwrap()).unwrap();
    assert!(saved.get("fetchedAtMs").and_then(crate::json::J::as_num).is_some());
    assert!(saved.get("document").and_then(|document| document.get("claude-fable-5")).is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_abandoned_first_caller_does_not_orphan_the_scan() {
    let setup = setup();
    std::fs::write(&setup.transcript, claude_line(1, 5, "claude-fable-5")).unwrap();
    let (release, gate) = tokio::sync::watch::channel(false);
    let mut options = Options::new(&setup.settings);
    options.gate = Some(gate);
    let harness = harness(&setup, options);
    let first = tokio::spawn({
        let service = harness.service.clone();
        async move { service.read_summary(window()).await }
    });
    while harness.rates.fetches.load(AtomicOrdering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    first.abort();
    let second = tokio::spawn({
        let service = harness.service.clone();
        async move { service.read_summary(window()).await }
    });
    release.send(true).unwrap();
    let summary = tokio::time::timeout(Duration::from_secs(10), second)
        .await
        .expect("not orphaned")
        .unwrap()
        .unwrap();
    assert_eq!(total_output(&summary), 5.0);
    assert_eq!(harness.rates.fetches.load(AtomicOrdering::SeqCst), 1);
}

#[tokio::test]
async fn invalid_windows() {
    let setup = setup();
    let harness = harness(&setup, Options::new(&setup.settings));
    let read = |input: Value| {
        let service = harness.service.clone();
        async move { service.read_summary(serde_json::from_value(input).unwrap()).await.unwrap_err() }
    };
    let backwards = read(json!({"timeZone": "UTC", "sinceDay": "2026-09-01", "untilDay": "2026-08-01"})).await;
    assert_eq!(backwards.reason, UsageReadErrorReason::InvalidWindow);
    assert_eq!(backwards.detail, "sinceDay '2026-09-01' is after untilDay '2026-08-01'");
    let hourly = read(json!({"timeZone": "UTC", "sinceDay": "2026-08-01", "untilDay": "2026-08-02", "resolution": "hour"})).await;
    assert_eq!(hourly.detail, "Hourly usage requires valid sinceTime and untilTime instants");
    let long = read(json!({
        "timeZone": "UTC", "sinceDay": "2026-08-01", "untilDay": "2026-08-02", "resolution": "hour",
        "sinceTime": "2026-08-01T00:00:00.000Z", "untilTime": "2026-08-02T00:00:00.001Z",
    }))
    .await;
    assert_eq!(long.detail, "Hourly usage window must be greater than zero and at most 24 hours");
    let encoded = serde_json::to_value(&long).unwrap();
    assert_eq!(
        encoded,
        json!({"_tag": "UsageReadError", "reason": "invalidWindow", "detail": "Hourly usage window must be greater than zero and at most 24 hours"})
    );
}
