//! `packages/shared/src/backgroundActivitySettings.ts` on encoded settings: durations are
//! milliseconds (`Schema.DurationFromMillis`).

use serde_json::{json, Map, Value};

/// One preset (`PRESET_SETTINGS`), or a resolved custom profile.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedBackgroundActivity {
    pub profile: &'static str,
    pub automatic_git_fetch_interval: f64,
    pub provider_health_refresh_interval: f64,
    pub host_power_monitor_active_interval: f64,
    pub host_power_monitor_idle_interval: f64,
    pub idle_client_ttl: f64,
    pub pause_when_host_locked: bool,
    pub pause_when_host_low_power: bool,
    pub pause_when_client_low_power: bool,
    pub pause_when_on_battery: bool,
}

pub const DEFAULT_BACKGROUND_ACTIVITY_PROFILE: &str = "balanced";
pub const DEFAULT_AUTOMATIC_GIT_FETCH_INTERVAL_MS: f64 = 30_000.0;
pub const DEFAULT_PROVIDER_HEALTH_REFRESH_INTERVAL_MS: f64 = 300_000.0;

/// `getBackgroundActivityPresetSettings`.
pub fn preset(profile: &str) -> ResolvedBackgroundActivity {
    match profile {
        "performance" => ResolvedBackgroundActivity {
            profile: "performance",
            automatic_git_fetch_interval: 15_000.0,
            provider_health_refresh_interval: 60_000.0,
            host_power_monitor_active_interval: 30_000.0,
            host_power_monitor_idle_interval: 120_000.0,
            idle_client_ttl: 45_000.0,
            pause_when_host_locked: true,
            pause_when_host_low_power: false,
            pause_when_client_low_power: false,
            pause_when_on_battery: false,
        },
        "battery-saver" => ResolvedBackgroundActivity {
            profile: "battery-saver",
            automatic_git_fetch_interval: 0.0,
            provider_health_refresh_interval: 900_000.0,
            host_power_monitor_active_interval: 60_000.0,
            host_power_monitor_idle_interval: 600_000.0,
            idle_client_ttl: 45_000.0,
            pause_when_host_locked: true,
            pause_when_host_low_power: true,
            pause_when_client_low_power: true,
            pause_when_on_battery: true,
        },
        _ => ResolvedBackgroundActivity {
            profile: "balanced",
            automatic_git_fetch_interval: DEFAULT_AUTOMATIC_GIT_FETCH_INTERVAL_MS,
            provider_health_refresh_interval: DEFAULT_PROVIDER_HEALTH_REFRESH_INTERVAL_MS,
            host_power_monitor_active_interval: 30_000.0,
            host_power_monitor_idle_interval: 300_000.0,
            idle_client_ttl: 45_000.0,
            pause_when_host_locked: true,
            pause_when_host_low_power: true,
            pause_when_client_low_power: true,
            pause_when_on_battery: false,
        },
    }
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn num_field(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(Value::as_f64)
}

fn bool_field(value: &Value, key: &str) -> Option<bool> {
    value.get(key).and_then(Value::as_bool)
}

/// A JS number as JSON (integral values without a fraction).
pub fn number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9.0e15 {
        json!(value as i64)
    } else {
        serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
    }
}

fn static_profile(profile: &str) -> &'static str {
    match profile {
        "performance" => "performance",
        "battery-saver" => "battery-saver",
        "custom" => "custom",
        _ => "balanced",
    }
}

/// `getBackgroundActivityBaseProfile`.
pub fn base_profile(background_activity: &Value) -> &'static str {
    match str_field(background_activity, "profile") {
        Some("custom") => static_profile(str_field(background_activity, "baseProfile").unwrap_or(DEFAULT_BACKGROUND_ACTIVITY_PROFILE)),
        Some(profile) => static_profile(profile),
        None => DEFAULT_BACKGROUND_ACTIVITY_PROFILE,
    }
}

