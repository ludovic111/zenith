//! The pure parts of `serverSettings.ts` and `packages/shared/src/serverSettings.ts`, on
//! encoded settings (`serde_json::Value` in canonical form, see [`super::schema`]).

use std::collections::HashSet;

use base64::Engine as _;
use serde_json::{json, Map, Value};

use super::background;
use super::schema::default_settings;
use crate::js::{deep_merge, js_equal};

/// On disk and on the wire a secret is replaced by this marker; sending it back means "keep
/// what the server has".
pub const SECRET_REDACTED: &str = "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}";

/// The Bitbucket token fields and their secret names.
pub const BITBUCKET_SECRET_FIELDS: [(&str, &str); 2] = [("accessToken", "bitbucket-access-token"), ("apiToken", "bitbucket-api-token")];

/// Drivers whose legacy `providers.<kind>.enabled` defaults to off and is restored from
/// provider history.
pub const OPTIONAL_PROVIDERS: [&str; 3] = ["cursor", "grok", "opencode"];

/// Keys compared as a whole when stripping defaults (never field by field).
const ATOMIC_SETTINGS_KEYS: [&str; 6] = [
    "backgroundActivity",
    "automaticGitFetchInterval",
    "providerHealthRefreshInterval",
    "sourceControlWriterModelSelection",
    "textGenerationModelSelection",
    "pullRequestMergeMethod",
];

pub const DEFAULT_TEXT_GENERATION_MODEL: &str = "gpt-6-luna";

/// `DEFAULT_TEXT_GENERATION_MODEL_BY_PROVIDER`.
fn default_text_generation_model(driver: &str) -> Option<&'static str> {
    match driver {
        "codex" => Some(DEFAULT_TEXT_GENERATION_MODEL),
        "antigravity" => Some("antigravity-default"),
        "claudeAgent" => Some("claude-haiku-4-5"),
        "cursor" => Some("composer-2"),
        "opencode" => Some("openai/gpt-5"),
        _ => None,
    }
}

/// `DEFAULT_MODEL_BY_PROVIDER`.
fn default_model(driver: &str) -> Option<&'static str> {
    match driver {
        "codex" => Some("gpt-6-astra"),
        "claudeAgent" => Some("claude-fable-5-1"),
        "cursor" => Some("auto"),
        "grok" => Some("grok-build"),
        "opencode" => Some("openai/gpt-5"),
        "antigravity" => Some("antigravity-default"),
        _ => None,
    }
}

fn base64url(text: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text.as_bytes())
}

/// `provider-env-<b64url(instance)>-<b64url(name)>`.
pub fn provider_environment_secret_name(instance_id: &str, name: &str) -> String {
    format!("provider-env-{}-{}", base64url(instance_id), base64url(name))
}

/// `usage-limit-source-<b64url(id)>`.
pub fn usage_limit_source_secret_name(source_id: &str) -> String {
    format!("usage-limit-source-{}", base64url(source_id))
}

fn obj(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}

fn get_str<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

/// `{...object, key: value}` on a JSON object (existing keys keep their place).
fn with(mut object: Value, key: &str, value: Value) -> Value {
    if let Value::Object(map) = &mut object {
        map.insert(key.to_owned(), value);
    }
    object
}

// ---------------------------------------------------------------------------------------------
// Provider enablement
// ---------------------------------------------------------------------------------------------

/// `providerInstanceConfigEnabledFlag`.
fn config_enabled_flag(config: Option<&Value>) -> Option<bool> {
    config.and_then(Value::as_object)?.get("enabled")?.as_bool()
}

/// `resolveProviderInstanceEnabled`: an explicit false on the envelope or in the config wins,
/// then envelope, config, the driver's default.
pub fn resolve_provider_instance_enabled(instance: &Value) -> bool {
    let config_enabled = config_enabled_flag(instance.get("config"));
    let envelope = instance.get("enabled").and_then(Value::as_bool);
    if envelope == Some(false) || config_enabled == Some(false) {
        return false;
    }
    envelope
        .or(config_enabled)
        .unwrap_or_else(|| default_enabled_for_driver(get_str(instance, "driver")))
}

