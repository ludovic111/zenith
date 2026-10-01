//! Codex subscription usage (`provider/Layers/codexUsageLimits.ts`). The `account/rateLimits/read`
//! response and the `account/rateLimits/updated` notification carry the same snapshot, so one
//! mapper serves the status probe and the turn-driven update; both emit windows with the same
//! ids so they merge onto the same rows.

use serde::Deserialize;
use serde_json::{json, Map, Value};
use zc_core::time::{parse_iso_millis, try_iso_from_millis};

use crate::errors::CodexAppServerError;

/// An absent key, an explicit `null`, or a value: the snapshot is merged key by key.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Field<T> {
    #[default]
    Absent,
    Null,
    Value(T),
}

impl<T> Field<T> {
    pub fn is_absent(&self) -> bool {
        matches!(self, Field::Absent)
    }
    pub fn value(&self) -> Option<&T> {
        match self {
            Field::Value(value) => Some(value),
            _ => None,
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Field<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Option::<T>::deserialize(deserializer)? {
            Some(value) => Field::Value(value),
            None => Field::Null,
        })
    }
}

/// One window of the snapshot.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CodexRateLimitWindow {
    #[serde(rename = "usedPercent")]
    pub used_percent: f64,
    #[serde(rename = "resetsAt", default)]
    pub resets_at: Option<f64>,
    #[serde(rename = "windowDurationMins", default)]
    pub window_duration_mins: Option<f64>,
}

/// The structural view of the generated `RateLimitSnapshot` both messages satisfy.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct CodexRateLimitSnapshot {
    #[serde(rename = "limitId", default)]
    pub limit_id: Field<String>,
    #[serde(rename = "planType", default)]
    pub plan_type: Field<String>,
    #[serde(rename = "rateLimitReachedType", default)]
    pub rate_limit_reached_type: Field<String>,
    #[serde(default)]
    pub primary: Field<CodexRateLimitWindow>,
    #[serde(default)]
    pub secondary: Field<CodexRateLimitWindow>,
}

impl CodexRateLimitSnapshot {
    /// Reads the snapshot out of a payload (lenient: a mismatching key is skipped).
    pub fn from_value(value: &Value) -> Option<Self> {
        serde_json::from_value(value.clone()).ok()
    }
}

/// The read response's `rateLimitResetCredits`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CodexResetCreditsSummary {
    #[serde(rename = "availableCount")]
    pub available_count: f64,
    #[serde(default)]
    pub credits: Option<Vec<CodexResetCredit>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CodexResetCredit {
    pub status: String,
    #[serde(rename = "expiresAt", default)]
    pub expires_at: Option<f64>,
}

const SESSION_MINS: f64 = 5.0 * 60.0;
const WEEK_MINS: f64 = 7.0 * 24.0 * 60.0;
const MONTH_MINS: f64 = 30.0 * 24.0 * 60.0;

fn iso_from_epoch_seconds(value: Option<f64>) -> Option<String> {
    let value = value?;
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    try_iso_from_millis((value * 1000.0).trunc() as i64)
}

fn kind_for_duration(mins: f64) -> &'static str {
    if mins >= MONTH_MINS {
        "monthly"
    } else if mins >= WEEK_MINS {
        "weekly"
    } else {
        "session"
    }
}

fn label_for_kind(kind: &str) -> &'static str {
    match kind {
        "session" => "Session",
        "weekly" => "Weekly",
        _ => "Monthly",
    }
}

/// `clampPercent`.
pub fn clamp_percent(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 100.0)
    } else {
        0.0
    }
}

/// A JSON number written like JavaScript (integral values without a fraction).
pub(crate) fn js_number(value: f64) -> Value {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9.0e15 {
        #[allow(clippy::cast_possible_truncation)]
        return Value::from(value as i64);
    }
    serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
}

