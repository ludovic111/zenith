//! Port of `apps/server/src/serverSettings.test.ts`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use zc_contracts::ServerSettingsOperation;
use zc_core::Defect;
use zc_settings::settings::logic::{
    create_model_selection, redact_server_settings_for_client, resolve_provider_instance_enabled, resolve_source_control_writer_model_selection,
    SECRET_REDACTED,
};
use zc_settings::settings::schema::{decode_patch, decode_settings, default_settings};
use zc_settings::settings::{SecretBackend, ServerSettingsService};

struct Fixture {
    _dir: tempfile::TempDir,
    settings_path: PathBuf,
    secrets: zc_core::ServerSecretStore,
    db: zc_db::Db,
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("userdata");
        std::fs::create_dir_all(&state_dir).unwrap();
        let secrets = zc_core::ServerSecretStore::open(state_dir.join("secrets")).await.unwrap();
        Self {
            settings_path: state_dir.join("settings.json"),
            _dir: dir,
            secrets,
            db: zc_db::Db::open_in_memory().unwrap(),
        }
    }

    fn service(&self) -> ServerSettingsService {
        self.service_with(Arc::new(self.secrets.clone()))
    }

    fn service_with(&self, secrets: Arc<dyn SecretBackend>) -> ServerSettingsService {
        ServerSettingsService::new(&self.settings_path, secrets, Arc::new(self.db.clone()))
    }

    fn write(&self, contents: &str) {
        std::fs::write(&self.settings_path, contents).unwrap();
    }

    fn read(&self) -> String {
        std::fs::read_to_string(&self.settings_path).unwrap()
    }

    fn persisted(&self) -> Value {
        serde_json::from_str(&self.read()).unwrap()
    }

    async fn sql(&self, sql: &'static str, params: Vec<Option<String>>) {
        self.db
            .call(move |conn| {
                conn.execute(sql, rusqlite::params_from_iter(params.iter()))
                    .map(|_| ())
                    .map_err(|error| zc_db::DbError::sql("test", error))
            })
            .await
            .unwrap();
    }

    /// `recordProviderUsage(provider, instanceId)`.
    async fn record_provider_usage(&self, provider: &str, instance_id: Option<&str>) {
        self.sql(
            "INSERT INTO projection_thread_sessions (thread_id, status, provider_name, provider_instance_id, updated_at) \
             VALUES (?1, 'ready', ?2, ?3, '2026-08-25T00:00:00.000Z')",
            vec![
                Some(format!("thread-{}", instance_id.unwrap_or(provider))),
                Some(provider.to_owned()),
                instance_id.map(str::to_owned),
            ],
        )
        .await;
    }
}

async fn update(service: &ServerSettingsService, patch: Value) -> Value {
    service.update_settings_value(&patch).await.expect("update succeeds")
}

async fn get(service: &ServerSettingsService) -> Value {
    service.get_settings_value().await.expect("settings load")
}

fn selection(instance_id: &str, model: &str, options: Option<Value>) -> Value {
    create_model_selection(&json!(instance_id), &json!(model), options.as_ref())
}

type FailGet = Box<dyn Fn(&str, Option<&[u8]>) -> bool + Send>;
type FailSet = Box<dyn Fn(&str, &[u8]) -> bool + Send>;

/// A secret store with failure switches, kept in memory.
#[derive(Default)]
struct MemorySecrets {
    values: Mutex<HashMap<String, Vec<u8>>>,
    fail_get: Mutex<Option<FailGet>>,
    fail_set: Mutex<Option<FailSet>>,
}

#[async_trait]
impl SecretBackend for MemorySecrets {
    async fn get(&self, name: &str) -> Result<Option<Vec<u8>>, Defect> {
        let value = self.values.lock().unwrap().get(name).cloned();
        if let Some(fail) = self.fail_get.lock().unwrap().as_ref() {
            if fail(name, value.as_deref()) {
                return Err(Defect::error("SecretStoreReadError", "Forced read failure."));
            }
        }
        Ok(value)
    }

    async fn set(&self, name: &str, value: &[u8]) -> Result<(), Defect> {
        // Like a store that renames before its chmod fails, the value lands before the error.
        self.values.lock().unwrap().insert(name.to_owned(), value.to_vec());
        if let Some(fail) = self.fail_set.lock().unwrap().as_ref() {
            if fail(name, value) {
                return Err(Defect::error("SecretStorePersistError", "Forced persist failure."));
            }
        }
        Ok(())
    }

    async fn remove(&self, name: &str) -> Result<(), Defect> {
        self.values.lock().unwrap().remove(name);
        Ok(())
    }
}