/// `resolveBackgroundActivitySettings`.
pub fn resolve(background_activity: &Value) -> ResolvedBackgroundActivity {
    let base = base_profile(background_activity);
    let preset = preset(base);
    let empty = Value::Object(Map::new());
    let overrides = if str_field(background_activity, "profile") == Some("custom") {
        background_activity.get("overrides").unwrap_or(&empty)
    } else {
        &empty
    };
    ResolvedBackgroundActivity {
        profile: base,
        automatic_git_fetch_interval: num_field(overrides, "automaticGitFetchInterval").unwrap_or(preset.automatic_git_fetch_interval),
        provider_health_refresh_interval: num_field(overrides, "providerHealthRefreshInterval").unwrap_or(preset.provider_health_refresh_interval),
        host_power_monitor_active_interval: num_field(overrides, "hostPowerMonitorActiveInterval").unwrap_or(preset.host_power_monitor_active_interval),
        host_power_monitor_idle_interval: num_field(overrides, "hostPowerMonitorIdleInterval").unwrap_or(preset.host_power_monitor_idle_interval),
        idle_client_ttl: num_field(overrides, "idleClientTtl").unwrap_or(preset.idle_client_ttl),
        pause_when_host_locked: bool_field(overrides, "pauseWhenHostLocked").unwrap_or(preset.pause_when_host_locked),
        pause_when_host_low_power: bool_field(overrides, "pauseWhenHostLowPower").unwrap_or(preset.pause_when_host_low_power),
        pause_when_client_low_power: bool_field(overrides, "pauseWhenClientLowPower").unwrap_or(preset.pause_when_client_low_power),
        pause_when_on_battery: bool_field(overrides, "pauseWhenOnBattery").unwrap_or(preset.pause_when_on_battery),
    }
}

fn resolved_equal(a: &ResolvedBackgroundActivity, b: &ResolvedBackgroundActivity) -> bool {
    a.automatic_git_fetch_interval == b.automatic_git_fetch_interval
        && a.provider_health_refresh_interval == b.provider_health_refresh_interval
        && a.host_power_monitor_active_interval == b.host_power_monitor_active_interval
        && a.host_power_monitor_idle_interval == b.host_power_monitor_idle_interval
        && a.idle_client_ttl == b.idle_client_ttl
        && a.pause_when_host_locked == b.pause_when_host_locked
        && a.pause_when_host_low_power == b.pause_when_host_low_power
        && a.pause_when_client_low_power == b.pause_when_client_low_power
        && a.pause_when_on_battery == b.pause_when_on_battery
}

/// `normalizeBackgroundActivitySettings`.
pub fn normalize(background_activity: &Value) -> Value {
    let profile = str_field(background_activity, "profile").unwrap_or(DEFAULT_BACKGROUND_ACTIVITY_PROFILE);
    if profile != "custom" {
        return json!({"schemaVersion": 1, "profile": profile, "overrides": {}});
    }
    let resolved = resolve(background_activity);
    let base = base_profile(background_activity);
    for candidate in [base, "balanced", "performance", "battery-saver"] {
        if resolved_equal(&resolved, &preset(candidate)) {
            return json!({"schemaVersion": 1, "profile": candidate, "overrides": {}});
        }
    }
    let preset = preset(base);
    let mut overrides = Map::new();
    let mut number_override = |key: &str, value: f64, preset_value: f64| {
        if value != preset_value {
            overrides.insert(key.to_owned(), number(value));
        }
    };
    number_override(
        "automaticGitFetchInterval",
        resolved.automatic_git_fetch_interval,
        preset.automatic_git_fetch_interval,
    );
    number_override(
        "providerHealthRefreshInterval",
        resolved.provider_health_refresh_interval,
        preset.provider_health_refresh_interval,
    );
    number_override(
        "hostPowerMonitorActiveInterval",
        resolved.host_power_monitor_active_interval,
        preset.host_power_monitor_active_interval,
    );
    number_override(
        "hostPowerMonitorIdleInterval",
        resolved.host_power_monitor_idle_interval,
        preset.host_power_monitor_idle_interval,
    );
    number_override("idleClientTtl", resolved.idle_client_ttl, preset.idle_client_ttl);
    let mut bool_override = |key: &str, value: bool, preset_value: bool| {
        if value != preset_value {
            overrides.insert(key.to_owned(), Value::Bool(value));
        }
    };
    bool_override("pauseWhenHostLocked", resolved.pause_when_host_locked, preset.pause_when_host_locked);
    bool_override("pauseWhenHostLowPower", resolved.pause_when_host_low_power, preset.pause_when_host_low_power);
    bool_override(
        "pauseWhenClientLowPower",
        resolved.pause_when_client_low_power,
        preset.pause_when_client_low_power,
    );
    bool_override("pauseWhenOnBattery", resolved.pause_when_on_battery, preset.pause_when_on_battery);
    json!({"schemaVersion": 1, "profile": "custom", "baseProfile": base, "overrides": overrides})
}

