//! `provider/Layers/claudeUsageLimits.ts` (+ the `providerUsageLimits.ts` constructors it uses):
//! Claude Code subscription windows from the status probe's `get_usage` control request and from
//! the `rate_limit_event`s streamed during a turn. Both produce windows with the same ids, so a
//! mid-turn event lands on the row the probe drew.
//!
//! Values are the contract JSON (`ServerProviderUsageLimits`, `ProviderUsageLimitsUpdate`).

use std::sync::{Arc, RwLock};

use serde_json::{json, Map, Value};

use crate::skills::locale_compare;

const SESSION_MINS: i64 = 5 * 60;
const WEEK_MINS: i64 = 7 * 24 * 60;
const OVERAGE_INCLUDED_EVENT_TYPE: &str = "seven_day_overage_included";

/// A JS number as JSON: integral values without a fraction, like `JSON.stringify`.
pub fn js_number(value: f64) -> Value {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9.007_199_254_740_992e15 {
        Value::from(value as i64)
    } else {
        serde_json::Number::from_f64(value).map(Value::Number).unwrap_or(Value::Null)
    }
}

/// `ClaudeScopedLimitNames`: the model-scoped weekly bucket the last probe named.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeScopedLimitNames {
    pub overage_included: Option<String>,
}

/// Shared between an instance's status probe (writer) and its adapter (reader).
pub type ScopedLimitNamesRef = Arc<RwLock<ClaudeScopedLimitNames>>;

/// `makeClaudeScopedLimitNames`.
pub fn make_scoped_limit_names() -> ScopedLimitNamesRef {
    Arc::new(RwLock::new(ClaudeScopedLimitNames::default()))
}

/// `clampPercent`.
pub fn clamp_percent(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 100.0)
    } else {
        0.0
    }
}

fn window_kind_order(kind: &str) -> u8 {
    match kind {
        "session" => 0,
        "weekly" => 1,
        "monthly" => 2,
        _ => 3,
    }
}

/// `makeUsageLimits`: windows sorted by kind, then id.
pub fn make_usage_limits(checked_at: &str, mut windows: Vec<Value>) -> Value {
    windows.sort_by(|a, b| {
        let kind = |w: &Value| window_kind_order(w.get("kind").and_then(Value::as_str).unwrap_or(""));
        let id = |w: &Value| w.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        kind(a).cmp(&kind(b)).then_with(|| locale_compare(&id(a), &id(b)))
    });
    json!({ "checkedAt": checked_at, "windows": windows })
}

/// `makeUnavailableUsageLimits`.
pub fn make_unavailable_usage_limits(checked_at: &str, reason: &str, message: Option<&str>) -> Value {
    let mut unavailable = Map::new();
    unavailable.insert("reason".into(), Value::String(reason.into()));
    if let Some(message) = message.filter(|m| !m.is_empty()) {
        unavailable.insert("message".into(), Value::String(message.into()));
    }
    json!({ "checkedAt": checked_at, "windows": [], "unavailable": unavailable })
}

/// `isoFromEpochSeconds`.
pub fn iso_from_epoch_seconds(value: Option<f64>) -> Option<String> {
    let value = value.filter(|v| v.is_finite() && *v > 0.0)?;
    zc_core::time::try_iso_from_millis((value * 1000.0).trunc() as i64)
}

/// `isoFromString` (`new Date(string)`): ISO instants and bare dates.
pub fn iso_from_string(value: Option<&str>) -> Option<String> {
    let value = value.filter(|v| !v.is_empty())?;
    zc_core::time::normalize_iso(value).or_else(|| {
        let bytes = value.as_bytes();
        (bytes.len() == 10 && bytes[4] == b'-' && bytes[7] == b'-')
            .then(|| zc_core::time::normalize_iso(&format!("{value}T00:00:00.000Z")))
            .flatten()
    })
}

fn make_window(id: &str, used_percent: f64, resets_at: Option<String>) -> Option<Value> {
    let (kind, label, duration) = match id {
        "five_hour" => ("session", "Session", SESSION_MINS),
        "seven_day" => ("weekly", "Weekly", WEEK_MINS),
        _ => return None,
    };
    let mut window = Map::new();
    window.insert("id".into(), Value::String(id.into()));
    window.insert("kind".into(), Value::String(kind.into()));
    window.insert("label".into(), Value::String(label.into()));
    window.insert("windowDurationMins".into(), Value::from(duration));
    window.insert("usedPercent".into(), js_number(clamp_percent(used_percent)));
    if let Some(resets_at) = resets_at {
        window.insert("resetsAt".into(), Value::String(resets_at));
    }
    Some(Value::Object(window))
}

