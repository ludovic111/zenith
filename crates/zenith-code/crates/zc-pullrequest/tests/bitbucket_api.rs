//! `BitbucketPullRequestApi.test.ts`: the pull request API over a scripted `request`.

#![allow(clippy::result_large_err)]

mod support_bitbucket;

use std::sync::Arc;

use serde_json::{json, Value};
use support_bitbucket::*;
use zc_contracts::{
    LitDeleted, PullRequestAction, PullRequestCheckStatus, PullRequestListState, PullRequestMergeMethod, PullRequestMergeability,
    PullRequestReviewCommentDraft, PullRequestReviewPosition, PullRequestReviewPositionDeleted, PullRequestReviewVerdict, PullRequestReviewerCandidateList,
};
use zc_pullrequest::bitbucket::api::{BitbucketPullRequestApi, BitbucketPullRequestApiError, ListPullRequestsInput};
use zc_pullrequest::provider::ProviderListCursor;
use zc_sourcecontrol::bitbucket::api::BitbucketResponseBody;
use zc_sourcecontrol::util::ManualClock;

const MAX_BYTES: usize = 8 * 1024 * 1024;

fn api(mock: &Arc<MockRequester>) -> (BitbucketPullRequestApi, Arc<ManualClock>) {
    let clock = ManualClock::new(0);
    (BitbucketPullRequestApi::with_requester(mock.clone(), clock.clone()), clock)
}

fn page(count: i64, first_number: i64, next: Option<&str>) -> String {
    let values: Vec<Value> = (0..count)
        .map(|index| {
            json!({
                "id": first_number + index,
                "title": format!("Pull request {}", first_number + index),
                "state": "OPEN",
                "created_on": "2026-06-16T05:04:32+00:00",
                "updated_on": "2026-06-16T05:04:33+00:00",
                "source": {"branch": {"name": "feat/page"}},
                "destination": {"branch": {"name": "master"}},
                "links": {"html": {"href": format!("https://bitbucket.example.test/acme/web/pull-requests/{first_number}")}},
            })
        })
        .collect();
    let mut value = json!({"pagelen": 50, "size": count, "values": values});
    if let Some(next) = next {
        value["next"] = json!(next);
    }
    value.to_string()
}

fn value_page(values: Value, next: Option<&str>) -> String {
    let mut value = json!({"values": values});
    if let Some(next) = next {
        value["next"] = json!(next);
    }
    value.to_string()
}

/// Who opened the pull request, and two accounts that could review it.
fn avery() -> Value {
    json!({"uuid": "{avery}", "nickname": "avery"})
}
fn quinn() -> Value {
    json!({"uuid": "{quinn}", "nickname": "quinn"})
}
fn robin() -> Value {
    json!({"uuid": "{robin}", "nickname": "robin"})
}

/// One pull request as `/pullrequests/{id}` answers with it.
fn pull_request_json(overrides: Value) -> String {
    let mut value = json!({
        "id": 7,
        "title": "Pull request 7",
        "state": "OPEN",
        "author": avery(),
        "created_on": "2026-06-16T05:04:32+00:00",
        "updated_on": "2026-06-16T05:04:33+00:00",
        "source": {"branch": {"name": "feat/page"}},
        "destination": {"branch": {"name": "master"}},
        "links": {"html": {"href": "https://bitbucket.example.test/acme/web/pull-requests/7"}},
    });
    if let (Value::Object(base), Value::Object(extra)) = (&mut value, overrides) {
        base.extend(extra);
    }
    value.to_string()
}

fn list(repository: &str, state: PullRequestListState, limit: i64) -> ListPullRequestsInput {
    ListPullRequestsInput {
        repository: repository.into(),
        state,
        limit,
        query: None,
        cursor: None,
    }
}

const NEXT: &str = "https://api.bitbucket.example.test/2.0/repositories/acme/web/pullrequests?page=2";

#[tokio::test]
async fn asks_for_reviewers_newest_first_at_bitbuckets_page_ceiling() {
    let mock = MockRequester::new();
    mock.once(response(page(3, 1, None)));
    let (api, _) = api(&mock);
    let batch = api.list_pull_requests(list("acme/web", PullRequestListState::Open, 50)).await.unwrap();
    assert_eq!(batch.items.len(), 3);
    assert!(!batch.truncated);
    let url = mock.call_at(0).url;
    assert!(url.contains("/repositories/acme/web/pullrequests"));
    assert!(url.contains("state=OPEN"));
    // Over 50 Bitbucket answers with an empty page and no error, so it is never exceeded.
    assert!(url.contains("pagelen=50"));
    assert!(url.contains("sort=-updated_on"));
    assert!(url.contains("fields=%2Bvalues.reviewers"));
}

