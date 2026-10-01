//! `gitHubPullRequestJson.test.ts`, test for test (people, organizations and repositories are
//! made up). The fixtures live in `support_github_json`, shared with the golden comparison.

#![recursion_limit = "512"]

mod support_github_json;

use regex::Regex;
use serde_json::{json, Value};
use support_github_json::*;
use zc_contracts::{
    PullRequestActor, PullRequestCheckStatus as Check, PullRequestChecksState as Rollup, PullRequestCommentKind, PullRequestDiffSide, PullRequestFileViewed,
    PullRequestFileViewedState, PullRequestLabelCandidate, PullRequestMergeCapabilities, PullRequestMergeMethod, PullRequestMergeability, PullRequestReaction,
    PullRequestReactionContent as Content, PullRequestReviewCommentDraft, PullRequestReviewDecision, PullRequestReviewThread, PullRequestReviewVerdict,
    PullRequestReviewerCandidate, PullRequestReviewerKind, PullRequestStackMembership, PullRequestState, PullRequestThreadComment,
};
use zc_pullrequest::github::json::*;
use zc_pullrequest::provider::ReviewerRef;

fn ok<T>(result: Result<T, DecodeFailure>) -> T {
    match result {
        Ok(value) => value,
        Err(failure) => panic!("expected a successful decode, got {failure}"),
    }
}

fn actor(login: &str, name: Option<&str>, avatar_url: Option<&str>) -> PullRequestActor {
    PullRequestActor {
        is_bot: None,
        login: login.into(),
        name: name.map(Into::into),
        avatar_url: avatar_url.map(Into::into),
    }
}

fn reaction(content: Content, count: i64, actors: &[&str], viewer_has_reacted: bool) -> PullRequestReaction {
    PullRequestReaction {
        content,
        count,
        actors: actors.iter().map(|actor| (*actor).to_owned()).collect(),
        viewer_has_reacted,
    }
}

fn thread_ids(page: &GitHubReviewThreadPage) -> Vec<String> {
    let threads: Vec<PullRequestReviewThread> = page.threads.iter().map(|entry| entry.thread.clone()).collect();
    review_thread_conversation(&threads).into_iter().map(|comment| comment.id).collect()
}

// pull request list decoding

#[test]
fn treats_a_merge_timestamp_as_merged_even_when_the_state_still_says_closed() {
    let batch = ok(decode_pull_request_list_json(&list_json(&[
        json!({ "state": "CLOSED", "mergedAt": "2026-07-03T00:00:00Z" }),
    ])));
    assert_eq!(batch.items[0].state, PullRequestState::Merged);
}

#[test]
fn normalizes_mergeability_and_defaults_unknown_values() {
    let batch = ok(decode_pull_request_list_json(&list_json(&[
        json!({ "mergeable": "CONFLICTING" }),
        json!({ "mergeable": "SOMETHING_NEW" }),
        json!({}),
    ])));
    assert_eq!(
        batch.items.iter().map(|entry| entry.mergeability).collect::<Vec<_>>(),
        vec![
            PullRequestMergeability::Conflicting,
            PullRequestMergeability::Unknown,
            PullRequestMergeability::Unknown
        ]
    );
}

#[test]
fn keeps_user_review_requests_and_drops_team_ones_which_are_not_logins() {
    let batch = ok(decode_pull_request_list_json(&list_json(&[
        json!({ "reviewRequests": [{ "login": "ada-example" }, { "slug": "web-platform" }] }),
    ])));
    assert_eq!(batch.items[0].review_request_logins, vec!["ada-example".to_owned()]);
    assert!(batch.items[0].has_team_review_request);
}

#[test]
fn normalizes_the_review_decision_and_reports_nothing_for_one_github_does_not_summarize() {
    let batch = ok(decode_pull_request_list_json(&list_json(&[
        json!({ "reviewDecision": "APPROVED" }),
        json!({ "reviewDecision": "CHANGES_REQUESTED" }),
        json!({ "reviewDecision": "REVIEW_REQUIRED" }),
        json!({ "reviewDecision": null }),
    ])));
    assert_eq!(
        batch.items.iter().map(|entry| entry.review_decision).collect::<Vec<_>>(),
        vec![
            Some(PullRequestReviewDecision::Approved),
            Some(PullRequestReviewDecision::ChangesRequested),
            Some(PullRequestReviewDecision::ReviewRequired),
            None
        ]
    );
}

#[test]
fn takes_the_verdict_from_the_latest_reviews_when_github_summarizes_none_as_for_a_bots_approval() {
    let batch = ok(decode_pull_request_list_json(&list_json(&[
        json!({ "reviewDecision": null, "latestReviews": [{ "author": { "login": "lint-bot" }, "state": "APPROVED" }] }),
        json!({ "reviewDecision": "REVIEW_REQUIRED", "latestReviews": [
            { "author": { "login": "ada-example" }, "state": "APPROVED" },
            { "author": { "login": "helper-bot" }, "state": "CHANGES_REQUESTED" },
        ] }),
        json!({ "reviewDecision": "APPROVED", "latestReviews": [{ "author": { "login": "helper-bot" }, "state": "CHANGES_REQUESTED" }] }),
        json!({ "reviewDecision": null, "latestReviews": [{ "author": { "login": "ada-example" }, "state": "COMMENTED" }] }),
    ])));
    assert_eq!(
        batch.items.iter().map(|entry| entry.review_decision).collect::<Vec<_>>(),
        vec![
            Some(PullRequestReviewDecision::Approved),
            Some(PullRequestReviewDecision::ChangesRequested),
            Some(PullRequestReviewDecision::Approved),
            None
        ]
    );
}

#[test]
fn rolls_the_head_commits_checks_up_to_the_one_word_a_row_has_space_for() {
    let batch = ok(decode_pull_request_list_json(&list_json(&[
        // A failure outranks a run still going; a completed run is read through its conclusion.
        json!({ "statusCheckRollup": [
            { "name": "lint", "status": "COMPLETED", "conclusion": "SUCCESS" },
            { "name": "build", "status": "IN_PROGRESS" },
            { "name": "test", "status": "COMPLETED", "conclusion": "FAILURE" },
        ] }),
        json!({ "statusCheckRollup": [{ "name": "lint", "status": "COMPLETED", "conclusion": "SUCCESS" }, { "name": "build", "status": "QUEUED" }] }),
        json!({ "statusCheckRollup": [{ "name": "lint", "status": "COMPLETED", "conclusion": "SUCCESS" }] }),
        // A commit status reports one `state` and no `status` at all.
        json!({ "statusCheckRollup": [{ "context": "ci/legacy", "state": "ERROR" }] }),
        // Neither a pass, a failure nor a wait is no verdict rather than a green tick.
        json!({ "statusCheckRollup": [{ "name": "lint", "status": "COMPLETED", "conclusion": "SKIPPED" }] }),
        // Cancelled reads as failing here and in the detail header, so the two never flap.
        json!({ "statusCheckRollup": [
            { "name": "lint", "status": "COMPLETED", "conclusion": "SUCCESS" },
            { "name": "test", "status": "COMPLETED", "conclusion": "CANCELLED" },
        ] }),
        json!({ "statusCheckRollup": [] }),
        json!({}),
    ])));
    assert_eq!(
        batch.items.iter().map(|entry| entry.checks_state).collect::<Vec<_>>(),
        vec![
            Some(Rollup::Failing),
            Some(Rollup::Pending),
            Some(Rollup::Passing),
            Some(Rollup::Failing),
            None,
            Some(Rollup::Failing),
            None,
            None
        ]
    );
}