fn scoped_window_id(display_name: &str) -> String {
    let lower = display_name.to_lowercase();
    let mut id = String::from("seven_day_");
    let mut in_run = false;
    for c in lower.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            id.push(c);
            in_run = false;
        } else if !in_run {
            id.push('_');
            in_run = true;
        }
    }
    id
}

fn scoped_window(display_name: &str, used_percent: f64, resets_at: Option<String>) -> Value {
    let mut window = Map::new();
    window.insert("id".into(), Value::String(scoped_window_id(display_name)));
    window.insert("kind".into(), Value::String("weekly".into()));
    window.insert("label".into(), Value::String(format!("Weekly · {display_name}")));
    window.insert("windowDurationMins".into(), Value::from(WEEK_MINS));
    window.insert("usedPercent".into(), js_number(clamp_percent(used_percent)));
    if let Some(resets_at) = resets_at {
        window.insert("resetsAt".into(), Value::String(resets_at));
    }
    Value::Object(window)
}

/// `claudeRateLimitEventToUpdate(info, names)`: the `ProviderUsageLimitsUpdate` for one
/// streamed `rate_limit_info`, or `None` (unknown window, or an overage-included bucket no probe
/// has named yet).
pub fn claude_rate_limit_event_to_update(info: &Value, names: &ClaudeScopedLimitNames) -> Option<Value> {
    let kind = info.get("rateLimitType").and_then(Value::as_str).filter(|k| !k.is_empty())?;
    let utilization = info.get("utilization").and_then(Value::as_f64)?;
    let used_percent = utilization * 100.0;
    let resets_at = iso_from_epoch_seconds(info.get("resetsAt").and_then(Value::as_f64));
    if let Some(window) = make_window(kind, used_percent, resets_at.clone()) {
        return Some(json!({ "windows": [window] }));
    }
    if kind == OVERAGE_INCLUDED_EVENT_TYPE {
        if let Some(name) = names.overage_included.as_deref() {
            return Some(json!({ "windows": [scoped_window(name, used_percent, resets_at)] }));
        }
    }
    None
}

/// `claudeUsageResponseToLimits`: the `get_usage` response (0–100 percentages, ISO resets) as
/// `ServerProviderUsageLimits`, plus the scoped bucket names it carried.
pub fn claude_usage_response_to_limits(response: &Value, checked_at: &str) -> (Value, ClaudeScopedLimitNames) {
    let available = response.get("rate_limits_available").and_then(Value::as_bool).unwrap_or(false);
    let rate_limits = response.get("rate_limits").filter(|v| v.is_object());
    let (true, Some(rate_limits)) = (available, rate_limits) else {
        return (
            make_unavailable_usage_limits(checked_at, "unsupported", None),
            ClaudeScopedLimitNames::default(),
        );
    };
    let mut windows = Vec::new();
    for id in ["five_hour", "seven_day"] {
        let Some(window) = rate_limits.get(id).filter(|w| w.is_object()) else {
            continue;
        };
        let Some(utilization) = window.get("utilization").and_then(Value::as_f64) else {
            continue;
        };
        windows.extend(make_window(id, utilization, iso_from_string(window.get("resets_at").and_then(Value::as_str))));
    }
    let mut overage_included = None;
    for entry in rate_limits.get("model_scoped").and_then(Value::as_array).into_iter().flatten() {
        let Some(display_name) = entry.get("display_name").and_then(Value::as_str) else {
            continue;
        };
        let Some(utilization) = entry.get("utilization").and_then(Value::as_f64) else {
            continue;
        };
        windows.push(scoped_window(
            display_name,
            utilization,
            iso_from_string(entry.get("resets_at").and_then(Value::as_str)),
        ));
        overage_included.get_or_insert_with(|| display_name.to_string());
    }
    (make_usage_limits(checked_at, windows), ClaudeScopedLimitNames { overage_included })
}