#[tokio::test]
async fn preserves_context_when_reading_a_provider_environment_secret_fails() {
    let fixture = Fixture::new().await;
    let secrets = Arc::new(MemorySecrets::default());
    *secrets.fail_get.lock().unwrap() = Some(Box::new(|_, _| true));
    let service = fixture.service_with(secrets);
    fixture.write(r#"{"providerInstances":{"codex_personal":{"driver":"codex","environment":[{"name":"OPENROUTER_API_KEY","value":"","sensitive":true,"valueRedacted":true}],"config":{}}}}"#);
    let error = service.get_settings_value().await.unwrap_err();
    assert_eq!(error.operation, ServerSettingsOperation::ReadSecret);
    assert_eq!(error.provider_instance_id.as_deref(), Some("codex_personal"));
    assert_eq!(error.environment_variable.as_deref(), Some("OPENROUTER_API_KEY"));
    assert_eq!(error.cause["name"], json!("SecretStoreReadError"));
    let message = zc_settings::errors::settings_error_message(&error);
    assert!(message.starts_with("Server settings read-secret failed for provider codex_personal and environment variable OPENROUTER_API_KEY at "));
    assert!(!message.contains("Forced read failure"));
}

#[tokio::test]
async fn identifies_provider_history_query_failures() {
    let fixture = Fixture::new().await;
    fixture.sql("DROP TABLE projection_thread_sessions", Vec::new()).await;
    let error = fixture.service().get_settings_value().await.unwrap_err();
    assert_eq!(error.operation, ServerSettingsOperation::ReadProviderHistory);
    assert_eq!(error.settings_path, fixture.settings_path.to_string_lossy());
}

#[test]
fn decodes_nested_settings_patches() {
    assert_eq!(
        decode_patch(&json!({"providers": {"codex": {"binaryPath": "/tmp/codex"}}})).unwrap(),
        json!({"providers": {"codex": {"binaryPath": "/tmp/codex"}}})
    );
    assert_eq!(
        decode_patch(&json!({"textGenerationModelSelection": {"options": [{"id": "fastMode", "value": false}]}})).unwrap(),
        json!({"textGenerationModelSelection": {"options": [{"id": "fastMode", "value": false}]}})
    );
}

#[test]
fn decodes_legacy_object_shaped_text_generation_options_from_settings_json() {
    let decoded = decode_settings(&json!({"textGenerationModelSelection": {
        "provider": "codex", "model": "gpt-5.4-mini", "options": {"reasoningEffort": "low"}
    }}))
    .unwrap();
    assert_eq!(
        decoded["textGenerationModelSelection"],
        json!({"instanceId": "codex", "model": "gpt-5.4-mini", "options": [{"id": "reasoningEffort", "value": "low"}]})
    );
}

#[tokio::test]
async fn deep_merges_nested_settings_updates_without_dropping_siblings() {
    let fixture = Fixture::new().await;
    let service = fixture.service();
    let model = default_settings()["textGenerationModelSelection"]["model"].clone();
    update(
        &service,
        json!({
            "providers": {
                "codex": {"binaryPath": "/usr/local/bin/codex", "homePath": "/Users/example/.codex"},
                "claudeAgent": {"binaryPath": "/usr/local/bin/claude", "customModels": ["claude-custom"]}
            },
            "textGenerationModelSelection": {"instanceId": "codex", "model": model, "options": [
                {"id": "reasoningEffort", "value": "high"}, {"id": "fastMode", "value": true}
            ]}
        }),
    )
    .await;
    let next = update(
        &service,
        json!({
            "providers": {"codex": {"binaryPath": "/opt/homebrew/bin/codex"}},
            "textGenerationModelSelection": {"options": [{"id": "fastMode", "value": false}]}
        }),
    )
    .await;
    assert_eq!(
        next["providers"]["codex"],
        json!({"enabled": true, "binaryPath": "/opt/homebrew/bin/codex", "homePath": "/Users/example/.codex", "shadowHomePath": "", "launchArgs": "", "customModels": []})
    );
    assert_eq!(
        next["providers"]["claudeAgent"],
        json!({"enabled": true, "binaryPath": "/usr/local/bin/claude", "homePath": "", "customModels": ["claude-custom"], "launchArgs": "", "autoCompactWindow": ""})
    );
    assert_eq!(
        next["textGenerationModelSelection"],
        json!({"instanceId": "codex", "model": model, "options": [
            {"id": "reasoningEffort", "value": "high"}, {"id": "fastMode", "value": false}
        ]})
    );
}

#[tokio::test]
async fn buffers_changes_after_a_subscription_is_acquired_but_before_it_is_consumed() {
    let fixture = Fixture::new().await;
    let service = fixture.service();
    let mut changes = service.subscribe_changes_value();
    update(&service, json!({"providers": {"codex": {"binaryPath": "/usr/local/bin/codex-next"}}})).await;
    let first = tokio::time::timeout(Duration::from_secs(1), changes.next()).await.unwrap().unwrap();
    assert_eq!(first["providers"]["codex"]["binaryPath"], json!("/usr/local/bin/codex-next"));
}

#[tokio::test]
async fn persists_custom_usage_prices_and_removes_them_from_the_settings_file() {
    let fixture = Fixture::new().await;
    let service = fixture.service();
    let prices = json!({"inputCostPerMillionTokens": 2, "outputCostPerMillionTokens": 8, "cacheReadCostPerMillionTokens": 0});
    update(&service, json!({"usagePriceOverrides": {"example-model": prices}})).await;
    let persisted = decode_settings(&fixture.persisted()).unwrap();
    assert_eq!(persisted["usagePriceOverrides"], json!({"example-model": prices}));
    update(&service, json!({"usagePriceOverrides": {"example-model": null}})).await;
    let restored = decode_settings(&fixture.persisted()).unwrap();
    assert_eq!(restored["usagePriceOverrides"], json!({}));
}

#[tokio::test]
async fn persists_and_broadcasts_thread_settlement_settings() {
    let fixture = Fixture::new().await;
    let service = fixture.service();
    let mut changes = service.subscribe_changes_value();
    let next = update(&service, json!({"sidebarAutoSettleAfterDays": null, "sidebarAutoSettleOnMerge": false})).await;
    let change = changes.next().await.unwrap();
    let persisted = fixture.persisted();
    assert_eq!(next["sidebarAutoSettleAfterDays"], Value::Null);
    assert_eq!(next["sidebarAutoSettleOnMerge"], json!(false));
    assert_eq!(change["sidebarAutoSettleAfterDays"], Value::Null);
    assert_eq!(change["sidebarAutoSettleOnMerge"], json!(false));
    assert_eq!(persisted["sidebarAutoSettleAfterDays"], Value::Null);
    assert_eq!(persisted["sidebarAutoSettleOnMerge"], json!(false));
}

#[tokio::test]
async fn preserves_model_when_switching_providers_via_text_generation_model_selection() {
    let fixture = Fixture::new().await;
    let service = fixture.service();
    update(
        &service,
        json!({"textGenerationModelSelection": selection("claudeAgent", "claude-sonnet-4-6", Some(json!([{"id": "effort", "value": "high"}])))}),
    )
    .await;
    let next = update(
        &service,
        json!({"textGenerationModelSelection": selection("codex", "gpt-5.4", Some(json!([{"id": "reasoningEffort", "value": "high"}])))}),
    )
    .await;
    assert_eq!(
        next["textGenerationModelSelection"],
        selection("codex", "gpt-5.4", Some(json!([{"id": "reasoningEffort", "value": "high"}])))
    );
}

#[tokio::test]
async fn preserves_custom_provider_instance_text_generation_selections() {
    let fixture = Fixture::new().await;
    let next = update(
        &fixture.service(),
        json!({
            "providerInstances": {"claude_openrouter": {"driver": "claudeAgent", "enabled": true, "config": {"customModels": ["openai/gpt-5.5"]}}},
            "textGenerationModelSelection": {"instanceId": "claude_openrouter", "model": "openai/gpt-5.5"}
        }),
    )
    .await;
    assert_eq!(
        next["textGenerationModelSelection"],
        json!({"instanceId": "claude_openrouter", "model": "openai/gpt-5.5"})
    );
}

#[tokio::test]
async fn uses_explicit_provider_instance_enabled_state_over_legacy_provider_enabled_state() {
    let fixture = Fixture::new().await;
    let next = update(
        &fixture.service(),
        json!({
            "providers": {"claudeAgent": {"enabled": false}},
            "providerInstances": {"claude_openrouter": {"driver": "claudeAgent", "enabled": true, "config": {"customModels": ["openai/gpt-5.5"]}}},
            "textGenerationModelSelection": {"instanceId": "claude_openrouter", "model": "openai/gpt-5.5"}
        }),
    )
    .await;
    assert_eq!(
        next["textGenerationModelSelection"],
        json!({"instanceId": "claude_openrouter", "model": "openai/gpt-5.5"})
    );
}

#[tokio::test]
async fn preserves_enabled_text_generation_selections_for_non_built_in_drivers() {
    let fixture = Fixture::new().await;
    let next = update(
        &fixture.service(),
        json!({
            "providerInstances": {"openrouter_text": {"driver": "openrouter", "enabled": true, "config": {"customModels": ["openai/gpt-5.5"]}}},
            "textGenerationModelSelection": {"instanceId": "openrouter_text", "model": "openai/gpt-5.5"}
        }),
    )
    .await;
    assert_eq!(
        next["textGenerationModelSelection"],
        json!({"instanceId": "openrouter_text", "model": "openai/gpt-5.5"})
    );
}

#[tokio::test]
async fn preserves_the_source_control_writer_selection_when_its_provider_instance_is_disabled() {
    let fixture = Fixture::new().await;
    let service = fixture.service();
    let writer = json!({"instanceId": "codex_writer", "model": "gpt-5.4-mini"});
    update(
        &service,
        json!({
            "providerInstances": {"codex_writer": {"driver": "codex", "enabled": true, "config": {}}},
            "sourceControlWriterModelSelection": writer
        }),
    )
    .await;
    let next = update(
        &service,
        json!({"providerInstances": {"codex_writer": {"driver": "codex", "enabled": false, "config": {}}}}),
    )
    .await;
    assert_eq!(next["sourceControlWriterModelSelection"], writer);
    assert_eq!(resolve_source_control_writer_model_selection(&next, None), next["textGenerationModelSelection"]);
    assert_eq!(get(&service).await["sourceControlWriterModelSelection"], writer);
    assert_eq!(fixture.persisted()["sourceControlWriterModelSelection"], writer);
    let restored = update(
        &service,
        json!({"providerInstances": {"codex_writer": {"driver": "codex", "enabled": true, "config": {}}}}),
    )
    .await;
    assert_eq!(resolve_source_control_writer_model_selection(&restored, None), writer);
}

#[tokio::test]
async fn drops_stale_text_generation_options_when_resetting_model_selection() {
    let fixture = Fixture::new().await;
    let service = fixture.service();
    let defaults = default_settings();
    let model = defaults["textGenerationModelSelection"]["model"].clone();
    let instance = defaults["textGenerationModelSelection"]["instanceId"].clone();
    update(
        &service,
        json!({"textGenerationModelSelection": {"instanceId": instance, "model": model, "options": [
            {"id": "reasoningEffort", "value": "high"}, {"id": "fastMode", "value": true}
        ]}}),
    )
    .await;
    let next = update(&service, json!({"textGenerationModelSelection": {"instanceId": instance, "model": model}})).await;
    assert_eq!(next["textGenerationModelSelection"], json!({"instanceId": instance, "model": model}));
}

#[tokio::test]
async fn replaces_provider_instance_maps_when_clearing_optional_fields() {
    let fixture = Fixture::new().await;
    let service = fixture.service();
    update(
        &service,
        json!({"providerInstances": {"codex": {"driver": "codex", "displayName": "Codex Work", "accentColor": "#7c3aed", "enabled": true, "config": {"homePath": "~/.codex"}}}}),
    )
    .await;
    let next = update(
        &service,
        json!({"providerInstances": {"codex": {"driver": "codex", "displayName": "Codex Work", "enabled": true, "config": {"homePath": "~/.codex"}}}}),
    )
    .await;
    assert_eq!(
        next["providerInstances"]["codex"],
        json!({"driver": "codex", "displayName": "Codex Work", "enabled": true, "config": {"homePath": "~/.codex"}})
    );
}

#[tokio::test]
async fn enables_previously_used_providers_from_sparse_settings_files() {
    let fixture = Fixture::new().await;
    fixture.write(r#"{"providers":{"opencode":{"serverUrl":"http://127.0.0.1:4096"}}}"#);
    fixture.record_provider_usage("opencode", Some("opencode")).await;
    let settings = get(&fixture.service()).await;
    assert_eq!(settings["providers"]["grok"]["enabled"], json!(false));
    assert_eq!(settings["providers"]["opencode"]["enabled"], json!(true));
    assert_eq!(settings["providers"]["cursor"]["enabled"], json!(false));
    assert_eq!(settings["providers"]["opencode"]["serverUrl"], json!("http://127.0.0.1:4096"));
}

#[tokio::test]
async fn preserves_existing_provider_instances_without_explicit_enabled_flags() {
    let fixture = Fixture::new().await;
    fixture.write(r#"{"providerInstances":{"cursor_work":{"driver":"cursor","config":{}},"grok":{"driver":"grok","config":{}},"opencode_work":{"driver":"opencode","config":{"serverUrl":"http://127.0.0.1:4096"}},"opencode_unused":{"driver":"opencode","config":{}}}}"#);
    fixture.record_provider_usage("cursor", Some("cursor_work")).await;
    fixture.record_provider_usage("grok", None).await;
    fixture.record_provider_usage("opencode", Some("opencode_work")).await;
    let settings = get(&fixture.service()).await;
    assert_eq!(settings["providers"]["cursor"]["enabled"], json!(true));
    for id in ["cursor_work", "grok", "opencode_work"] {
        assert_eq!(settings["providerInstances"][id]["enabled"], json!(true), "{id}");
    }
    assert!(!resolve_provider_instance_enabled(&settings["providerInstances"]["opencode_unused"]));
}

#[tokio::test]
async fn preserves_explicit_provider_disables_in_existing_settings_files() {
    let fixture = Fixture::new().await;
    fixture.write(r#"{"providers":{"grok":{"enabled":false},"opencode":{"enabled":false},"cursor":{"enabled":false}},"providerInstances":{"grok":{"driver":"grok","enabled":false,"config":{}},"opencode":{"driver":"opencode","config":{"enabled":false}},"cursor":{"driver":"cursor","enabled":false,"config":{}}}}"#);
    for provider in ["grok", "opencode", "cursor"] {
        fixture.record_provider_usage(provider, Some(provider)).await;
    }
    let settings = get(&fixture.service()).await;
    for provider in ["grok", "opencode", "cursor"] {
        assert_eq!(settings["providers"][provider]["enabled"], json!(false), "{provider}");
        assert_eq!(settings["providerInstances"][provider]["enabled"], json!(false), "{provider}");
    }
}

#[tokio::test]
async fn skips_a_disabled_provider_instance_when_picking_the_text_generation_fallback() {
    let fixture = Fixture::new().await;
    fixture.write(r#"{"providerInstances":{"codex":{"driver":"codex","enabled":false,"config":{}}}}"#);
    let settings = get(&fixture.service()).await;
    assert_eq!(settings["textGenerationModelSelection"]["instanceId"], json!("claudeAgent"));
    assert_eq!(settings["textGenerationModelSelection"]["model"], json!("claude-haiku-4-5"));
}

#[tokio::test]
async fn keeps_unused_providers_disabled_in_existing_sparse_settings_files() {
    let fixture = Fixture::new().await;
    fixture.write("{}");
    let settings = get(&fixture.service()).await;
    for provider in ["grok", "opencode", "cursor"] {
        assert_eq!(settings["providers"][provider]["enabled"], json!(false));
    }
}

#[tokio::test]
async fn preserves_provider_history_when_no_settings_file_exists() {
    let fixture = Fixture::new().await;
    fixture.record_provider_usage("grok", Some("grok")).await;
    let settings = get(&fixture.service()).await;
    assert_eq!(settings["providers"]["grok"]["enabled"], json!(true));
    assert_eq!(settings["providers"]["opencode"]["enabled"], json!(false));
    assert_eq!(settings["providers"]["cursor"]["enabled"], json!(false));
    assert!(!fixture.settings_path.exists(), "loading alone writes nothing");
}

#[tokio::test]
async fn preserves_provider_history_when_the_settings_file_is_invalid() {
    let fixture = Fixture::new().await;
    fixture.write("{invalid json");
    fixture.record_provider_usage("cursor", Some("cursor")).await;
    let settings = get(&fixture.service()).await;
    assert_eq!(settings["providers"]["cursor"]["enabled"], json!(true));
    assert_eq!(settings["providers"]["grok"]["enabled"], json!(false));
    assert_eq!(settings["providers"]["opencode"]["enabled"], json!(false));
}

#[tokio::test]
async fn preserves_valid_provider_flags_when_another_settings_field_is_invalid() {
    let fixture = Fixture::new().await;
    fixture.write(r#"{"addProjectBaseDirectory":42,"providers":{"cursor":{"enabled":false},"grok":{"enabled":true}}}"#);
    fixture.record_provider_usage("cursor", Some("cursor")).await;
    let settings = get(&fixture.service()).await;
    assert_eq!(settings["providers"]["cursor"]["enabled"], json!(false));
    assert_eq!(settings["providers"]["grok"]["enabled"], json!(true));
    assert_eq!(settings["providers"]["opencode"]["enabled"], json!(false));
}

#[tokio::test]
async fn restores_providers_from_persisted_runtime_sessions() {
    let fixture = Fixture::new().await;
    fixture
        .sql(
            "INSERT INTO provider_session_runtime (thread_id, provider_name, provider_instance_id, adapter_key, status, last_seen_at) \
             VALUES ('thread-opencode-runtime', 'opencode', 'opencode', 'opencode', 'ready', '2026-08-25T00:00:00.000Z')",
            Vec::new(),
        )
        .await;
    let settings = get(&fixture.service()).await;
    assert_eq!(settings["providers"]["grok"]["enabled"], json!(false));
    assert_eq!(settings["providers"]["opencode"]["enabled"], json!(true));
    assert_eq!(settings["providers"]["cursor"]["enabled"], json!(false));
}

#[tokio::test]
async fn persists_explicit_disables_after_a_provider_has_been_used() {
    let fixture = Fixture::new().await;
    let service = fixture.service();
    fixture.record_provider_usage("grok", Some("grok")).await;
    assert_eq!(get(&service).await["providers"]["grok"]["enabled"], json!(true));
    let settings = update(&service, json!({"providers": {"grok": {"enabled": false}}})).await;
    assert_eq!(settings["providers"]["grok"]["enabled"], json!(false));
    assert_eq!(fixture.persisted()["providers"]["grok"]["enabled"], json!(false));
}

#[tokio::test]
async fn persists_explicit_provider_enables_before_their_first_use() {
    let fixture = Fixture::new().await;
    let service = fixture.service();
    update(
        &service,
        json!({"providers": {"cursor": {"enabled": true}, "grok": {"enabled": true}, "opencode": {"enabled": true}}}),
    )
    .await;
    update(&service, json!({"addProjectBaseDirectory": "~/Development"})).await;
    let persisted = fixture.persisted();
    for provider in ["cursor", "grok", "opencode"] {
        assert_eq!(persisted["providers"][provider]["enabled"], json!(true));
    }
}

#[tokio::test]
async fn keeps_optional_providers_disabled_after_a_new_installation_writes_settings() {
    let fixture = Fixture::new().await;
    let service = fixture.service();
    let initial = get(&service).await;
    for provider in ["grok", "opencode", "cursor"] {
        assert_eq!(initial["providers"][provider]["enabled"], json!(false));
    }
    let next = update(
        &service,
        json!({"addProjectBaseDirectory": "~/Development", "providerInstances": {"grok": {"driver": "grok", "config": {}}}}),
    )
    .await;
    for provider in ["grok", "opencode", "cursor"] {
        assert_eq!(next["providers"][provider]["enabled"], json!(false));
    }
    assert!(!resolve_provider_instance_enabled(&next["providerInstances"]["grok"]));
    let persisted = fixture.persisted();
    for provider in ["cursor", "grok", "opencode"] {
        assert_eq!(persisted["providers"][provider]["enabled"], json!(false));
    }
    assert!(persisted["providerInstances"]["grok"].get("enabled").is_none());
}

#[tokio::test]
async fn folds_a_legacy_in_config_enabled_flag_into_the_envelope_on_load() {
    let fixture = Fixture::new().await;
    fixture.write(r#"{"providerInstances":{"grok":{"driver":"grok","enabled":true,"config":{"enabled":false}},"codex_work":{"driver":"codex","config":{"enabled":true,"homePath":"~/.codex"}},"cursor":{"driver":"cursor","config":{"enabled":"nope"}}}}"#);
    let settings = get(&fixture.service()).await;
    assert_eq!(settings["providerInstances"]["grok"], json!({"driver": "grok", "enabled": false, "config": {}}));
    assert_eq!(
        settings["providerInstances"]["codex_work"],
        json!({"driver": "codex", "enabled": true, "config": {"homePath": "~/.codex"}})
    );
    assert_eq!(
        settings["providerInstances"]["cursor"],
        json!({"driver": "cursor", "config": {"enabled": "nope"}})
    );
}

#[tokio::test]
async fn folds_in_config_enabled_flags_arriving_through_updates() {
    let fixture = Fixture::new().await;
    let next = update(
        &fixture.service(),
        json!({"providerInstances": {"grok": {"driver": "grok", "enabled": true, "config": {"enabled": false, "binaryPath": "/opt/grok"}}}}),
    )
    .await;
    assert_eq!(
        next["providerInstances"]["grok"],
        json!({"driver": "grok", "enabled": false, "config": {"binaryPath": "/opt/grok"}})
    );
}

#[tokio::test]
async fn trims_provider_path_settings_when_updates_are_applied() {
    let fixture = Fixture::new().await;
    let next = update(
        &fixture.service(),
        json!({"providers": {
            "codex": {"binaryPath": "  /opt/homebrew/bin/codex  ", "homePath": "   "},
            "claudeAgent": {"binaryPath": "  /opt/homebrew/bin/claude  "},
            "opencode": {"binaryPath": "  /opt/homebrew/bin/opencode  ", "serverUrl": "  http://127.0.0.1:4096  ", "serverPassword": "  secret-password  "}
        }}),
    )
    .await;
    assert_eq!(
        next["providers"]["codex"],
        json!({"enabled": true, "binaryPath": "/opt/homebrew/bin/codex", "homePath": "", "shadowHomePath": "", "launchArgs": "", "customModels": []})
    );
    assert_eq!(
        next["providers"]["claudeAgent"],
        json!({"enabled": true, "binaryPath": "/opt/homebrew/bin/claude", "homePath": "", "customModels": [], "launchArgs": "", "autoCompactWindow": ""})
    );
    assert_eq!(
        next["providers"]["opencode"],
        json!({"enabled": false, "binaryPath": "/opt/homebrew/bin/opencode", "serverUrl": "http://127.0.0.1:4096", "serverPassword": "secret-password", "customModels": []})
    );
}

#[tokio::test]
async fn trims_observability_settings_when_updates_are_applied() {
    let fixture = Fixture::new().await;
    let next = update(
        &fixture.service(),
        json!({
            "addProjectBaseDirectory": "  ~/Development  ",
            "observability": {
                "otlpTracesUrl": "  http://localhost:4318/v1/traces  ",
                "otlpMetricsUrl": "  http://localhost:4318/v1/metrics  ",
                "otlpLogsUrl": "  http://localhost:4318/v1/logs  "
            }
        }),
    )
    .await;
    assert_eq!(next["addProjectBaseDirectory"], json!("~/Development"));
    assert_eq!(
        next["observability"],
        json!({"otlpTracesUrl": "http://localhost:4318/v1/traces", "otlpMetricsUrl": "http://localhost:4318/v1/metrics", "otlpLogsUrl": "http://localhost:4318/v1/logs"})
    );
}

#[tokio::test]
async fn defaults_blank_binary_paths_to_provider_executables() {
    let fixture = Fixture::new().await;
    let next = update(
        &fixture.service(),
        json!({"providers": {"codex": {"binaryPath": "   "}, "claudeAgent": {"binaryPath": ""}}}),
    )
    .await;
    assert_eq!(next["providers"]["codex"]["binaryPath"], json!("codex"));
    assert_eq!(next["providers"]["claudeAgent"]["binaryPath"], json!("claude"));
}

#[tokio::test]
async fn writes_non_default_settings_and_explicit_optional_provider_defaults_to_disk() {
    let fixture = Fixture::new().await;
    let next = update(
        &fixture.service(),
        json!({
            "addProjectBaseDirectory": "~/Development",
            "observability": {"otlpTracesUrl": "http://localhost:4318/v1/traces", "otlpMetricsUrl": "http://localhost:4318/v1/metrics"},
            "providers": {
                "codex": {"binaryPath": "/opt/homebrew/bin/codex"},
                "opencode": {"serverUrl": "http://127.0.0.1:4096", "serverPassword": "secret-password"}
            },
            "automaticGitFetchInterval": 10_000
        }),
    )
    .await;
    assert_eq!(next["providers"]["codex"]["binaryPath"], json!("/opt/homebrew/bin/codex"));
    let expected = json!({
        "addProjectBaseDirectory": "~/Development",
        "observability": {"otlpTracesUrl": "http://localhost:4318/v1/traces", "otlpMetricsUrl": "http://localhost:4318/v1/metrics"},
        "providers": {
            "codex": {"binaryPath": "/opt/homebrew/bin/codex"},
            "cursor": {"enabled": false},
            "grok": {"enabled": false},
            "opencode": {"enabled": false, "serverUrl": "http://127.0.0.1:4096", "serverPassword": "secret-password"}
        },
        "backgroundActivity": {"schemaVersion": 1, "profile": "custom", "baseProfile": "balanced", "overrides": {"automaticGitFetchInterval": 10_000}},
        "automaticGitFetchInterval": 10_000
    });
    assert_eq!(fixture.persisted(), expected);
    // Byte for byte: `ServerSettings` declaration order, two-space indentation, a final newline.
    let in_declaration_order = json!({
        "backgroundActivity": expected["backgroundActivity"],
        "automaticGitFetchInterval": 10_000,
        "addProjectBaseDirectory": "~/Development",
        "providers": expected["providers"],
        "observability": expected["observability"]
    });
    assert_eq!(fixture.read(), format!("{}\n", zc_settings::js::stringify_pretty(&in_declaration_order)));
}

#[tokio::test]
async fn keeps_the_inline_value_on_disk_when_secret_migration_fails() {
    let fixture = Fixture::new().await;
    let secrets = Arc::new(MemorySecrets::default());
    *secrets.fail_set.lock().unwrap() = Some(Box::new(|_, _| true));
    let service = fixture.service_with(secrets);
    let original = r#"{"providerInstances":{"codex_personal":{"driver":"codex","environment":[{"name":"API_TOKEN","value":"inline-test-token","sensitive":true}],"config":{}}}}"#;
    fixture.write(original);
    let error = service
        .update_settings_value(&json!({"providerInstances": {"codex_personal": {
            "driver": "codex",
            "environment": [{"name": "API_TOKEN", "value": "", "sensitive": true, "valueRedacted": true}],
            "config": {}
        }}}))
        .await
        .unwrap_err();
    assert_eq!(error.operation, ServerSettingsOperation::WriteSecret);
    assert_eq!(error.cause["name"], json!("SecretStorePersistError"));
    assert_eq!(fixture.read(), original);
    assert_eq!(
        get(&service).await["providerInstances"]["codex_personal"]["environment"][0]["value"],
        json!("inline-test-token")
    );
}

async fn inline_secret_case(variable: Value, expected: &str, duplicate: bool) {
    let fixture = Fixture::new().await;
    let service = fixture.service();
    fixture.write(if duplicate {
        r#"{"providerInstances":{"codex_personal":{"driver":"codex","environment":[{"name":"API_TOKEN","value":"inline-test-token","sensitive":true},{"name":"API_TOKEN","value":"last-inline-test-token","sensitive":true}],"config":{}}}}"#
    } else {
        r#"{"providerInstances":{"codex_personal":{"driver":"codex","environment":[{"name":"API_TOKEN","value":"inline-test-token","sensitive":true}],"config":{}}}}"#
    });
    let initial = get(&service).await;
    assert_eq!(
        initial["providerInstances"]["codex_personal"]["environment"][0]["value"],
        json!("inline-test-token")
    );
    let environment = if duplicate { json!([variable, variable]) } else { json!([variable]) };
    let next = update(
        &service,
        json!({"providerInstances": {"codex_personal": {"driver": "codex", "displayName": "Renamed provider", "environment": environment, "config": {}}}}),
    )
    .await;
    assert_eq!(next["providerInstances"]["codex_personal"]["environment"][0]["value"], json!(expected));
    let raw = fixture.read();
    assert!(!raw.contains("inline-test-token"));
    assert!(!raw.contains("replacement-test-token"));
    let reloaded = get(&fixture.service()).await;
    assert_eq!(reloaded["providerInstances"]["codex_personal"]["environment"][0]["value"], json!(expected));
}

#[tokio::test]
async fn preserves_an_inline_secret_on_a_redacted_settings_save() {
    inline_secret_case(
        json!({"name": "API_TOKEN", "value": "", "sensitive": true, "valueRedacted": true}),
        "inline-test-token",
        false,
    )
    .await;
}

#[tokio::test]
async fn preserves_the_effective_last_inline_secret_when_names_are_duplicated() {
    inline_secret_case(
        json!({"name": "API_TOKEN", "value": "", "sensitive": true, "valueRedacted": true}),
        "last-inline-test-token",
        true,
    )
    .await;
}

#[tokio::test]
async fn replaces_an_inline_secret_with_an_explicit_value() {
    inline_secret_case(
        json!({"name": "API_TOKEN", "value": "replacement-test-token", "sensitive": true}),
        "replacement-test-token",
        false,
    )
    .await;
}

#[tokio::test]
async fn clears_an_inline_secret_with_an_explicit_empty_value() {
    inline_secret_case(json!({"name": "API_TOKEN", "value": "", "sensitive": true}), "", false).await;
}

#[tokio::test]
async fn preserves_duplicate_secret_operation_order() {
    for sensitive_last in [true, false] {
        let fixture = Fixture::new().await;
        let secret = json!({"name": "API_TOKEN", "value": "secret-last", "sensitive": true});
        let plain = json!({"name": "API_TOKEN", "value": "plain-last", "sensitive": false});
        let environment = if sensitive_last { json!([plain, secret]) } else { json!([secret, plain]) };
        let next = update(
            &fixture.service(),
            json!({"providerInstances": {"codex_duplicate": {"driver": "codex", "environment": environment, "config": {}}}}),
        )
        .await;
        let environment = next["providerInstances"]["codex_duplicate"]["environment"].as_array().unwrap().clone();
        assert_eq!(
            environment.last().unwrap()["value"],
            json!(if sensitive_last { "secret-last" } else { "plain-last" })
        );
        let sensitive = environment.iter().find(|v| v["sensitive"] == json!(true)).unwrap();
        assert_eq!(sensitive["value"], json!(if sensitive_last { "secret-last" } else { "" }));
    }
}

#[tokio::test]
async fn stores_sensitive_provider_instance_environment_values_outside_settings_json() {
    let fixture = Fixture::new().await;
    let service = fixture.service();
    let next = update(
        &service,
        json!({"providerInstances": {"codex_personal": {"driver": "codex", "environment": [
            {"name": "OPENROUTER_API_KEY", "value": "sk-or-secret", "sensitive": true},
            {"name": "ANTHROPIC_BASE_URL", "value": "https://openrouter.ai/api", "sensitive": false}
        ], "config": {}}}}),
    )
    .await;
    assert_eq!(
        next["providerInstances"]["codex_personal"]["environment"],
        json!([
            {"name": "OPENROUTER_API_KEY", "value": "sk-or-secret", "sensitive": true, "valueRedacted": true},
            {"name": "ANTHROPIC_BASE_URL", "value": "https://openrouter.ai/api", "sensitive": false}
        ])
    );
    let raw = fixture.read();
    assert!(!raw.contains("sk-or-secret"));
    assert_eq!(
        fixture.persisted()["providerInstances"]["codex_personal"]["environment"],
        json!([
            {"name": "OPENROUTER_API_KEY", "value": "", "sensitive": true, "valueRedacted": true},
            {"name": "ANTHROPIC_BASE_URL", "value": "https://openrouter.ai/api", "sensitive": false}
        ])
    );
    let round_tripped = update(
        &service,
        json!({"providerInstances": {"codex_personal": {"driver": "codex", "displayName": "Codex Personal", "environment": [
            {"name": "OPENROUTER_API_KEY", "value": "", "sensitive": true, "valueRedacted": true},
            {"name": "ANTHROPIC_BASE_URL", "value": "https://openrouter.ai/api", "sensitive": false}
        ], "config": {}}}}),
    )
    .await;
    assert_eq!(
        round_tripped["providerInstances"]["codex_personal"]["environment"][0]["value"],
        json!("sk-or-secret")
    );
}

#[tokio::test]
async fn keeps_bitbucket_tokens_in_the_secret_store_and_tells_clients_only_that_one_is_set() {
    let fixture = Fixture::new().await;
    let service = fixture.service();
    let saved = update(
        &service,
        json!({"bitbucket": {"email": "me@example.com", "accessToken": "bb-access", "apiToken": "bb-api"}}),
    )
    .await;
    assert_eq!(
        saved["bitbucket"],
        json!({"email": "me@example.com", "accessToken": "bb-access", "apiToken": "bb-api"})
    );
    let raw = fixture.read();
    assert!(!raw.contains("bb-access") && !raw.contains("bb-api"));
    assert!(raw.contains("me@example.com"));
    let for_client = redact_server_settings_for_client(saved)["bitbucket"].clone();
    assert_eq!(for_client["email"], json!("me@example.com"));
    assert_eq!(for_client["accessToken"], json!(SECRET_REDACTED));
    assert_eq!(for_client["apiToken"], json!(SECRET_REDACTED));
    update(&service, json!({"bitbucket": for_client})).await;
    update(&service, json!({"bitbucket": {"email": "other@example.com"}})).await;
    assert_eq!(
        get(&service).await["bitbucket"],
        json!({"email": "other@example.com", "accessToken": "bb-access", "apiToken": "bb-api"})
    );
    let cleared = update(&service, json!({"bitbucket": {"accessToken": ""}})).await;
    assert_eq!(cleared["bitbucket"]["accessToken"], json!(""));
    assert_eq!(cleared["bitbucket"]["apiToken"], json!("bb-api"));
    assert_eq!(fixture.secrets.get("bitbucket-access-token").await.unwrap(), None);
    assert_eq!(redact_server_settings_for_client(cleared)["bitbucket"]["accessToken"], json!(""));
}

#[tokio::test]
async fn removes_a_bitbucket_secret_once_its_token_is_cleared_by_hand_in_settings_json() {
    let fixture = Fixture::new().await;
    fixture.secrets.set("bitbucket-access-token", b"stale-token").await.unwrap();
    fixture.write("{}");
    update(&fixture.service(), json!({"cursorKeychainUsageEnabled": true})).await;
    assert_eq!(fixture.secrets.get("bitbucket-access-token").await.unwrap(), None);
}

#[tokio::test]
async fn moves_a_hand_edited_bitbucket_token_into_the_secret_store_when_settings_load() {
    let fixture = Fixture::new().await;
    fixture.write(r#"{"bitbucket":{"accessToken":"hand-edited-token"}}"#);
    let loaded = get(&fixture.service()).await;
    assert_eq!(loaded["bitbucket"]["accessToken"], json!("hand-edited-token"));
    assert!(!fixture.read().contains("hand-edited-token"));
    assert_eq!(
        fixture.secrets.get("bitbucket-access-token").await.unwrap().as_deref(),
        Some(&b"hand-edited-token"[..])
    );
}

#[tokio::test]
async fn moves_a_hand_edited_bitbucket_token_into_the_secret_store_when_a_client_echoes_the_marker() {
    let fixture = Fixture::new().await;
    let failing = Arc::new(MemorySecrets::default());
    // The load-time move fails, so the token is still inline when the client echoes the marker.
    *failing.fail_set.lock().unwrap() = Some(Box::new(|_, value| value == b"hand-edited-token"));
    fixture.write(r#"{"bitbucket":{"email":"me@example.com","apiToken":"hand-edited-token"}}"#);
    let service = fixture.service_with(failing.clone());
    let for_client = redact_server_settings_for_client(get(&service).await)["bitbucket"].clone();
    *failing.fail_set.lock().unwrap() = None;
    let updated = update(&service, json!({"bitbucket": {"email": "new@example.com", "apiToken": for_client["apiToken"]}})).await;
    assert_eq!(updated["bitbucket"]["apiToken"], json!("hand-edited-token"));
    assert_eq!(get(&service).await["bitbucket"]["apiToken"], json!("hand-edited-token"));
    assert!(!fixture.read().contains("hand-edited-token"));
}

#[tokio::test]
async fn materializes_provider_secrets_for_consumers() {
    let fixture = Fixture::new().await;
    let service = fixture.service();
    update(
        &service,
        json!({"providerInstances": {"codex_terminal": {"driver": "codex", "environment": [
            {"name": "OPENROUTER_API_KEY", "value": "sk-terminal-secret", "sensitive": true}
        ], "config": {"homePath": "~/.codex-terminal"}}}}),
    )
    .await;
    let settings = get(&service).await;
    assert_eq!(
        settings["providerInstances"]["codex_terminal"]["environment"][0]["value"],
        json!("sk-terminal-secret")
    );
    let persisted = fixture.read();
    assert!(!persisted.contains("sk-terminal-secret"));
    assert!(persisted.contains("\"valueRedacted\": true"));
}

#[cfg(unix)]
#[tokio::test]
async fn rolls_back_provider_secret_changes_when_the_settings_file_commit_fails() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new().await;
    let service = fixture.service();
    let instance = |value: &str| {
        json!({"providerInstances": {"codex_write_failure": {"driver": "codex", "environment": [
            {"name": "OPENROUTER_API_KEY", "value": value, "sensitive": true}
        ], "config": {}}}})
    };
    update(&service, instance("sk-kept")).await;
    // The atomic write creates its temp directory next to the file: a read-only state
    // directory makes the commit fail (the secrets directory stays writable).
    let state_dir = fixture.settings_path.parent().unwrap().to_path_buf();
    std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let failed = service.update_settings_value(&instance("sk-new")).await;
    let failed_removal = service.update_settings_value(&json!({"providerInstances": {}})).await;
    std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(failed.unwrap_err().operation, ServerSettingsOperation::WriteFile);
    assert_eq!(failed_removal.unwrap_err().operation, ServerSettingsOperation::WriteFile);
    assert_eq!(
        get(&service).await["providerInstances"]["codex_write_failure"]["environment"][0]["value"],
        json!("sk-kept")
    );
}

async fn rolls_back_after(failure: &str) {
    let fixture = Fixture::new().await;
    let secrets = Arc::new(MemorySecrets::default());
    let service = fixture.service_with(secrets.clone());
    let instance = |value: &str| {
        json!({"providerInstances": {"codex_materialization_failure": {"driver": "codex", "environment": [
            {"name": "OPENROUTER_API_KEY", "value": value, "sensitive": true}
        ], "config": {}}}})
    };
    update(&service, instance("sk-kept")).await;
    if failure == "response materialization" {
        *secrets.fail_get.lock().unwrap() = Some(Box::new(|_, value| value == Some(b"sk-new")));
    } else {
        *secrets.fail_set.lock().unwrap() = Some(Box::new(|_, value| value == b"sk-new"));
    }
    assert!(service.update_settings_value(&instance("sk-new")).await.is_err());
    *secrets.fail_get.lock().unwrap() = None;
    *secrets.fail_set.lock().unwrap() = None;
    assert_eq!(
        get(&service).await["providerInstances"]["codex_materialization_failure"]["environment"][0]["value"],
        json!("sk-kept")
    );
}

#[tokio::test]
async fn rolls_back_provider_secret_changes_after_response_materialization_fails() {
    rolls_back_after("response materialization").await;
}

#[tokio::test]
async fn rolls_back_provider_secret_changes_after_partially_committed_write_fails() {
    rolls_back_after("partially committed write").await;
}

#[tokio::test]
async fn folds_legacy_project_overrides_into_project_settings_overrides_once() {
    let fixture = Fixture::new().await;
    let service = fixture.service();
    let script = json!({"id": "check", "name": "Check", "command": "npm test", "icon": "play", "runOnWorktreeCreate": false});
    let model = selection("codex", "gpt-5.5", None);
    let scripts_json = serde_json::to_string(&json!([script])).unwrap();
    for (project, model_column, env_mode, auto_pull) in [
        ("project-legacy", Some(model.to_string()), Some("worktree"), "1"),
        ("project-scripted", None, None, "0"),
    ] {
        fixture
            .sql(
                "INSERT INTO projection_projects (project_id, title, workspace_root, default_model_selection_json, \
                 default_thread_env_mode, auto_pull, scripts_json, created_at, updated_at) \
                 VALUES (?1, 'Project', ?2, ?3, ?4, CAST(?5 AS INTEGER), ?6, '2026-08-25T00:00:00.000Z', '2026-08-25T00:00:00.000Z')",
                vec![
                    Some(project.to_owned()),
                    Some(format!("/tmp/{project}")),
                    model_column,
                    env_mode.map(str::to_owned),
                    Some(auto_pull.to_owned()),
                    Some(scripts_json.clone()),
                ],
            )
            .await;
    }
    fixture.write(r#"{"projectAgentBrowserAccessOverrides":{"project-legacy":false},"projectAutoPullOverrides":{"project-scripted":true},"projectScriptOverrides":{"project-legacy":null}}"#);
    let settings = get(&service).await;
    assert_eq!(settings["projectSettingsFolded"], json!(true));
    assert_eq!(
        settings["projectSettingsOverrides"],
        json!({
            "project-legacy": {"enableAgentBrowserAccess": false, "defaultModelSelection": model, "defaultThreadEnvMode": "worktree", "defaultAutoPull": true},
            "project-scripted": {"defaultAutoPull": true, "defaultProjectScripts": [script]}
        })
    );
    assert_eq!(settings["projectAutoPullOverrides"], json!({"project-legacy": true, "project-scripted": true}));
    assert_eq!(settings["projectScriptOverrides"], json!({"project-scripted": [script]}));
    update(&service, json!({"projectSettingsOverrides": {"project-legacy": null}})).await;
    let persisted = decode_settings(&fixture.persisted()).unwrap();
    assert_eq!(persisted["projectSettingsFolded"], json!(true));
    assert!(persisted["projectSettingsOverrides"].get("project-legacy").is_none());
}

#[tokio::test]
async fn leaves_an_unreadable_settings_json_untouched_instead_of_folding_over_it() {
    let fixture = Fixture::new().await;
    fixture
        .sql(
            "INSERT INTO projection_projects (project_id, title, workspace_root, auto_pull, scripts_json, created_at, updated_at) \
             VALUES ('project-broken', 'Project', '/tmp/project-broken', 1, '[]', '2026-08-25T00:00:00.000Z', '2026-08-25T00:00:00.000Z')",
            Vec::new(),
        )
        .await;
    let broken = r#"{"defaultAutoPull": tru"#;
    fixture.write(broken);
    let settings = get(&fixture.service()).await;
    assert_eq!(settings["projectSettingsFolded"], json!(false));
    assert_eq!(settings["projectSettingsOverrides"], json!({}));
    assert_eq!(fixture.read(), broken);
}

#[tokio::test]
async fn picks_up_hand_edits_through_the_directory_watch() {
    let fixture = Fixture::new().await;
    let service = fixture.service();
    service.start().await.unwrap();
    service.ready().await.unwrap();
    let mut changes = service.subscribe_changes_value();
    // Give the watcher a moment to attach before the edit.
    tokio::time::sleep(Duration::from_millis(200)).await;
    fixture.write(r#"{"addProjectBaseDirectory": "~/edited"}"#);
    let change = tokio::time::timeout(Duration::from_secs(10), changes.next())
        .await
        .expect("a change within 10 s")
        .unwrap();
    assert_eq!(change["addProjectBaseDirectory"], json!("~/edited"));
    assert_eq!(get(&service).await["addProjectBaseDirectory"], json!("~/edited"));
}

#[tokio::test]
async fn implements_the_settings_port() {
    use zc_ports::SettingsService;
    let fixture = Fixture::new().await;
    let port: Arc<dyn SettingsService> = Arc::new(fixture.service());
    let mut changes = port.subscribe_changes();
    let patch: zc_contracts::ServerSettingsPatch = serde_json::from_value(json!({"responseStreamingMode": "turn"})).unwrap();
    let next = port.update_settings(patch).await.unwrap();
    assert_eq!(serde_json::to_value(&next).unwrap()["responseStreamingMode"], json!("turn"));
    let change = changes.next().await.unwrap();
    assert_eq!(serde_json::to_value(&change).unwrap()["responseStreamingMode"], json!("turn"));
    let current = port.get_settings().await.unwrap();
    assert_eq!(serde_json::to_value(&current).unwrap()["responseStreamingMode"], json!("turn"));
}