#[test]
fn skips_malformed_entries_but_still_counts_them_so_paging_does_not_stop_early() {
    let one = list_json(&[json!({})]);
    let raw = format!("[{},{{\"number\":\"not-a-number\"}}]", &one[1..one.len() - 1]);
    let batch = ok(decode_pull_request_list_json(&raw));
    assert_eq!(batch.items.len(), 1);
    assert_eq!(batch.raw_count, 2);
}

// pull request search decoding

#[test]
fn keeps_stack_membership_beside_search_results_without_extra_per_pr_reads() {
    let batch = ok(decode_pull_request_search_json(&search_with_stack()));
    assert_eq!(
        batch.items[0].item.stack,
        Some(PullRequestStackMembership {
            number: 3,
            size: 2,
            position: 1,
            base: "main".into()
        })
    );
    assert_eq!(batch.items[1].item.stack, None);
    assert!(pull_request_search_graph_ql_query(20, true).contains("stackEntry"));
    assert!(!pull_request_search_graph_ql_query(20, false).contains("stackEntry"));
}

#[test]
fn maps_the_rollup_enum_the_search_answers_with_onto_the_same_three_words() {
    let raw = search_value(&[Some("SUCCESS"), Some("FAILURE"), Some("ERROR"), Some("PENDING"), Some("EXPECTED"), None]).to_string();
    let batch = ok(decode_pull_request_search_json(&raw));
    assert_eq!(
        batch.items.iter().map(|entry| entry.item.checks_state).collect::<Vec<_>>(),
        vec![
            Some(Rollup::Passing),
            Some(Rollup::Failing),
            Some(Rollup::Failing),
            Some(Rollup::Pending),
            Some(Rollup::Pending),
            None
        ]
    );
    assert_eq!(batch.items[0].repository, REPOSITORY);
}

// pull request detail decoding

#[test]
fn maps_check_run_status_and_commit_status_state_onto_one_vocabulary() {
    let detail = ok(decode_pull_request_detail_json(&detail_json()));
    assert_eq!(
        detail.checks.iter().map(|check| (check.name.as_str(), check.status)).collect::<Vec<_>>(),
        vec![("build", Check::Pending), ("test", Check::Failure), ("ci/legacy", Check::Success)]
    );
}

#[test]
fn keeps_a_workflow_waiting_for_approval_out_of_the_passing_state() {
    let detail = ok(decode_pull_request_detail_json(&detail_with(json!({ "statusCheckRollup": [
        { "__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS" },
        { "__typename": "CheckRun", "name": "contributor tests", "status": "COMPLETED", "conclusion": "ACTION_REQUIRED" },
    ] }))));
    assert_eq!(
        detail.checks.iter().map(|check| check.status).collect::<Vec<_>>(),
        vec![Check::Success, Check::ActionRequired]
    );
    assert_eq!(detail.item.checks_state, Some(Rollup::Pending));
}

#[test]
fn decodes_workflow_runs_that_can_be_approved() {
    let runs = ok(decode_workflow_run_approvals_json(
        &json!([
            { "databaseId": 10, "workflowName": "contributor tests", "url": "https://example.test/10" },
            { "databaseId": 11, "workflowName": null, "url": null },
        ])
        .to_string(),
    ));
    assert_eq!(
        runs,
        vec![
            GitHubWorkflowRunApproval {
                id: 10,
                name: "contributor tests".into(),
                url: Some("https://example.test/10".into())
            },
            GitHubWorkflowRunApproval {
                id: 11,
                name: "Workflow run 11".into(),
                url: None
            },
        ]
    );
}

#[test]
fn reads_an_auto_merge_request_and_strategy_its_null_as_off_and_its_absence_as_neither() {
    let armed = |entry: Value| ok(decode_pull_request_detail_json(&detail_with(entry)));
    let squash = armed(json!({ "autoMergeRequest": { "enabledBy": { "login": "ada-example" }, "mergeMethod": "SQUASH" } }));
    assert_eq!(
        (squash.auto_merge_enabled, squash.auto_merge_method),
        (Some(true), Some(PullRequestMergeMethod::Squash))
    );
    assert_eq!(armed(json!({ "autoMergeRequest": null })).auto_merge_enabled, Some(false));
    // `gh` not answering for the field at all is not GitHub saying the merge is unarmed.
    assert_eq!(armed(json!({})).auto_merge_enabled, None);
}

#[test]
fn shows_a_re_running_check_once_as_the_run_that_is_happening_now() {
    // While a workflow is re-run, the rollup reports the finished run and its replacement.
    let detail = ok(decode_pull_request_detail_json(&detail_with(json!({ "statusCheckRollup": [
        { "__typename": "CheckRun", "name": "Prepare PR size config", "workflowName": "PR Size", "status": "COMPLETED", "conclusion": "SUCCESS",
          "startedAt": "2026-08-11T16:06:20Z", "completedAt": "2026-08-11T16:06:25Z" },
        { "__typename": "CheckRun", "name": "Prepare PR size config", "workflowName": "PR Size", "status": "IN_PROGRESS", "conclusion": "",
          "startedAt": "2026-08-11T17:01:04Z", "completedAt": "0001-01-01T00:00:00Z" },
    ] }))));
    assert_eq!(
        detail.checks.iter().map(|check| (check.name.as_str(), check.status)).collect::<Vec<_>>(),
        vec![("Prepare PR size config", Check::Pending)]
    );
    assert_eq!(detail.item.checks_state, Some(Rollup::Pending));
}

#[test]
fn merges_reviews_with_comments_in_time_order_and_keeps_a_bodyless_approval() {
    let activity = ok(decode_pull_request_activity_json(&detail_json()));
    // r2 approved without writing anything, which is still the event worth seeing.
    assert_eq!(
        activity.comments.iter().map(|comment| comment.id.as_str()).collect::<Vec<_>>(),
        vec!["r1", "c1", "r2"]
    );
    assert_eq!(activity.comments.last().unwrap().review_state.as_deref(), Some("APPROVED"));
}

#[test]
fn keeps_every_attributed_commit_author_including_an_unlinked_signature() {
    let activity = ok(decode_pull_request_activity_json(&detail_json()));
    assert_eq!(
        activity.commits[0].authors,
        Some(vec![
            actor("ada-example", Some("Ada Example"), None),
            actor("Pair Author", Some("Pair Author"), None)
        ])
    );
}

#[test]
fn drops_the_bodyless_review_github_opens_to_hold_line_comments() {
    let activity = ok(decode_pull_request_activity_json(&detail_with(json!({ "reviews": [
        // A container with a state and nothing to read; its comments come from the review threads.
        { "id": "r4", "body": "", "state": "COMMENTED", "submittedAt": "2026-07-07T00:00:00Z" },
        { "id": "r5", "body": "Looks good.", "state": "COMMENTED", "submittedAt": "2026-07-08T00:00:00Z" },
    ] }))));
    assert_eq!(
        activity.comments.iter().map(|comment| comment.id.as_str()).collect::<Vec<_>>(),
        vec!["c1", "r5"]
    );
}

#[test]
fn keeps_a_bodyless_verdict_review_which_is_the_event_itself() {
    for state in ["APPROVED", "CHANGES_REQUESTED", "DISMISSED"] {
        let activity = ok(decode_pull_request_activity_json(&detail_with(
            json!({ "reviews": [{ "id": "r6", "body": "", "state": state, "submittedAt": "2026-07-07T00:00:00Z" }] }),
        )));
        assert!(activity.comments.iter().any(|comment| comment.id == "r6"), "{state}");
    }
}