#[tokio::test]
async fn follows_the_cursor_bitbucket_sends_rather_than_counting_offsets() {
    let mock = MockRequester::new();
    mock.once(response(page(50, 1, Some(NEXT)))).once(response(page(50, 51, None)));
    let (api, _) = api(&mock);
    let batch = api.list_pull_requests(list("acme/web", PullRequestListState::Open, 100)).await.unwrap();
    assert_eq!(batch.items.len(), 100);
    assert!(!batch.truncated);
    assert_eq!(mock.call_at(1).url, NEXT);
}

#[tokio::test]
async fn stops_at_the_callers_page_and_says_more_remain() {
    let mock = MockRequester::new();
    mock.once(response(page(50, 1, Some(NEXT))));
    let (api, _) = api(&mock);
    let batch = api.list_pull_requests(list("acme/web", PullRequestListState::Open, 50)).await.unwrap();
    assert_eq!(batch.items.len(), 50);
    assert!(batch.truncated);
    assert_eq!(mock.calls().len(), 1);
}

#[tokio::test]
async fn counts_the_rows_it_walked_past_as_more_to_come() {
    // Bitbucket pages in fifties whatever was asked for, so a request for ninety-nine reads a
    // hundred and drops one. That row is more results.
    let mock = MockRequester::new();
    mock.once(response(page(50, 1, Some(NEXT)))).once(response(page(50, 51, None)));
    let (api, _) = api(&mock);
    let batch = api.list_pull_requests(list("acme/web", PullRequestListState::Open, 99)).await.unwrap();
    assert_eq!(batch.items.len(), 99);
    assert!(batch.truncated);
}

