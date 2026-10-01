//! Port of `ThreadSettlementPolicy.test.ts`.

use serde_json::{json, Value};
use zc_reactors::settlement::policy::{resolve_auto_settlement_at, SettlementPullRequest};

const NOW: &str = "2026-08-28T12:00:00.000Z";

fn thread(overrides: Value) -> Value {
    let mut thread = json!({
        "id": "thread-1", "projectId": "project-1", "title": "Thread", "modelSelection": {"instanceId": "codex", "model": "gpt-5"},
        "runtimeMode": "full-access", "interactionMode": "default", "pullRequests": [], "branch": "feature", "worktreePath": "/repo",
        "latestTurn": null, "createdAt": "2026-08-01T00:00:00.000Z", "updatedAt": "2026-08-20T00:00:00.000Z", "archivedAt": null,
        "settledOverride": null, "settledAt": null, "session": null, "latestUserMessageAt": "2026-08-20T00:00:00.000Z",
        "hasPendingApprovals": false, "hasPendingUserInput": false, "hasActionableProposedPlan": false,
    });
    for (key, value) in overrides.as_object().unwrap() {
        thread[key] = value.clone();
    }
    thread
}

fn pr(state: &str, closed_at: Option<&str>, merged_at: Option<&str>) -> SettlementPullRequest {
    SettlementPullRequest::new(state, closed_at, merged_at)
}

/// `decide(thread, pullRequest, {days = 3, merge = true})`.
fn decide(thread: &Value, pull_request: Option<SettlementPullRequest>, days: Option<f64>, merge: bool) -> bool {
    resolve_auto_settlement_at(thread, pull_request.as_ref(), NOW, days, merge).is_some()
}

fn d(thread: &Value) -> bool {
    decide(thread, None, Some(3.0), true)
}

fn turn(requested: &str, started: &str, completed: &str) -> Value {
    json!({"turnId": "turn-1", "state": "completed", "requestedAt": requested, "startedAt": started, "completedAt": completed, "assistantMessageId": null})
}

#[test]
fn returns_the_last_activity_time_for_persisted_settlement() {
    let t = thread(json!({"latestTurn": turn("2026-08-19T00:00:00.000Z", "2026-08-19T00:01:00.000Z", "2026-08-21T00:00:00.000Z")}));
    assert_eq!(
        resolve_auto_settlement_at(&t, None, NOW, Some(3.0), true).as_deref(),
        Some("2026-08-21T00:00:00.000Z")
    );
}

#[test]
fn uses_creation_time_for_pr_settlement_when_the_thread_has_no_activity() {
    let t = thread(json!({"latestUserMessageAt": null, "latestTurn": null, "updatedAt": "2026-08-27T00:00:00.000Z"}));
    assert_eq!(
        resolve_auto_settlement_at(&t, Some(&pr("closed", Some(NOW), None)), NOW, None, true).as_deref(),
        Some("2026-08-01T00:00:00.000Z")
    );
}

#[test]
fn settles_inactive_threads_and_leaves_never_used_threads_active() {
    assert!(d(&thread(json!({}))));
    assert!(!d(&thread(json!({"latestUserMessageAt": null}))));
    assert!(!decide(&thread(json!({})), None, None, true));
}

#[test]
fn keeps_a_thread_active_at_the_exact_inactivity_boundary() {
    assert!(!d(&thread(json!({"latestUserMessageAt": "2026-08-25T12:00:00.000Z"}))));
}

#[test]
fn settles_inactive_threads_with_open_pull_requests() {
    let mut open = pr("open", None, None);
    open.updated_at = Some(NOW.into());
    assert!(decide(&thread(json!({})), Some(open), Some(3.0), true));
}

#[test]
fn settles_closed_requests_and_honors_the_merge_setting() {
    assert!(decide(&thread(json!({})), Some(pr("closed", Some(NOW), None)), Some(3.0), false));
    assert!(decide(&thread(json!({})), Some(pr("merged", None, Some(NOW))), Some(3.0), false));
    assert!(!decide(&thread(json!({})), Some(pr("merged", None, Some(NOW))), None, false));
}