#[test]
fn drops_a_review_that_carries_neither_a_body_nor_a_state() {
    let activity = ok(decode_pull_request_activity_json(&detail_with(
        json!({ "reviews": [{ "id": "r3", "body": "  ", "submittedAt": "2026-07-07T00:00:00Z" }] }),
    )));
    assert_eq!(activity.comments.iter().map(|comment| comment.id.as_str()).collect::<Vec<_>>(), vec!["c1"]);
}

// review thread decoding

#[test]
fn keeps_a_reviewer_who_has_already_reviewed_app_or_person_with_their_avatar() {
    let page = ok(decode_review_threads_json(&review_roster(
        vec![json!({ "login": "jules-example", "name": "Jules", "avatarUrl": "https://avatars.example.test/j.png" })],
        // An app that has reviewed is no longer an outstanding request.
        vec![json!({ "__typename": "Bot", "login": "lint-bot", "avatarUrl": "https://avatars.example.test/in/1.png" })],
    )));
    assert_eq!(page.bot_logins.iter().cloned().collect::<Vec<_>>(), vec!["lint-bot".to_owned()]);
    assert_eq!(
        page.reviewers,
        vec![
            actor("jules-example", Some("Jules"), Some("https://avatars.example.test/j.png")),
            PullRequestActor {
                is_bot: Some(true),
                ..actor("lint-bot", None, Some("https://avatars.example.test/in/1.png"))
            },
        ]
    );
}

#[test]
fn carries_per_commit_line_counts_from_the_pull_request_connection() {
    let page = ok(decode_review_threads_json(&commits_page(json!([
        { "commit": { "oid": "abc123", "additions": 18, "deletions": 7 } },
        { "commit": { "oid": "def456", "additions": 3, "deletions": 0 } },
    ]))));
    assert_eq!(
        page.commit_stats.into_iter().collect::<Vec<_>>(),
        vec![
            ("abc123".to_owned(), GitHubLineStats { additions: 18, deletions: 7 }),
            ("def456".to_owned(), GitHubLineStats { additions: 3, deletions: 0 })
        ]
    );
}

#[test]
fn omits_misleading_line_counts_from_merge_commits() {
    let page = ok(decode_review_threads_json(&commits_page(json!([
        { "commit": { "oid": "merge123", "additions": 36_858, "deletions": 12_928, "parents": { "totalCount": 2 } } },
    ]))));
    assert!(page.commit_stats.is_empty());
}

#[test]
fn decodes_the_newest_commits_off_the_same_connection_oldest_to_newest() {
    let page = ok(decode_review_threads_json(&commits_page(json!([
        { "commit": { "oid": "abc123", "messageHeadline": "Ship the timeline", "committedDate": "2026-07-05T00:00:00Z", "additions": 18, "deletions": 7,
          "authors": { "nodes": [{ "name": "Jules", "user": { "login": "jules-example" } }] } } },
        { "commit": { "oid": "def456", "messageHeadline": "Fix the flaky test", "committedDate": "2026-07-06T00:00:00Z" } },
    ]))));
    assert_eq!(
        serde_json::to_value(&page.commits).unwrap(),
        json!([
            { "oid": "abc123", "messageHeadline": "Ship the timeline", "committedDate": "2026-07-05T00:00:00Z", "authors": [{ "login": "jules-example", "name": "Jules", "avatarUrl": null }] },
            { "oid": "def456", "messageHeadline": "Fix the flaky test", "committedDate": "2026-07-06T00:00:00Z", "authors": [] },
        ])
    );
}

#[test]
fn lists_someone_who_was_asked_and_then_answered_only_once() {
    let person = json!({ "login": "jules-example", "avatarUrl": "https://avatars.example.test/j.png" });
    let page = ok(decode_review_threads_json(&review_roster(vec![person.clone()], vec![person])));
    assert_eq!(page.reviewers.len(), 1);
}

#[test]
fn skips_a_team_request_which_names_nobody_to_show() {
    let page = ok(decode_review_threads_json(&review_roster(vec![Value::Null], vec![])));
    assert!(page.reviewers.is_empty());
}

#[test]
fn keeps_the_conversation_when_a_request_is_from_a_team_which_has_no_login() {
    // GraphQL answers with an empty object for a union member the query has no fragment for.
    let page = ok(decode_review_threads_json(&review_roster(
        vec![
            json!({}),
            json!({ "login": "jules-example", "avatarUrl": "https://avatars.example.test/j.png" }),
        ],
        vec![],
    )));
    assert_eq!(page.reviewers, vec![actor("jules-example", None, Some("https://avatars.example.test/j.png"))]);
}

#[test]
fn carries_a_resolved_thread_into_the_conversation_which_was_still_said() {
    let page = ok(decode_review_threads_json(&threads_page_default(json!([
        { "id": "PRRT_a", "isResolved": false, "path": "apps/server/src/ws.ts", "comments": { "nodes": [{ "id": "t1", "body": "fix this", "createdAt": "2026-07-01T00:00:00Z" }] } },
        { "id": "PRRT_b", "isResolved": true, "path": "apps/web/src/main.tsx", "comments": { "nodes": [{ "id": "t2", "body": "done", "createdAt": "2026-07-01T00:00:00Z" }] } },
    ]))));
    let threads: Vec<PullRequestReviewThread> = page.threads.iter().map(|entry| entry.thread.clone()).collect();
    let comments = review_thread_conversation(&threads);
    assert_eq!(comments.iter().map(|comment| comment.id.as_str()).collect::<Vec<_>>(), vec!["t1", "t2"]);
    assert_eq!(
        (comments[0].kind, comments[0].path.as_deref()),
        (PullRequestCommentKind::ReviewComment, Some("apps/server/src/ws.ts"))
    );
}

#[test]
fn carries_every_reply_not_only_the_remark_each_thread_opened_with() {
    let page = ok(decode_review_threads_json(&threads_page_default(
        json!([{ "id": "PRRT_c", "isResolved": false, "path": "apps/server/src/ws.ts", "comments": { "nodes": [
        { "id": "t1", "body": "fix this", "createdAt": "2026-07-01T00:00:00Z" },
        { "id": "t2", "body": "fixed", "createdAt": "2026-07-01T01:00:00Z" },
    ] } }]),
    )));
    assert_eq!(thread_ids(&page), vec!["t1", "t2"]);
}

#[test]
fn hands_back_the_cursor_the_next_page_of_threads_carries_on_from() {
    let page = ok(decode_review_threads_json(&threads_page(
        json!([{ "id": "PRRT_d", "path": "apps/server/src/ws.ts", "isResolved": false, "comments": { "nodes": [{ "id": "t1", "createdAt": "2026-07-01T00:00:00Z" }] } }]),
        80,
        json!({ "hasNextPage": true, "endCursor": "Y3Vyc29yOjE" }),
    )));
    assert_eq!(page.next_cursor.as_deref(), Some("Y3Vyc29yOjE"));
}

#[test]
fn keeps_githubs_own_count_of_a_thread_whose_comments_were_not_all_read() {
    let page = ok(decode_review_threads_json(&threads_page_default(
        json!([{ "id": "PRRT_e", "path": "apps/server/src/ws.ts", "isResolved": false, "comments": {
        "totalCount": 140, "pageInfo": { "hasNextPage": true, "endCursor": "Y3Vyc29yOjI" }, "nodes": [{ "id": "t1", "createdAt": "2026-07-01T00:00:00Z" }],
    } }]),
    )));
    assert_eq!(
        (page.threads[0].comment_count, page.threads[0].next_comment_cursor.as_deref()),
        (140, Some("Y3Vyc29yOjI"))
    );
}