#[tokio::test]
async fn searches_with_a_filter_expression() {
    let mock = MockRequester::new();
    mock.once(response(page(0, 1, None)));
    let (api, _) = api(&mock);
    api.list_pull_requests(ListPullRequestsInput {
        query: Some("page".into()),
        ..list("acme/web", PullRequestListState::Open, 50)
    })
    .await
    .unwrap();
    assert_eq!(mock.filter_of_call(0).as_deref(), Some(r#"(title ~ "page" OR description ~ "page")"#));
    // The state filter beside it still stands, which the brackets are there to keep.
    assert!(mock.call_at(0).url.contains("state=OPEN"));
}

#[tokio::test]
async fn escapes_a_quote_and_a_backslash_so_a_search_cannot_reshape_the_filter() {
    let mock = MockRequester::new();
    mock.once(response(page(0, 1, None)));
    let (api, _) = api(&mock);
    api.list_pull_requests(ListPullRequestsInput {
        query: Some(r#"a\" OR state = "MERGED""#.into()),
        ..list("acme/web", PullRequestListState::Open, 50)
    })
    .await
    .unwrap();
    let literal = r#"a\\\" OR state = \"MERGED\""#;
    assert_eq!(mock.filter_of_call(0), Some(format!(r#"(title ~ "{literal}" OR description ~ "{literal}")"#)));
}

#[tokio::test]
async fn asks_for_no_filter_at_all_when_the_reader_typed_only_spaces() {
    let mock = MockRequester::new();
    mock.once(response(page(0, 1, None)));
    let (api, _) = api(&mock);
    api.list_pull_requests(ListPullRequestsInput {
        query: Some("   ".into()),
        ..list("acme/web", PullRequestListState::Open, 50)
    })
    .await
    .unwrap();
    assert_eq!(mock.filter_of_call(0), None);
}

#[tokio::test]
async fn carries_on_from_the_instant_the_last_slice_ended_on() {
    let mock = MockRequester::new();
    mock.once(response(page(0, 1, None)));
    let (api, _) = api(&mock);
    api.list_pull_requests(ListPullRequestsInput {
        cursor: Some(ProviderListCursor {
            updated_before: "2026-07-02T00:00:00.123456+00:00".into(),
            delivered: 50,
        }),
        ..list("acme/web", PullRequestListState::Open, 50)
    })
    .await
    .unwrap();
    // Inclusive, so the rows already sent at that instant come back for the caller to drop.
    assert_eq!(mock.filter_of_call(0).as_deref(), Some("updated_on <= 2026-07-02T00:00:00.123456+00:00"));
    assert!(mock.call_at(0).url.contains("sort=-updated_on"));
}

#[tokio::test]
async fn narrows_by_the_readers_words_and_by_where_it_left_off_at_once() {
    let mock = MockRequester::new();
    mock.once(response(page(0, 1, None)));
    let (api, _) = api(&mock);
    api.list_pull_requests(ListPullRequestsInput {
        query: Some("page".into()),
        cursor: Some(ProviderListCursor {
            updated_before: "2026-07-02T00:00:00+00:00".into(),
            delivered: 50,
        }),
        ..list("acme/web", PullRequestListState::Open, 50)
    })
    .await
    .unwrap();
    // Bitbucket takes one `q`, so the two narrowings are joined, and the search keeps its
    // brackets, which keeps the AND out of its OR.
    assert_eq!(
        mock.filter_of_call(0).as_deref(),
        Some(r#"(title ~ "page" OR description ~ "page") AND updated_on <= 2026-07-02T00:00:00+00:00"#)
    );
}

#[tokio::test]
async fn asks_for_declined_pull_requests_on_the_closed_tab() {
    let mock = MockRequester::new();
    mock.once(response(page(0, 1, None)));
    let (api, _) = api(&mock);
    api.list_pull_requests(list("acme/web", PullRequestListState::Closed, 50)).await.unwrap();
    assert!(mock.call_at(0).url.contains("state=DECLINED"));
}

#[tokio::test]
async fn asks_for_every_state_at_once_on_the_all_tab() {
    let mock = MockRequester::new();
    mock.once(response(page(0, 1, None)));
    let (api, _) = api(&mock);
    api.list_pull_requests(list("acme/web", PullRequestListState::All, 50)).await.unwrap();
    // Bitbucket unions repeated state parameters, which is the only way to span them.
    let url = mock.call_at(0).url;
    for state in ["OPEN", "MERGED", "DECLINED", "SUPERSEDED"] {
        assert!(url.contains(&format!("state={state}")), "{url}");
    }
}

#[tokio::test]
async fn counts_a_superseded_pull_request_as_closed() {
    let mock = MockRequester::new();
    mock.once(response(page(0, 1, None)));
    let (api, _) = api(&mock);
    api.list_pull_requests(list("acme/web", PullRequestListState::Closed, 50)).await.unwrap();
    assert!(mock.call_at(0).url.contains("state=DECLINED"));
    assert!(mock.call_at(0).url.contains("state=SUPERSEDED"));
}

#[tokio::test]
async fn refuses_a_repository_that_is_not_workspace_and_slug() {
    let mock = MockRequester::new();
    let (api, _) = api(&mock);
    let error = api.list_pull_requests(list("acme/team/web", PullRequestListState::Open, 50)).await.unwrap_err();
    assert_eq!(error.tag(), "BitbucketRepositoryUnsupportedError");
    assert!(mock.calls().is_empty());
}

const PATCH: &str = "diff --git a/a.ts b/a.ts\n--- a/a.ts\n+++ b/a.ts\n@@ -1 +1 @@\n-a\n+b\n";

#[tokio::test]
async fn returns_the_diff_verbatim_because_bitbucket_already_sends_a_patch() {
    let mock = MockRequester::new();
    mock.once(response(PATCH));
    let (api, _) = api(&mock);
    let diff = api.get_pull_request_diff("acme/web", 7, None).await.unwrap();
    assert_eq!(diff.patch, PATCH);
    assert!(!diff.truncated);
    let call = mock.call_at(0);
    assert_eq!(call.url, "/repositories/acme/web/pullrequests/7/diff");
    // A diff of any size would otherwise be read into memory whole.
    assert_eq!(call.max_bytes, Some(MAX_BYTES));
}

#[tokio::test]
async fn reads_a_named_commits_own_patch() {
    let mock = MockRequester::new();
    mock.once(response(PATCH));
    let (api, _) = api(&mock);
    let diff = api
        .get_pull_request_diff("acme/web", 7, Some("a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0"))
        .await
        .unwrap();
    assert_eq!(diff.patch, PATCH);
    let call = mock.call_at(0);
    assert_eq!(call.url, "/repositories/acme/web/diff/a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0");
    assert_eq!(call.max_bytes, Some(MAX_BYTES));
}

#[tokio::test]
async fn refuses_a_commit_that_is_not_a_sha_rather_than_reading_it_into_a_url() {
    let mock = MockRequester::new();
    let (api, _) = api(&mock);
    let error = api
        .get_pull_request_diff("acme/web", 7, Some("../../acme/other/diff/deadbeef"))
        .await
        .unwrap_err();
    assert_eq!(error.tag(), "BitbucketDiffCommitError");
    assert!(mock.calls().is_empty());
}

fn pairs(revisions: &[(String, String)]) -> Vec<(&str, &str)> {
    revisions.iter().map(|(path, revision)| (path.as_str(), revision.as_str())).collect()
}

#[tokio::test]
async fn reads_every_file_version_the_patch_states_not_only_the_paths_asked_about() {
    let mock = MockRequester::new();
    mock.once(response(
        [
            "diff --git a/a.ts b/a.ts",
            "index 1111111..2222222 100644",
            "--- a/a.ts",
            "+++ b/a.ts",
            "@@ -1 +1 @@",
            "-a",
            "+b",
            "diff --git a/b.ts b/b.ts",
            "index 3333333..4444444 100644",
            "--- a/b.ts",
            "+++ b/b.ts",
            "@@ -1 +1 @@",
            "-c",
            "+d",
            "",
        ]
        .join("\n"),
    ));
    let (api, _) = api(&mock);
    let revisions = api.get_file_revisions("acme/web", 71, &["a.ts".into(), "missing.ts".into()]).await.unwrap();
    // `b.ts` was not asked about and is reported anyway; `missing.ts` was asked about and the
    // whole patch was read without finding it, which is what a deleted file looks like.
    assert_eq!(pairs(&revisions.revisions), vec![("a.ts", "2222222"), ("b.ts", "4444444"), ("missing.ts", "")]);
    assert!(revisions.complete);
    assert_eq!(mock.call_at(0).url, "/repositories/acme/web/pullrequests/71/diff");
}

#[tokio::test]
async fn says_nothing_about_the_files_past_the_end_of_a_patch_it_could_not_read_whole() {
    let mock = MockRequester::new();
    mock.once(Ok(BitbucketResponseBody {
        body: "diff --git a/a.ts b/a.ts\nindex 1111111..2222222 100644\n@@ -1 +1 @@\n".into(),
        truncated: true,
    }));
    let (api, _) = api(&mock);
    let revisions = api
        .get_file_revisions("acme/web", 72, &["a.ts".into(), "past-the-cut.ts".into()])
        .await
        .unwrap();
    assert_eq!(pairs(&revisions.revisions), vec![("a.ts", "2222222")]);
    // `past-the-cut.ts` gets no empty version, and nothing here may be held as the whole story.
    assert!(!revisions.complete);
}

#[tokio::test]
async fn reads_the_patch_once_for_a_run_of_ticks_not_once_a_tick() {
    let mock = MockRequester::new();
    mock.always(response(
        [
            "diff --git a/a.ts b/a.ts",
            "index 1111111..2222222 100644",
            "@@ -1 +1 @@",
            "diff --git a/b.ts b/b.ts",
            "index 3333333..4444444 100644",
            "@@ -1 +1 @@",
            "",
        ]
        .join("\n"),
    ));
    let (api, _) = api(&mock);
    let first = api.get_file_revisions("acme/web", 74, &["a.ts".into()]).await.unwrap();
    // A path nobody has asked about before, which is what every tick after the first names.
    let second = api.get_file_revisions("acme/web", 74, &["b.ts".into()]).await.unwrap();
    let both = vec![("a.ts", "2222222"), ("b.ts", "4444444")];
    assert_eq!(pairs(&first.revisions), both);
    assert_eq!(pairs(&second.revisions), both);
    assert_eq!(mock.calls().len(), 1);
}

#[tokio::test]
async fn reads_the_patch_afresh_once_the_one_it_held_has_aged_out() {
    let mock = MockRequester::new();
    mock.always(response("diff --git a/a.ts b/a.ts\nindex 1111111..2222222 100644\n@@ -1 +1 @@\n"));
    let (api, clock) = api(&mock);
    let paths = ["a.ts".to_owned()];
    api.get_file_revisions("acme/web", 75, &paths).await.unwrap();
    api.get_file_revisions("acme/web", 75, &paths).await.unwrap();
    assert_eq!(mock.calls().len(), 1);
    // Well inside the window the caller holds versions for: a refresh drops what it holds so that
    // the read after it reaches Bitbucket, and this must not answer that read instead.
    clock.advance(30_000);
    api.get_file_revisions("acme/web", 75, &paths).await.unwrap();
    assert_eq!(mock.calls().len(), 2);
}

#[tokio::test]
async fn does_not_hold_a_failed_patch_read() {
    let mock = MockRequester::new();
    mock.once(Err(response_error(500, None)))
        .once(response("diff --git a/a.ts b/a.ts\nindex 1111111..2222222 100644\n@@ -1 +1 @@\n"));
    let (api, _) = api(&mock);
    let paths = ["a.ts".to_owned()];
    assert!(api.get_file_revisions("acme/web", 76, &paths).await.is_err());
    // The tick after a failure reaches Bitbucket rather than being handed the same error.
    assert!(api.get_file_revisions("acme/web", 76, &paths).await.is_ok());
    assert_eq!(mock.calls().len(), 2);
}

#[tokio::test]
async fn asks_bitbucket_nothing_when_no_file_has_been_ticked_off() {
    let mock = MockRequester::new();
    let (api, _) = api(&mock);
    let revisions = api.get_file_revisions("acme/web", 73, &[]).await.unwrap();
    assert!(revisions.revisions.is_empty());
    assert!(mock.calls().is_empty());
}

#[tokio::test]
async fn aggregates_every_diffstat_page() {
    let next = "https://api.bitbucket.example.test/2.0/diffstat?page=2";
    let mock = MockRequester::new();
    mock.once(response(value_page(
        json!([{"lines_added": 9, "lines_removed": 2}, {"lines_added": 3, "lines_removed": 1}]),
        Some(next),
    )))
    .once(response(value_page(json!([{"lines_added": 4, "lines_removed": 7}]), None)));
    let (api, _) = api(&mock);
    let stat = api.get_diff_stat("acme/web", 7).await.unwrap();
    assert_eq!((stat.additions, stat.deletions, stat.changed_files), (16, 10, 3));
    assert_eq!(mock.call_at(1).url, next);
}

#[tokio::test]
async fn returns_the_complete_commit_timeline_oldest_first_across_pages() {
    let next = "https://api.bitbucket.example.test/2.0/commits?page=2";
    let mock = MockRequester::new();
    mock.once(response(value_page(
        json!([{"hash": "ddd", "message": "fourth", "date": "2026-07-04T00:00:00Z"}, {"hash": "ccc", "message": "third", "date": "2026-07-03T00:00:00Z"}]),
        Some(next),
    )))
    .once(response(value_page(
        json!([{"hash": "bbb", "message": "second", "date": "2026-07-02T00:00:00Z"}, {"hash": "aaa", "message": "first", "date": "2026-07-01T00:00:00Z"}]),
        None,
    )));
    let (api, _) = api(&mock);
    let commits = api.list_commits("acme/web", 7).await.unwrap();
    assert_eq!(
        commits.iter().map(|commit| commit.oid.as_str()).collect::<Vec<_>>(),
        vec!["aaa", "bbb", "ccc", "ddd"]
    );
    assert_eq!(mock.call_at(1).url, next);
}

#[tokio::test]
async fn returns_build_statuses_from_every_page() {
    let next = "https://api.bitbucket.example.test/2.0/statuses?page=2";
    let mock = MockRequester::new();
    mock.once(response(value_page(json!([{"name": "Build", "state": "SUCCESSFUL"}]), Some(next))))
        .once(response(value_page(json!([{"name": "Lint", "state": "FAILED"}]), None)));
    let (api, _) = api(&mock);
    let checks = api.list_checks("acme/web", 7).await.unwrap();
    assert_eq!(
        checks.iter().map(|check| (check.name.as_str(), check.status)).collect::<Vec<_>>(),
        vec![("Build", PullRequestCheckStatus::Success), ("Lint", PullRequestCheckStatus::Failure)]
    );
    assert_eq!(mock.call_at(1).url, next);
}

#[tokio::test]
async fn reads_an_empty_conflict_list_as_mergeable() {
    let mock = MockRequester::new();
    mock.once(response(page(0, 1, None)));
    let (api, _) = api(&mock);
    assert_eq!(api.get_mergeability("acme/web", 7).await.unwrap(), PullRequestMergeability::Mergeable);
    assert_eq!(mock.call_at(0).url, "/repositories/acme/web/pullrequests/7/conflicts");
}

#[tokio::test]
async fn merges_with_bitbuckets_own_name_for_the_strategy() {
    let mock = MockRequester::new();
    mock.always(response("{}"));
    let (api, _) = api(&mock);
    api.run_action("acme/web", 7, PullRequestAction::Merge, Some(PullRequestMergeMethod::Rebase))
        .await
        .unwrap();
    let call = mock.call_at(0);
    assert_eq!(call.method, "POST");
    assert_eq!(call.url, "/repositories/acme/web/pullrequests/7/merge");
    assert_eq!(call.body.as_deref(), Some(r#"{"merge_strategy":"rebase_fast_forward"}"#));
}

#[tokio::test]
async fn closes_a_pull_request_by_declining_it() {
    let mock = MockRequester::new();
    mock.always(response("{}"));
    let (api, _) = api(&mock);
    api.run_action("acme/web", 7, PullRequestAction::Close, None).await.unwrap();
    let call = mock.call_at(0);
    assert_eq!(call.method, "POST");
    assert_eq!(call.url, "/repositories/acme/web/pullrequests/7/decline");
}

#[tokio::test]
async fn posts_a_comment_as_a_json_document_so_the_body_stays_text() {
    let mock = MockRequester::new();
    mock.always(response("{}"));
    let (api, _) = api(&mock);
    api.comment("acme/web", 7, "true").await.unwrap();
    let call = mock.call_at(0);
    assert_eq!(call.method, "POST");
    assert_eq!(call.url, "/repositories/acme/web/pullrequests/7/comments");
    assert_eq!(call.body.as_deref(), Some(r#"{"content":{"raw":"true"}}"#));
}

#[tokio::test]
async fn rewrites_a_title_alone_without_touching_anything_else() {
    let mock = MockRequester::new();
    mock.always(response("{}"));
    let (api, _) = api(&mock);
    api.update_change_request("acme/web", 7, Some("A new title"), None).await.unwrap();
    let call = mock.call_at(0);
    assert_eq!(call.method, "PUT");
    assert_eq!(call.url, "/repositories/acme/web/pullrequests/7");
    // Bitbucket's PUT is a partial update, so a field left out of the body is left as it was.
    assert_eq!(mock.body_of_call(0), json!({"title": "A new title"}));
}

#[tokio::test]
async fn leaves_out_the_half_of_the_pull_request_it_was_not_asked_about() {
    let mock = MockRequester::new();
    mock.always(response("{}"));
    let (api, _) = api(&mock);
    api.update_change_request("acme/web", 7, None, Some("New body.")).await.unwrap();
    assert_eq!(mock.body_of_call(0), json!({"description": "New body."}));
}

#[tokio::test]
async fn writes_both_fields_when_both_were_rewritten() {
    let mock = MockRequester::new();
    mock.always(response("{}"));
    let (api, _) = api(&mock);
    api.update_change_request("acme/web", 7, Some("A new title"), Some("New body.")).await.unwrap();
    assert_eq!(mock.body_of_call(0), json!({"title": "A new title", "description": "New body."}));
}

#[tokio::test]
async fn rewrites_a_comment_where_it_stands_whichever_kind_it_is() {
    let mock = MockRequester::new();
    mock.always(response("{}"));
    let (api, _) = api(&mock);
    api.update_comment("acme/web", 7, "10", "Edited.").await.unwrap();
    let call = mock.call_at(0);
    assert_eq!(call.method, "PUT");
    assert_eq!(call.url, "/repositories/acme/web/pullrequests/7/comments/10");
    assert_eq!(call.body.as_deref(), Some(r#"{"content":{"raw":"Edited."}}"#));
}

#[tokio::test]
async fn fails_the_read_when_bitbucket_answers_with_something_unreadable() {
    let mock = MockRequester::new();
    mock.once(response(json!({"error": "nope"}).to_string()));
    let (api, _) = api(&mock);
    let error = api.get_pull_request("acme/web", 7).await.unwrap_err();
    assert_eq!(error.tag(), "BitbucketPullRequestReadError");
}

#[tokio::test]
async fn states_a_failure_once_without_stacking_one_message_inside_another() {
    let mock = MockRequester::new();
    mock.once(Err(response_error(500, None)));
    let (api, _) = api(&mock);
    let error = api.get_viewer().await.unwrap_err();
    // The fact only; the provider adds the operation around it.
    assert_eq!(error.detail(), "Bitbucket returned HTTP 500.");
}

#[tokio::test]
async fn fails_when_the_credentials_belong_to_no_named_account() {
    let mock = MockRequester::new();
    mock.once(response("{}"));
    let (api, _) = api(&mock);
    let error = api.get_viewer().await.unwrap_err();
    assert!(matches!(error, BitbucketPullRequestApiError::ViewerUnavailable));
    assert_eq!(error.tag(), "BitbucketViewerUnavailableError");
}

#[tokio::test]
async fn follows_bitbuckets_cursor_and_reassembles_a_thread_that_spans_two_pages() {
    let next = "https://api.bitbucket.example.test/2.0/comments?page=2";
    let mock = MockRequester::new();
    mock.once(response(
        json!({"next": next, "values": [{"id": 10, "content": {"raw": "rename this"}, "user": {"nickname": "avery"}, "created_on": "2026-06-16T05:04:32+00:00", "inline": {"path": "src/a.ts", "to": 12}}]})
            .to_string(),
    ))
    // The reply arrives a page after the remark it answers, which is why the threads are only
    // assembled once every page is in hand.
    .once(response(
        json!({"values": [{"id": 11, "content": {"raw": "done"}, "user": {"nickname": "julius"}, "created_on": "2026-06-16T06:04:32+00:00", "parent": {"id": 10}}]}).to_string(),
    ));
    let (api, _) = api(&mock);
    let conversation = api.list_comments("acme/web", 7).await.unwrap();
    assert_eq!(mock.call_at(1).url, next);
    assert_eq!(
        conversation.comments.iter().map(|comment| comment.id.as_str()).collect::<Vec<_>>(),
        vec!["10", "11"]
    );
    assert_eq!(
        conversation.threads[0].comments.iter().map(|comment| comment.id.as_str()).collect::<Vec<_>>(),
        vec!["10", "11"]
    );
    assert!(!conversation.truncated);
}

#[tokio::test]
async fn stops_the_comment_walk_at_its_bound_and_says_the_conversation_was_cut_short() {
    // Bitbucket that always names a next page: the walk has to end itself.
    let mock = MockRequester::new();
    mock.always(response(
        json!({"next": "https://api.bitbucket.example.test/2.0/comments?page=2", "values": [{"id": 10, "content": {"raw": "again"}, "created_on": "2026-06-16T05:04:32+00:00"}]}).to_string(),
    ));
    let (api, _) = api(&mock);
    let conversation = api.list_comments("acme/web", 7).await.unwrap();
    assert_eq!(mock.calls().len(), 10);
    assert!(conversation.truncated);
}

#[tokio::test]
async fn reassembles_a_thread_from_the_flat_comment_list_replies_included() {
    let mock = MockRequester::new();
    mock.once(response(
        json!({"values": [
            {"id": 10, "content": {"raw": "rename this"}, "user": {"nickname": "avery"}, "created_on": "2026-06-16T05:04:32+00:00",
             "inline": {"path": "src/a.ts", "to": 12, "from": null}, "resolution": {"type": "pullrequest_comment_resolution"}},
            {"id": 11, "content": {"raw": "done"}, "user": {"nickname": "julius"}, "created_on": "2026-06-16T06:04:32+00:00", "parent": {"id": 10}},
            // A reply to a reply still belongs to the thread its root opened.
            {"id": 12, "content": {"raw": "thanks"}, "user": {"nickname": "avery"}, "created_on": "2026-06-16T07:04:32+00:00", "parent": {"id": 11}},
            {"id": 13, "content": {"raw": "ship it"}, "user": {"nickname": "avery"}, "created_on": "2026-06-16T08:04:32+00:00"},
        ]})
        .to_string(),
    ));
    let (api, _) = api(&mock);
    let conversation = api.list_comments("acme/web", 7).await.unwrap();
    assert_eq!(conversation.threads.len(), 1);
    let thread = &conversation.threads[0];
    assert_eq!(
        (thread.id.as_str(), thread.path.as_str(), thread.line, thread.side, thread.is_resolved),
        ("10", "src/a.ts", Some(12), zc_contracts::PullRequestDiffSide::Right, true)
    );
    assert_eq!(
        thread.comments.iter().map(|comment| comment.id.as_str()).collect::<Vec<_>>(),
        vec!["10", "11", "12"]
    );
}

#[tokio::test]
async fn writes_a_reviews_line_comments_its_summary_then_its_verdict() {
    let mock = MockRequester::new();
    mock.always(response("{}"));
    let (api, _) = api(&mock);
    let comments = vec![PullRequestReviewCommentDraft {
        path: "src/a.ts".into(),
        old_path: None,
        position: PullRequestReviewPosition::Deleted(PullRequestReviewPositionDeleted {
            kind: LitDeleted,
            old_line: 12,
        }),
        body: "why remove?".into(),
    }];
    api.submit_review("acme/web", 7, PullRequestReviewVerdict::RequestChanges, "Two things.", &comments)
        .await
        .unwrap();
    assert!(mock.call_at(0).url.contains("/pullrequests/7/comments"));
    assert_eq!(
        mock.body_of_call(0),
        json!({"content": {"raw": "why remove?"}, "inline": {"path": "src/a.ts", "from": 12}})
    );
    assert!(mock.call_at(1).url.contains("/pullrequests/7/comments"));
    // The verdict goes last, so a review that failed part-way is never a rejection either.
    assert!(mock.call_at(2).url.contains("/pullrequests/7/request-changes"));
}

#[tokio::test]
async fn resolves_by_creating_the_sub_resource_and_unresolves_by_deleting_it() {
    let mock = MockRequester::new();
    mock.always(response("{}"));
    let (api, _) = api(&mock);
    api.set_comment_resolution("acme/web", 7, "10", true).await.unwrap();
    api.set_comment_resolution("acme/web", 7, "10", false).await.unwrap();
    assert_eq!(mock.call_at(0).method, "POST");
    assert_eq!(mock.call_at(1).method, "DELETE");
    assert!(mock.call_at(0).url.contains("/comments/10/resolve"));
}

#[tokio::test]
async fn replies_by_naming_the_comment_it_answers() {
    let mock = MockRequester::new();
    mock.always(response("{}"));
    let (api, _) = api(&mock);
    api.reply_to_comment("acme/web", 7, "10", "Fixed.").await.unwrap();
    assert_eq!(mock.body_of_call(0), json!({"content": {"raw": "Fixed."}, "parent": {"id": 10}}));
}

#[tokio::test]
async fn asks_for_the_credentials_permission_on_this_repository_and_nobody_elses() {
    let mock = MockRequester::new();
    mock.always(response(
        json!({"values": [{"type": "repository_permission", "permission": "read"}]}).to_string(),
    ));
    let (api, _) = api(&mock);
    assert!(!api.get_repository_permission("acme/web").await.unwrap());
    assert!(mock.call_at(0).url.contains("/user/permissions/repositories"));
    assert_eq!(mock.filter_of_call(0).as_deref(), Some(r#"repository.full_name="acme/web""#));
}

#[tokio::test]
async fn escapes_a_repository_name_before_it_goes_inside_a_filter_literal() {
    let mock = MockRequester::new();
    mock.always(response(json!({"values": []}).to_string()));
    let (api, _) = api(&mock);
    api.get_repository_permission(r#"acme/we"b"#).await.unwrap();
    // A quote would otherwise end the literal and leave the rest standing as filter syntax.
    assert_eq!(mock.filter_of_call(0).as_deref(), Some(r#"repository.full_name="acme/we\"b""#));
}

#[tokio::test]
async fn reads_a_removed_permissions_endpoint_as_granted_rather_than_failing_the_merge_on_it() {
    // Bitbucket retired /user/permissions/repositories under CHANGE-2770: every account now gets
    // HTTP 410 here, whatever it may do.
    let mock = MockRequester::new();
    mock.always(Err(response_error(410, None)));
    let (api, _) = api(&mock);
    assert!(api.get_repository_permission("acme/web").await.unwrap());
}

#[tokio::test]
async fn still_fails_the_permission_read_on_a_failure_that_is_not_the_removed_endpoint() {
    let mock = MockRequester::new();
    mock.always(Err(response_error(401, None)));
    let (api, _) = api(&mock);
    let error = api.get_repository_permission("acme/web").await.unwrap_err();
    assert_eq!(error.tag(), "BitbucketResponseError");
}

#[tokio::test]
async fn reads_the_workspaces_people_and_marks_whoever_is_already_a_reviewer() {
    let mock = MockRequester::new();
    mock.once(response(pull_request_json(json!({"reviewers": [quinn()]})))).once(response(
        json!({"values": [{"user": avery()}, {"user": quinn()}, {"user": robin()}]}).to_string(),
    ));
    let (api, _) = api(&mock);
    let list: PullRequestReviewerCandidateList = api.list_reviewer_candidates("acme/web", 7).await.unwrap();
    // The people live on the workspace: nothing on a repository lists who may review it.
    assert_eq!(mock.call_at(1).url, "/workspaces/acme/members?pagelen=50");
    assert_eq!(
        list.candidates
            .iter()
            .map(|candidate| (candidate.id.as_str(), candidate.is_requested))
            .collect::<Vec<_>>(),
        vec![("{quinn}", true), ("{robin}", false)]
    );
    assert!(!list.truncated);
}

#[tokio::test]
async fn writes_the_reviewer_set_back_with_the_one_being_asked_added_to_it() {
    let mock = MockRequester::new();
    mock.once(response(pull_request_json(json!({"reviewers": [quinn()]})))).once(response("{}"));
    let (api, _) = api(&mock);
    api.set_reviewer_request("acme/web", 7, &["{robin}".into()], true).await.unwrap();
    // Bitbucket writes `reviewers` whole, so the one already on the pull request travels with
    // the new one or the request would take them off it.
    let call = mock.call_at(1);
    assert_eq!(call.method, "PUT");
    assert_eq!(call.url, "/repositories/acme/web/pullrequests/7");
    assert_eq!(mock.body_of_call(1), json!({"reviewers": [{"uuid": "{quinn}"}, {"uuid": "{robin}"}]}));
}

#[tokio::test]
async fn takes_a_reviewer_out_of_the_set_rather_than_clearing_it() {
    let mock = MockRequester::new();
    mock.once(response(pull_request_json(json!({"reviewers": [quinn(), robin()]}))))
        .once(response("{}"));
    let (api, _) = api(&mock);
    api.set_reviewer_request("acme/web", 7, &["{robin}".into()], false).await.unwrap();
    assert_eq!(mock.body_of_call(1), json!({"reviewers": [{"uuid": "{quinn}"}]}));
}