fn default_enabled_for_driver(driver: &str) -> bool {
    default_settings()["providers"]
        .get(driver)
        .and_then(|provider| provider.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

/// `isProviderDriverKind`: a trimmed slug, letter first, at most 64 characters.
fn is_provider_driver_kind(value: &str) -> bool {
    let mut chars = value.chars();
    value.len() <= 64 && chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// `isModelSelectionProviderEnabled`.
pub fn is_model_selection_provider_enabled(settings: &Value, selection: &Value) -> bool {
    let instance_id = get_str(selection, "instanceId");
    if let Some(instance) = settings["providerInstances"].get(instance_id) {
        return resolve_provider_instance_enabled(instance);
    }
    is_provider_driver_kind(instance_id) && settings["providers"].get(instance_id).and_then(|provider| provider.get("enabled")) == Some(&Value::Bool(true))
}

/// `resolveSourceControlWriterModelSelection(settings, providers?)`. `providers` are encoded
/// `ServerProvider`s.
pub fn resolve_source_control_writer_model_selection(settings: &Value, providers: Option<&[Value]>) -> Value {
    let selection = &settings["sourceControlWriterModelSelection"];
    if !selection.is_object() || !is_model_selection_provider_enabled(settings, selection) {
        return settings["textGenerationModelSelection"].clone();
    }
    let Some(providers) = providers else {
        return selection.clone();
    };
    let provider = providers.iter().find(|candidate| candidate.get("instanceId") == selection.get("instanceId"));
    match provider {
        Some(provider) if provider.get("enabled") == Some(&Value::Bool(true)) && is_provider_available(provider) => selection.clone(),
        _ => settings["textGenerationModelSelection"].clone(),
    }
}

/// `isProviderAvailable`: an absent availability means available.
fn is_provider_available(provider: &Value) -> bool {
    provider.get("availability").and_then(Value::as_str) != Some("unavailable")
}

/// `resolveTextGenerationProvider`: when the text generation selection points at a disabled
/// provider, fall back to the first enabled driver and its default model.
pub fn resolve_text_generation_provider(settings: Value) -> Value {
    if is_model_selection_provider_enabled(&settings, &settings["textGenerationModelSelection"]) {
        return settings;
    }
    let fallback = obj(&settings["providers"]).and_then(|providers| {
        providers.iter().find_map(|(driver, provider)| {
            let enabled = match settings["providerInstances"].get(driver) {
                None => provider.get("enabled").and_then(Value::as_bool).unwrap_or(false),
                Some(instance) => resolve_provider_instance_enabled(instance),
            };
            enabled.then(|| driver.clone())
        })
    });
    let Some(fallback) = fallback else {
        return settings;
    };
    let model = default_text_generation_model(&fallback)
        .or_else(|| default_model(&fallback))
        .unwrap_or(DEFAULT_TEXT_GENERATION_MODEL);
    with(settings, "textGenerationModelSelection", json!({"instanceId": fallback, "model": model}))
}

/// `foldProviderInstanceEnabledFlags`: move a boolean in-config `enabled` onto the envelope (an
/// explicit false on either side wins). Returns whether anything changed.
pub fn fold_provider_instance_enabled_flags(settings: &mut Value) -> bool {
    let Some(instances) = settings.get_mut("providerInstances").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut changed = false;
    for instance in instances.values_mut() {
        let Some(config_enabled) = config_enabled_flag(instance.get("config")) else {
            continue;
        };
        let envelope = instance.get("enabled").and_then(Value::as_bool);
        let resolved = if envelope == Some(false) || !config_enabled {
            false
        } else {
            envelope.unwrap_or(config_enabled)
        };
        let mut config = instance["config"].as_object().cloned().unwrap_or_default();
        config.remove("enabled");
        let map = instance.as_object_mut().expect("instance is an object");
        map.insert("enabled".into(), Value::Bool(resolved));
        map.insert("config".into(), Value::Object(config));
        changed = true;
    }
    changed
}

/// A provider that has run on this machine (`projection_thread_sessions` /
/// `provider_session_runtime`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderHistoryEntry {
    pub provider_name: String,
    pub provider_instance_id: Option<String>,
}

/// The legacy enabled flags as written in the file (absent ≠ false).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PersistedProviderFlags {
    pub cursor: Option<bool>,
    pub grok: Option<bool>,
    pub opencode: Option<bool>,
}

impl PersistedProviderFlags {
    /// `decodePersistedOptionalProviderSettingsJsonExit`: `None` when the shape is invalid
    /// (`providers` or an entry that is not an object, an `enabled` that is not a boolean).
    pub fn decode(raw: &Value) -> Option<Self> {
        let Value::Object(root) = raw else {
            return None;
        };
        let Some(providers) = root.get("providers") else {
            return Some(Self::default());
        };
        let providers = providers.as_object()?;
        let flag = |name: &str| -> Option<Option<bool>> {
            match providers.get(name) {
                None => Some(None),
                Some(Value::Object(entry)) => match entry.get("enabled") {
                    None => Some(None),
                    Some(Value::Bool(flag)) => Some(Some(*flag)),
                    Some(_) => None,
                },
                Some(_) => None,
            }
        };
        Some(Self {
            cursor: flag("cursor")?,
            grok: flag("grok")?,
            opencode: flag("opencode")?,
        })
    }

    fn get(&self, name: &str) -> Option<bool> {
        match name {
            "cursor" => self.cursor,
            "grok" => self.grok,
            "opencode" => self.opencode,
            _ => None,
        }
    }
}

/// `restoreUsedProviders`: optional drivers the machine has used stay enabled unless the file
/// says otherwise; instances of them without an explicit flag are enabled when used.
pub fn restore_used_providers(mut settings: Value, persisted: &PersistedProviderFlags, history: &[ProviderHistoryEntry]) -> Value {
    let used_providers: HashSet<&str> = history.iter().map(|entry| entry.provider_name.as_str()).collect();
    let used_instances: HashSet<&str> = history
        .iter()
        .map(|entry| entry.provider_instance_id.as_deref().unwrap_or(&entry.provider_name))
        .collect();
    if let Some(instances) = settings.get_mut("providerInstances").and_then(Value::as_object_mut) {
        for (instance_id, instance) in instances.iter_mut() {
            let driver = get_str(instance, "driver");
            if instance.get("enabled").is_none() && OPTIONAL_PROVIDERS.contains(&driver) && used_instances.contains(instance_id.as_str()) {
                instance
                    .as_object_mut()
                    .expect("instance is an object")
                    .insert("enabled".into(), Value::Bool(true));
            }
        }
    }
    if let Some(providers) = settings.get_mut("providers").and_then(Value::as_object_mut) {
        for name in OPTIONAL_PROVIDERS {
            let enabled = persisted.get(name).unwrap_or(used_providers.contains(name));
            if let Some(Value::Object(provider)) = providers.get_mut(name) {
                provider.insert("enabled".into(), Value::Bool(enabled));
            }
        }
    }
    settings
}

// ---------------------------------------------------------------------------------------------
// Project overrides
// ---------------------------------------------------------------------------------------------

/// `deriveLegacyProjectOverrides`: the per-key maps older clients read, derived from
/// `projectSettingsOverrides`.
pub fn derive_legacy_project_overrides(project_settings_overrides: &Value) -> [(&'static str, Value); 3] {
    let mut browser = Map::new();
    let mut auto_pull = Map::new();
    let mut scripts = Map::new();
    if let Some(entries) = obj(project_settings_overrides) {
        for (project_id, entry) in entries {
            if let Some(value) = entry.get("enableAgentBrowserAccess") {
                browser.insert(project_id.clone(), value.clone());
            }
            if let Some(value) = entry.get("defaultAutoPull") {
                auto_pull.insert(project_id.clone(), value.clone());
            }
            if let Some(value) = entry.get("defaultProjectScripts") {
                scripts.insert(project_id.clone(), value.clone());
            }
        }
    }
    [
        ("projectAgentBrowserAccessOverrides", Value::Object(browser)),
        ("projectAutoPullOverrides", Value::Object(auto_pull)),
        ("projectScriptOverrides", Value::Object(scripts)),
    ]
}

/// `{...settings, ...deriveLegacyProjectOverrides(settings)}`.
pub fn with_derived_legacy_overrides(mut settings: Value) -> Value {
    let derived = derive_legacy_project_overrides(&settings["projectSettingsOverrides"]);
    if let Value::Object(map) = &mut settings {
        for (key, value) in derived {
            map.insert(key.to_owned(), value);
        }
    }
    settings
}

/// One row of `projection_projects` (not deleted), for the one-time fold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyProjectSettingsRow {
    pub project_id: String,
    pub default_model_selection: Option<String>,
    pub default_thread_env_mode: Option<String>,
    pub auto_pull: i64,
    pub scripts: String,
}

/// `foldLegacyProjectSettings`: fold the legacy per-project maps and the project columns into
/// `projectSettingsOverrides` once (keys already there win), then mark it folded.
pub fn fold_legacy_project_settings(settings: Value, rows: &[LegacyProjectSettingsRow]) -> Value {
    if settings["projectSettingsFolded"] == Value::Bool(true) {
        return settings;
    }
    let empty = |key: &str| obj(&settings[key]).is_none_or(Map::is_empty);
    if rows.is_empty() && empty("projectAgentBrowserAccessOverrides") && empty("projectAutoPullOverrides") && empty("projectScriptOverrides") {
        return settings;
    }
    let mut entries: Map<String, Value> = obj(&settings["projectSettingsOverrides"]).cloned().unwrap_or_default();
    let mut set = |project_id: &str, key: &str, value: Value| {
        let entry = entries.entry(project_id.to_owned()).or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(entry) = entry {
            if !entry.contains_key(key) {
                entry.insert(key.to_owned(), value);
            }
        }
    };
    if let Some(map) = obj(&settings["projectAgentBrowserAccessOverrides"]) {
        for (project_id, value) in map {
            set(project_id, "enableAgentBrowserAccess", value.clone());
        }
    }
    if let Some(map) = obj(&settings["projectAutoPullOverrides"]) {
        for (project_id, value) in map {
            set(project_id, "defaultAutoPull", value.clone());
        }
    }
    let mut reset_scripts = HashSet::new();
    if let Some(map) = obj(&settings["projectScriptOverrides"]) {
        for (project_id, value) in map {
            if value.is_null() {
                reset_scripts.insert(project_id.clone());
            } else {
                set(project_id, "defaultProjectScripts", value.clone());
            }
        }
    }
    for row in rows {
        let model = row
            .default_model_selection
            .as_deref()
            .and_then(|json| serde_json::from_str::<Value>(json).ok())
            .and_then(|value| decode_model_selection(&value));
        if let Some(model) = model {
            set(&row.project_id, "defaultModelSelection", model);
        }
        if let Some(mode @ ("local" | "worktree")) = row.default_thread_env_mode.as_deref() {
            set(&row.project_id, "defaultThreadEnvMode", Value::String(mode.to_owned()));
        }
        if row.auto_pull == 1 {
            set(&row.project_id, "defaultAutoPull", Value::Bool(true));
        }
        let scripts = serde_json::from_str::<Value>(&row.scripts)
            .ok()
            .and_then(|value| decode_project_scripts(&value));
        if let Some(scripts) = scripts {
            if scripts.as_array().is_some_and(|items| !items.is_empty()) && !reset_scripts.contains(&row.project_id) {
                set(&row.project_id, "defaultProjectScripts", scripts);
            }
        }
    }
    entries.retain(|_, entry| obj(entry).is_some_and(|entry| !entry.is_empty()));
    let entries = Value::Object(entries);
    let settings = with(settings, "projectSettingsOverrides", entries.clone());
    let mut settings = with(settings, "projectSettingsFolded", Value::Bool(true));
    for (key, value) in derive_legacy_project_overrides(&entries) {
        settings = with(settings, key, value);
    }
    settings
}

/// `Schema.decodeUnknownOption(Schema.fromJsonString(Schema.NullOr(ModelSelection)))` for one
/// stored column (`None` for `null` or anything that does not decode).
fn decode_model_selection(value: &Value) -> Option<Value> {
    if value.is_null() {
        return None;
    }
    let decoded = super::schema::decode_settings(&json!({ "defaultModelSelection": value })).ok()?;
    let selection = decoded.get("defaultModelSelection")?.clone();
    (!selection.is_null()).then_some(selection)
}

/// `Schema.decodeUnknownOption(Schema.fromJsonString(Schema.Array(ProjectScript)))`.
fn decode_project_scripts(value: &Value) -> Option<Value> {
    let decoded = super::schema::decode_settings(&json!({ "defaultProjectScripts": value })).ok()?;
    decoded.get("defaultProjectScripts").cloned()
}

/// `resolveProjectAgentBrowserAccess` (deprecated in TS, kept for older readers).
pub fn resolve_project_agent_browser_access(settings: &Value, project_id: &str) -> bool {
    settings["projectSettingsOverrides"]
        .get(project_id)
        .and_then(|entry| entry.get("enableAgentBrowserAccess"))
        .or_else(|| settings["projectAgentBrowserAccessOverrides"].get(project_id))
        .and_then(Value::as_bool)
        .unwrap_or_else(|| settings["enableAgentBrowserAccess"].as_bool().unwrap_or(true))
}

/// `resolveProjectAutoPull`: existing opt-ins stay enabled until overridden or reset.
pub fn resolve_project_auto_pull(settings: &Value, project_id: &str, legacy_auto_pull: Option<bool>) -> bool {
    settings["projectSettingsOverrides"]
        .get(project_id)
        .and_then(|entry| entry.get("defaultAutoPull"))
        .or_else(|| settings["projectAutoPullOverrides"].get(project_id))
        .and_then(Value::as_bool)
        .unwrap_or_else(|| legacy_auto_pull == Some(true) || settings["defaultAutoPull"] == Value::Bool(true))
}

/// The OTLP URLs of a `settings.json`, for startup before the settings service runs
/// (`parsePersistedServerObservabilitySettings`): trimmed, `None` when blank or unreadable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PersistedObservabilitySettings {
    pub otlp_traces_url: Option<String>,
    pub otlp_metrics_url: Option<String>,
    pub otlp_logs_url: Option<String>,
}