#[test]
fn ends_a_threads_walk_on_the_last_page_which_still_names_a_cursor() {
    let decoded = ok(decode_review_thread_comments_json(&thread_comments_page(
        None,
        json!([{ "id": "t9", "body": "last", "createdAt": "2026-07-01T00:00:00Z" }]),
        json!({ "hasNextPage": false, "endCursor": "Y3Vyc29yOjk" }),
    )));
    assert_eq!(decoded.comments.iter().map(|comment| comment.id.as_str()).collect::<Vec<_>>(), vec!["t9"]);
    assert_eq!(decoded.next_cursor, None);
    assert!(decoded.belongs_to_pull_request);
}

// reaction decoding

#[test]
fn keeps_a_named_group_widens_a_group_whose_reactors_were_cut_short_drops_an_unknown_content_and_an_empty_group() {
    let decoded = ok(decode_review_thread_comments_json(&comment_with_groups(json!([
        { "content": "THUMBS_UP", "viewerHasReacted": true, "reactors": { "totalCount": 2, "nodes": [{ "login": "jules-example" }, { "login": "bea-example" }] } },
        // Not one of the eight the contract carries.
        { "content": "PARTY_PARROT", "reactors": { "totalCount": 1, "nodes": [{ "login": "helper-bot" }] } },
        // Nobody behind it, which GitHub still answers a group for.
        { "content": "HEART", "reactors": { "totalCount": 0, "nodes": [] } },
        // More reactors than the bounded read named, and no `viewerHasReacted` at all.
        { "content": "ROCKET", "reactors": { "totalCount": 140, "nodes": [{ "login": "a" }, { "login": "b" }, { "login": "c" }] } },
    ]))));
    assert_eq!(
        decoded.comments[0].reactions,
        Some(vec![
            reaction(Content::ThumbsUp, 2, &["jules-example", "bea-example"], true),
            reaction(Content::Rocket, 140, &["a", "b", "c"], false)
        ])
    );
}

#[test]
fn leaves_the_viewers_own_login_out_of_actors_matched_case_insensitively_while_count_still_counts_them() {
    let decoded = ok(decode_review_thread_comments_json(&thread_comments_page(
        Some("Bea-Example"),
        json!([{ "id": "t1", "body": "nice", "createdAt": "2026-07-01T00:00:00Z", "reactionGroups": [
            { "content": "HEART", "viewerHasReacted": true, "reactors": { "totalCount": 2, "nodes": [{ "login": "bea-example" }, { "login": "jules-example" }] } },
        ] }]),
        json!({ "hasNextPage": false, "endCursor": null }),
    )));
    assert_eq!(decoded.comments[0].reactions, Some(vec![reaction(Content::Heart, 2, &["jules-example"], true)]));
}

// repository access decoding

const MERGE_CAPABILITIES: PullRequestMergeCapabilities = PullRequestMergeCapabilities {
    merge: true,
    squash: false,
    rebase: true,
};

#[test]
fn reads_merge_settings_with_viewer_permissions() {
    assert_eq!(
        ok(decode_viewer_permissions_json(&repository_access(Some(json!("ADMIN"))))).merge_capabilities,
        MERGE_CAPABILITIES
    );
}

#[test]
fn fails_rather_than_defaulting_open_when_a_setting_is_missing() {
    let decoded = decode_viewer_permissions_json(&json!({ "data": { "repository": { "pullRequest": null, "mergeCommitAllowed": true } } }).to_string());
    assert_eq!(
        decoded.unwrap_err().message(),
        "Missing key\n  at [\"data\"][\"repository\"][\"squashMergeAllowed\"]"
    );
}

#[test]
fn counts_the_roles_that_can_push_as_write_and_the_ones_that_cannot_as_read() {
    for permission in ["ADMIN", "MAINTAIN", "WRITE"] {
        assert!(
            ok(decode_viewer_permissions_json(&repository_access(Some(json!(permission))))).viewer.can_write,
            "{permission}"
        );
    }
    for permission in ["TRIAGE", "READ", "NONE"] {
        assert!(
            !ok(decode_viewer_permissions_json(&repository_access(Some(json!(permission))))).viewer.can_write,
            "{permission}"
        );
    }
}

#[test]
fn withholds_write_where_gh_names_no_permission_which_is_not_a_standing_it_gave() {
    assert!(!ok(decode_viewer_permissions_json(&repository_access(None))).viewer.can_write);
    assert!(!ok(decode_viewer_permissions_json(&repository_access(Some(Value::Null)))).viewer.can_write);
}

// viewer permission decoding

fn access(can_write: bool, can_triage: bool, can_update: bool, did_author: bool) -> GitHubViewerRepositoryAccess {
    GitHubViewerRepositoryAccess {
        viewer: GitHubViewerAccess {
            can_write,
            can_triage,
            can_update,
            did_author,
            can_update_branch: None,
        },
        merge_capabilities: MERGE_CAPABILITIES,
    }
}

#[test]
fn reads_the_repositorys_role_and_the_pull_requests_own_viewer_fields_together() {
    let decoded = ok(decode_viewer_permissions_json(&viewer_permissions(
        json!({ "viewerPermission": "READ", "pullRequest": { "viewerCanUpdate": true, "viewerDidAuthor": true } }),
    )));
    assert_eq!(decoded, access(false, false, true, true));
}

#[test]
fn says_no_to_a_passer_by_on_a_repository_they_can_only_read() {
    let decoded = ok(decode_viewer_permissions_json(&viewer_permissions(
        json!({ "viewerPermission": "READ", "pullRequest": { "viewerCanUpdate": false, "viewerDidAuthor": false } }),
    )));
    assert_eq!(decoded, access(false, false, false, false));
}

#[test]
fn reads_silence_as_permission_but_not_as_authorship() {
    // Updating is a permission (the host refuses if it must); authorship is a fact.
    assert_eq!(
        ok(decode_viewer_permissions_json(&viewer_permissions(json!({ "pullRequest": null })))),
        access(false, false, true, false)
    );
}

#[test]
fn reads_triage_as_enough_to_label_and_not_enough_to_write() {
    let decoded = ok(decode_viewer_permissions_json(&viewer_permissions(
        json!({ "viewerPermission": "TRIAGE", "pullRequest": { "viewerCanUpdate": false, "viewerDidAuthor": false } }),
    )));
    assert!(decoded.viewer.can_triage);
    assert!(!decoded.viewer.can_write);
    assert_eq!(
        decoded.repository_access(),
        GitHubRepositoryAccess {
            merge_capabilities: MERGE_CAPABILITIES,
            can_write: false
        }
    );
}

// label candidate decoding

fn label(name: &str, color: Option<&str>, description: Option<&str>, is_applied: bool) -> PullRequestLabelCandidate {
    PullRequestLabelCandidate {
        name: name.into(),
        color: color.map(Into::into),
        description: description.map(Into::into),
        is_applied,
    }
}

