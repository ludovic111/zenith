//! `orchestration/ThreadSettlementPolicy.ts`: when an idle thread settles on its own. Shells
//! are wire `OrchestrationThreadShell` JSON.

use serde_json::Value;

use crate::js::{parse_date_millis, str_of};

const DAY_MS: i64 = 24 * 60 * 60 * 1_000;
const QUEUED_TURN_START_GRACE_MS: i64 = 2 * 60 * 1_000;

/// `SettlementPullRequest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementPullRequest {
    /// `open | closed | merged`.
    pub state: String,
    pub closed_at: Option<String>,
    pub merged_at: Option<String>,
    pub updated_at: Option<String>,
}

impl SettlementPullRequest {
    pub fn new(state: &str, closed_at: Option<&str>, merged_at: Option<&str>) -> Self {
        Self {
            state: state.to_owned(),
            closed_at: closed_at.map(str::to_owned),
            merged_at: merged_at.map(str::to_owned),
            updated_at: None,
        }
    }
}

fn string_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// `latestTimestamp(values)`: the value with the latest `Date.parse` (unparseable ones lose).
fn latest_timestamp<'a>(values: &[Option<&'a str>]) -> Option<&'a str> {
    let mut latest: Option<&str> = None;
    let mut latest_ms = f64::NEG_INFINITY;
    for value in values.iter().flatten() {
        let ms = parse_date_millis(value).map(|ms| ms as f64).unwrap_or(f64::NAN);
        if ms > latest_ms {
            latest = Some(value);
            latest_ms = ms;
        }
    }
    latest
}

fn latest_turn_field<'a>(thread: &'a Value, key: &str) -> Option<&'a str> {
    thread.get("latestTurn").and_then(|turn| string_at(turn, key))
}

/// `threadHasQueuedTurnStart(thread, now)`: a recent user message no turn adopted yet.
pub fn thread_has_queued_turn_start(thread: &Value, now: &str) -> bool {
    let Some(latest_user_message_at) = string_at(thread, "latestUserMessageAt") else {
        return false;
    };
    if thread.get("session").and_then(|session| string_at(session, "status")) == Some("error") {
        return false;
    }
    let (Some(message_at), Some(now_ms)) = (parse_date_millis(latest_user_message_at), parse_date_millis(now)) else {
        return false;
    };
    if (now_ms - message_at).abs() > QUEUED_TURN_START_GRACE_MS {
        return false;
    }
    if thread.get("latestTurn").is_none_or(Value::is_null) {
        return true;
    }
    ["requestedAt", "startedAt", "completedAt"]
        .iter()
        .all(|key| match latest_turn_field(thread, key) {
            None => true,
            Some(value) => parse_date_millis(value).is_some_and(|ms| ms < message_at),
        })
}

fn pull_request_settles(thread: &Value, pull_request: &SettlementPullRequest, auto_settle_on_merge: bool) -> bool {
    if pull_request.state != "closed" && (pull_request.state != "merged" || !auto_settle_on_merge) {
        return false;
    }
    let terminal_at = if pull_request.state == "merged" {
        &pull_request.merged_at
    } else {
        &pull_request.closed_at
    };
    let Some(terminal_at) = terminal_at else { return false };
    let user_anchor = latest_timestamp(&[
        string_at(thread, "createdAt"),
        string_at(thread, "latestUserMessageAt"),
        latest_turn_field(thread, "requestedAt"),
    ]);
    let Some(user_anchor) = user_anchor else { return false };
    match (parse_date_millis(terminal_at), parse_date_millis(user_anchor)) {
        (Some(pull_request_at), Some(anchor_at)) => pull_request_at >= anchor_at,
        _ => false,
    }
}

/// `visibleThreadPullRequests(links)`.
pub fn visible_thread_pull_requests(thread: &Value) -> Vec<&Value> {
    thread
        .get("pullRequests")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|link| string_at(link, "source") != Some("stack-dismissed"))
        .collect()
}