/// `parsePersistedServerObservabilitySettings(raw)`.
pub fn parse_persisted_server_observability_settings(raw: &str) -> PersistedObservabilitySettings {
    let decoded = zc_core::parse_lenient_json(raw)
        .ok()
        .and_then(|value| super::schema::decode_settings(&value).ok());
    let Some(decoded) = decoded else {
        return PersistedObservabilitySettings::default();
    };
    let url = |key: &str| {
        decoded["observability"][key]
            .as_str()
            .map(crate::js::js_trim)
            .filter(|url| !url.is_empty())
            .map(str::to_owned)
    };
    PersistedObservabilitySettings {
        otlp_traces_url: url("otlpTracesUrl"),
        otlp_metrics_url: url("otlpMetricsUrl"),
        otlp_logs_url: url("otlpLogsUrl"),
    }
}

// ---------------------------------------------------------------------------------------------
// Persisted form
// ---------------------------------------------------------------------------------------------

/// `PERSISTED_SERVER_SETTINGS_DEFAULTS`: the defaults, without the optional providers' enabled
/// flags (both states are written, because history cannot recover a new opt-in).
pub fn persisted_server_settings_defaults() -> Value {
    let mut defaults = default_settings();
    for name in OPTIONAL_PROVIDERS {
        if let Some(Value::Object(provider)) = defaults["providers"].get_mut(name) {
            provider.remove("enabled");
        }
    }
    defaults
}