#[test]
fn marks_the_labels_the_pull_request_already_wears() {
    let list = ok(decode_label_candidates_json(&label_candidates(
        json!([{ "name": "bug", "color": "d73a4a", "description": "Something is broken" }, { "name": "size:XL", "color": "e4572e", "description": null }]),
        &["size:XL"],
        false,
    )));
    assert_eq!(
        list.candidates,
        vec![
            label("bug", Some("d73a4a"), Some("Something is broken"), false),
            label("size:XL", Some("e4572e"), None, true)
        ]
    );
    assert!(!list.truncated);
}

#[test]
fn keeps_a_worn_label_the_repository_no_longer_defines_so_it_can_be_taken_off() {
    let list = ok(decode_label_candidates_json(&label_candidates(json!([{ "name": "bug" }]), &["legacy"], false)));
    assert_eq!(
        list.candidates.iter().map(|label| (label.name.as_str(), label.is_applied)).collect::<Vec<_>>(),
        vec![("legacy", true), ("bug", false)]
    );
}

#[test]
fn says_so_when_the_repository_defines_more_labels_than_the_read_asked_for() {
    assert!(ok(decode_label_candidates_json(&label_candidates(json!([]), &[], true))).truncated);
}

// review thread decoding (the conversation read)

#[test]
fn carries_what_the_reader_may_do_with_the_pull_request_off_the_conversation_read() {
    let viewer = ok(decode_review_threads_json(&threads_with(
        json!([]),
        json!({ "viewerCanUpdate": false, "viewerDidAuthor": false }),
    )))
    .viewer;
    assert_eq!(
        viewer,
        GitHubPullRequestViewerFields {
            can_update: false,
            did_author: false
        }
    );
    let viewer = ok(decode_review_threads_json(&threads_with(json!([]), json!({})))).viewer;
    assert_eq!(
        viewer,
        GitHubPullRequestViewerFields {
            can_update: true,
            did_author: false
        }
    );
}

#[test]
fn anchors_a_thread_to_its_line_and_side_keeping_the_whole_conversation() {
    let page = ok(decode_review_threads_json(&threads_with(
        json!([{ "id": "PRRT_1", "isResolved": false, "isOutdated": false, "path": "src/a.ts", "line": 42, "diffSide": "LEFT",
                 "comments": { "totalCount": 2, "nodes": [thread_comment("c1", "first"), thread_comment("c2", "second")] } }]),
        json!({}),
    )));
    let comment = |id: &str, body: &str| PullRequestThreadComment {
        id: id.into(),
        author: Some(actor("bea-example", None, Some("https://avatars.example.test/b.png"))),
        body: body.into(),
        created_at: "2026-07-01T00:00:00Z".into(),
        url: Some(format!("https://github.example.test/acme/web/pull/1#discussion_r{id}")),
        reactions: Some(vec![]),
    };
    assert_eq!(
        page.threads.iter().map(|entry| entry.thread.clone()).collect::<Vec<_>>(),
        vec![PullRequestReviewThread {
            id: "PRRT_1".into(),
            path: "src/a.ts".into(),
            line: Some(42),
            side: PullRequestDiffSide::Left,
            is_resolved: false,
            is_outdated: false,
            comments: vec![comment("c1", "first"), comment("c2", "second")],
            comment_count: None,
            next_comments_cursor: None,
        }]
    );
}

#[test]
fn leaves_an_outdated_thread_without_a_line_rather_than_pinning_it_to_a_stale_one() {
    let page = ok(decode_review_threads_json(&threads_with(
        json!([{ "id": "PRRT_2", "isResolved": true, "isOutdated": true, "path": "src/a.ts", "line": null, "diffSide": "RIGHT",
                 "comments": { "totalCount": 1, "nodes": [thread_comment("c3", "stale")] } }]),
        json!({}),
    )));
    let thread = &page.threads[0].thread;
    assert_eq!((thread.line, thread.is_outdated, thread.is_resolved), (None, true, true));
}

#[test]
fn keeps_a_resolved_thread_in_the_conversation_as_well_as_against_its_line() {
    let page = ok(decode_review_threads_json(&threads_with(
        json!([{ "id": "PRRT_3", "isResolved": true, "path": "src/a.ts", "line": 7, "diffSide": "RIGHT", "comments": { "totalCount": 1, "nodes": [thread_comment("c4", "done")] } }]),
        json!({}),
    )));
    assert_eq!(thread_ids(&page), vec!["c4"]);
    assert_eq!(page.threads.len(), 1);
}

#[test]
fn puts_an_issue_comments_and_a_reviews_reactions_in_reactions_by_id_and_the_pull_requests_own_in_reactions() {
    let page = ok(decode_review_threads_json(&threads_with(
        json!([]),
        json!({
            "reactionGroups": [{ "content": "HEART", "viewerHasReacted": true, "reactors": { "totalCount": 1, "nodes": [{ "login": "bea-example" }] } }],
            "comments": { "nodes": [{ "id": "c1", "reactionGroups": [{ "content": "THUMBS_UP", "reactors": { "totalCount": 1, "nodes": [{ "login": "jules-example" }] } }] }] },
            "reviews": { "nodes": [{ "id": "r1", "reactionGroups": [{ "content": "EYES", "reactors": { "totalCount": 1, "nodes": [{ "login": "helper-bot" }] } }] }] },
        }),
    )));
    assert_eq!(page.reactions, vec![reaction(Content::Heart, 1, &["bea-example"], true)]);
    assert_eq!(
        page.reactions_by_id.into_iter().collect::<Vec<_>>(),
        vec![
            ("c1".to_owned(), vec![reaction(Content::ThumbsUp, 1, &["jules-example"], false)]),
            ("r1".to_owned(), vec![reaction(Content::Eyes, 1, &["helper-bot"], false)]),
        ]
    );
}

#[test]
fn leaves_the_viewers_own_login_out_of_the_pull_requests_own_reactions_matched_case_insensitively_while_count_still_counts_them() {
    let page = ok(decode_review_threads_json(
        &json!({ "data": { "viewer": { "login": "Bea-Example" }, "repository": { "pullRequest": {
            "reviewThreads": { "totalCount": 0, "nodes": [] },
            "reactionGroups": [{ "content": "HEART", "viewerHasReacted": true, "reactors": { "totalCount": 2, "nodes": [{ "login": "bea-example" }, { "login": "jules-example" }] } }],
        } } } })
        .to_string(),
    ));
    assert_eq!(page.reactions, vec![reaction(Content::Heart, 2, &["jules-example"], true)]);
}

// decodePullRequestNodeIdJson

#[test]
fn reads_the_pull_requests_own_node_id_which_a_reaction_on_its_description_is_addressed_by() {
    assert_eq!(
        ok(decode_pull_request_node_id_json(
            &json!({ "data": { "repository": { "pullRequest": { "id": "PR_kwDOA" } } } }).to_string()
        )),
        "PR_kwDOA"
    );
}

// REVIEW_THREADS_GRAPHQL_QUERY

#[test]
fn caps_the_initial_query_after_the_104_point_rate_limit_regression() {
    let captures = Regex::new(r"(?s)reviewThreads\(first: (\d+).*?comments\(first: (\d+)\)")
        .unwrap()
        .captures(REVIEW_THREADS_GRAPHQL_QUERY)
        .expect("review-thread connections");
    let threads: u64 = captures[1].parse().unwrap();
    let comments: u64 = captures[2].parse().unwrap();
    assert!(threads * comments <= 1_000);
}

#[test]
fn asks_for_reaction_groups_on_the_pull_request_itself_its_comments_its_reviews_and_each_threads_comments() {
    assert_eq!(REVIEW_THREADS_GRAPHQL_QUERY.matches("reactionGroups").count(), 4);
    assert!(REVIEW_THREADS_GRAPHQL_QUERY.contains("reviews(first:"));
}

