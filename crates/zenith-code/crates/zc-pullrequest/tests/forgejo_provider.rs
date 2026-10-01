//! The Forgejo provider against a scripted host behind `tea` (no TS test file exists for
//! `ForgejoPullRequestProvider.ts`; these follow its behaviour): REST paths and bodies, the
//! capability set, permissions, paging, and error mapping.

#![allow(clippy::result_large_err, clippy::too_many_arguments)]

mod support_forgejo;

use serde_json::{json, Value};
use support_forgejo::*;
use zc_contracts::{
    PullRequestAction, PullRequestBaseComparison, PullRequestCommentKind, PullRequestDiffFileContentsInputChangeType, PullRequestDiffSide,
    PullRequestInvolvement, PullRequestListState, PullRequestMergeMethod, PullRequestReactionContent, PullRequestReviewCommentDraft, PullRequestReviewVerdict,
    PullRequestReviewerKind, PullRequestUpdateMethod,
};
use zc_pullrequest::forgejo::provider::forgejo_capabilities;
use zc_pullrequest::provider::*;
use zc_pullrequest::{ProviderFailureReason, PullRequestProviderApi};

fn home() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn list_input(state: PullRequestListState, limit: i64, delivered: Option<i64>) -> ListChangeRequestsInput {
    ListChangeRequestsInput {
        cwd: "/w".into(),
        repository: "acme/web".into(),
        host: "forge.example.test".into(),
        state,
        involvement: PullRequestInvolvement::All,
        viewer: "maria".into(),
        limit,
        query: Some("ignored".into()),
        cursor: delivered.map(|delivered| ProviderListCursor {
            updated_before: "2026-07-02T00:00:00Z".into(),
            delivered,
        }),
        filters: None,
    }
}

fn pulls(numbers: std::ops::Range<i64>) -> Value {
    Value::Array(numbers.map(|number| pull(number, json!({}))).collect())
}

#[test]
fn declares_the_capabilities_and_optional_methods_of_the_ts_provider() {
    assert_eq!(
        serde_json::to_value(forgejo_capabilities()).unwrap(),
        json!({
            "diff": true,
            "comment": true,
            "actions": ["merge", "close", "reopen", "update-branch"],
            "mergeMethods": ["merge", "squash", "rebase"],
            "updateMethods": ["merge", "rebase"],
            "search": false,
            "reactions": true,
            "viewedFiles": "environment",
            "review": {"inlineComment": true, "reply": false, "resolve": false, "verdicts": ["comment", "approve", "request-changes"]},
            "reviewers": {"request": true, "listCandidates": true},
            "edit": {"changeRequest": true, "comment": true},
            "labels": true,
        })
    );
    let home = home();
    assert_eq!(
        Host::new().provider(home.path()).optional_methods(),
        OptionalMethods {
            get_change_request_summary: true,
            get_diff_file_contents: true,
            get_file_revisions: true,
            update_change_request: true,
            update_comment: true,
            list_label_candidates: true,
            set_labels: true,
            ..OptionalMethods::default()
        }
    );
}

#[tokio::test]
async fn lists_recently_updated_pull_requests_reading_merged_as_closed() {
    let home = home();
    let host = Host::new();
    host.on("GET repos/acme/web/pulls?state=closed&sort=recentupdate&limit=50&page=1", ok(pulls(1..4)));
    let page = host
        .provider(home.path())
        .list_change_requests(list_input(PullRequestListState::Merged, 2, None))
        .await
        .unwrap();
    assert_eq!(page.items.iter().map(|item| item.number).collect::<Vec<_>>(), vec![1, 2]);
    // A third row was there, so the page can be carried on from.
    assert!(page.truncated);
    assert!(page.continues);
    assert_eq!(page.cursor_advance, Some(2));
    // The search text is not sent: Forgejo's API has none.
    assert_eq!(host.requests(), vec!["GET repos/acme/web/pulls?state=closed&sort=recentupdate&limit=50&page=1"]);
}