/// `stripDefaultServerSettings`: what differs from `defaults` (`None` = nothing). A missing
/// default keeps the value whole.
pub fn strip_default_server_settings(current: &Value, defaults: Option<&Value>) -> Option<Value> {
    let Some(defaults) = defaults else {
        return Some(current.clone());
    };
    if current.is_array() || defaults.is_array() {
        return (!js_equal(current, defaults)).then(|| current.clone());
    }
    if let (Value::Object(current_map), Value::Object(defaults_map)) = (current, defaults) {
        let mut next = Map::new();
        for (key, value) in current_map {
            if ATOMIC_SETTINGS_KEYS.contains(&key.as_str()) {
                let same = defaults_map.get(key).is_some_and(|default| js_equal(value, default));
                if !same {
                    next.insert(key.clone(), value.clone());
                }
            } else if let Some(stripped) = strip_default_server_settings(value, defaults_map.get(key)) {
                next.insert(key.clone(), stripped);
            }
        }
        return (!next.is_empty()).then_some(Value::Object(next));
    }
    (!js_equal(current, defaults)).then(|| current.clone())
}

// ---------------------------------------------------------------------------------------------
// Redaction
// ---------------------------------------------------------------------------------------------

fn redact_secret(value: &str) -> &'static str {
    if value.is_empty() {
        ""
    } else {
        SECRET_REDACTED
    }
}