// reviewer candidate decoding

fn candidate(id: &str, kind: PullRequestReviewerKind, name: Option<&str>, is_requested: bool) -> PullRequestReviewerCandidate {
    PullRequestReviewerCandidate {
        is_bot: None,
        login: id.into(),
        name: name.map(Into::into),
        avatar_url: None,
        id: id.into(),
        kind,
        is_requested,
    }
}

#[test]
fn leaves_the_author_out_of_the_people_their_own_pull_request_can_be_sent_to() {
    let list = ok(decode_reviewer_candidates_json(&reviewer_candidates(
        json!([{ "login": "bea-example" }, { "login": "ada-example", "name": "Ada Example" }]),
        vec![],
        Some("bea-example"),
        false,
    )));
    assert_eq!(
        list.candidates,
        vec![candidate("ada-example", PullRequestReviewerKind::User, Some("Ada Example"), false)]
    );
    assert!(!list.truncated);
}

#[test]
fn marks_whoever_has_already_been_asked_and_leaves_the_rest_to_be_asked() {
    let list = ok(decode_reviewer_candidates_json(&reviewer_candidates(
        json!([{ "login": "ada-example" }, { "login": "helper-bot" }]),
        vec![json!({ "login": "ada-example" })],
        None,
        false,
    )));
    assert_eq!(
        list.candidates
            .iter()
            .map(|candidate| (candidate.login.as_str(), candidate.is_requested))
            .collect::<Vec<_>>(),
        vec![("ada-example", true), ("helper-bot", false)]
    );
}

#[test]
fn keeps_a_requested_team_apart_from_the_people_so_the_request_can_be_taken_back() {
    let list = ok(decode_reviewer_candidates_json(&reviewer_candidates(
        json!([{ "login": "ada-example" }]),
        vec![json!({ "slug": "reviewers", "name": "Reviewers" })],
        None,
        false,
    )));
    assert_eq!(
        list.candidates,
        vec![
            candidate("reviewers", PullRequestReviewerKind::Team, Some("Reviewers"), true),
            candidate("ada-example", PullRequestReviewerKind::User, None, false)
        ]
    );
}

#[test]
fn says_so_when_the_repository_has_more_people_than_the_read_asked_for() {
    assert!(
        ok(decode_reviewer_candidates_json(&reviewer_candidates(
            json!([{ "login": "ada-example" }]),
            vec![],
            None,
            true
        )))
        .truncated
    );
}

// reviewer request payload

#[test]
fn sends_people_and_teams_in_the_two_lists_github_keeps_them_in() {
    let reviewers = [
        ReviewerRef {
            id: "ada-example".into(),
            kind: PullRequestReviewerKind::User,
        },
        ReviewerRef {
            id: "reviewers".into(),
            kind: PullRequestReviewerKind::Team,
        },
        ReviewerRef {
            id: "helper-bot".into(),
            kind: PullRequestReviewerKind::User,
        },
    ];
    assert_eq!(
        build_reviewer_request_json(&reviewers),
        r#"{"reviewers":["ada-example","helper-bot"],"team_reviewers":["reviewers"]}"#
    );
}

