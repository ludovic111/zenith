//! The server-settings reads the provider core needs, over the settings port's JSON value
//! (`zc_ports::contracts::ServerSettings`, the decoded `ServerSettings` as WP-07 holds it):
//! `deriveProviderInstanceConfigMap` (`ProviderInstanceRegistryHydration.ts`), the instance
//! enabled flag (`providerInstanceConfigEnabledFlag`), the provider health refresh interval
//! (shared `resolveServerBackgroundActivitySettings`) and the project-scoped booleans
//! (shared `resolveProjectSettings` for one key).

use serde_json::{Map, Value};
use zc_contracts::{ProviderDriverKind, ProviderInstanceConfig, ProviderInstanceId};

/// `DEFAULT_PROVIDER_HEALTH_REFRESH_INTERVAL` (5 minutes).
pub const DEFAULT_PROVIDER_HEALTH_REFRESH_INTERVAL_MS: i64 = 5 * 60 * 1000;
const DEFAULT_AUTOMATIC_GIT_FETCH_INTERVAL_MS: i64 = 30 * 1000;

/// `deriveProviderInstanceConfigMap(settings)`: the explicit `providerInstances` entries in
/// settings-author order, then one envelope per built-in driver whose default id
/// (`defaultInstanceIdForDriver(kind) === kind`) is not taken, built from the legacy
/// `providers.<kind>` blob. Entries that do not decode are skipped (TS rejects the whole file
/// before it gets here).
pub fn derive_provider_instance_config_map(
    settings: &Value,
    built_in_driver_kinds: &[ProviderDriverKind],
) -> Vec<(ProviderInstanceId, ProviderInstanceConfig)> {
    let mut merged: Vec<(ProviderInstanceId, ProviderInstanceConfig)> = Vec::new();
    if let Some(instances) = settings.get("providerInstances").and_then(Value::as_object) {
        for (raw_id, raw_entry) in instances {
            match serde_json::from_value::<ProviderInstanceConfig>(raw_entry.clone()) {
                Ok(entry) => merged.push((ProviderInstanceId::from(raw_id.as_str()), entry)),
                Err(error) => tracing::warn!(instance_id = %raw_id, %error, "skipping a provider instance entry that does not decode"),
            }
        }
    }
    let empty = Value::Object(Map::new());
    for driver_kind in built_in_driver_kinds {
        if merged.iter().any(|(id, _)| id.as_str() == driver_kind.as_str()) {
            continue;
        }
        let legacy = settings
            .get("providers")
            .and_then(|providers| providers.get(driver_kind.as_str()))
            .unwrap_or(&empty);
        merged.push((
            ProviderInstanceId::from(driver_kind.as_str()),
            ProviderInstanceConfig {
                driver: driver_kind.clone(),
                display_name: None,
                accent_color: None,
                environment: None,
                enabled: None,
                config: Some(legacy.clone()),
            },
        ));
    }
    merged
}

/// `providerInstanceConfigEnabledFlag(config)`.
pub fn config_enabled_flag(config: Option<&Value>) -> Option<bool> {
    config
        .and_then(Value::as_object)
        .and_then(|object| object.get("enabled"))
        .and_then(Value::as_bool)
}

/// `resolveEntryEnabled`: an explicit `false` on the envelope or the raw blob wins, then the
/// envelope flag, then the decoded config's flag, then enabled.
pub fn resolve_entry_enabled(entry: &ProviderInstanceConfig, typed_config: &Value) -> bool {
    if entry.enabled == Some(false) || config_enabled_flag(entry.config.as_ref()) == Some(false) {
        return false;
    }
    entry.enabled.or_else(|| config_enabled_flag(Some(typed_config))).unwrap_or(true)
}

fn preset_provider_health_interval_ms(profile: &str) -> i64 {
    match profile {
        "performance" => 60 * 1000,
        "battery-saver" => 15 * 60 * 1000,
        _ => DEFAULT_PROVIDER_HEALTH_REFRESH_INTERVAL_MS,
    }
}

/// `resolveServerBackgroundActivitySettings(settings).providerHealthRefreshInterval`, in ms.
pub fn provider_health_refresh_interval_ms(settings: &Value) -> i64 {
    let background = settings.get("backgroundActivity");
    let profile = background.and_then(|b| b.get("profile")).and_then(Value::as_str).unwrap_or("balanced");
    let base_profile = background.and_then(|b| b.get("baseProfile")).and_then(Value::as_str);
    let overrides = background.and_then(|b| b.get("overrides")).and_then(Value::as_object);
    let background_is_default = profile == "balanced" && base_profile.is_none() && overrides.is_none_or(Map::is_empty);
    let legacy_profile = settings.get("backgroundActivityProfile").and_then(Value::as_str).unwrap_or("balanced");
    let git_fetch = settings
        .get("automaticGitFetchInterval")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_AUTOMATIC_GIT_FETCH_INTERVAL_MS);
    let health = settings
        .get("providerHealthRefreshInterval")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_PROVIDER_HEALTH_REFRESH_INTERVAL_MS);
    let has_legacy_overrides =
        legacy_profile != "balanced" || git_fetch != DEFAULT_AUTOMATIC_GIT_FETCH_INTERVAL_MS || health != DEFAULT_PROVIDER_HEALTH_REFRESH_INTERVAL_MS;
    if background_is_default && has_legacy_overrides {
        // The legacy profile with its flat interval as an override: the flat value either way.
        return health;
    }
    let base = if profile == "custom" { base_profile.unwrap_or("balanced") } else { profile };
    let preset = preset_provider_health_interval_ms(base);
    if profile == "custom" {
        overrides
            .and_then(|o| o.get("providerHealthRefreshInterval"))
            .and_then(Value::as_i64)
            .unwrap_or(preset)
    } else {
        preset
    }
}