/// `codexRateLimitsToWindows`: positions, not durations; when Codex omits the duration, paid
/// plans have the 5-hour + weekly pair and Free/Go one monthly allowance. Only the main
/// (`codex`) allowance is shown.
fn codex_rate_limits_to_windows(snapshot: &CodexRateLimitSnapshot) -> Vec<Value> {
    if let Some(limit_id) = snapshot.limit_id.value() {
        if !limit_id.is_empty() && limit_id != "codex" {
            return Vec::new();
        }
    }
    let monthly_plan = matches!(snapshot.plan_type.value().map(String::as_str), Some("free" | "go"));
    let positions = [
        ("primary", &snapshot.primary, if monthly_plan { MONTH_MINS } else { SESSION_MINS }),
        ("secondary", &snapshot.secondary, WEEK_MINS),
    ];
    let mut windows = Vec::new();
    for (id, window, fallback) in positions {
        let Some(window) = window.value() else { continue };
        if !window.used_percent.is_finite() {
            continue;
        }
        let duration = window.window_duration_mins.unwrap_or(fallback);
        let kind = kind_for_duration(duration);
        let mut entry = Map::new();
        entry.insert("id".into(), json!(id));
        entry.insert("kind".into(), json!(kind));
        entry.insert("label".into(), json!(label_for_kind(kind)));
        entry.insert("usedPercent".into(), js_number(clamp_percent(window.used_percent)));
        entry.insert("windowDurationMins".into(), js_number(duration));
        if let Some(resets_at) = iso_from_epoch_seconds(window.resets_at) {
            entry.insert("resetsAt".into(), json!(resets_at));
        }
        windows.push(Value::Object(entry));
    }
    windows
}

/// `codexResetCreditsToContract`.
pub fn codex_reset_credits_to_contract(summary: Option<&CodexResetCreditsSummary>) -> Option<Value> {
    let summary = summary?;
    let soonest = summary
        .credits
        .iter()
        .flatten()
        .filter(|credit| credit.status == "available")
        .filter_map(|credit| credit.expires_at)
        .fold(None, |soonest: Option<f64>, value| Some(soonest.map_or(value, |current| current.min(value))));
    let mut credits = Map::new();
    credits.insert("availableCount".into(), js_number(summary.available_count.max(0.0)));
    if let Some(next) = iso_from_epoch_seconds(soonest) {
        credits.insert("nextExpiresAt".into(), json!(next));
    }
    Some(Value::Object(credits))
}

fn window_kind_order(window: &Value) -> u8 {
    match window["kind"].as_str() {
        Some("session") => 0,
        Some("weekly") => 1,
        _ => 2,
    }
}

/// `makeUsageLimits`: windows sorted by kind, then id.
pub fn make_usage_limits(checked_at: &str, mut windows: Vec<Value>) -> Value {
    windows.sort_by(|left, right| {
        window_kind_order(left)
            .cmp(&window_kind_order(right))
            .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
    });
    json!({ "checkedAt": checked_at, "windows": windows })
}

/// `makeUnavailableUsageLimits`.
pub fn make_unavailable_usage_limits(checked_at: &str, reason: &str, message: Option<&str>) -> Value {
    let mut unavailable = Map::new();
    unavailable.insert("reason".into(), json!(reason));
    if let Some(message) = message.filter(|message| !message.is_empty()) {
        unavailable.insert("message".into(), json!(message));
    }
    json!({ "checkedAt": checked_at, "windows": [], "unavailable": unavailable })
}

/// `codexRateLimitsToLimits`: the probe's limits; the main bucket is picked from
/// `rateLimitsByLimitId.codex` when present (the legacy snapshot can name another limit).
pub fn codex_rate_limits_to_limits(
    snapshot: &CodexRateLimitSnapshot,
    rate_limits_by_limit_id: Option<&serde_json::Map<String, Value>>,
    reset_credits: Option<&CodexResetCreditsSummary>,
    checked_at: &str,
) -> Value {
    let main = rate_limits_by_limit_id
        .and_then(|by_id| by_id.get("codex"))
        .and_then(CodexRateLimitSnapshot::from_value);
    let windows = codex_rate_limits_to_windows(main.as_ref().unwrap_or(snapshot));
    let mut limits = make_usage_limits(checked_at, windows);
    if let Some(credits) = codex_reset_credits_to_contract(reset_credits) {
        limits["resetCredits"] = credits;
    }
    limits
}

/// `codexRateLimitsToUpdate`: `{windows}` or nothing.
pub fn codex_rate_limits_to_update(snapshot: &CodexRateLimitSnapshot) -> Option<Value> {
    let windows = codex_rate_limits_to_windows(snapshot);
    (!windows.is_empty()).then(|| json!({ "windows": windows }))
}