/// `resolveAutoSettlementAt({thread, pullRequest, now, autoSettleAfterDays, autoSettleOnMerge})`:
/// when the thread settles, or `None`.
pub fn resolve_auto_settlement_at(
    thread: &Value,
    pull_request: Option<&SettlementPullRequest>,
    now: &str,
    auto_settle_after_days: Option<f64>,
    auto_settle_on_merge: bool,
) -> Option<String> {
    let mut pull_request = pull_request.cloned();
    let links = visible_thread_pull_requests(thread);
    let snapshot_of = |link: &Value| link.get("snapshot").filter(|snapshot| !snapshot.is_null()).cloned();
    if links
        .iter()
        .any(|link| snapshot_of(link).is_none_or(|snapshot| string_at(&snapshot, "state") == Some("open")))
    {
        return None;
    }
    if !links.is_empty() {
        let terminal_timestamp = |link: &Value| -> f64 {
            let snapshot = snapshot_of(link);
            let value = snapshot.as_ref().and_then(|snapshot| {
                if string_at(snapshot, "state") == Some("merged") {
                    string_at(snapshot, "mergedAt")
                } else {
                    string_at(snapshot, "closedAt")
                }
            });
            value.and_then(parse_date_millis).map(|ms| ms as f64).unwrap_or(f64::NEG_INFINITY)
        };
        let mut latest = links[0];
        for candidate in &links[1..] {
            if terminal_timestamp(candidate) > terminal_timestamp(latest) {
                latest = candidate;
            }
        }
        pull_request = snapshot_of(latest).map(|snapshot| SettlementPullRequest {
            state: string_at(&snapshot, "state").unwrap_or("").to_owned(),
            merged_at: string_at(&snapshot, "mergedAt").map(str::to_owned),
            closed_at: string_at(&snapshot, "closedAt").map(str::to_owned),
            updated_at: None,
        });
    }
    if !is_auto_settlement_candidate(thread, now) {
        return None;
    }
    let activity_at = latest_timestamp(&[
        string_at(thread, "latestUserMessageAt"),
        latest_turn_field(thread, "requestedAt"),
        latest_turn_field(thread, "startedAt"),
        latest_turn_field(thread, "completedAt"),
    ]);
    if let Some(pull_request) = &pull_request {
        if pull_request_settles(thread, pull_request, auto_settle_on_merge) {
            return activity_at.or_else(|| string_at(thread, "createdAt")).map(str::to_owned);
        }
    }
    let (Some(days), Some(activity_at)) = (auto_settle_after_days, activity_at) else {
        return None;
    };
    let activity_ms = parse_date_millis(activity_at)? as f64;
    let now_ms = parse_date_millis(now)? as f64;
    (activity_ms < now_ms - days * DAY_MS as f64).then(|| activity_at.to_owned())
}

/// `isAutoSettlementCandidate(thread, now)`: the cheap checks before any source control lookup.
pub fn is_auto_settlement_candidate(thread: &Value, now: &str) -> bool {
    let is_set = |key: &str| thread.get(key).is_some_and(|value| !value.is_null());
    if is_set("archivedAt") || is_set("settledOverride") || is_set("autoSettleDisabledAt") {
        return false;
    }
    if thread.get("hasPendingApprovals") == Some(&Value::Bool(true)) || thread.get("hasPendingUserInput") == Some(&Value::Bool(true)) {
        return false;
    }
    let session_status = thread.get("session").and_then(|session| string_at(session, "status"));
    if matches!(session_status, Some("starting" | "running")) {
        return false;
    }
    if is_set("backgroundLiveness") {
        return false;
    }
    if thread_has_queued_turn_start(thread, now) {
        return false;
    }
    let now_ms = parse_date_millis(now);
    let snoozed_until = str_of(thread, "snoozedUntil");
    let woke = match (snoozed_until.and_then(parse_date_millis), now_ms) {
        _ if snoozed_until.is_none() => true,
        (Some(until), Some(now_ms)) => until <= now_ms,
        _ => false,
    };
    if woke {
        return true;
    }
    let snoozed_at = str_of(thread, "snoozedAt");
    let woke_on_error = session_status == Some("error")
        && (snoozed_at.is_none()
            || match (
                thread
                    .get("session")
                    .and_then(|session| string_at(session, "updatedAt"))
                    .and_then(parse_date_millis),
                snoozed_at.and_then(parse_date_millis),
            ) {
                (Some(updated), Some(snoozed)) => updated > snoozed,
                _ => false,
            });
    let woke_on_completion = snoozed_at.is_some()
        && latest_turn_field(thread, "state") == Some("completed")
        && match (
            latest_turn_field(thread, "completedAt").and_then(parse_date_millis),
            snoozed_at.and_then(parse_date_millis),
        ) {
            (Some(completed), Some(snoozed)) => completed > snoozed,
            _ => false,
        };
    woke_on_error || woke_on_completion
}