#[test]
fn does_not_settle_again_after_user_activity_newer_than_the_pr() {
    assert!(!decide(
        &thread(json!({"latestUserMessageAt": "2026-08-27T00:00:00.000Z"})),
        Some(pr("merged", None, Some("2026-08-26T00:00:00.000Z"))),
        None,
        true
    ));
}

fn ignores_metadata_edits(state: &str) {
    let mut request = pr(state, Some("2026-08-26T00:00:00.000Z"), Some("2026-08-26T00:00:00.000Z"));
    request.updated_at = Some(NOW.into());
    assert!(!decide(
        &thread(json!({"latestUserMessageAt": "2026-08-27T00:00:00.000Z"})),
        Some(request),
        None,
        true
    ));
    let mut bare = pr(state, None, None);
    bare.updated_at = Some(NOW.into());
    assert!(!decide(&thread(json!({})), Some(bare), None, true));
}

#[test]
fn ignores_metadata_edits_after_resumed_work_for_closed_requests() {
    ignores_metadata_edits("closed");
}

#[test]
fn ignores_metadata_edits_after_resumed_work_for_merged_requests() {
    ignores_metadata_edits("merged");
}

#[test]
fn does_not_inherit_a_terminal_pull_request_older_than_the_thread() {
    assert!(!decide(
        &thread(json!({"createdAt": "2026-08-20T00:00:00.000Z", "latestUserMessageAt": null})),
        Some(pr("closed", Some("2026-08-19T00:00:00.000Z"), None)),
        None,
        true
    ));
}

#[test]
fn requires_a_comparable_pr_timestamp_for_immediate_settlement() {
    let recent = thread(json!({"latestUserMessageAt": "2026-08-27T00:00:00.000Z"}));
    assert!(!decide(&recent, Some(pr("closed", None, None)), Some(3.0), true));
    assert!(!decide(&recent, Some(pr("merged", None, Some("unknown"))), Some(3.0), true));
    assert!(decide(&thread(json!({})), Some(pr("closed", None, None)), Some(3.0), true));
}

#[test]
fn uses_user_request_time_instead_of_completion_time_as_the_pr_anchor() {
    let t = thread(json!({"latestTurn": turn("2026-08-25T00:00:00.000Z", "2026-08-25T00:01:00.000Z", "2026-08-27T00:00:00.000Z")}));
    assert!(decide(&t, Some(pr("merged", None, Some("2026-08-26T00:00:00.000Z"))), Some(3.0), true));
}

#[test]
fn blocks_pins_snooze_pending_work_live_sessions_and_queued_starts() {
    assert!(!d(&thread(json!({"settledOverride": "active"}))));
}

#[test]
fn never_settles_a_thread_whose_auto_settle_is_turned_off_by_inactivity_or_merge() {
    let held = thread(json!({"autoSettleDisabledAt": "2026-08-21T00:00:00.000Z"}));
    assert!(!d(&held));
    assert!(!decide(&held, Some(pr("merged", None, Some("2026-08-21T00:00:00.000Z"))), Some(3.0), true));
    assert!(d(&thread(json!({"autoSettleDisabledAt": null}))));
    assert!(!d(&thread(json!({"snoozedUntil": "2026-08-29T00:00:00.000Z"}))));
    assert!(!d(&thread(json!({"hasPendingApprovals": true}))));
    assert!(!d(&thread(json!({"hasPendingUserInput": true}))));
    assert!(!d(&thread(json!({"backgroundLiveness": "working"}))));
    assert!(!d(&thread(json!({"backgroundLiveness": "monitoring"}))));
    assert!(!d(&thread(json!({"session": {
        "threadId": "thread-1", "status": "running", "providerName": "codex", "runtimeMode": "full-access",
        "activeTurnId": "turn-1", "lastError": null, "updatedAt": NOW,
    }}))));
    assert!(!d(&thread(json!({"latestUserMessageAt": "2026-08-28T11:59:00.000Z", "latestTurn": null}))));
}