/// `codexRateLimitsFailureMessage`: a bounded, client-safe reason.
pub fn codex_rate_limits_failure_message(error: &CodexAppServerError) -> String {
    match error {
        CodexAppServerError::Request(error) => format!("Codex could not read usage (JSON-RPC {}).", error.code),
        CodexAppServerError::Spawn { .. } => "Codex could not be started to read usage.".to_owned(),
        CodexAppServerError::ProcessExited { .. } => "Codex exited before it could report usage.".to_owned(),
        _ => "Codex did not answer the usage request.".to_owned(),
    }
}

/// `mergeCodexRateLimits`: a key the update omits keeps its earlier value; model-specific
/// snapshots (Spark) never replace the main allowance.
pub fn merge_codex_rate_limits(previous: Option<CodexRateLimitSnapshot>, update: &CodexRateLimitSnapshot) -> Option<CodexRateLimitSnapshot> {
    if let Some(limit_id) = update.limit_id.value() {
        if !limit_id.is_empty() && limit_id != "codex" {
            return previous;
        }
    }
    let Some(mut merged) = previous else {
        return Some(update.clone());
    };
    if !update.limit_id.is_absent() {
        merged.limit_id = update.limit_id.clone();
    }
    if !update.plan_type.is_absent() {
        merged.plan_type = update.plan_type.clone();
    }
    if !update.rate_limit_reached_type.is_absent() {
        merged.rate_limit_reached_type = update.rate_limit_reached_type.clone();
    }
    if !update.primary.is_absent() {
        merged.primary = update.primary.clone();
    }
    if !update.secondary.is_absent() {
        merged.secondary = update.secondary.clone();
    }
    Some(merged)
}

/// `formatCodexUsageLimitWait`: `5d 5h`, `3h 20m`, `12m`.
fn format_wait(wait_ms: i64) -> String {
    let total_minutes = (wait_ms + 59_999).div_euclid(60_000);
    let days = total_minutes / (24 * 60);
    let hours = (total_minutes % (24 * 60)) / 60;
    let minutes = total_minutes % 60;
    if days > 0 {
        return if hours == 0 { format!("{days}d") } else { format!("{days}d {hours}h") };
    }
    if hours == 0 {
        return format!("{total_minutes}m");
    }
    if minutes == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h {minutes}m")
    }
}

fn next_step(rate_limit_reached_type: Option<&str>) -> &'static str {
    match rate_limit_reached_type {
        Some("workspace_owner_credits_depleted" | "workspace_member_credits_depleted") => {
            " The workspace has no credits to continue sooner: ask your workspace owner to add credits, or send the message again once the limit resets."
        }
        Some("workspace_owner_usage_limit_reached" | "workspace_member_usage_limit_reached") => {
            " The workspace spend limit is reached: ask your workspace owner to raise it, or send the message again once the limit resets."
        }
        _ => " Send the message again once the limit resets.",
    }
}