/// `resolveServerBackgroundActivitySettings`: the structured setting, or the legacy flat
/// fields when the structured one is still the default.
pub fn resolve_server(settings: &Value) -> ResolvedBackgroundActivity {
    let empty = Value::Object(Map::new());
    let background_activity = settings.get("backgroundActivity").unwrap_or(&empty);
    let is_default = str_field(background_activity, "profile").unwrap_or(DEFAULT_BACKGROUND_ACTIVITY_PROFILE) == DEFAULT_BACKGROUND_ACTIVITY_PROFILE
        && background_activity.get("baseProfile").is_none()
        && background_activity.get("overrides").and_then(Value::as_object).is_none_or(Map::is_empty);
    let legacy_profile = static_profile(str_field(settings, "backgroundActivityProfile").unwrap_or(DEFAULT_BACKGROUND_ACTIVITY_PROFILE));
    let git = num_field(settings, "automaticGitFetchInterval").unwrap_or(DEFAULT_AUTOMATIC_GIT_FETCH_INTERVAL_MS);
    let health = num_field(settings, "providerHealthRefreshInterval").unwrap_or(DEFAULT_PROVIDER_HEALTH_REFRESH_INTERVAL_MS);
    let has_legacy_overrides = legacy_profile != DEFAULT_BACKGROUND_ACTIVITY_PROFILE
        || git != DEFAULT_AUTOMATIC_GIT_FETCH_INTERVAL_MS
        || health != DEFAULT_PROVIDER_HEALTH_REFRESH_INTERVAL_MS;
    if is_default && has_legacy_overrides {
        let legacy = preset(legacy_profile);
        let matches = git == legacy.automatic_git_fetch_interval && health == legacy.provider_health_refresh_interval;
        let mut overrides = Map::new();
        if git != legacy.automatic_git_fetch_interval {
            overrides.insert("automaticGitFetchInterval".into(), number(git));
        }
        if health != legacy.provider_health_refresh_interval {
            overrides.insert("providerHealthRefreshInterval".into(), number(health));
        }
        return resolve(&json!({
            "schemaVersion": 1,
            "profile": if matches { legacy_profile } else { "custom" },
            "baseProfile": legacy_profile,
            "overrides": overrides,
        }));
    }
    resolve(background_activity)
}

/// `normalizeServerBackgroundActivitySettings`.
pub fn normalize_server(settings: &Value) -> Value {
    let resolved = resolve_server(settings);
    normalize(&json!({
        "schemaVersion": 1,
        "profile": "custom",
        "baseProfile": resolved.profile,
        "overrides": {
            "automaticGitFetchInterval": number(resolved.automatic_git_fetch_interval),
            "providerHealthRefreshInterval": number(resolved.provider_health_refresh_interval),
            "hostPowerMonitorActiveInterval": number(resolved.host_power_monitor_active_interval),
            "hostPowerMonitorIdleInterval": number(resolved.host_power_monitor_idle_interval),
            "idleClientTtl": number(resolved.idle_client_ttl),
            "pauseWhenHostLocked": resolved.pause_when_host_locked,
            "pauseWhenHostLowPower": resolved.pause_when_host_low_power,
            "pauseWhenClientLowPower": resolved.pause_when_client_low_power,
            "pauseWhenOnBattery": resolved.pause_when_on_battery,
        },
    }))
}