/// Whether any project override sets `key`.
pub fn any_project_overrides(settings: &Value, key: &str) -> bool {
    settings
        .get("projectSettingsOverrides")
        .and_then(Value::as_object)
        .is_some_and(|overrides| overrides.values().any(|entry| entry.get(key).is_some_and(|value| !value.is_null())))
}

/// A boolean server setting resolved for one project (`resolveProjectSettings(settings,
/// projectId).settings[key]`): the project's override, else the environment value, else
/// `default`.
pub fn project_scoped_bool(settings: &Value, project_id: Option<&str>, key: &str, default: bool) -> bool {
    let environment = settings.get(key).and_then(Value::as_bool).unwrap_or(default);
    let Some(project_id) = project_id else {
        return environment;
    };
    settings
        .get("projectSettingsOverrides")
        .and_then(|overrides| overrides.get(project_id))
        .and_then(|entry| entry.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(environment)
}

/// A top-level boolean setting.
pub fn bool_setting(settings: &Value, key: &str, default: bool) -> bool {
    settings.get(key).and_then(Value::as_bool).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn explicit_instances_win_and_legacy_blobs_fill_the_rest() {
        let settings = json!({
            "providerInstances": {
                "codex_work": {"driver": "codex", "displayName": "Work"},
                "codex": {"driver": "codex", "config": {"binaryPath": "/opt/codex"}},
                "fork": {"driver": "forkDriver"}
            },
            "providers": {"claudeAgent": {"enabled": false}}
        });
        let kinds: Vec<ProviderDriverKind> = ["codex", "claudeAgent", "cursor"].into_iter().map(ProviderDriverKind::from).collect();
        let map = derive_provider_instance_config_map(&settings, &kinds);
        let ids: Vec<&str> = map.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["codex_work", "codex", "fork", "claudeAgent", "cursor"]);
        assert_eq!(map[1].1.config, Some(json!({"binaryPath": "/opt/codex"})));
        assert_eq!(map[3].1.config, Some(json!({"enabled": false})));
        assert_eq!(map[4].1.config, Some(json!({})));
    }

    #[test]
    fn enabled_resolution_is_most_restrictive() {
        let entry = |enabled: Option<bool>, config: Value| ProviderInstanceConfig {
            driver: "codex".into(),
            display_name: None,
            accent_color: None,
            environment: None,
            enabled,
            config: Some(config),
        };
        assert!(!resolve_entry_enabled(
            &entry(Some(true), json!({"enabled": false})),
            &json!({"enabled": false})
        ));
        assert!(!resolve_entry_enabled(&entry(Some(false), json!({})), &json!({"enabled": true})));
        assert!(resolve_entry_enabled(&entry(None, json!({})), &json!({"enabled": true})));
        assert!(!resolve_entry_enabled(&entry(None, json!({})), &json!({"enabled": false})));
        assert!(resolve_entry_enabled(&entry(None, json!({})), &json!({})));
    }

    #[test]
    fn provider_health_interval() {
        assert_eq!(provider_health_refresh_interval_ms(&json!({})), DEFAULT_PROVIDER_HEALTH_REFRESH_INTERVAL_MS);
        assert_eq!(
            provider_health_refresh_interval_ms(&json!({"backgroundActivity": {"profile": "performance"}})),
            60_000
        );
        assert_eq!(
            provider_health_refresh_interval_ms(
                &json!({"backgroundActivity": {"profile": "custom", "baseProfile": "battery-saver", "overrides": {"providerHealthRefreshInterval": 1234}}})
            ),
            1234
        );
        assert_eq!(provider_health_refresh_interval_ms(&json!({"providerHealthRefreshInterval": 0})), 0);
    }

    #[test]
    fn project_scoped_booleans() {
        let settings = json!({"enableAgentBrowserAccess": true, "projectSettingsOverrides": {"p1": {"enableAgentBrowserAccess": false}}});
        assert!(any_project_overrides(&settings, "enableAgentBrowserAccess"));
        assert!(!any_project_overrides(&settings, "enableAgentDeviceAccess"));
        assert!(!project_scoped_bool(&settings, Some("p1"), "enableAgentBrowserAccess", false));
        assert!(project_scoped_bool(&settings, Some("p2"), "enableAgentBrowserAccess", false));
    }
}