/// `codexUsageLimitMessage`: what a usage-limit stop shows instead of the provider sentence. The
/// window named is the exhausted one yet to reset, latest first; `at_iso` is the stopping
/// event's time.
pub fn codex_usage_limit_message(snapshot: Option<&CodexRateLimitSnapshot>, at_iso: &str) -> String {
    let at_ms = parse_iso_millis(at_iso);
    let windows = match (snapshot, at_ms) {
        (Some(snapshot), Some(_)) => codex_rate_limits_to_windows(snapshot),
        _ => Vec::new(),
    };
    let mut reset = String::new();
    let mut latest_reset_ms = i64::MIN;
    if let Some(at_ms) = at_ms {
        for window in &windows {
            if window["usedPercent"].as_f64().unwrap_or(0.0) < 100.0 {
                continue;
            }
            let Some(reset_ms) = window["resetsAt"].as_str().and_then(parse_iso_millis) else {
                continue;
            };
            if reset_ms <= at_ms || reset_ms <= latest_reset_ms {
                continue;
            }
            latest_reset_ms = reset_ms;
            reset = format!(
                " The {} limit resets in {}.",
                window["kind"].as_str().unwrap_or_default(),
                format_wait(reset_ms - at_ms)
            );
        }
    }
    let reached = snapshot.and_then(|snapshot| snapshot.rate_limit_reached_type.value().map(String::as_str));
    format!("Codex usage limit reached.{reset}{}", next_step(reached))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHECKED_AT: &str = "2026-07-18T10:00:00.000Z";

    fn snapshot(value: Value) -> CodexRateLimitSnapshot {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn maps_primary_and_secondary_onto_session_and_weekly() {
        let limits = codex_rate_limits_to_limits(
            &snapshot(
                json!({"planType": "plus", "primary": {"usedPercent": 12, "resetsAt": 1_784_000_000, "windowDurationMins": 300}, "secondary": {"usedPercent": 47, "resetsAt": 1_784_500_000, "windowDurationMins": 10080}}),
            ),
            None,
            None,
            CHECKED_AT,
        );
        assert_eq!(
            limits,
            json!({"checkedAt": CHECKED_AT, "windows": [
                {"id": "primary", "kind": "session", "label": "Session", "usedPercent": 12, "windowDurationMins": 300, "resetsAt": "2026-07-14T03:33:20.000Z"},
                {"id": "secondary", "kind": "weekly", "label": "Weekly", "usedPercent": 47, "windowDurationMins": 10080, "resetsAt": "2026-07-19T22:26:40.000Z"}
            ]})
        );
    }

    #[test]
    fn a_lone_primary_is_monthly_on_free_and_go() {
        let limits = codex_rate_limits_to_limits(
            &snapshot(json!({"planType": "free", "primary": {"usedPercent": 80, "resetsAt": null}})),
            None,
            None,
            CHECKED_AT,
        );
        assert_eq!(
            limits["windows"],
            json!([{"id": "primary", "kind": "monthly", "label": "Monthly", "usedPercent": 80, "windowDurationMins": 43_200}])
        );
    }

    #[test]
    fn selects_the_main_allowance_and_leaves_spark_out() {
        let spark = json!({"limitId": "codex_bengalfox", "primary": {"usedPercent": 0, "windowDurationMins": 300}, "secondary": {"usedPercent": 90, "windowDurationMins": 10080}});
        let by_id = json!({"codex_bengalfox": spark, "codex": {"secondary": {"usedPercent": 42, "windowDurationMins": 10080}}});
        let limits = codex_rate_limits_to_limits(&snapshot(spark.clone()), by_id.as_object(), None, CHECKED_AT);
        assert_eq!(
            limits["windows"],
            json!([{"id": "secondary", "kind": "weekly", "label": "Weekly", "usedPercent": 42, "windowDurationMins": 10080}])
        );
        let empty = Map::new();
        let plain = snapshot(json!({"primary": {"usedPercent": 12, "windowDurationMins": 300}}));
        assert_eq!(
            codex_rate_limits_to_limits(&plain, Some(&empty), None, CHECKED_AT),
            codex_rate_limits_to_limits(&plain, None, None, CHECKED_AT)
        );
        assert_eq!(
            codex_rate_limits_to_limits(
                &snapshot(json!({"limitId": "codex_bengalfox", "secondary": {"usedPercent": 90}})),
                None,
                None,
                CHECKED_AT
            )["windows"],
            json!([])
        );
    }

    #[test]
    fn updates_carry_only_the_named_windows() {
        assert_eq!(
            codex_rate_limits_to_update(&snapshot(json!({"secondary": {"usedPercent": 51, "windowDurationMins": 10080}}))),
            Some(json!({"windows": [{"id": "secondary", "kind": "weekly", "label": "Weekly", "usedPercent": 51, "windowDurationMins": 10080}]}))
        );
        assert_eq!(codex_rate_limits_to_update(&snapshot(json!({"planType": "plus"}))), None);
        assert_eq!(
            codex_rate_limits_to_update(&snapshot(
                json!({"limitId": "codex_bengalfox", "primary": {"usedPercent": 0, "windowDurationMins": 300}})
            )),
            None
        );
        assert!(codex_rate_limits_to_update(&snapshot(
            json!({"limitId": "codex", "secondary": {"usedPercent": 42, "windowDurationMins": 10080}})
        ))
        .is_some());
    }

    #[test]
    fn failure_messages() {
        let request = CodexAppServerError::Request(crate::errors::RequestError::internal_error("GET … 401 Unauthorized"));
        assert_eq!(codex_rate_limits_failure_message(&request), "Codex could not read usage (JSON-RPC -32603).");
        assert_eq!(
            codex_rate_limits_failure_message(&CodexAppServerError::ProcessExited { code: Some(1), pid: None }),
            "Codex exited before it could report usage."
        );
    }

    #[test]
    fn reset_credits() {
        let summary: CodexResetCreditsSummary = serde_json::from_value(json!({"availableCount": 2, "credits": [
            {"status": "available", "expiresAt": 1_784_500_000}, {"status": "redeemed", "expiresAt": 1_700_000_000}, {"status": "available", "expiresAt": 1_784_000_000}
        ]}))
        .unwrap();
        assert_eq!(
            codex_reset_credits_to_contract(Some(&summary)),
            Some(json!({"availableCount": 2, "nextExpiresAt": "2026-07-14T03:33:20.000Z"}))
        );
        let zero: CodexResetCreditsSummary = serde_json::from_value(json!({"availableCount": 0})).unwrap();
        assert_eq!(codex_reset_credits_to_contract(Some(&zero)), Some(json!({"availableCount": 0})));
        assert_eq!(codex_reset_credits_to_contract(None), None);
        let one: CodexResetCreditsSummary = serde_json::from_value(json!({"availableCount": 1})).unwrap();
        assert_eq!(
            codex_rate_limits_to_limits(
                &snapshot(json!({"primary": {"usedPercent": 5, "windowDurationMins": 300}})),
                None,
                Some(&one),
                CHECKED_AT
            )["resetCredits"],
            json!({"availableCount": 1})
        );
    }

    #[test]
    fn usage_limit_messages() {
        let at = "2026-01-01T00:00:00.000Z";
        let at_seconds = 1_767_225_600_i64;
        assert_eq!(
            codex_usage_limit_message(
                Some(&snapshot(json!({"limitId": "codex", "rateLimitReachedType": "workspace_owner_credits_depleted",
                    "primary": {"usedPercent": 40, "resetsAt": at_seconds + 3_600, "windowDurationMins": 300},
                    "secondary": {"usedPercent": 100, "resetsAt": at_seconds + 5 * 86_400 + 5 * 3_600, "windowDurationMins": 10_080}}))),
                at
            ),
            "Codex usage limit reached. The weekly limit resets in 5d 5h. The workspace has no credits to continue sooner: ask your workspace owner to add credits, or send the message again once the limit resets."
        );
        assert_eq!(
            codex_usage_limit_message(
                Some(&snapshot(json!({"limitId": "codex", "rateLimitReachedType": "workspace_member_usage_limit_reached",
                    "primary": {"usedPercent": 100, "resetsAt": at_seconds + 3 * 3_600 + 20 * 60, "windowDurationMins": 300}}))),
                at
            ),
            "Codex usage limit reached. The session limit resets in 3h 20m. The workspace spend limit is reached: ask your workspace owner to raise it, or send the message again once the limit resets."
        );
        assert_eq!(
            codex_usage_limit_message(Some(&snapshot(json!({"limitId": "codex", "rateLimitReachedType": "workspace_member_credits_depleted"}))), at),
            "Codex usage limit reached. The workspace has no credits to continue sooner: ask your workspace owner to add credits, or send the message again once the limit resets."
        );
        assert_eq!(
            codex_usage_limit_message(None, at),
            "Codex usage limit reached. Send the message again once the limit resets."
        );
    }

    #[test]
    fn merge_keeps_windows_an_update_omits() {
        let merged = merge_codex_rate_limits(
            Some(snapshot(
                json!({"limitId": "codex", "planType": "business", "primary": {"usedPercent": 100, "resetsAt": 1_800_000_000, "windowDurationMins": 300}}),
            )),
            &snapshot(json!({"rateLimitReachedType": "rate_limit_reached"})),
        )
        .unwrap();
        assert_eq!(merged.rate_limit_reached_type, Field::Value("rate_limit_reached".into()));
        assert_eq!(merged.plan_type, Field::Value("business".into()));
        assert!(merged.primary.value().is_some());
        let main = snapshot(json!({"limitId": "codex", "primary": {"usedPercent": 100, "resetsAt": 1_800_000_000, "windowDurationMins": 300}}));
        assert_eq!(
            merge_codex_rate_limits(Some(main.clone()), &snapshot(json!({"limitId": "spark", "primary": {"usedPercent": 3}}))),
            Some(main)
        );
    }
}