#[tokio::test]
async fn carries_on_from_a_row_offset_in_the_hosts_own_page_size() {
    let home = home();
    let host = Host::new();
    // A server capping pages at three rows.
    let first = Answer {
        link: Some(r#"<https://forge.example.test/api/v1/x?page=2>; rel="next""#.into()),
        ..ok(pulls(1..4))
    };
    host.on("GET repos/acme/web/pulls?state=open&sort=recentupdate&limit=50&page=1", first)
        .on(
            "GET repos/acme/web/pulls?state=open&sort=recentupdate&limit=50&page=2",
            ok(Value::Array(vec![pull(4, json!({})), Value::Null, pull(6, json!({}))])),
        )
        .on("GET repos/acme/web/pulls?state=open&sort=recentupdate&limit=50&page=3", ok(json!([])));
    let page = host
        .provider(home.path())
        .list_change_requests(list_input(PullRequestListState::Open, 5, Some(4)))
        .await
        .unwrap();
    // Offset four is the second row of page two; the null row is counted but not shown.
    assert_eq!(page.items.iter().map(|item| item.number).collect::<Vec<_>>(), vec![6]);
    assert_eq!(page.cursor_advance, Some(2));
    assert!(!page.truncated);
    assert_eq!(
        host.requests(),
        vec![
            "GET repos/acme/web/pulls?state=open&sort=recentupdate&limit=50&page=1",
            "GET repos/acme/web/pulls?state=open&sort=recentupdate&limit=50&page=2",
            "GET repos/acme/web/pulls?state=open&sort=recentupdate&limit=50&page=3",
        ]
    );
}

fn repo(extra: Value) -> Value {
    let mut value = json!({"full_name": "acme/web", "permissions": {"push": false, "admin": false}});
    for (key, field) in extra.as_object().unwrap() {
        value[key] = field.clone();
    }
    value
}

#[tokio::test]
async fn reads_the_detail_with_its_statuses_permissions_and_merge_settings() {
    let home = home();
    let host = Host::new();
    host.on(
        "GET repos/acme/web/pulls/7",
        ok(pull(
            7,
            json!({"merge_base": "older", "changed_files": 4, "head": {"ref": "f", "sha": "a/b c", "repo": null}}),
        )),
    )
    .on(
        "GET repos/acme/web",
        ok(repo(
            json!({"permissions": {"push": true, "admin": false}, "allow_squash_merge": false, "allow_rebase_update": true}),
        )),
    )
    .on("GET user", ok(json!({"login": "kit"})))
    .on(
        "GET repos/acme/web/statuses/a%2Fb%20c?sort=recentupdate&limit=50&page=1",
        ok(json!([
            {"context": "ci", "status": "success", "description": "", "target_url": "https://ci.example.test/1", "updated_at": "2026-07-02T00:00:00Z"},
            {"context": "ci", "status": "failure", "description": "old", "target_url": null, "updated_at": "2026-07-01T00:00:00Z"},
        ])),
    )
    .on("GET repos/acme/web/statuses/a%2Fb%20c?sort=recentupdate&limit=50&page=2", ok(Value::Null));
    let detail = host.provider(home.path()).get_change_request(reference(7)).await.unwrap();
    assert_eq!(detail.body, "Ships it.");
    assert_eq!(detail.changed_files, 4);
    assert_eq!(detail.base_comparison, Some(PullRequestBaseComparison::Behind));
    assert_eq!(detail.change_request.head_repository_name_with_owner, Some(None));
    assert_eq!(detail.checks.len(), 1);
    assert_eq!(detail.reviewers.iter().map(|actor| actor.login.as_str()).collect::<Vec<_>>(), vec!["kit"]);
    assert!(detail.merge_capabilities.merge && !detail.merge_capabilities.squash && detail.merge_capabilities.rebase);
    // A writer who did not open it: every action, every verdict, both update methods.
    let permissions = serde_json::to_value(&detail.viewer_permissions).unwrap();
    assert_eq!(
        permissions,
        json!({
            "actions": ["merge", "close", "reopen", "update-branch"],
            "comment": true,
            "resolve": false,
            "verdicts": ["comment", "approve", "request-changes"],
            "requestReviewers": true,
            "updateMethods": ["merge", "rebase"],
            "labels": true,
        })
    );
}

#[tokio::test]
async fn gives_an_author_without_write_access_their_own_controls_only() {
    let home = home();
    let host = Host::new();
    host.on("GET repos/acme/web/pulls/7", ok(pull(7, json!({"is_locked": true}))))
        .on("GET repos/acme/web", ok(repo(json!({}))))
        .on("GET user", ok(json!({"login": "maria"})));
    let permissions = host
        .provider(home.path())
        .get_viewer_permissions(ViewerPermissionsInput {
            change_request: reference(7),
            include_update_branch: None,
        })
        .await
        .unwrap();
    assert_eq!(permissions.actions, vec![PullRequestAction::Close, PullRequestAction::Reopen]);
    // Locked, and the author cannot write: no comment, and only the comment verdict.
    assert!(!permissions.comment);
    assert_eq!(permissions.verdicts, vec![PullRequestReviewVerdict::Comment]);
    assert!(permissions.request_reviewers);
    assert_eq!(permissions.update_methods, Some(Vec::new()));
    assert_eq!(permissions.labels, Some(false));
}

#[tokio::test]
async fn offers_nothing_on_an_archived_repository() {
    let home = home();
    let host = Host::new();
    host.on("GET repos/acme/web/pulls/7", ok(pull(7, json!({}))))
        .on(
            "GET repos/acme/web",
            ok(repo(json!({"archived": true, "permissions": {"push": true, "admin": true}}))),
        )
        .on("GET user", ok(json!({"login": "kit"})));
    let permissions = host
        .provider(home.path())
        .get_viewer_permissions(ViewerPermissionsInput {
            change_request: reference(7),
            include_update_branch: None,
        })
        .await
        .unwrap();
    assert!(permissions.actions.is_empty() && permissions.verdicts.is_empty() && !permissions.comment && !permissions.request_reviewers);
}

#[tokio::test]
async fn sends_merge_close_and_update_actions_to_their_endpoints() {
    let home = home();
    let host = Host::new();
    host.on("POST repos/acme/web/pulls/7/merge", ok(json!({})))
        .on("PATCH repos/acme/web/pulls/7", ok(json!({})))
        .on("POST repos/acme/web/pulls/7/update?style=rebase", ok(json!({})));
    let provider = host.provider(home.path());
    let action = |action: PullRequestAction, merge_method: Option<PullRequestMergeMethod>, update_method: Option<PullRequestUpdateMethod>| RunActionInput {
        change_request: reference(7),
        action,
        stack_number: None,
        expected_stack_heads: None,
        merge_method,
        update_method,
    };
    provider
        .run_action(action(PullRequestAction::Merge, Some(PullRequestMergeMethod::Squash), None))
        .await
        .unwrap();
    provider.run_action(action(PullRequestAction::Close, None, None)).await.unwrap();
    provider
        .run_action(action(PullRequestAction::UpdateBranch, None, Some(PullRequestUpdateMethod::Rebase)))
        .await
        .unwrap();
    assert_eq!(host.body("POST repos/acme/web/pulls/7/merge"), Some(json!({"Do": "squash"})));
    assert_eq!(host.body("PATCH repos/acme/web/pulls/7"), Some(json!({"state": "closed"})));
    assert_eq!(host.body("POST repos/acme/web/pulls/7/update?style=rebase"), None);
    let error = provider.run_action(action(PullRequestAction::Ready, None, None)).await.unwrap_err();
    assert_eq!(error.operation, "ready");
    assert_eq!(error.detail, "Forgejo does not expose ready through its API.");
    assert_eq!(host.requests().len(), 3);
}

#[tokio::test]
async fn submits_a_review_with_its_inline_comments_against_the_head() {
    let home = home();
    let host = Host::new();
    host.on("GET repos/acme/web/pulls/7", ok(pull(7, json!({}))))
        .on("POST repos/acme/web/pulls/7/reviews", ok(json!({})));
    let comments: Vec<PullRequestReviewCommentDraft> = serde_json::from_value(json!([
        {"path": "src/a.ts", "position": {"kind": "added", "newLine": 3}, "body": "new"},
        {"path": "src/b.ts", "oldPath": "src/old-b.ts", "position": {"kind": "deleted", "oldLine": 4}, "body": "gone"},
        {"path": "src/c.ts", "oldPath": "src/old-c.ts", "position": {"kind": "context", "oldLine": 5, "newLine": 6, "side": "left"}, "body": "left"},
        {"path": "src/d.ts", "position": {"kind": "context", "oldLine": 7, "newLine": 8, "side": "right"}, "body": "right"},
    ]))
    .unwrap();
    host.provider(home.path())
        .submit_review(SubmitReviewInput {
            change_request: reference(7),
            verdict: PullRequestReviewVerdict::RequestChanges,
            body: "Some changes.".into(),
            comments,
        })
        .await
        .unwrap();
    assert_eq!(
        host.body("POST repos/acme/web/pulls/7/reviews"),
        Some(json!({
            "event": "REQUEST_CHANGES",
            "body": "Some changes.",
            "commit_id": "head7",
            "comments": [
                {"path": "src/a.ts", "body": "new", "old_position": 0, "new_position": 3},
                {"path": "src/old-b.ts", "body": "gone", "old_position": 4, "new_position": 0},
                {"path": "src/old-c.ts", "body": "left", "old_position": 5, "new_position": 0},
                {"path": "src/d.ts", "body": "right", "old_position": 0, "new_position": 8},
            ],
        }))
    );
}

#[tokio::test]
async fn reacts_to_a_review_through_the_issue_comment_it_is_kept_as() {
    let home = home();
    let host = Host::new();
    host.on(
        "GET repos/acme/web/pulls/7/reviews/3",
        ok(json!({"id": 3, "body": "", "user": null, "state": "APPROVED", "submitted_at": "2026-07-01T00:00:00Z", "html_url": "https://forge.example.test/acme/web/pulls/7#issuecomment-42", "comments_count": 0})),
    )
    .on("DELETE repos/acme/web/issues/comments/42/reactions", ok(json!({})))
    .on("POST repos/acme/web/issues/7/reactions", ok(json!({})));
    let provider = host.provider(home.path());
    let react = |subject: Option<&str>, reacted: bool| SetReactionInput {
        change_request: reference(7),
        subject_id: subject.map(Into::into),
        content: PullRequestReactionContent::ThumbsUp,
        reacted,
    };
    provider.set_reaction(react(Some("review:3"), false)).await.unwrap();
    assert_eq!(host.body("DELETE repos/acme/web/issues/comments/42/reactions"), Some(json!({"content": "+1"})));
    provider.set_reaction(react(None, true)).await.unwrap();
    assert_eq!(host.body("POST repos/acme/web/issues/7/reactions"), Some(json!({"content": "+1"})));
    assert_eq!(
        provider.set_reaction(react(Some("review:03"), true)).await.unwrap_err().detail,
        "Invalid Forgejo review ID."
    );
    assert_eq!(
        provider.set_reaction(react(Some("4a"), true)).await.unwrap_err().detail,
        "Invalid Forgejo comment ID."
    );
}

#[tokio::test]
async fn writes_labels_by_id_and_refuses_one_that_does_not_exist() {
    let home = home();
    let host = Host::new();
    host.on(
        "GET repos/acme/web/labels?limit=50&page=1",
        ok(json!([{"id": 1, "name": "backend"}, {"id": 2, "name": "ui", "color": null}])),
    )
    .on("GET repos/acme/web/labels?limit=50&page=2", ok(json!([])))
    .on("DELETE repos/acme/web/issues/7/labels/1", ok(json!({})))
    .on("DELETE repos/acme/web/issues/7/labels/2", ok(json!({})));
    let provider = host.provider(home.path());
    let labels = |names: &[&str], applied: bool| SetLabelsInput {
        change_request: reference(7),
        labels: names.iter().map(|name| (*name).to_owned()).collect(),
        applied,
    };
    provider.set_labels(labels(&["ui", "backend"], false)).await.unwrap();
    assert!(host.requests().ends_with(&[
        "DELETE repos/acme/web/issues/7/labels/1".to_owned(),
        "DELETE repos/acme/web/issues/7/labels/2".to_owned()
    ]));
    let error = provider.set_labels(labels(&["missing"], true)).await.unwrap_err();
    assert_eq!(
        (error.operation.as_str(), error.detail.as_str()),
        ("setLabels", "One or more requested labels could not be found.")
    );
}

#[tokio::test]
async fn lists_assignees_as_reviewer_candidates_less_the_author() {
    let home = home();
    let host = Host::new();
    host.on("GET repos/acme/web/pulls/7", ok(pull(7, json!({}))))
        .on(
            "GET repos/acme/web/assignees?limit=50&page=1",
            ok(json!([{"login": "maria"}, {"login": "kit", "avatar_url": ""}, {"login": "", "full_name": "x"}, {"login": "lee"}])),
        )
        .on("GET repos/acme/web/assignees?limit=50&page=2", ok(json!([])));
    let list = host.provider(home.path()).list_reviewer_candidates(reference(7)).await.unwrap();
    assert_eq!(
        list.candidates
            .iter()
            .map(|candidate| (candidate.id.as_str(), candidate.is_requested, candidate.kind))
            .collect::<Vec<_>>(),
        vec![("kit", true, PullRequestReviewerKind::User), ("lee", false, PullRequestReviewerKind::User)]
    );
    assert!(!list.truncated);
}

#[tokio::test]
async fn reads_file_revisions_off_the_patch_and_fills_the_rest_as_removed() {
    let home = home();
    let host = Host::new();
    host.on(
        "GET repos/acme/web/pulls/7.diff",
        Answer {
            status: 200,
            body: "diff --git a/src/a.ts b/src/a.ts\nindex 1111111..2222222 100644\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1 +1 @@\n-a\n+b\n".into(),
            link: None,
            truncated: false,
        },
    );
    let revisions = host
        .provider(home.path())
        .get_file_revisions(FileRevisionsInput {
            change_request: reference(7),
            paths: vec!["src/a.ts".into(), "src/gone.ts".into()],
        })
        .await
        .unwrap();
    assert_eq!(
        revisions.revisions,
        vec![("src/a.ts".to_owned(), "2222222".to_owned()), ("src/gone.ts".to_owned(), String::new())]
    );
    assert_eq!(revisions.complete, Some(true));
}

#[tokio::test]
async fn reads_both_sides_of_a_file_through_the_contents_api() {
    let home = home();
    let host = Host::new();
    host.on("GET repos/acme/web/pulls/7", ok(pull(7, json!({"merge_base": ""}))))
        .on(
            "GET repos/acme/web/contents/src/a%20b.ts?ref=base",
            ok(json!({"content": "b2xkCg==", "encoding": "base64"})),
        )
        .on(
            "GET repos/maria/web/contents/src/a%20b.ts?ref=head7",
            ok(json!({"content": "bmV3\nCg==", "encoding": "base64"})),
        );
    let contents = host
        .provider(home.path())
        .get_diff_file_contents(DiffFileContentsInput {
            change_request: reference(7),
            commit: None,
            change_type: PullRequestDiffFileContentsInputChangeType::Change,
            old_path: "src/a b.ts".into(),
            new_path: "src/a b.ts".into(),
        })
        .await
        .unwrap();
    assert_eq!((contents.old_contents.as_str(), contents.new_contents.as_str()), ("old\n", "new\n"));
}

#[tokio::test]
async fn maps_tool_and_http_failures_to_provider_reasons() {
    let home = home();
    let host = Host::new();
    host.on("GET user", status(401, "{}"))
        .on("GET repos/acme/web/pulls/8", status(429, "{}"))
        .on("GET repos/acme/web/pulls/9", status(500, "{}"));
    let provider = host.provider(home.path());
    let viewer = provider
        .get_viewer(ProviderHostRef {
            cwd: "/w".into(),
            host: Some("forge.example.test".into()),
        })
        .await
        .unwrap_err();
    assert_eq!((viewer.reason, viewer.operation.as_str()), (ProviderFailureReason::Unauthenticated, "user"));
    let limited = provider.get_change_request_summary(reference(8)).await.unwrap_err();
    assert_eq!(limited.reason, ProviderFailureReason::RateLimited);
    let failed = provider.get_change_request_summary(reference(9)).await.unwrap_err();
    assert_eq!(
        (failed.reason, failed.operation.as_str(), failed.detail.as_str()),
        (
            ProviderFailureReason::Failed,
            "repos/acme/web/pulls/9",
            "Forgejo API request failed (HTTP 500)."
        )
    );
    let missing = provider.get_change_request_summary(reference(10)).await.unwrap_err();
    assert_eq!(missing.detail, "Forgejo repository or pull request was not found.");
    *host.missing_tea.lock().unwrap() = true;
    let unusable = provider.get_change_request_summary(reference(7)).await.unwrap_err();
    assert_eq!(unusable.reason, ProviderFailureReason::MissingTool);
}

#[tokio::test]
async fn refuses_an_oversized_or_invalid_answer() {
    let home = home();
    let host = Host::new();
    host.on(
        "GET repos/acme/web/pulls/7",
        Answer {
            truncated: true,
            ..ok(pull(7, json!({})))
        },
    )
    .on("GET repos/acme/web/pulls/8", ok(json!({"number": "8"})));
    let provider = host.provider(home.path());
    let oversized = provider.get_change_request_summary(reference(7)).await.unwrap_err();
    assert_eq!(
        (oversized.operation.as_str(), oversized.detail.as_str()),
        ("repos/acme/web/pulls/7", "Forgejo response exceeded the output limit.")
    );
    let invalid = provider.get_change_request_summary(reference(8)).await.unwrap_err();
    assert_eq!(invalid.detail, "Forgejo returned an invalid response.");
    assert_eq!(invalid.cause.and_then(|cause| cause.name()).as_deref(), Some("SchemaError"));
}

#[tokio::test]
async fn builds_one_timeline_from_comments_reviews_and_inline_comments() {
    let home = home();
    let host = Host::new();
    let user = |login: &str| json!({"login": login});
    host.on(
        "GET repos/acme/web/issues/7/comments",
        ok(json!([{"id": 10, "body": "Later.", "user": user("kit"), "created_at": "2026-07-03T00:00:00Z"}])),
    )
    .on(
        "GET repos/acme/web/pulls/7/reviews?limit=50&page=1",
        ok(json!([
            {"id": 3, "body": "Looks good", "user": user("lee"), "state": "APPROVED", "submitted_at": "2026-07-02T00:00:00Z", "html_url": "https://forge.example.test/acme/web/pulls/7#issuecomment-11", "comments_count": 1},
            {"id": 4, "body": "", "user": user("lee"), "state": "PENDING", "submitted_at": "2026-07-02T00:00:00Z", "comments_count": 2},
            {"id": 5, "body": "", "user": user("maria"), "state": "REQUEST_REVIEW", "submitted_at": "2026-07-01T00:00:00Z", "comments_count": 0},
        ])),
    )
    .on("GET repos/acme/web/pulls/7/reviews?limit=50&page=2", ok(json!([])))
    .on("GET repos/acme/web/pulls/7/commits?limit=50&page=1", ok(json!([{"sha": "c1", "author": null, "commit": {"message": "One", "committer": {"date": "2026-07-01T00:00:00Z"}}, "parents": []}])))
    .on("GET repos/acme/web/pulls/7/commits?limit=50&page=2", ok(json!([])))
    .on("GET repos/acme/web/issues/7/reactions?limit=50&page=1", ok(json!([{"content": "rocket", "user": user("maria")}])))
    .on("GET repos/acme/web/issues/7/reactions?limit=50&page=2", ok(Value::Null))
    .on("GET user", ok(user("maria")))
    .on(
        "GET repos/acme/web/pulls/7/reviews/3/comments",
        ok(json!([{"id": 12, "body": "Here.", "user": user("lee"), "created_at": "2026-07-01T12:00:00Z", "path": "src/a.ts", "position": 0, "original_position": 3, "commit_id": "c", "original_commit_id": "o", "resolver": user("maria")}])),
    )
    .on("GET repos/acme/web/issues/comments/10/reactions", ok(json!([{"content": "heart", "user": user("lee")}])))
    .on("GET repos/acme/web/issues/comments/11/reactions", ok(Value::Null))
    .on("GET repos/acme/web/issues/comments/12/reactions", ok(json!([{"content": "eyes", "user": user("maria")}])));
    let activity = host.provider(home.path()).get_change_request_activity(reference(7)).await.unwrap();
    assert_eq!(
        activity.comments.iter().map(|comment| (comment.id.as_str(), comment.kind)).collect::<Vec<_>>(),
        vec![
            ("12", PullRequestCommentKind::ReviewComment),
            ("review:3", PullRequestCommentKind::Review),
            ("10", PullRequestCommentKind::IssueComment),
        ]
    );
    assert_eq!(activity.comment_count, 3);
    assert!(!activity.comments_truncated);
    assert_eq!(activity.review_threads.len(), 1);
    let thread = &activity.review_threads[0];
    assert_eq!((thread.side, thread.line, thread.is_resolved), (PullRequestDiffSide::Left, Some(3), true));
    assert!(thread.comments[0].reactions.as_ref().unwrap()[0].viewer_has_reacted);
    assert_eq!(activity.commits.len(), 1);
    assert_eq!(
        activity.reactions.as_ref().map(|reactions| reactions[0].content),
        Some(PullRequestReactionContent::Rocket)
    );
    // The pending review's comments were never asked for.
    assert!(!host.requests().iter().any(|request| request.contains("reviews/4/comments")));
}

#[tokio::test]
async fn refuses_thread_replies_and_resolution_which_the_api_does_not_have() {
    let home = home();
    let provider = Host::new().provider(home.path());
    let reply = provider
        .reply_to_thread(ReplyToThreadInput {
            change_request: reference(7),
            thread_id: "1".into(),
            body: "x".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(
        (reply.operation.as_str(), reply.detail.as_str()),
        ("thread replies", "Forgejo does not expose thread replies through its API.")
    );
    let resolve = provider
        .set_thread_resolution(SetThreadResolutionInput {
            change_request: reference(7),
            thread_id: "1".into(),
            resolved: true,
        })
        .await
        .unwrap_err();
    assert_eq!(resolve.detail, "Forgejo does not expose thread resolution through its API.");
}