#[test]
fn allows_a_fresh_completion_to_wake_snooze_before_settlement() {
    assert!(d(&thread(json!({
        "snoozedAt": "2026-08-19T00:00:00.000Z",
        "snoozedUntil": "2026-08-29T00:00:00.000Z",
        "latestTurn": turn("2026-08-18T00:00:00.000Z", "2026-08-18T00:01:00.000Z", "2026-08-20T00:00:00.000Z"),
    }))));
}

fn linked(number: u32, snapshot: Value) -> Value {
    json!({
        "host": "github.com", "repository": "org/repo", "number": number, "url": format!("https://github.com/org/repo/pull/{number}"),
        "source": "manual", "linkedAt": NOW, "stack": null, "snapshot": snapshot,
    })
}

fn terminal(state: &str, at: &str, updated_at: Option<&str>) -> Value {
    json!({
        "state": state, "title": "Change", "headBranch": "feature", "baseBranch": "main", "isDraft": false, "closedAt": at,
        "mergedAt": if state == "merged" { Some(at) } else { None }, "updatedAt": updated_at.unwrap_or(at), "syncedAt": NOW,
    })
}

#[test]
fn blocks_both_inactivity_and_merge_settlement_while_auto_settle_is_off() {
    let merged = linked(1, terminal("merged", NOW, None));
    assert!(d(&thread(json!({"latestUserMessageAt": "2026-08-01T00:00:00.000Z"}))));
    assert!(decide(&thread(json!({"pullRequests": [merged]})), None, None, true));
    assert!(!d(&thread(
        json!({"autoSettleDisabledAt": NOW, "latestUserMessageAt": "2026-08-01T00:00:00.000Z"})
    )));
    assert!(!decide(
        &thread(json!({"autoSettleDisabledAt": NOW, "pullRequests": [merged]})),
        None,
        None,
        true
    ));
}

fn uses_the_latest_transition(state: &str) {
    let old = linked(1, terminal(state, "2026-08-19T00:00:00.000Z", Some(NOW)));
    let recent = linked(2, terminal(state, "2026-08-21T00:00:00.000Z", None));
    assert!(decide(&thread(json!({"pullRequests": [old, recent]})), None, None, true));
    assert!(decide(&thread(json!({"pullRequests": [recent, old]})), None, None, true));
    assert!(!decide(&thread(json!({"pullRequests": [old]})), None, None, true));
}

#[test]
fn uses_the_latest_actual_closed_transition_despite_later_comments_on_another_pr() {
    uses_the_latest_transition("closed");
}

#[test]
fn uses_the_latest_actual_merged_transition_despite_later_comments_on_another_pr() {
    uses_the_latest_transition("merged");
}

#[test]
fn keeps_unknown_and_open_links_active_even_after_the_inactivity_window() {
    let merged = linked(1, terminal("merged", NOW, None));
    let unknown = linked(2, Value::Null);
    let mut open_snapshot = terminal("closed", NOW, None);
    open_snapshot["state"] = json!("open");
    open_snapshot["closedAt"] = Value::Null;
    let open = linked(3, open_snapshot);
    assert!(!d(&thread(json!({"pullRequests": [merged, unknown]}))));
    assert!(!d(&thread(json!({"pullRequests": [merged, open]}))));
    let mut dismissed = unknown.clone();
    dismissed["source"] = json!("stack-dismissed");
    assert!(d(&thread(json!({"pullRequests": [merged, dismissed]}))));
}

#[test]
fn honors_merge_settings_and_ignores_missing_terminal_timestamps() {
    let merged = linked(1, terminal("merged", NOW, None));
    assert!(!decide(&thread(json!({"pullRequests": [merged]})), None, None, false));
    let mut missing_snapshot = terminal("merged", NOW, None);
    missing_snapshot["mergedAt"] = Value::Null;
    let missing = linked(2, missing_snapshot);
    assert!(!decide(&thread(json!({"pullRequests": [missing]})), None, None, true));
    assert!(decide(&thread(json!({"pullRequests": [missing, merged]})), None, None, true));
}