#[test]
fn sends_both_lists_even_where_one_of_them_is_empty_which_is_what_github_reads() {
    let reviewers = [ReviewerRef {
        id: "ada-example".into(),
        kind: PullRequestReviewerKind::User,
    }];
    assert_eq!(build_reviewer_request_json(&reviewers), r#"{"reviewers":["ada-example"],"team_reviewers":[]}"#);
}

// review submission payload

#[test]
fn sends_the_verdict_the_summary_and_every_line_comment_in_one_body() {
    let comments: Vec<PullRequestReviewCommentDraft> = serde_json::from_value(json!([
        { "path": "src/a.ts", "position": { "kind": "added", "newLine": 12 }, "body": "rename this" },
        { "path": "src/b.ts", "position": { "kind": "deleted", "oldLine": 3 }, "body": "why remove?" },
    ]))
    .unwrap();
    let payload = build_review_submission_json(PullRequestReviewVerdict::RequestChanges, "Two things.", &comments);
    assert_eq!(
        payload,
        r#"{"event":"REQUEST_CHANGES","body":"Two things.","comments":[{"path":"src/a.ts","line":12,"side":"RIGHT","body":"rename this"},{"path":"src/b.ts","line":3,"side":"LEFT","body":"why remove?"}]}"#
    );
}

#[test]
fn sends_an_approval_with_no_words_and_no_comments() {
    assert_eq!(
        build_review_submission_json(PullRequestReviewVerdict::Approve, "", &[]),
        r#"{"event":"APPROVE","body":"","comments":[]}"#
    );
}

// decodePullRequestFilesJson

fn patch_of(files: Value) -> GitHubPullRequestFilesPatch {
    ok(decode_pull_request_files_json(&files.to_string()))
}

#[test]
fn quotes_literal_backslashes_without_interpreting_them_as_escapes() {
    let result = patch_of(json!([{ "filename": r"src\notes.ts", "status": "modified", "patch": "@@ -1 +1 @@\n-old\n+new" }]));
    assert_eq!(
        result.patch,
        [
            r#"diff --git "a/src\\notes.ts" "b/src\\notes.ts""#,
            r#"--- "a/src\\notes.ts""#,
            r#"+++ "b/src\\notes.ts""#,
            "@@ -1 +1 @@",
            "-old",
            "+new",
            ""
        ]
        .join("\n")
    );
}

#[test]
fn preserves_spaces_and_literal_backslashes_in_both_rename_paths() {
    let result = patch_of(json!([{ "previous_filename": r" old\name.ts ", "filename": r" new\name.ts ", "status": "renamed" }]));
    assert_eq!(
        result.patch,
        [
            r#"diff --git "a/ old\\name.ts " "b/ new\\name.ts ""#,
            r#"rename from " old\\name.ts ""#,
            r#"rename to " new\\name.ts ""#,
            r#"--- "a/ old\\name.ts ""#,
            r#"+++ "b/ new\\name.ts ""#,
            "",
        ]
        .join("\n")
    );
}

#[test]
fn assembles_a_unified_patch_the_files_api_does_not_return() {
    let result = patch_of(json!([{ "filename": "src/app.ts", "status": "modified", "patch": "@@ -1 +1 @@\n-old\n+new" }]));
    assert_eq!(
        result.patch,
        [
            "diff --git a/src/app.ts b/src/app.ts",
            "--- a/src/app.ts",
            "+++ b/src/app.ts",
            "@@ -1 +1 @@",
            "-old",
            "+new",
            ""
        ]
        .join("\n")
    );
    assert!(!result.truncated);
    assert_eq!(result.raw_count, 1);
}

#[test]
fn points_an_added_file_at_dev_null_on_the_left_and_a_removed_one_on_the_right() {
    let result = patch_of(json!([
        { "filename": "src/new.ts", "status": "added", "patch": "@@ -0,0 +1 @@\n+hello" },
        { "filename": "src/gone.ts", "status": "removed", "patch": "@@ -1 +0,0 @@\n-bye" },
    ]));
    assert_eq!(
        result.patch,
        [
            "diff --git a/src/new.ts b/src/new.ts",
            "new file mode 100644",
            "--- /dev/null",
            "+++ b/src/new.ts",
            "@@ -0,0 +1 @@",
            "+hello",
            "diff --git a/src/gone.ts b/src/gone.ts",
            "deleted file mode 100644",
            "--- a/src/gone.ts",
            "+++ /dev/null",
            "@@ -1 +0,0 @@",
            "-bye",
            "",
        ]
        .join("\n")
    );
}

#[test]
fn names_both_paths_of_a_rename_counting_its_hunks_against_the_old_one() {
    let result = patch_of(json!([{ "filename": "src/new.ts", "status": "renamed", "previous_filename": "src/old.ts", "patch": "@@ -1 +1 @@\n-old\n+new" }]));
    assert_eq!(
        result.patch,
        [
            "diff --git a/src/old.ts b/src/new.ts",
            "rename from src/old.ts",
            "rename to src/new.ts",
            "--- a/src/old.ts",
            "+++ b/src/new.ts",
            "@@ -1 +1 @@",
            "-old",
            "+new",
            "",
        ]
        .join("\n")
    );
}

#[test]
fn still_lists_a_file_github_sent_no_hunks_for_and_says_what_was_withheld() {
    let result = patch_of(json!([
        // Binary: it changed, and none of it can be shown.
        { "filename": "logo.png", "status": "modified", "additions": 4, "deletions": 2 },
        { "filename": "src/app.ts", "status": "modified", "additions": 1, "deletions": 1, "patch": "@@ -1 +1 @@\n-old\n+new" },
    ]));
    assert!(result.patch.contains("diff --git a/logo.png b/logo.png"));
    assert!(result.patch.contains("diff --git a/src/app.ts b/src/app.ts"));
    assert!(result.truncated);
    assert_eq!(result.raw_count, 2);
    assert_eq!(
        serde_json::to_value(&result.omitted_file_stats).unwrap(),
        json!([{ "path": "logo.png", "additions": 4, "deletions": 2 }])
    );
}

#[test]
fn does_not_call_a_pure_rename_incomplete_since_it_has_no_hunks_to_withhold() {
    let result = patch_of(json!([{ "filename": "src/new.ts", "previous_filename": "src/old.ts", "status": "renamed", "additions": 0, "deletions": 0 }]));
    assert!(result.patch.contains("rename from src/old.ts"));
    assert!(!result.truncated);
}

// how far a branch trails its base

#[test]
fn reads_the_commit_count_and_whether_this_viewer_may_move_the_branch() {
    let decoded = ok(decode_base_comparison_json(&comparison(
        json!({ "viewerCanUpdateBranch": true, "baseRef": { "compare": { "behindBy": 12 } } }),
    )));
    assert_eq!(serde_json::to_value(decoded).unwrap(), json!({ "behindBy": 12, "viewerCanUpdate": true }));
}

#[test]
fn reads_a_current_branch_as_nothing_to_do() {
    let decoded = ok(decode_base_comparison_json(&comparison(
        json!({ "viewerCanUpdateBranch": false, "baseRef": { "compare": { "behindBy": 0 } } }),
    )));
    assert_eq!(serde_json::to_value(decoded).unwrap(), json!({ "behindBy": 0, "viewerCanUpdate": false }));
}

#[test]
fn answers_unknown_where_the_head_could_not_be_compared() {
    // A fork whose repository is gone: a null comparison beside a perfectly good pull request.
    assert_eq!(
        ok(decode_base_comparison_json(&comparison(
            json!({ "viewerCanUpdateBranch": true, "baseRef": null })
        )))
        .behind_by,
        None
    );
    assert_eq!(
        ok(decode_base_comparison_json(&comparison(Value::Null))),
        GitHubBaseComparison {
            behind_by: None,
            viewer_can_update: false
        }
    );
}

#[test]
fn refuses_a_body_that_is_not_the_answer_to_this_question() {
    assert!(decode_base_comparison_json("{").is_err());
}

// decodePullRequestFilesViewedJson

fn viewed(path: &str, state: PullRequestFileViewedState) -> PullRequestFileViewed {
    PullRequestFileViewed { path: path.into(), state }
}

#[test]
fn reads_each_files_state_and_where_the_next_page_carries_on() {
    let decoded = ok(decode_pull_request_files_viewed_json(&files_viewed_page(
        json!([{ "path": "src/a.ts", "viewerViewedState": "VIEWED" }, { "path": "src/b.ts", "viewerViewedState": "UNVIEWED" }, { "path": "src/c.ts", "viewerViewedState": "DISMISSED" }]),
        json!({ "hasNextPage": true, "endCursor": "cursor-2" }),
    )));
    assert_eq!(
        decoded,
        GitHubPullRequestFilesViewedPage {
            files: vec![
                viewed("src/a.ts", PullRequestFileViewedState::Viewed),
                viewed("src/b.ts", PullRequestFileViewedState::Unviewed),
                viewed("src/c.ts", PullRequestFileViewedState::Dismissed)
            ],
            next_cursor: Some("cursor-2".into()),
        }
    );
}

#[test]
fn treats_a_state_it_has_never_heard_of_as_unread_rather_than_failing_the_page() {
    let decoded = ok(decode_pull_request_files_viewed_json(&files_viewed_page(
        json!([{ "path": "src/a.ts", "viewerViewedState": "SOMETHING_NEW" }]),
        json!({ "hasNextPage": false, "endCursor": null }),
    )));
    assert_eq!(
        decoded,
        GitHubPullRequestFilesViewedPage {
            files: vec![viewed("src/a.ts", PullRequestFileViewedState::Unviewed)],
            next_cursor: None
        }
    );
}

#[test]
fn answers_empty_for_a_pull_request_the_host_has_nothing_to_say_about() {
    let decoded = ok(decode_pull_request_files_viewed_json(
        &json!({ "data": { "repository": { "pullRequest": null } } }).to_string(),
    ));
    assert_eq!(
        decoded,
        GitHubPullRequestFilesViewedPage {
            files: vec![],
            next_cursor: None
        }
    );
}

// buildSetFilesViewedGraphQlMutation

#[test]
fn asks_for_nothing_when_nothing_was_pressed() {
    assert_eq!(build_set_files_viewed_graph_ql_mutation::<&str>(&[]), None);
}

#[test]
fn clears_and_restores_in_one_document_each_file_under_its_own_alias() {
    let mutation = build_set_files_viewed_graph_ql_mutation(&[("src/a.ts", true), ("src/b.ts", false)]).unwrap();
    assert!(mutation.query.contains("mutation($pullRequestId: ID!, $path0: String!, $path1: String!)"));
    assert!(mutation
        .query
        .contains("f0: markFileAsViewed(input: { pullRequestId: $pullRequestId, path: $path0 })"));
    assert!(mutation
        .query
        .contains("f1: unmarkFileAsViewed(input: { pullRequestId: $pullRequestId, path: $path1 })"));
    assert_eq!(
        mutation.variables,
        vec![("path0".to_owned(), "src/a.ts".to_owned()), ("path1".to_owned(), "src/b.ts".to_owned())]
    );
}

#[test]
fn keeps_a_path_out_of_the_document_so_one_cannot_be_read_as_part_of_it() {
    let path = "\") { __typename } evil: markFileAsViewed(input: { path: \"x";
    let mutation = build_set_files_viewed_graph_ql_mutation(&[(path, true)]).unwrap();
    assert!(!mutation.query.contains("evil"));
    assert_eq!(mutation.variables[0].1, path);
}

// host-native stack decoding

fn expect_stack(overrides: Value) -> GitHubPullRequestStack {
    ok(decode_pull_request_stacks_json(&stacks_json(stack(overrides)))).expect("a stack")
}

fn layer(number: i64, head_branch: &str, state: PullRequestState) -> GitHubPullRequestStackLayer {
    GitHubPullRequestStackLayer {
        title: None,
        is_draft: None,
        head_sha: None,
        number,
        head_branch: head_branch.into(),
        state,
    }
}

#[test]
fn reads_the_first_stack_bottom_to_top_with_merged_at_outranking_state() {
    assert_eq!(
        expect_stack(json!({})),
        GitHubPullRequestStack {
            id: "42".into(),
            number: 3,
            url: "https://api.github.example.test/repos/acme/web/stacks/3".into(),
            base: "main".into(),
            layers: vec![
                layer(10, "feat/one", PullRequestState::Merged),
                layer(11, "feat/two", PullRequestState::Open),
                layer(12, "feat/three", PullRequestState::Closed)
            ],
        }
    );
}

#[test]
fn retains_the_detailed_layer_titles_draft_state_and_expected_revision() {
    let stack = expect_stack(json!({ "pull_requests": [
        { "number": 11, "title": "Second layer", "draft": true, "head": { "ref": "feat/two", "sha": "abc123" }, "state": "open", "merged_at": null },
    ] }));
    assert_eq!(
        stack.layers,
        vec![GitHubPullRequestStackLayer {
            title: Some("Second layer".into()),
            is_draft: Some(true),
            head_sha: Some("abc123".into()),
            ..layer(11, "feat/two", PullRequestState::Open)
        }]
    );
}

#[test]
fn accepts_a_base_named_as_a_bare_branch_which_is_what_the_preview_started_out_sending() {
    assert_eq!(expect_stack(json!({ "base": "develop" })).base, "develop");
}

#[test]
fn prefers_the_page_a_person_opens_over_the_api_url_where_the_host_reports_one() {
    assert_eq!(
        expect_stack(json!({ "html_url": "https://github.example.test/acme/web/stacks/3" })).url,
        "https://github.example.test/acme/web/stacks/3"
    );
}

#[test]
fn falls_back_to_the_node_id_then_the_number_for_a_stack_without_an_id() {
    let without_id = ok(decode_pull_request_stacks_json(&stacks_json(without(&stack(json!({})), "id")))).unwrap();
    assert_eq!(without_id.id, "STK_kwDO");
    assert_eq!(expect_stack(json!({ "id": null, "node_id": null })).id, "3");
}

#[test]
fn reads_an_empty_listing_as_not_stacked() {
    assert_eq!(ok(decode_pull_request_stacks_json("[]")), None);
}

#[test]
fn refuses_a_stack_without_a_number_or_without_its_pull_requests() {
    assert!(decode_pull_request_stacks_json(&stacks_json(without(&stack(json!({})), "number"))).is_err());
    assert!(decode_pull_request_stacks_json(&stacks_json(without(&stack(json!({})), "pull_requests"))).is_err());
    assert!(decode_pull_request_stacks_json("{").is_err());
}

// pull request stack membership batches

#[test]
fn maps_aliases_while_skipping_missing_pull_requests_and_incomplete_memberships() {
    let memberships = ok(decode_pull_request_stack_memberships_json(&stack_memberships()));
    assert_eq!(
        memberships.into_iter().collect::<Vec<_>>(),
        vec![(
            0,
            PullRequestStackMembership {
                number: 3,
                size: 2,
                base: "main".into(),
                position: 1
            }
        )]
    );
}

#[test]
fn refuses_malformed_responses_and_unsafe_query_selectors() {
    assert!(decode_pull_request_stack_memberships_json("{\"errors\":[]}").is_err());
    assert_eq!(build_pull_request_stack_memberships_graph_ql_query("acme/web\") { x } #", &[1]), None);
    assert_eq!(build_pull_request_stack_memberships_graph_ql_query("acme/web", &[0]), None);
    // `[1.5]` of the TS test cannot be written: the numbers are integers here.
    assert_eq!(build_pull_request_stack_memberships_graph_ql_query("acme/web", &[]), None);
    assert!(build_pull_request_stack_memberships_graph_ql_query("acme/web", &[7, 8])
        .unwrap()
        .contains("pullRequest(number: 8)"));
}

// batched pull request summaries

#[test]
fn refuses_a_repository_graphql_cannot_address_rather_than_writing_it_into_the_document() {
    assert_eq!(build_pull_request_summaries_graph_ql_query(&[("acme/web\") { x } #", 1)]), None);
    assert_eq!(build_pull_request_summaries_graph_ql_query(&[("acme/web", 0)]), None);
    assert_eq!(build_pull_request_summaries_graph_ql_query::<&str>(&[]), None);
}

#[test]
fn files_each_answer_by_its_alias_and_skips_what_github_or_the_decoder_could_not_give() {
    let summaries = ok(decode_pull_request_summaries_json(&summaries()));
    assert_eq!(summaries.keys().copied().collect::<Vec<_>>(), vec![0]);
    let summary = &summaries[&0];
    assert_eq!(summary.number, 7);
    assert_eq!(summary.state, PullRequestState::Merged);
    assert_eq!(summary.merged_at.as_deref(), Some("2026-08-24T00:00:00Z"));
    let author = summary.author.as_ref().unwrap();
    assert_eq!((author.login.as_str(), author.is_bot), ("deps-bot", Some(true)));
    assert_eq!(summary.checks_state, Some(Rollup::Failing));
    assert_eq!(summary.mergeability, PullRequestMergeability::Unknown);
    assert_eq!(summary.additions, 0);
}

// Beyond the TS tests: the reads the CLI makes that its test file leaves to the golden corpus.

#[test]
fn reads_the_core_detail_with_the_viewers_standing_and_the_base_comparison() {
    let core = ok(decode_pull_request_core_json(&core_value().to_string()));
    assert_eq!(core.detail.item.review_request_logins, vec!["bea-example".to_owned()]);
    assert!(core.detail.item.has_team_review_request);
    assert_eq!(core.detail.head_sha.as_deref(), Some("0123abcd"));
    assert_eq!(core.detail.auto_merge_method, Some(PullRequestMergeMethod::Rebase));
    assert_eq!(
        core.detail.checks.iter().map(|check| (check.name.as_str(), check.status)).collect::<Vec<_>>(),
        vec![
            ("ci/legacy", Check::Success),
            ("CI / build", Check::Success),
            ("Release / build", Check::Failure),
            ("lint", Check::Pending)
        ]
    );
    assert!(core.checks_truncated);
    assert_eq!(
        serde_json::to_value(&core.comparison).unwrap(),
        json!({ "behindBy": 4, "viewerCanUpdate": true })
    );
    assert!(core.viewer_access.viewer.can_write && core.viewer_access.viewer.can_update && !core.viewer_access.viewer.did_author);
}

#[test]
fn reports_a_decode_failure_as_a_schema_error_cause() {
    let failure = decode_pull_request_detail_json("{\"number\":\"7\"}").unwrap_err();
    assert_eq!(failure.message(), "Invalid type\n  at [\"number\"]");
    assert_eq!(
        failure.cause().defect(),
        json!({ "name": "SchemaError", "message": "Invalid type\n  at [\"number\"]" })
    );
}

#[test]
fn names_reactions_the_way_github_spells_them() {
    assert_eq!(git_hub_reaction_content(Content::ThumbsUp), "THUMBS_UP");
    assert_eq!(git_hub_reaction_content(Content::Eyes), "EYES");
}