/// `redactProviderEnvironmentVariable`.
pub fn redact_provider_environment_variable(variable: &Value) -> Value {
    let mut variable = variable.clone();
    let Value::Object(map) = &mut variable else {
        return variable;
    };
    if map.get("sensitive") != Some(&Value::Bool(true)) {
        map.remove("valueRedacted");
        return variable;
    }
    let had_value = map.get("value").and_then(Value::as_str).is_some_and(|value| !value.is_empty());
    let was_redacted = map.get("valueRedacted") == Some(&Value::Bool(true));
    map.insert("value".into(), Value::String(String::new()));
    if had_value || was_redacted {
        map.insert("valueRedacted".into(), Value::Bool(true));
    }
    variable
}

/// `redactServerSettingsForClient`: sensitive provider environment values, usage-hub keys and
/// Bitbucket tokens become the marker (or stay empty).
pub fn redact_server_settings_for_client(mut settings: Value) -> Value {
    if let Some(instances) = settings.get_mut("providerInstances").and_then(Value::as_object_mut) {
        for instance in instances.values_mut() {
            if let Some(Value::Array(environment)) = instance.get_mut("environment") {
                for variable in environment.iter_mut() {
                    *variable = redact_provider_environment_variable(variable);
                }
            }
        }
    }
    if let Some(sources) = settings.get_mut("usageLimitSources").and_then(Value::as_object_mut) {
        for source in sources.values_mut() {
            let key = redact_secret(get_str(source, "managementKey"));
            if let Value::Object(source) = source {
                source.insert("managementKey".into(), Value::String(key.to_owned()));
            }
        }
    }
    if let Some(Value::Object(bitbucket)) = settings.get_mut("bitbucket") {
        for (field, _) in BITBUCKET_SECRET_FIELDS {
            let redacted = redact_secret(bitbucket.get(field).and_then(Value::as_str).unwrap_or(""));
            bitbucket.insert(field.to_owned(), Value::String(redacted.to_owned()));
        }
    }
    settings
}

// ---------------------------------------------------------------------------------------------
// Secrets on update
// ---------------------------------------------------------------------------------------------

