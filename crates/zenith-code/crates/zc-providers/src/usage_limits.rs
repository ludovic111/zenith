//! Port of `provider/providerUsageLimits.ts`: folding runtime rate-limit updates and probe
//! results into the usage limits a provider snapshot publishes.

use zc_contracts::{
    JsNumber, LenientVec, ProviderUsageLimitsUpdate, ServerProviderUsageLimits, ServerProviderUsageLimitsUnavailable,
    ServerProviderUsageLimitsUnavailableReason, ServerProviderUsageWindow, ServerProviderUsageWindowKind,
};

fn kind_rank(kind: ServerProviderUsageWindowKind) -> u8 {
    match kind {
        ServerProviderUsageWindowKind::Session => 0,
        ServerProviderUsageWindowKind::Weekly => 1,
        ServerProviderUsageWindowKind::Monthly => 2,
        ServerProviderUsageWindowKind::Other => 3,
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

fn sort_windows(windows: impl IntoIterator<Item = ServerProviderUsageWindow>) -> Vec<ServerProviderUsageWindow> {
    let mut windows: Vec<_> = windows.into_iter().collect();
    windows.sort_by(|left, right| kind_rank(left.kind).cmp(&kind_rank(right.kind)).then_with(|| left.id.cmp(&right.id)));
    windows
}

/// `makeUsageLimits`.
pub fn make_usage_limits(checked_at: &str, windows: impl IntoIterator<Item = ServerProviderUsageWindow>) -> ServerProviderUsageLimits {
    ServerProviderUsageLimits {
        checked_at: checked_at.to_owned(),
        windows: LenientVec(sort_windows(windows)),
        reset_credits: None,
        external_usage: None,
        unavailable: None,
    }
}

/// `makeUnavailableUsageLimits`.
pub fn make_unavailable_usage_limits(checked_at: &str, reason: ServerProviderUsageLimitsUnavailableReason, message: Option<&str>) -> ServerProviderUsageLimits {
    ServerProviderUsageLimits {
        checked_at: checked_at.to_owned(),
        windows: LenientVec(Vec::new()),
        reset_credits: None,
        external_usage: None,
        unavailable: Some(ServerProviderUsageLimitsUnavailable {
            reason,
            message: message.filter(|message| !message.is_empty()).map(str::to_owned),
        }),
    }
}

fn windows_equal(a: &ServerProviderUsageWindow, b: &ServerProviderUsageWindow) -> bool {
    a.id == b.id
        && a.kind == b.kind
        && a.label == b.label
        && a.used_percent.0 == b.used_percent.0
        && a.resets_at == b.resets_at
        && a.window_duration_mins == b.window_duration_mins
}

/// `applyUsageLimitsUpdate`: windows upsert by id; omitted windows keep their values; a window
/// without `resetsAt` / `windowDurationMins` keeps the previous ones. `None` means "unchanged"
/// (the TS function returns the same object).
pub fn apply_usage_limits_update(
    previous: Option<&ServerProviderUsageLimits>,
    update: &ProviderUsageLimitsUpdate,
    checked_at: &str,
) -> Option<ServerProviderUsageLimits> {
    let unsupported = previous
        .and_then(|previous| previous.unavailable.as_ref())
        .is_some_and(|unavailable| unavailable.reason == ServerProviderUsageLimitsUnavailableReason::Unsupported);
    if update.windows.is_empty() || unsupported {
        return None;
    }
    let mut merged: Vec<ServerProviderUsageWindow> = previous.map(|previous| previous.windows.0.clone()).unwrap_or_default();
    let mut changed = false;
    for window in &update.windows {
        let existing = merged.iter().position(|candidate| candidate.id == window.id);
        let mut next = window.clone();
        next.used_percent = JsNumber(clamp_percent(window.used_percent.0));
        if let Some(index) = existing {
            let existing = &merged[index];
            if next.resets_at.is_none() && existing.resets_at.is_some() {
                next.resets_at = existing.resets_at.clone();
            }
            if next.window_duration_mins.is_none() && existing.window_duration_mins.is_some() {
                next.window_duration_mins = existing.window_duration_mins;
            }
            if !windows_equal(existing, &next) {
                merged[index] = next;
                changed = true;
            }
        } else {
            merged.push(next);
            changed = true;
        }
    }
    if !changed && previous.is_some_and(|previous| previous.unavailable.is_none()) {
        return None;
    }
    let mut limits = make_usage_limits(checked_at, merged);
    limits.reset_credits = previous.and_then(|previous| previous.reset_credits.clone());
    Some(limits)
}

/// `resolveUsageLimitsAfterProbe`: a failed probe keeps the last good bars; `unsupported`
/// replaces them.
pub fn resolve_usage_limits_after_probe(
    published: Option<&ServerProviderUsageLimits>,
    probed: Option<&ServerProviderUsageLimits>,
) -> Option<ServerProviderUsageLimits> {
    let probe_failed = probed
        .and_then(|probed| probed.unavailable.as_ref())
        .is_some_and(|unavailable| unavailable.reason == ServerProviderUsageLimitsUnavailableReason::ProbeFailed);
    if probe_failed {
        if let Some(published) = published.filter(|published| published.unavailable.is_none()) {
            return Some(published.clone());
        }
    }
    probed.cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn window(value: serde_json::Value) -> ServerProviderUsageWindow {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn merges_sparse_updates() {
        let previous = make_usage_limits(
            "t0",
            [window(
                json!({"id": "weekly", "kind": "weekly", "label": "Weekly", "usedPercent": 10, "resetsAt": "2026-01-08T00:00:00.000Z"}),
            )],
        );
        let update = ProviderUsageLimitsUpdate {
            windows: vec![
                window(json!({"id": "weekly", "kind": "weekly", "label": "Weekly", "usedPercent": 150})),
                window(json!({"id": "session", "kind": "session", "label": "Session", "usedPercent": 5})),
            ],
        };
        let merged = apply_usage_limits_update(Some(&previous), &update, "t1").unwrap();
        assert_eq!(
            serde_json::to_value(&merged).unwrap(),
            json!({"checkedAt": "t1", "windows": [
                {"id": "session", "kind": "session", "label": "Session", "usedPercent": 5},
                {"id": "weekly", "kind": "weekly", "label": "Weekly", "usedPercent": 100, "resetsAt": "2026-01-08T00:00:00.000Z"}
            ]})
        );
        assert!(apply_usage_limits_update(Some(&merged), &update, "t2").is_none());
        let unsupported = make_unavailable_usage_limits("t", ServerProviderUsageLimitsUnavailableReason::Unsupported, None);
        assert!(apply_usage_limits_update(Some(&unsupported), &update, "t3").is_none());
    }

    #[test]
    fn failed_probes_keep_the_last_good_bars() {
        let good = make_usage_limits("t0", []);
        let failed = make_unavailable_usage_limits("t1", ServerProviderUsageLimitsUnavailableReason::ProbeFailed, Some("timeout"));
        assert_eq!(resolve_usage_limits_after_probe(Some(&good), Some(&failed)), Some(good.clone()));
        assert_eq!(resolve_usage_limits_after_probe(None, Some(&failed)), Some(failed));
    }
}