/// `recordClaudeUsageResponse`: map the response and remember the names for the adapter.
pub fn record_claude_usage_response(names: &ScopedLimitNamesRef, response: &Value, checked_at: &str) -> Value {
    let (limits, scoped) = claude_usage_response_to_limits(response, checked_at);
    if let Ok(mut guard) = names.write() {
        *guard = scoped;
    }
    limits
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHECKED_AT: &str = "2026-03-01T00:00:00.000Z";

    #[test]
    fn maps_get_usage_windows_and_scoped_buckets() {
        let (limits, names) = claude_usage_response_to_limits(
            &json!({
                "rate_limits_available": true,
                "rate_limits": {
                    "seven_day": {"utilization": 30, "resets_at": "2026-03-05T00:00:00Z"},
                    "five_hour": {"utilization": 120, "resets_at": null},
                    "model_scoped": [{"display_name": "Fable", "utilization": 12.5, "resets_at": "2026-03-06T00:00:00.000Z"}, {"display_name": "Other", "utilization": null, "resets_at": null}]
                }
            }),
            CHECKED_AT,
        );
        assert_eq!(names.overage_included.as_deref(), Some("Fable"));
        assert_eq!(
            limits,
            json!({"checkedAt": CHECKED_AT, "windows": [
                {"id": "five_hour", "kind": "session", "label": "Session", "windowDurationMins": 300, "usedPercent": 100},
                {"id": "seven_day", "kind": "weekly", "label": "Weekly", "windowDurationMins": 10080, "usedPercent": 30, "resetsAt": "2026-03-05T00:00:00.000Z"},
                {"id": "seven_day_fable", "kind": "weekly", "label": "Weekly · Fable", "windowDurationMins": 10080, "usedPercent": 12.5, "resetsAt": "2026-03-06T00:00:00.000Z"}
            ]})
        );
    }

    #[test]
    fn reports_unsupported_without_rate_limits() {
        let (limits, names) = claude_usage_response_to_limits(&json!({"rate_limits_available": false}), CHECKED_AT);
        assert_eq!(
            limits,
            json!({"checkedAt": CHECKED_AT, "windows": [], "unavailable": {"reason": "unsupported"}})
        );
        assert_eq!(names, ClaudeScopedLimitNames::default());
    }

    #[test]
    fn maps_streamed_events_onto_the_same_rows() {
        let names = ClaudeScopedLimitNames::default();
        assert_eq!(
            claude_rate_limit_event_to_update(&json!({"rateLimitType": "five_hour", "utilization": 0.25, "resetsAt": 1_772_323_200}), &names),
            Some(
                json!({"windows": [{"id": "five_hour", "kind": "session", "label": "Session", "windowDurationMins": 300, "usedPercent": 25, "resetsAt": "2026-03-01T00:00:00.000Z"}]})
            )
        );
        assert_eq!(
            claude_rate_limit_event_to_update(&json!({"rateLimitType": "seven_day_overage_included", "utilization": 0.4}), &names),
            None
        );
        let named = ClaudeScopedLimitNames {
            overage_included: Some("Fable".into()),
        };
        assert_eq!(
            claude_rate_limit_event_to_update(&json!({"rateLimitType": "seven_day_overage_included", "utilization": 0.4}), &named),
            Some(json!({"windows": [{"id": "seven_day_fable", "kind": "weekly", "label": "Weekly · Fable", "usedPercent": 40, "windowDurationMins": 10080}]}))
        );
        assert_eq!(
            claude_rate_limit_event_to_update(&json!({"rateLimitType": "seven_day_opus", "utilization": 0.4}), &named),
            None
        );
        assert_eq!(claude_rate_limit_event_to_update(&json!({"rateLimitType": "five_hour"}), &named), None);
    }

    // Ports of `claudeUsageLimits.test.ts`, case by case.

    const TS_CHECKED_AT: &str = "2026-07-18T10:00:00.000Z";

    #[test]
    fn maps_the_session_weekly_and_model_scoped_weekly_windows() {
        let (limits, names) = claude_usage_response_to_limits(
            &json!({
                "rate_limits_available": true,
                "rate_limits": {
                    "five_hour": {"utilization": 54, "resets_at": "2026-07-18T14:39:00Z"},
                    "seven_day": {"utilization": 18.4, "resets_at": "2026-07-24T08:59:00+00:00"},
                    "seven_day_opus": {"utilization": 3, "resets_at": null},
                    "model_scoped": [
                        {"display_name": "Fable", "utilization": 73, "resets_at": "2026-07-24T08:59:00Z"},
                        {"display_name": "Ghost", "utilization": null, "resets_at": null}
                    ],
                    "extra_usage": {"is_enabled": false, "monthly_limit": null, "used_credits": null, "utilization": null}
                }
            }),
            TS_CHECKED_AT,
        );
        assert_eq!(
            names,
            ClaudeScopedLimitNames {
                overage_included: Some("Fable".into())
            }
        );
        assert_eq!(
            limits,
            json!({"checkedAt": TS_CHECKED_AT, "windows": [
                {"id": "five_hour", "kind": "session", "label": "Session", "usedPercent": 54, "windowDurationMins": 300, "resetsAt": "2026-07-18T14:39:00.000Z"},
                {"id": "seven_day", "kind": "weekly", "label": "Weekly", "usedPercent": 18.4, "windowDurationMins": 10080, "resetsAt": "2026-07-24T08:59:00.000Z"},
                {"id": "seven_day_fable", "kind": "weekly", "label": "Weekly · Fable", "usedPercent": 73, "windowDurationMins": 10080, "resetsAt": "2026-07-24T08:59:00.000Z"}
            ]})
        );
    }

    #[test]
    fn names_the_overage_included_bucket_only_from_a_scoped_entry_that_drew_a_row() {
        let (_, names) = claude_usage_response_to_limits(
            &json!({
                "rate_limits_available": true,
                "rate_limits": {
                    "model_scoped": [
                        {"display_name": "Ghost", "utilization": null, "resets_at": null},
                        {"display_name": "Fable", "utilization": 5, "resets_at": null}
                    ]
                }
            }),
            TS_CHECKED_AT,
        );
        assert_eq!(
            names,
            ClaudeScopedLimitNames {
                overage_included: Some("Fable".into())
            }
        );
    }

    #[test]
    fn reports_api_key_and_bedrock_accounts_as_unsupported() {
        let (limits, _) = claude_usage_response_to_limits(&json!({"rate_limits_available": false, "rate_limits": null}), TS_CHECKED_AT);
        assert_eq!(
            limits,
            json!({"checkedAt": TS_CHECKED_AT, "windows": [], "unavailable": {"reason": "unsupported"}})
        );
    }

    #[test]
    fn skips_a_window_the_endpoint_reports_without_a_utilization() {
        let (limits, _) = claude_usage_response_to_limits(
            &json!({
                "rate_limits_available": true,
                "rate_limits": {
                    "five_hour": {"utilization": null, "resets_at": null},
                    "seven_day": {"utilization": 250, "resets_at": null}
                }
            }),
            TS_CHECKED_AT,
        );
        assert_eq!(
            limits["windows"],
            json!([{"id": "seven_day", "kind": "weekly", "label": "Weekly", "usedPercent": 100, "windowDurationMins": 10080}])
        );
    }

    #[test]
    fn scales_the_fractional_utilization_and_epoch_second_reset_onto_the_probe_window_id() {
        assert_eq!(
            claude_rate_limit_event_to_update(
                &json!({"status": "allowed_warning", "rateLimitType": "seven_day", "utilization": 0.85, "resetsAt": 1_784_000_000}),
                &ClaudeScopedLimitNames::default(),
            ),
            Some(
                json!({"windows": [{"id": "seven_day", "kind": "weekly", "label": "Weekly", "usedPercent": 85, "windowDurationMins": 10080, "resetsAt": "2026-07-14T03:33:20.000Z"}]})
            )
        );
    }

    #[test]
    fn lands_the_streamed_overage_included_bucket_on_the_row_the_probe_named() {
        let event = json!({"status": "allowed", "rateLimitType": "seven_day_overage_included", "utilization": 0.4});
        // No probe has named the bucket yet: guessing would open a stray row.
        assert_eq!(claude_rate_limit_event_to_update(&event, &ClaudeScopedLimitNames::default()), None);
        assert_eq!(
            claude_rate_limit_event_to_update(
                &event,
                &ClaudeScopedLimitNames {
                    overage_included: Some("Fable".into())
                }
            ),
            Some(json!({"windows": [{"id": "seven_day_fable", "kind": "weekly", "label": "Weekly · Fable", "usedPercent": 40, "windowDurationMins": 10080}]}))
        );
    }

    #[test]
    fn ignores_windows_the_page_does_not_render_and_events_without_a_utilization() {
        let names = ClaudeScopedLimitNames::default();
        assert_eq!(
            claude_rate_limit_event_to_update(&json!({"status": "allowed", "rateLimitType": "seven_day_opus", "utilization": 0.1}), &names),
            None
        );
        assert_eq!(
            claude_rate_limit_event_to_update(&json!({"status": "rejected", "rateLimitType": "five_hour"}), &names),
            None
        );
    }
}