/// One secret store operation an update implies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretChange {
    pub secret_name: String,
    pub provider_instance_id: Option<String>,
    pub environment_variable: Option<String>,
    pub kind: SecretChangeKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretChangeKind {
    Write(Vec<u8>),
    /// `remove-secret` or `remove-stale-secret`.
    Remove(&'static str),
}

/// `persistProviderEnvironmentSecrets(current, next)`: move secret values out of the settings
/// (replacing them with markers) and list the secret store operations that implies.
pub fn persist_provider_environment_secrets(current: &Value, next: Value) -> (Value, Vec<SecretChange>) {
    let mut next = next;
    let mut changes = Vec::new();
    let mut next_secret_keys = HashSet::new();
    if let Some(instances) = next.get_mut("providerInstances").and_then(Value::as_object_mut) {
        for (instance_id, instance) in instances.iter_mut() {
            let Some(Value::Array(environment)) = instance.get("environment") else {
                continue;
            };
            let mut rewritten = Vec::with_capacity(environment.len());
            for variable in environment {
                let name = get_str(variable, "name").to_owned();
                let secret_name = provider_environment_secret_name(instance_id, &name);
                let change = |kind| SecretChange {
                    secret_name: secret_name.clone(),
                    provider_instance_id: Some(instance_id.clone()),
                    environment_variable: Some(name.clone()),
                    kind,
                };
                if variable.get("sensitive") != Some(&Value::Bool(true)) {
                    changes.push(change(SecretChangeKind::Remove("remove-secret")));
                    rewritten.push(redact_provider_environment_variable(variable));
                    continue;
                }
                next_secret_keys.insert(secret_name.clone());
                let value_redacted = variable.get("valueRedacted") == Some(&Value::Bool(true));
                // Last value wins for duplicate names, like the provider environment.
                let previous = if value_redacted {
                    current["providerInstances"]
                        .get(instance_id)
                        .and_then(|instance| instance.get("environment"))
                        .and_then(Value::as_array)
                        .and_then(|entries| entries.iter().rev().find(|entry| get_str(entry, "name") == name))
                } else {
                    None
                };
                let inline_value = previous
                    .filter(|previous| {
                        previous.get("sensitive") == Some(&Value::Bool(true))
                            && previous.get("valueRedacted") != Some(&Value::Bool(true))
                            && !get_str(previous, "value").is_empty()
                    })
                    .map(|previous| get_str(previous, "value").to_owned());
                if !value_redacted || inline_value.is_some() {
                    let value = inline_value.unwrap_or_else(|| get_str(variable, "value").to_owned());
                    if !value.is_empty() {
                        changes.push(change(SecretChangeKind::Write(value.into_bytes())));
                        let redacted = with(variable.clone(), "value", Value::String(String::new()));
                        rewritten.push(with(redacted, "valueRedacted", Value::Bool(true)));
                    } else {
                        changes.push(change(SecretChangeKind::Remove("remove-secret")));
                        let mut rest = variable.clone();
                        if let Value::Object(map) = &mut rest {
                            map.remove("valueRedacted");
                        }
                        rewritten.push(rest);
                    }
                    continue;
                }
                rewritten.push(redact_provider_environment_variable(variable));
            }
            if let Value::Object(map) = instance {
                map.insert("environment".into(), Value::Array(rewritten));
            }
        }
    }
    if let Some(instances) = obj(&current["providerInstances"]) {
        for (instance_id, instance) in instances {
            for variable in instance.get("environment").and_then(Value::as_array).into_iter().flatten() {
                if variable.get("sensitive") != Some(&Value::Bool(true)) {
                    continue;
                }
                let name = get_str(variable, "name");
                let secret_name = provider_environment_secret_name(instance_id, name);
                if next_secret_keys.contains(&secret_name) {
                    continue;
                }
                changes.push(SecretChange {
                    secret_name,
                    provider_instance_id: Some(instance_id.clone()),
                    environment_variable: Some(name.to_owned()),
                    kind: SecretChangeKind::Remove("remove-stale-secret"),
                });
            }
        }
    }
    let next_source_ids: HashSet<String> = obj(&next["usageLimitSources"])
        .map(|sources| sources.keys().cloned().collect())
        .unwrap_or_default();
    if let Some(sources) = next.get_mut("usageLimitSources").and_then(Value::as_object_mut) {
        for (source_id, source) in sources.iter_mut() {
            let secret_name = usage_limit_source_secret_name(source_id);
            let key = get_str(source, "managementKey").to_owned();
            if key == SECRET_REDACTED {
                continue;
            }
            let change = |kind| SecretChange {
                secret_name: secret_name.clone(),
                provider_instance_id: None,
                environment_variable: None,
                kind,
            };
            if key.is_empty() {
                changes.push(change(SecretChangeKind::Remove("remove-secret")));
                continue;
            }
            changes.push(change(SecretChangeKind::Write(key.into_bytes())));
            if let Value::Object(map) = source {
                map.insert("managementKey".into(), Value::String(SECRET_REDACTED.to_owned()));
            }
        }
    }
    if let Some(sources) = obj(&current["usageLimitSources"]) {
        for source_id in sources.keys() {
            if next_source_ids.contains(source_id) {
                continue;
            }
            changes.push(SecretChange {
                secret_name: usage_limit_source_secret_name(source_id),
                provider_instance_id: None,
                environment_variable: None,
                kind: SecretChangeKind::Remove("remove-stale-secret"),
            });
        }
    }
    if let Some(Value::Object(bitbucket)) = next.get_mut("bitbucket") {
        for (field, secret_name) in BITBUCKET_SECRET_FIELDS {
            let mut value = bitbucket.get(field).and_then(Value::as_str).unwrap_or("").to_owned();
            if value == SECRET_REDACTED {
                // The marker keeps what is saved; a plaintext value hand-edited into the file
                // is not in the store yet, so it moves there instead of being dropped.
                let inline = get_str(&current["bitbucket"], field);
                if inline == SECRET_REDACTED || inline.is_empty() {
                    continue;
                }
                value = inline.to_owned();
            }
            let change = |kind| SecretChange {
                secret_name: secret_name.to_owned(),
                provider_instance_id: None,
                environment_variable: None,
                kind,
            };
            if value.is_empty() {
                changes.push(change(SecretChangeKind::Remove("remove-secret")));
                continue;
            }
            changes.push(change(SecretChangeKind::Write(value.into_bytes())));
            bitbucket.insert(field.to_owned(), Value::String(SECRET_REDACTED.to_owned()));
        }
    }
    (next, changes)
}

// ---------------------------------------------------------------------------------------------
// Patches
// ---------------------------------------------------------------------------------------------

/// `mergeSettingsEntries`: upsert each patched entry, `null` removes it.
fn merge_settings_entries(current: &Value, patch: &Value) -> Map<String, Value> {
    let mut next = obj(current).cloned().unwrap_or_default();
    if let Some(patch) = obj(patch) {
        for (id, config) in patch {
            if config.is_null() {
                next.shift_remove(id);
            } else {
                next.insert(id.clone(), config.clone());
            }
        }
    }
    next
}

/// `translateLegacyProjectOverridePatch`: legacy per-key project maps in a patch become
/// entries of `projectSettingsOverrides`.
fn translate_legacy_project_override_patch(current: &Value, patch: &Map<String, Value>) -> Map<String, Value> {
    const LEGACY: [(&str, &str); 3] = [
        ("projectAgentBrowserAccessOverrides", "enableAgentBrowserAccess"),
        ("projectAutoPullOverrides", "defaultAutoPull"),
        ("projectScriptOverrides", "defaultProjectScripts"),
    ];
    if LEGACY.iter().all(|(map, _)| !patch.contains_key(*map)) {
        return patch.clone();
    }
    let mut rest = patch.clone();
    for (map, _) in LEGACY {
        rest.shift_remove(map);
    }
    let canonical = obj(rest.get("projectSettingsOverrides").unwrap_or(&Value::Null)).cloned().unwrap_or_default();
    let canonical_ids: HashSet<String> = canonical.keys().cloned().collect();
    let mut entries = canonical;
    for (map, key) in LEGACY {
        let Some(map) = patch.get(map).and_then(Value::as_object) else {
            continue;
        };
        for (project_id, value) in map {
            if canonical_ids.contains(project_id) {
                continue;
            }
            let base = entries
                .get(project_id)
                .filter(|entry| !entry.is_null())
                .or_else(|| current["projectSettingsOverrides"].get(project_id))
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            let mut entry = obj(&base).cloned().unwrap_or_default();
            if value.is_null() {
                entry.shift_remove(key);
            } else {
                entry.insert(key.to_owned(), value.clone());
            }
            let entry = if entry.is_empty() { Value::Null } else { Value::Object(entry) };
            entries.insert(project_id.clone(), entry);
        }
    }
    rest.insert("projectSettingsOverrides".into(), Value::Object(entries));
    rest
}

/// `createModelSelection(instanceId, model, options?)`.
pub fn create_model_selection(instance_id: &Value, model: &Value, options: Option<&Value>) -> Value {
    let mut selection = Map::new();
    selection.insert("instanceId".into(), instance_id.clone());
    selection.insert("model".into(), model.clone());
    if let Some(Value::Array(options)) = options {
        if !options.is_empty() {
            selection.insert("options".into(), Value::Array(options.clone()));
        }
    }
    Value::Object(selection)
}

/// `mergeModelSelectionOptionsById`.
fn merge_model_selection_options(current: Option<&Value>, patch: Option<&Value>) -> Option<Value> {
    let Some(patch) = patch else {
        return current.cloned();
    };
    let patch = patch.as_array().cloned().unwrap_or_default();
    if patch.is_empty() {
        return None;
    }
    let mut merged: Vec<(Value, Value)> = Vec::new();
    let mut upsert = |id: &Value, value: &Value| {
        if let Some(entry) = merged.iter_mut().find(|(existing, _)| existing == id) {
            entry.1 = value.clone();
        } else {
            merged.push((id.clone(), value.clone()));
        }
    };
    for entry in current.and_then(Value::as_array).into_iter().flatten() {
        upsert(&entry["id"], &entry["value"]);
    }
    for entry in &patch {
        upsert(&entry["id"], &entry["value"]);
    }
    Some(Value::Array(merged.into_iter().map(|(id, value)| json!({"id": id, "value": value})).collect()))
}

/// `applyServerSettingsPatch(current, patch)` on the canonical settings and a decoded patch
/// ([`super::schema::decode_patch`]). The result is not canonical yet (keys may be appended);
/// the caller normalizes it.
pub fn apply_server_settings_patch(current: &Value, raw_patch: &Value) -> Value {
    let Some(raw_patch) = obj(raw_patch) else {
        return current.clone();
    };
    let patch = translate_legacy_project_override_patch(current, raw_patch);
    let selection_patch = patch.get("textGenerationModelSelection").cloned();
    let take = |key: &str| patch.get(key).cloned();
    let automatic_git_fetch_interval = take("automaticGitFetchInterval");
    let provider_health_refresh_interval = take("providerHealthRefreshInterval");
    let background_activity_profile = take("backgroundActivityProfile");
    let background_activity = take("backgroundActivity");
    let worktree_cleanup_patch = take("worktreeCleanup");
    let usage_limit_sources_patch = take("usageLimitSources");
    let usage_price_overrides_patch = take("usagePriceOverrides");
    let project_settings_overrides_patch = take("projectSettingsOverrides");
    let mut patch_for_merge = patch.clone();
    for key in [
        "automaticGitFetchInterval",
        "providerHealthRefreshInterval",
        "backgroundActivityProfile",
        "backgroundActivity",
        "worktreeCleanup",
        "usageLimitSources",
        "usagePriceOverrides",
        "projectSettingsOverrides",
        "projectAgentBrowserAccessOverrides",
        "projectAutoPullOverrides",
        "projectScriptOverrides",
    ] {
        patch_for_merge.shift_remove(key);
    }

    let current_background_activity = background::normalize_server(current);
    let has_interval = automatic_git_fetch_interval.is_some() || provider_health_refresh_interval.is_some();
    let interval_overrides = |mut overrides: Map<String, Value>| {
        if let Some(value) = &automatic_git_fetch_interval {
            overrides.insert("automaticGitFetchInterval".into(), value.clone());
        }
        if let Some(value) = &provider_health_refresh_interval {
            overrides.insert("providerHealthRefreshInterval".into(), value.clone());
        }
        Value::Object(overrides)
    };
    let background_activity_patch = if let Some(profile) = &background_activity_profile {
        let mut value = Map::new();
        value.insert("schemaVersion".into(), json!(1));
        value.insert("profile".into(), if has_interval { json!("custom") } else { profile.clone() });
        if has_interval {
            value.insert("baseProfile".into(), profile.clone());
        }
        value.insert("overrides".into(), interval_overrides(Map::new()));
        Some(Value::Object(value))
    } else if has_interval {
        let base = if current_background_activity["profile"] == "custom" {
            obj(&current_background_activity["overrides"]).cloned().unwrap_or_default()
        } else {
            Map::new()
        };
        Some(json!({
            "schemaVersion": 1,
            "profile": "custom",
            "baseProfile": background::base_profile(&current_background_activity),
            "overrides": interval_overrides(base),
        }))
    } else {
        None
    };

    let mut next = deep_merge(current, &Value::Object(patch_for_merge));
    if let Some(worktree_cleanup) = &worktree_cleanup_patch {
        let value = if worktree_cleanup.get("mode").and_then(Value::as_str) == Some("custom") {
            let storage = &next["storageCleanup"];
            let mut rules = Map::new();
            for key in ["worktreeAfterDays", "worktreeOnMerge", "worktreeOnDelete", "worktreeUnchanged"] {
                rules.insert(key.into(), storage.get(key).cloned().unwrap_or(Value::Null));
            }
            if current["worktreeCleanup"].get("mode").and_then(Value::as_str) == Some("custom") {
                if let Some(current_rules) = obj(&current["worktreeCleanup"]["rules"]) {
                    for (key, value) in current_rules {
                        rules.insert(key.clone(), value.clone());
                    }
                }
            }
            if let Some(patch_rules) = obj(&worktree_cleanup["rules"]) {
                for (key, value) in patch_rules {
                    rules.insert(key.clone(), value.clone());
                }
            }
            json!({"mode": "custom", "rules": rules})
        } else {
            worktree_cleanup.clone()
        };
        next = with(next, "worktreeCleanup", value);
    }
    let merged_background_activity = match &background_activity {
        Some(patch) => {
            let mut merged = deep_merge(&current_background_activity, patch);
            if let Some(overrides) = patch.get("overrides") {
                merged = with(merged, "overrides", overrides.clone());
            }
            merged
        }
        None => background_activity_patch.unwrap_or(current_background_activity),
    };
    next = with(next, "backgroundActivity", merged_background_activity);
    if let Some(instances) = patch.get("providerInstances") {
        next = with(next, "providerInstances", instances.clone());
    }
    if let Some(entries) = &project_settings_overrides_patch {
        let mut merged = merge_settings_entries(&current["projectSettingsOverrides"], entries);
        merged.retain(|_, entry| obj(entry).is_some_and(|entry| !entry.is_empty()));
        next = with(next, "projectSettingsOverrides", Value::Object(merged));
    }
    for key in ["defaultModelSelection", "defaultProjectScripts", "sourceControlWriterModelSelection"] {
        if let Some(value) = patch.get(key) {
            next = with(next, key, value.clone());
        }
    }
    if let Some(entries) = &usage_limit_sources_patch {
        let merged = merge_settings_entries(&current["usageLimitSources"], entries);
        next = with(next, "usageLimitSources", Value::Object(merged));
    }
    if let Some(entries) = &usage_price_overrides_patch {
        let merged = merge_settings_entries(&current["usagePriceOverrides"], entries);
        next = with(next, "usagePriceOverrides", Value::Object(merged));
    }
    if let Some(value) = &automatic_git_fetch_interval {
        next = with(next, "automaticGitFetchInterval", value.clone());
    }
    if let Some(value) = &provider_health_refresh_interval {
        next = with(next, "providerHealthRefreshInterval", value.clone());
    }

    let normalized_background_activity = background::normalize(&next["backgroundActivity"]);
    let resolved = background::resolve(&normalized_background_activity);
    next = with_derived_legacy_overrides(next);
    next = with(next, "backgroundActivity", normalized_background_activity);
    next = with(next, "automaticGitFetchInterval", background::number(resolved.automatic_git_fetch_interval));
    next = with(
        next,
        "providerHealthRefreshInterval",
        background::number(resolved.provider_health_refresh_interval),
    );
    next = with(next, "backgroundActivityProfile", json!(resolved.profile));

    let Some(selection_patch) = selection_patch else {
        return next;
    };
    let current_selection = &current["textGenerationModelSelection"];
    let instance_id = selection_patch.get("instanceId").unwrap_or(&current_selection["instanceId"]);
    let model = selection_patch.get("model").unwrap_or(&current_selection["model"]);
    let replace = selection_patch.get("instanceId").is_some() || selection_patch.get("model").is_some();
    let options = if replace {
        selection_patch.get("options").cloned()
    } else {
        merge_model_selection_options(current_selection.get("options"), selection_patch.get("options"))
    };
    let selection = create_model_selection(instance_id, model, options.as_ref());
    with(next, "textGenerationModelSelection", selection)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_names_use_unpadded_base64url() {
        assert_eq!(
            provider_environment_secret_name("codex_personal", "OPENROUTER_API_KEY"),
            "provider-env-Y29kZXhfcGVyc29uYWw-T1BFTlJPVVRFUl9BUElfS0VZ"
        );
        assert_eq!(usage_limit_source_secret_name("hub"), "usage-limit-source-aHVi");
    }

    #[test]
    fn strips_defaults_but_keeps_optional_provider_flags() {
        let defaults = persisted_server_settings_defaults();
        let mut settings = default_settings();
        settings["providers"]["cursor"]["enabled"] = json!(false);
        settings["providers"]["codex"]["binaryPath"] = json!("/opt/codex");
        assert_eq!(
            strip_default_server_settings(&settings, Some(&defaults)),
            Some(json!({"providers": {
                "codex": {"binaryPath": "/opt/codex"},
                "cursor": {"enabled": false},
                "grok": {"enabled": false},
                "opencode": {"enabled": false}
            }}))
        );
        assert_eq!(strip_default_server_settings(&default_settings(), Some(&default_settings())), None);
    }

    #[test]
    fn redacts_secrets_for_clients() {
        let mut settings = default_settings();
        settings["providerInstances"] = json!({"codex": {"driver": "codex", "environment": [
            {"name": "A", "value": "secret", "sensitive": true},
            {"name": "B", "value": "plain", "sensitive": false, "valueRedacted": true}
        ]}});
        settings["bitbucket"]["apiToken"] = json!("token");
        let redacted = redact_server_settings_for_client(settings);
        assert_eq!(
            redacted["providerInstances"]["codex"]["environment"],
            json!([
                {"name": "A", "value": "", "sensitive": true, "valueRedacted": true},
                {"name": "B", "value": "plain", "sensitive": false}
            ])
        );
        assert_eq!(redacted["bitbucket"]["apiToken"], json!(SECRET_REDACTED));
        assert_eq!(redacted["bitbucket"]["accessToken"], json!(""));
    }
}
