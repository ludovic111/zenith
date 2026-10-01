//! `GitHubPullRequestCli.test.ts`: every `gh` invocation the pull request feature makes, asserted
//! by argv and stdin against a fake `gh` (see `support_github`).

#![allow(clippy::result_large_err)]

mod support_github;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use support_github::*;
use zc_contracts::{
    PullRequestAction, PullRequestCommentUpdateInputKind, PullRequestDiffFileContentsInputChangeType, PullRequestFileViewedState, PullRequestInvolvement,
    PullRequestListFilters, PullRequestListFiltersChecks, PullRequestListFiltersDraft, PullRequestListFiltersReview, PullRequestListState,
    PullRequestMergeMethod, PullRequestReactionContent, PullRequestReviewCommentDraft, PullRequestReviewPosition, PullRequestReviewPositionAdded,
    PullRequestReviewVerdict, PullRequestReviewerKind, PullRequestUpdateMethod,
};
use zc_pullrequest::github::cli::*;
use zc_pullrequest::github::json::BASE_COMPARISON_GRAPHQL_QUERY;
use zc_pullrequest::provider::*;
use zc_sourcecontrol::github::cli::current_pinned_github_credential;
use zc_sourcecontrol::github::{with_github_reserve, GitHubCliErrorKind};
use zc_sourcecontrol::util::date_parse_millis;

const RESET_AT: &str = "2099-08-13T14:00:00Z";

fn rate_limit(remaining: i64) -> Value {
    json!({"cost": 1, "limit": 5_000, "remaining": remaining, "resetAt": RESET_AT})
}

fn core_response(pull_request: Value) -> Value {
    let mut base = json!({
        "number": 7,
        "title": "Pull request 7",
        "url": "https://github.com/acme/web/pull/7",
        "headRefName": "feature",
        "baseRefName": "main",
        "headRefOid": "abc123",
        "state": "OPEN",
        "createdAt": "2026-07-01T00:00:00Z",
        "updatedAt": "2026-07-02T00:00:00Z",
        "viewerCanUpdate": true,
        "viewerDidAuthor": false,
        "viewerCanUpdateBranch": true,
        "baseRef": {"compare": {"behindBy": 2}},
        "reviewRequests": {"nodes": []},
        "labels": {"nodes": []},
        "commits": {"nodes": []},
    });
    for (key, value) in pull_request.as_object().cloned().unwrap_or_default() {
        base[key] = value;
    }
    json!({"data": {"repository": {
        "mergeCommitAllowed": true,
        "squashMergeAllowed": false,
        "rebaseMergeAllowed": true,
        "viewerPermission": "WRITE",
        "pullRequest": base,
    }}})
}

fn pull_requests(count: i64, first: i64, overrides: impl Fn(i64) -> Value) -> String {
    let rows: Vec<Value> = (0..count)
        .map(|index| {
            let number = first + index;
            let mut row = json!({
                "number": number,
                "title": format!("Pull request {number}"),
                "url": format!("https://github.com/acme/web/pull/{number}"),
                "headRefName": "feat/page",
                "baseRefName": "main",
                "createdAt": "2026-07-01T00:00:00Z",
                "updatedAt": "2026-07-02T00:00:00Z",
            });
            for (key, value) in overrides(number).as_object().cloned().unwrap_or_default() {
                row[key] = value;
            }
            row
        })
        .collect();
    Value::Array(rows).to_string()
}

fn plain(count: i64, first: i64) -> String {
    pull_requests(count, first, |_| json!({}))
}

fn pull_request_files(count: i64, first: i64) -> String {
    let files: Vec<Value> = (0..count)
        .map(|index| json!({"filename": format!("src/file{}.ts", first + index), "status": "modified", "patch": "@@ -1 +1 @@\n-old\n+new"}))
        .collect();
    Value::Array(files).to_string()
}

fn thread_comments(ids: &[&str], end_cursor: Option<&str>, total_count: usize) -> Value {
    json!({
        "totalCount": total_count,
        "pageInfo": {"hasNextPage": end_cursor.is_some(), "endCursor": end_cursor},
        "nodes": ids.iter().map(|id| json!({"id": id, "body": id, "createdAt": "2026-07-01T00:00:00Z"})).collect::<Vec<_>>(),
    })
}

fn thread(id: &str, comment_ids: &[&str]) -> Value {
    json!({
        "id": id,
        "path": "src/a.ts",
        "line": 1,
        "diffSide": "RIGHT",
        "isResolved": false,
        "isOutdated": false,
        "comments": thread_comments(comment_ids, None, comment_ids.len()),
    })
}

fn review_threads_page(nodes: Vec<Value>, end_cursor: Option<&str>) -> String {
    json!({"data": {"repository": {"pullRequest": {"reviewThreads": {
        "totalCount": nodes.len(),
        "pageInfo": {"hasNextPage": end_cursor.is_some(), "endCursor": end_cursor},
        "nodes": nodes,
    }}}}})
    .to_string()
}

fn thread_comments_page(ids: &[&str], end_cursor: Option<&str>, total_count: usize, pull_request_id: &str) -> String {
    json!({"data": {
        "repository": {"pullRequest": {"id": "PR_7"}},
        "node": {"pullRequest": {"id": pull_request_id}, "comments": thread_comments(ids, end_cursor, total_count)},
    }})
    .to_string()
}

fn search_item(number: i64, repository: &str, updated_at: &str) -> Value {
    json!({
        "number": number,
        "title": format!("Pull request {number}"),
        "url": format!("https://github.com/{repository}/pull/{number}"),
        "author": {"login": "octocat", "avatarUrl": "https://avatars/octocat"},
        "headRefName": "feat/page",
        "baseRefName": "main",
        "state": "OPEN",
        "isDraft": false,
        "mergeable": "MERGEABLE",
        "createdAt": "2026-07-01T00:00:00Z",
        "updatedAt": updated_at,
        "repository": {"nameWithOwner": repository},
        "reviewRequests": {"nodes": [{"requestedReviewer": {"login": "hubot"}}]},
        "labels": {"nodes": [{"name": "bug", "color": "ff0000"}]},
    })
}

fn search_page(nodes: Vec<Value>, has_next_page: bool) -> Reply {
    json(json!({"data": {"search": {"pageInfo": {"hasNextPage": has_next_page}, "nodes": nodes}}}))
}

fn list(host: &str, state: PullRequestListState, involvement: PullRequestInvolvement, limit: i64) -> ListChangeRequestsInput {
    ListChangeRequestsInput {
        cwd: "/w".into(),
        repository: "acme/web".into(),
        host: host.into(),
        state,
        involvement,
        viewer: "bilal".into(),
        limit,
        query: None,
        cursor: None,
        filters: None,
    }
}

fn open_list(limit: i64) -> ListChangeRequestsInput {
    list("github.com", PullRequestListState::Open, PullRequestInvolvement::All, limit)
}

fn search(repositories: &[&str], state: PullRequestListState, involvement: PullRequestInvolvement, limit: i64) -> ListChangeRequestsAcrossInput {
    ListChangeRequestsAcrossInput {
        cwd: "/w".into(),
        host: "github.com".into(),
        repositories: strings(repositories),
        state,
        involvement,
        viewer: "bilal".into(),
        limit,
        query: None,
        cursor: None,
        filters: None,
    }
}

fn filters() -> PullRequestListFilters {
    PullRequestListFilters {
        draft: None,
        review: None,
        checks: None,
        labels: None,
        excluded_labels: None,
        author: None,
    }
}

fn cursor() -> ProviderListCursor {
    ProviderListCursor {
        updated_before: "2026-07-02T00:00:00Z".into(),
        delivered: 10,
    }
}

fn action(action: PullRequestAction) -> RunActionInput {
    RunActionInput {
        change_request: pr("github.com", 7),
        action,
        stack_number: None,
        expected_stack_heads: None,
        merge_method: None,
        update_method: None,
    }
}

fn diff(cursor: Option<&str>, commit: Option<&str>) -> GetDiffInput {
    GetDiffInput {
        change_request: pr("github.com", 7),
        cursor: cursor.map(str::to_owned),
        commit: commit.map(str::to_owned),
    }
}

fn file_contents(commit: Option<&str>, change_type: PullRequestDiffFileContentsInputChangeType, path: &str) -> DiffFileContentsInput {
    DiffFileContentsInput {
        change_request: pr("github.com", 7),
        commit: commit.map(str::to_owned),
        change_type,
        old_path: path.into(),
        new_path: path.into(),
    }
}

fn approval_input() -> WorkflowApprovalInput {
    WorkflowApprovalInput {
        change_request: pr("github.com", 7),
        head_sha: "abc123".into(),
        head_branch: "feat/page".into(),
        head_repository_owner: "octocat".into(),
    }
}

fn reaction(number: i64, subject_id: Option<&str>, content: PullRequestReactionContent, reacted: bool) -> SetReactionInput {
    SetReactionInput {
        change_request: pr("github.com", number),
        subject_id: subject_id.map(str::to_owned),
        content,
        reacted,
    }
}

fn set_viewed(number: i64, files: &[(&str, bool)]) -> SetFilesViewedInput {
    SetFilesViewedInput {
        change_request: pr("github.com", number),
        files: files.iter().map(|(path, viewed)| ((*path).to_owned(), *viewed)).collect(),
    }
}

fn node_id(id: &str) -> Reply {
    json(json!({"data": {"repository": {"pullRequest": {"id": id}}}}))
}

fn subject(pull_request_id: &str, node_id: &str, node_pull_request_id: &str) -> Reply {
    json(json!({"data": {
        "repository": {"pullRequest": {"id": pull_request_id}},
        "node": {"id": node_id, "pullRequest": {"id": node_pull_request_id}},
    }}))
}

// ---------------------------------------------------------------------------------------------
// Credentials and routing identity
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn keeps_a_verified_credential_through_an_auth_switch_and_separates_token_fingerprints() {
    let (gh, cli, _clock) = setup();
    let active = Arc::new(Mutex::new("broad-credential".to_owned()));
    let token = active.clone();
    gh.respond(move |input| match input.args[0].as_str() {
        "auth" => ok(&token.lock().unwrap()),
        "api" => ok(r#"{"id":123,"login":"same-account"}"#),
        _ => ok(""),
    });
    let first = cli
        .with_verified_credential("/repo", "github.com", |identity| {
            let cli = cli.clone();
            let active = active.clone();
            async move {
                *active.lock().unwrap() = "restricted-credential".into();
                assert_eq!(cli.get_viewer_login("/repo", "github.com").await.unwrap(), "same-account");
                cli.comment_on_pull_request(CommentInput {
                    change_request: ChangeRequestRef {
                        cwd: "/repo".into(),
                        repository: "owner/repo".into(),
                        host: "github.com".into(),
                        number: 1,
                    },
                    body: "comment".into(),
                })
                .await
                .unwrap();
                identity
            }
        })
        .await
        .unwrap();
    let second = cli
        .with_verified_credential("/repo", "github.com", |identity| async move { identity })
        .await
        .unwrap();
    assert_eq!(first.account_id, second.account_id);
    assert_ne!(first.credential_fingerprint, second.credential_fingerprint);
    let encoded = format!("{first:?}{second:?}");
    assert!(!encoded.contains("broad-credential"));
    assert!(!encoded.contains("restricted-credential"));
    let calls = gh.calls();
    let comment = calls.iter().find(|call| call.args[0] == "pr").unwrap();
    let env = |call: &zc_core::process::ProcessRunInput, key: &str| call.env.as_ref().and_then(|env| env.get(key).cloned().flatten());
    assert_eq!(env(comment, "GH_TOKEN").as_deref(), Some("broad-credential"));
    assert_eq!(env(comment, "GITHUB_TOKEN").as_deref(), Some("broad-credential"));
    assert_eq!(env(comment, "GH_DEBUG").as_deref(), Some(""));
    let api_tokens: Vec<Option<String>> = calls.iter().filter(|call| call.args[0] == "api").map(|call| env(call, "GH_TOKEN")).collect();
    assert_eq!(api_tokens, [Some("broad-credential".to_owned()), Some("restricted-credential".to_owned())]);
    assert_eq!(
        cli.get_routing_identity("/repo", "github.com").await.unwrap(),
        RoutingIdentity {
            account_id: "123".into(),
            viewer: "same-account".into(),
        }
    );
}

#[tokio::test]
async fn coalesces_concurrent_identity_verification_for_the_same_host_and_credential() {
    let (gh, cli, _clock) = setup();
    gh.respond_async(|input| {
        let auth = input.args[0] == "auth";
        async move {
            if auth {
                ok("shared-credential")
            } else {
                tokio::task::yield_now().await;
                ok(r#"{"id":123,"login":"viewer"}"#)
            }
        }
    });
    let reads = (0..4).map(|_| cli.get_routing_identity("/w", "github.identity-flight.test"));
    let results = futures::future::join_all(reads).await;
    for result in results {
        assert_eq!(
            result.unwrap(),
            RoutingIdentity {
                account_id: "123".into(),
                viewer: "viewer".into(),
            }
        );
    }
    assert_eq!(gh.calls().iter().filter(|call| call.args[0] == "api").count(), 1);
}

#[tokio::test]
async fn lets_another_identity_reader_continue_when_the_first_verification_is_interrupted() {
    let (gh, cli, _clock) = setup();
    let first_started = Arc::new(tokio::sync::Notify::new());
    let second_started = Arc::new(tokio::sync::Notify::new());
    let tokens = Arc::new(AtomicUsize::new(0));
    let verifications = Arc::new(AtomicUsize::new(0));
    {
        let (first_started, second_started, tokens, verifications) = (first_started.clone(), second_started.clone(), tokens.clone(), verifications.clone());
        gh.respond_async(move |input| {
            let auth = input.args[0] == "auth";
            let (first_started, second_started, tokens, verifications) = (first_started.clone(), second_started.clone(), tokens.clone(), verifications.clone());
            async move {
                if auth {
                    if tokens.fetch_add(1, Ordering::SeqCst) + 1 == 2 {
                        second_started.notify_one();
                    }
                    return ok("cancel-credential");
                }
                if verifications.fetch_add(1, Ordering::SeqCst) + 1 == 1 {
                    first_started.notify_one();
                    return futures::future::pending().await;
                }
                ok(r#"{"id":123,"login":"viewer"}"#)
            }
        });
    }
    let first = tokio::spawn({
        let cli = cli.clone();
        async move { cli.get_routing_identity("/w", "github.identity-cancel.test").await }
    });
    first_started.notified().await;
    let second = tokio::spawn({
        let cli = cli.clone();
        async move { cli.get_routing_identity("/w", "github.identity-cancel.test").await }
    });
    second_started.notified().await;
    first.abort();
    assert_eq!(
        second.await.unwrap().unwrap(),
        RoutingIdentity {
            account_id: "123".into(),
            viewer: "viewer".into(),
        }
    );
    assert_eq!(verifications.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn fails_when_the_authenticated_account_has_no_login() {
    let (gh, cli, _clock) = setup();
    gh.once(ok("  "));
    let error = cli.get_viewer_login("/w", "github.com").await.unwrap_err();
    assert_eq!(error.tag(), "GitHubViewerLoginUnavailableError");
}

#[tokio::test]
async fn looks_up_the_authenticated_account_on_the_requested_enterprise_host() {
    let (gh, cli, _clock) = setup();
    gh.once(ok("enterprise-test-credential")).once(ok(r#"{"id":456,"login":"enterprise-user"}"#));
    assert_eq!(cli.get_viewer_login("/w", "github.acme.com").await.unwrap(), "enterprise-user");
    assert_eq!(gh.args(0), strings(&["auth", "token", "--hostname", "github.acme.com"]));
    assert_eq!(gh.args(1), strings(&["api", "user", "--hostname", "github.acme.com"]));
}

#[tokio::test]
async fn reuses_verified_credentials_offline_and_refuses_an_unverified_replacement() {
    let (gh, cli, _clock) = setup();
    let host = "github.identity-cache.test";
    let identity = |account_id: &str| RoutingIdentity {
        account_id: account_id.into(),
        viewer: "maria-rcks".into(),
    };
    gh.once(ok("test-credential-a")).once(ok(r#"{"id":123,"login":"maria-rcks"}"#));
    assert_eq!(cli.get_routing_identity("/w", host).await.unwrap(), identity("123"));
    assert_eq!(gh.env(1, "GH_ENTERPRISE_TOKEN").as_deref(), Some("test-credential-a"));
    assert_eq!(gh.env(1, "GH_DEBUG").as_deref(), Some(""));

    gh.once(ok("test-credential-a"));
    assert_eq!(cli.get_routing_identity("/w", host).await.unwrap(), identity("123"));
    assert_eq!(gh.count(), 3);

    gh.once(ok("test-credential-b")).once(failed("upstream failed with test-credential-b"));
    let failure = cli.get_routing_identity("/w", host).await.unwrap_err();
    assert_eq!(failure.tag(), "GitHubViewerLoginUnavailableError");
    assert!(!failure.to_string().contains("test-credential-b"));
    assert!(!format!("{failure:?}").contains("test-credential-b"));
    assert_eq!(gh.env(4, "GH_ENTERPRISE_TOKEN").as_deref(), Some("test-credential-b"));
    assert_eq!(gh.env(4, "GH_DEBUG").as_deref(), Some(""));

    gh.once(ok("test-credential-b")).once(ok(r#"{"id":456,"login":"maria-rcks"}"#));
    assert_eq!(cli.get_routing_identity("/w", host).await.unwrap(), identity("456"));
}

#[tokio::test]
async fn pins_every_call_inside_a_verified_credential_scope() {
    let (gh, cli, _clock) = setup();
    gh.respond(|input| match input.args[0].as_str() {
        "auth" => ok("scoped-credential"),
        "api" => ok(r#"{"id":9,"login":"someone"}"#),
        _ => ok(""),
    });
    let credential = cli.verified_credential("/w", "GitHub.com").await.unwrap();
    assert_eq!(credential.identity.viewer, "someone");
    assert!(credential.identity.credential_fingerprint.starts_with("github.com:"));
    let seen = Arc::new(Mutex::new(None));
    let slot = seen.clone();
    credential
        .scope
        .run(Box::pin(async move {
            *slot.lock().unwrap() = current_pinned_github_credential().map(|pinned| (pinned.host, pinned.token));
        }))
        .await;
    assert_eq!(seen.lock().unwrap().clone(), Some(("github.com".to_owned(), "scoped-credential".to_owned())));
}

// ---------------------------------------------------------------------------------------------
// GraphQL budget, previews and summaries
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn admits_only_one_concurrent_preview_above_the_reserve_and_resumes_after_reset() {
    let (gh, cli, clock) = setup();
    let host = "preview-budget.example";
    cli.github().budget().observe(
        host,
        &json!({"data": {"rateLimit": {"cost": 1, "limit": 5_000, "remaining": 501, "resetAt": RESET_AT}}}).to_string(),
    );
    gh.always(json(json!({"data": {
        "repository": {"pullRequest": {
            "number": 7,
            "title": "Fast previews",
            "url": "https://preview-budget.example/acme/web/pull/7",
            "state": "OPEN",
            "isDraft": false,
            "createdAt": "2026-07-01T00:00:00Z",
            "author": null,
        }},
        "rateLimit": rate_limit(500),
    }})));
    let reads = (1..=20).map(|number| cli.get_pull_request_preview(pr(host, number)));
    let results = futures::future::join_all(reads).await;
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    for result in &results {
        if let Err(error) = result {
            assert_eq!(error.tag(), "SourceControlRateLimitPausedError");
        }
    }
    assert_eq!(gh.count(), 1);
    clock.set(date_parse_millis(RESET_AT).unwrap());
    cli.get_pull_request_preview(pr(host, 7)).await.unwrap();
    assert_eq!(gh.count(), 2);
}

#[tokio::test]
async fn loads_the_complete_hover_card_with_one_graphql_request() {
    let (gh, cli, _clock) = setup();
    gh.once(json(json!({"data": {"repository": {"pullRequest": {
        "number": 7,
        "title": "Fast previews",
        "url": "https://github.example/acme/web/pull/7",
        "state": "MERGED",
        "isDraft": false,
        "createdAt": "2026-07-01T00:00:00Z",
        "author": {"login": "octocat", "name": "Octo Cat", "avatarUrl": "https://github.example/avatar.png"},
    }}}})));
    let preview = cli.get_pull_request_preview(pr("github.example", 7)).await.unwrap();
    assert_eq!(preview.number, 7);
    assert_eq!(preview.title, "Fast previews");
    assert_eq!(preview.url, "https://github.example/acme/web/pull/7");
    assert_eq!(preview.state, zc_contracts::PullRequestState::Merged);
    assert!(!preview.is_draft);
    assert_eq!(preview.created_at, "2026-07-01T00:00:00Z");
    let author = preview.author.unwrap();
    assert_eq!(author.login, "octocat");
    assert_eq!(author.name.as_deref(), Some("Octo Cat"));
    assert_eq!(author.avatar_url.as_deref(), Some("https://github.example/avatar.png"));
    assert_eq!(gh.count(), 1);
    assert_eq!(gh.args(0)[..4], strings(&["api", "graphql", "--hostname", "github.example"]));
}

#[tokio::test]
async fn reads_linked_pull_requests_on_one_host_together_filed_back_by_position() {
    let (gh, cli, _clock) = setup();
    let node = |number: i64| {
        json!({
            "number": number,
            "title": format!("Pull request {number}"),
            "url": format!("https://github.com/acme/web/pull/{number}"),
            "author": {"__typename": "User", "login": "octocat", "name": "Octo Cat", "avatarUrl": null},
            "baseRefName": "main",
            "headRefName": format!("feat/{number}"),
            "state": "OPEN",
            "isDraft": false,
            "mergeable": "MERGEABLE",
            "reviewDecision": null,
            "latestReviews": {"nodes": [{"state": "APPROVED", "author": {"login": "reviewer"}}]},
            "additions": 12,
            "deletions": 3,
            "changedFiles": 2,
            "updatedAt": "2026-08-24T12:34:56.000Z",
            "mergedAt": null,
            "closedAt": null,
            "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "SUCCESS"}}}]},
        })
    };
    gh.once(json(json!({"data": {"s0": {"pullRequest": node(7)}, "s1": {"pullRequest": node(8)}}})));
    let (seven, eight) = futures::join!(
        cli.get_pull_request_summary(pr("github.com", 7)),
        cli.get_pull_request_summary(pr("github.com", 8))
    );
    let (seven, eight) = (seven.unwrap(), eight.unwrap());
    assert_eq!(seven.number, 7);
    assert_eq!(seven.state, zc_contracts::PullRequestState::Open);
    assert_eq!(seven.head_branch, "feat/7");
    assert_eq!(seven.author.clone().flatten().unwrap().login, "octocat");
    assert_eq!(seven.changed_files, Some(2));
    assert_eq!(seven.review_decision, Some(Some(zc_contracts::PullRequestReviewDecision::Approved)));
    assert_eq!(seven.checks_state, Some(Some(zc_contracts::PullRequestChecksState::Passing)));
    assert_eq!(seven.mergeability, Some(zc_contracts::PullRequestMergeability::Mergeable));
    assert_eq!(eight.head_branch, "feat/8");
    assert_eq!(gh.count(), 1);
    let document = gh.args(0).last().cloned().unwrap();
    assert!(document.contains(r#"s0: repository(owner: "acme", name: "web") { pullRequest(number: 7)"#));
    assert!(document.contains("pullRequest(number: 8)"));
}

#[tokio::test]
async fn reads_a_pull_request_the_batch_said_nothing_about_on_its_own() {
    let (gh, cli, _clock) = setup();
    gh.once(ok(r#"{"data":{"s0":{"pullRequest":null}}}"#)).once(json(json!({
        "number": 7,
        "title": "Reuse the summary",
        "url": "https://github.com/acme/web/pull/7",
        "author": {"login": "octocat", "name": "Octo Cat"},
        "baseRefName": "main",
        "headRefName": "feat/summary",
        "state": "OPEN",
        "isDraft": false,
        "mergeable": "MERGEABLE",
        "reviewDecision": "APPROVED",
        "additions": 12,
        "deletions": 3,
        "changedFiles": 2,
        "createdAt": "2026-08-20T00:00:00.000Z",
        "updatedAt": "2026-08-24T12:34:56.000Z",
        "reviewRequests": [],
        "labels": [],
        "statusCheckRollup": [{"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS", "name": "ci"}],
        "body": "",
    })));
    let summary = cli.get_pull_request_summary(pr("github.com", 7)).await.unwrap();
    assert_eq!(summary.head_branch, "feat/summary");
    assert_eq!(summary.checks_state, Some(Some(zc_contracts::PullRequestChecksState::Passing)));
    assert_eq!(gh.count(), 2);
    let args = gh.args(1);
    assert_eq!(args[..6], strings(&["pr", "view", "7", "--repo", "github.com/acme/web", "--json"]));
    assert_eq!(args.len(), 7);
    assert!(args[6].contains("statusCheckRollup"));
}

#[tokio::test]
async fn fails_every_summary_of_a_batch_the_budget_paused() {
    let (gh, cli, _clock) = setup();
    cli.github()
        .budget()
        .observe("github.com", &json!({"data": {"rateLimit": rate_limit(100)}}).to_string());
    let (seven, eight) = futures::join!(
        cli.get_pull_request_summary(pr("github.com", 7)),
        cli.get_pull_request_summary(pr("github.com", 8))
    );
    // Reading them one at a time would only spend what the pause is saving.
    assert_eq!(seven.unwrap_err().tag(), "SourceControlRateLimitPausedError");
    assert_eq!(eight.unwrap_err().tag(), "SourceControlRateLimitPausedError");
    assert_eq!(gh.count(), 0);
}

// ---------------------------------------------------------------------------------------------
// Stacks
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn reads_the_stack_a_pull_request_is_in_through_the_stacks_preview_on_its_host() {
    let (gh, cli, _clock) = setup();
    gh.once(json(json!([{
        "id": 42,
        "number": 3,
        "url": "https://api.github.com/repos/acme/web/stacks/3",
        "base": {"ref": "main"},
        "pull_requests": [
            {"number": 6, "head": {"ref": "feat/one"}, "state": "closed", "merged_at": "2026-09-02"},
            {"number": 7, "head": {"ref": "feat/two"}, "state": "open", "merged_at": null},
        ],
    }])));
    let stack = cli.get_pull_request_stack(pr("ghe.example.com", 7), false).await.unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(&stack).unwrap(),
        json!({
            "id": "42",
            "number": 3,
            "url": "https://api.github.com/repos/acme/web/stacks/3",
            "base": "main",
            "layers": [
                {"number": 6, "headBranch": "feat/one", "state": "merged"},
                {"number": 7, "headBranch": "feat/two", "state": "open"},
            ],
        })
    );
    assert_eq!(
        gh.args(0),
        strings(&["api", "--hostname", "ghe.example.com", "repos/acme/web/stacks?pull_request=7"])
    );
}

#[tokio::test]
async fn fetches_layer_titles_only_when_the_caller_asks_for_stack_details() {
    let (gh, cli, _clock) = setup();
    let minimal = json!({
        "url": "https://api.github.com/repos/acme/web/stacks/3",
        "number": 3,
        "base": {"ref": "main"},
        "pull_requests": [{"number": 7, "head": {"ref": "feat/two", "sha": "abc123"}, "state": "open", "merged_at": null}],
    });
    let mut detailed = minimal.clone();
    detailed["pull_requests"][0]["title"] = json!("Second layer");
    detailed["pull_requests"][0]["draft"] = json!(false);
    gh.once(json(json!([minimal]))).once(json(detailed));
    let stack = cli.get_pull_request_stack(pr("github.com", 7), true).await.unwrap().unwrap();
    let layer = &stack.layers[0];
    assert_eq!(layer.title.as_deref(), Some("Second layer"));
    assert_eq!(layer.head_sha.as_deref(), Some("abc123"));
    assert_eq!(layer.is_draft, Some(false));
    assert_eq!(gh.args(1), strings(&["api", "--hostname", "github.com", "repos/acme/web/stacks/3"]));
}

#[tokio::test]
async fn reads_an_empty_stacks_listing_as_not_stacked() {
    let (gh, cli, _clock) = setup();
    gh.once(ok("[]"));
    assert_eq!(cli.get_pull_request_stack(pr("github.com", 7), false).await.unwrap(), None);
}

#[tokio::test]
async fn reads_a_host_that_refuses_the_stacks_preview_as_not_stacked() {
    let (gh, cli, _clock) = setup();
    gh.once(not_found());
    assert_eq!(cli.get_pull_request_stack(pr("github.com", 7), false).await.unwrap(), None);
}

#[tokio::test]
async fn does_not_read_a_signed_out_gh_as_an_unstacked_pull_request() {
    let (gh, cli, _clock) = setup();
    gh.once(unauthenticated());
    let error = cli.get_pull_request_stack(pr("github.com", 7), false).await.unwrap_err();
    assert_eq!(error.tag(), "GitHubCliAuthenticationError");
}

#[tokio::test]
async fn preserves_transient_stack_failures_instead_of_reporting_no_stack() {
    let (gh, cli, _clock) = setup();
    gh.once(failed("HTTP 503"));
    let error = cli.get_pull_request_stack(pr("github.com", 7), false).await.unwrap_err();
    assert_eq!(error.tag(), "GitHubCliCommandError");
}

#[tokio::test]
async fn reports_a_stacks_answer_it_cannot_read_against_the_stack_read() {
    let (gh, cli, _clock) = setup();
    gh.once(ok(r#"[{"id":42}]"#));
    let error = cli.get_pull_request_stack(pr("github.com", 7), false).await.unwrap_err();
    assert_eq!(error.tag(), "GitHubPullRequestReadError");
    assert!(matches!(&error, GitHubPullRequestCliError::Read { operation, .. } if operation == "getPullRequestStack"));
}

// ---------------------------------------------------------------------------------------------
// Listing and search
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn asks_for_one_row_more_than_the_page_to_probe_for_a_next_page() {
    let (gh, cli, _clock) = setup();
    gh.once(ok(&plain(3, 1)));
    gh.always(ok(r#"{"data":{}}"#));
    let batch = cli.list_pull_requests(open_list(10)).await.unwrap();
    assert_eq!(batch.items.len(), 3);
    assert!(!batch.truncated);
    let args = gh.args(0);
    for expected in ["--repo", "github.com/acme/web", "--state", "open", "--limit", "11"] {
        assert!(args.contains(&expected.to_owned()), "{expected} in {args:?}");
    }
}

#[tokio::test]
async fn reports_truncation_from_the_extra_row_counted_before_decoding() {
    let (gh, cli, _clock) = setup();
    gh.once(ok(&plain(11, 1)));
    gh.always(ok(r#"{"data":{}}"#));
    let batch = cli.list_pull_requests(open_list(10)).await.unwrap();
    assert_eq!(batch.items.len(), 10);
    assert!(batch.truncated);
}

#[tokio::test]
async fn excludes_merged_pull_requests_from_the_closed_tab() {
    let (gh, cli, _clock) = setup();
    gh.always(ok("[]"));
    cli.list_pull_requests(list("github.com", PullRequestListState::Closed, PullRequestInvolvement::All, 10))
        .await
        .unwrap();
    assert_eq!(gh.search_of(0).as_deref(), Some("is:unmerged sort:updated-desc"));
}

#[tokio::test]
async fn narrows_to_the_author_on_the_authored_tab() {
    let (gh, cli, _clock) = setup();
    gh.always(ok("[]"));
    cli.list_pull_requests(list("github.com", PullRequestListState::Open, PullRequestInvolvement::Authored, 10))
        .await
        .unwrap();
    let args = gh.args(0);
    assert!(args.contains(&"--author".to_owned()));
    assert!(args.contains(&"bilal".to_owned()));
}

#[tokio::test]
async fn narrows_through_search_on_the_reviewing_tab() {
    let (gh, cli, _clock) = setup();
    gh.always(ok("[]"));
    cli.list_pull_requests(list("github.com", PullRequestListState::Open, PullRequestInvolvement::Reviewing, 10))
        .await
        .unwrap();
    assert_eq!(gh.search_of(0).as_deref(), Some("review-requested:bilal sort:updated-desc"));
}

#[tokio::test]
async fn carries_every_repository_and_every_qualifier_into_one_search() {
    let (gh, cli, _clock) = setup();
    gh.always(search_page(vec![], false));
    let mut input = search(
        &["acme/web", "pingdotgg/t3code"],
        PullRequestListState::Closed,
        PullRequestInvolvement::Reviewing,
        10,
    );
    input.query = Some("pull requests page".into());
    input.cursor = Some(cursor());
    cli.search_pull_requests(input).await.unwrap();
    assert_eq!(gh.count(), 1);
    assert_eq!(
        gh.search_query_of(0).as_deref(),
        Some(
            r#"is:pr is:closed is:unmerged review-requested:bilal "pull requests page" updated:<=2026-07-02T00:00:00Z sort:updated-desc repo:acme/web repo:pingdotgg/t3code"#
        )
    );
}

#[tokio::test]
async fn narrows_a_search_to_the_author_and_to_merged_on_the_merged_tab() {
    let (gh, cli, _clock) = setup();
    gh.always(search_page(vec![], false));
    cli.search_pull_requests(search(&["acme/web"], PullRequestListState::Merged, PullRequestInvolvement::Authored, 10))
        .await
        .unwrap();
    assert_eq!(
        gh.search_query_of(0).as_deref(),
        Some("is:pr is:merged author:bilal sort:updated-desc repo:acme/web")
    );
}

#[tokio::test]
async fn keeps_a_searched_for_qualifier_inside_the_phrase_and_out_of_argv() {
    let (gh, cli, _clock) = setup();
    gh.always(search_page(vec![], false));
    let mut input = search(&["acme/web"], PullRequestListState::Open, PullRequestInvolvement::All, 10);
    input.query = Some(r#"x" is:merged repo:evil/repo"#.into());
    cli.search_pull_requests(input).await.unwrap();
    assert_eq!(
        gh.search_query_of(0).as_deref(),
        Some(r#"is:pr is:open "x\" is:merged repo:evil/repo" sort:updated-desc repo:acme/web"#)
    );
    assert!(!gh.args(0).contains(&"-f".to_owned()));
}

#[tokio::test]
async fn refuses_to_search_for_a_repository_github_cannot_address() {
    let (gh, cli, _clock) = setup();
    let failure = cli
        .search_pull_requests(search(
            &["acme/web", "acme/web is:merged"],
            PullRequestListState::Open,
            PullRequestInvolvement::All,
            10,
        ))
        .await
        .unwrap_err();
    assert_eq!(failure.tag(), "GitHubRepositorySelectorError");
    assert_eq!(gh.count(), 0);
}

#[tokio::test]
async fn files_each_searched_row_under_the_repository_it_came_from() {
    let (gh, cli, _clock) = setup();
    gh.always(search_page(
        vec![
            search_item(7, "acme/web", "2026-07-03T00:00:00Z"),
            search_item(9, "pingdotgg/t3code", "2026-07-02T00:00:00Z"),
            // Not a pull request, which `is:pr` excludes and a decode skips rather than fails on.
            json!({}),
        ],
        false,
    ));
    let batch = cli
        .search_pull_requests(search(
            &["acme/web", "pingdotgg/t3code"],
            PullRequestListState::Open,
            PullRequestInvolvement::All,
            10,
        ))
        .await
        .unwrap();
    let rows: Vec<(String, i64, Option<String>)> = batch
        .items
        .iter()
        .map(|item| {
            (
                item.repository.clone(),
                item.item.number,
                item.item.author.as_ref().and_then(|author| author.avatar_url.clone()),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("acme/web".to_owned(), 7, Some("https://avatars/octocat".to_owned())),
            ("pingdotgg/t3code".to_owned(), 9, Some("https://avatars/octocat".to_owned())),
        ]
    );
    // The listing leaves the line counts to a read of their own.
    assert!(batch.items.iter().all(|item| item.item.additions == 0 && item.item.deletions == 0));
    assert!(!batch.truncated);
}

#[tokio::test]
async fn reports_truncation_from_the_extra_row_and_from_a_page_github_says_has_more() {
    let (gh, cli, _clock) = setup();
    gh.once(search_page(
        vec![
            search_item(1, "acme/web", "2026-07-03T00:00:00Z"),
            search_item(2, "acme/web", "2026-07-02T00:00:00Z"),
            search_item(3, "acme/web", "2026-07-01T00:00:00Z"),
        ],
        false,
    ))
    .once(search_page(vec![search_item(1, "acme/web", "2026-07-03T00:00:00Z")], true));
    let read = || cli.search_pull_requests(search(&["acme/web"], PullRequestListState::Open, PullRequestInvolvement::All, 2));
    let overflowing = read().await.unwrap();
    let capped = read().await.unwrap();
    assert_eq!(overflowing.items.len(), 2);
    assert!(overflowing.truncated);
    assert!(capped.truncated);
}

fn memberships(stacked: &[usize]) -> Reply {
    let mut data = serde_json::Map::new();
    for index in stacked {
        data.insert(
            format!("s{index}"),
            json!({"pullRequest": {"stack": {"number": 3, "size": 2, "baseRefName": "main"}, "stackEntry": {"position": 1}}}),
        );
    }
    json(json!({"data": data}))
}

fn is_membership_read(input: &zc_core::process::ProcessRunInput) -> bool {
    input.args.iter().any(|arg| arg.contains("query PullRequestStackMemberships"))
}

#[tokio::test]
async fn enriches_only_the_visible_fallback_rows_after_filtering_and_widening() {
    let (gh, cli, _clock) = setup();
    let listings = Arc::new(Mutex::new(vec![
        plain(0, 1),
        pull_requests(3, 1, |_| json!({"isDraft": true})),
        pull_requests(6, 1, |number| json!({"isDraft": number < 4})),
    ]));
    gh.respond(move |input| {
        if is_membership_read(input) {
            return json(json!({"data": {
                "s0": {"pullRequest": {"stack": {"number": 3, "size": 2, "baseRefName": "main"}, "stackEntry": {"position": 1}}},
                "s1": {"pullRequest": {"stack": null, "stackEntry": null}},
            }}));
        }
        ok(&listings.lock().unwrap().remove(0))
    });
    let mut input = open_list(2);
    input.filters = Some(PullRequestListFilters {
        draft: Some(PullRequestListFiltersDraft::Hide),
        ..filters()
    });
    let batch = cli.list_pull_requests(input).await.unwrap();
    assert_eq!(batch.items.iter().map(|item| item.number).collect::<Vec<_>>(), [4, 5]);
    assert_eq!(
        serde_json::to_value(&batch.items[0].stack).unwrap(),
        json!({"number": 3, "size": 2, "base": "main", "position": 1})
    );
    assert_eq!(batch.items[1].stack, None);
    assert!(batch.truncated);
    assert!(!batch.continues);
    let membership_reads: Vec<_> = gh.calls().into_iter().filter(is_membership_read).collect();
    assert_eq!(membership_reads.len(), 1);
    let query = membership_reads[0].args.last().cloned().unwrap();
    assert!(query.contains("pullRequest(number: 4)"));
    assert!(query.contains("pullRequest(number: 5)"));
    assert!(!query.contains("pullRequest(number: 1)"));
    assert!(!query.contains("pullRequest(number: 6)"));
}

#[tokio::test]
async fn batches_membership_reads_and_keeps_successful_rows_when_one_chunk_fails() {
    let (gh, cli, _clock) = setup();
    gh.once(ok(&plain(27, 1)));
    gh.respond(|input| {
        if input.args.last().is_some_and(|query| query.contains("pullRequest(number: 26)")) {
            return failed("HTTP 502");
        }
        memberships(&[0])
    });
    let batch = cli.list_pull_requests(open_list(26)).await.unwrap();
    assert_eq!(batch.items.iter().map(|item| item.number).collect::<Vec<_>>(), (1..=26).collect::<Vec<_>>());
    assert_eq!(
        serde_json::to_value(&batch.items[0].stack).unwrap(),
        json!({"number": 3, "size": 2, "base": "main", "position": 1})
    );
    assert_eq!(batch.items[25].stack, None);
    assert!(batch.truncated);
    assert!(batch.continues);
    assert_eq!(gh.calls().iter().filter(|call| is_membership_read(call)).count(), 2);
}

#[tokio::test]
async fn skips_membership_enrichment_for_empty_pages_and_enterprise_hosts() {
    let (gh, cli, _clock) = setup();
    gh.once(ok("[]")).once(ok("[]")).once(ok(&plain(1, 7)));
    let empty = cli.list_pull_requests(open_list(2)).await.unwrap();
    let enterprise = cli
        .list_pull_requests(list("github.acme.test", PullRequestListState::Open, PullRequestInvolvement::All, 2))
        .await
        .unwrap();
    assert!(empty.items.is_empty());
    assert_eq!(enterprise.items.iter().map(|item| item.number).collect::<Vec<_>>(), [7]);
    assert!(!gh.calls().iter().any(is_membership_read));
}

#[tokio::test]
async fn reads_the_line_counts_in_chunks_and_files_them_back_by_position() {
    let (gh, cli, _clock) = setup();
    // Every chunk answers for its first alias only, so a row GitHub said nothing about is dropped.
    gh.always(json(json!({"data": {"s0": {"pullRequest": {"additions": 4, "deletions": 1}}}})));
    let stats = cli
        .list_pull_request_stats(ListChangeRequestStatsInput {
            cwd: "/w".into(),
            host: "github.com".into(),
            change_requests: (1..=26).map(|number| ("acme/web".to_owned(), number)).collect(),
        })
        .await
        .unwrap();
    assert_eq!(gh.count(), 2);
    let stat = |number| GitHubPullRequestStat {
        repository: "acme/web".into(),
        number,
        additions: 4,
        deletions: 1,
    };
    assert_eq!(stats, [stat(1), stat(26)]);
    let document = gh.args(0).last().cloned().unwrap();
    assert!(document.contains(r#"s0: repository(owner: "acme", name: "web")"#));
    assert!(document.contains("pullRequest(number: 25)"));
}

#[tokio::test]
async fn refuses_to_look_up_counts_for_a_repository_github_cannot_address() {
    let (gh, cli, _clock) = setup();
    let failure = cli
        .list_pull_request_stats(ListChangeRequestStatsInput {
            cwd: "/w".into(),
            host: "github.com".into(),
            change_requests: vec![(r#"acme/web") { x } #"#.into(), 1)],
        })
        .await
        .unwrap_err();
    assert_eq!(failure.tag(), "GitHubRepositorySelectorError");
    assert_eq!(gh.count(), 0);
}

#[tokio::test]
async fn hands_a_search_to_github_rather_than_to_the_rows_already_read() {
    let (gh, cli, _clock) = setup();
    gh.always(ok("[]"));
    let mut input = open_list(10);
    input.query = Some("pull requests page".into());
    cli.list_pull_requests(input).await.unwrap();
    assert_eq!(gh.search_of(0).as_deref(), Some(r#""pull requests page" sort:updated-desc"#));
}

#[tokio::test]
async fn joins_a_search_onto_the_tabs_own_qualifiers_instead_of_replacing_them() {
    let (gh, cli, _clock) = setup();
    gh.always(ok("[]"));
    let mut input = list("github.com", PullRequestListState::Closed, PullRequestInvolvement::Reviewing, 10);
    input.query = Some("page".into());
    cli.list_pull_requests(input).await.unwrap();
    assert_eq!(gh.args(0).iter().filter(|arg| *arg == "--search").count(), 1);
    assert_eq!(
        gh.search_of(0).as_deref(),
        Some(r#"review-requested:bilal is:unmerged "page" sort:updated-desc"#)
    );
}

#[tokio::test]
async fn carries_the_further_narrowings_into_the_search_as_qualifiers() {
    let (gh, cli, _clock) = setup();
    gh.always(ok("[]"));
    let mut input = open_list(10);
    input.filters = Some(PullRequestListFilters {
        draft: Some(PullRequestListFiltersDraft::Hide),
        review: Some(PullRequestListFiltersReview::ChangesRequested),
        checks: Some(PullRequestListFiltersChecks::Failing),
        labels: Some(vec![strings(&["needs design"]), strings(&[r#"quo"te"#])]),
        excluded_labels: Some(strings(&["wip"])),
        author: Some("octocat".into()),
    });
    cli.list_pull_requests(input).await.unwrap();
    assert_eq!(
        gh.search_of(0).as_deref(),
        Some(r#"label:"needs design" label:"quote" -label:"wip" author:"octocat" draft:false review:changes_requested status:failure sort:updated-desc"#)
    );
}

#[tokio::test]
async fn resolves_an_author_filter_of_me_to_the_viewer_not_the_literal_word() {
    let (gh, cli, _clock) = setup();
    gh.always(ok("[]"));
    let mut input = open_list(10);
    input.filters = Some(PullRequestListFilters {
        author: Some("me".into()),
        ..filters()
    });
    cli.list_pull_requests(input).await.unwrap();
    assert_eq!(gh.search_of(0).as_deref(), Some(r#"author:"bilal" sort:updated-desc"#));
}

#[tokio::test]
async fn sends_one_label_qualifier_per_group_its_names_joined_the_way_github_ors_them() {
    let (gh, cli, _clock) = setup();
    gh.always(ok("[]"));
    let mut input = open_list(10);
    input.filters = Some(PullRequestListFilters {
        labels: Some(vec![strings(&["size:S", "size:XS"]), strings(&["bug"])]),
        ..filters()
    });
    cli.list_pull_requests(input).await.unwrap();
    let expected = r#"label:"size:S","size:XS" label:"bug" sort:updated-desc"#;
    assert_eq!(gh.search_of(0).as_deref(), Some(expected));
    assert!(gh.args(0).contains(&expected.to_owned()));
}

#[tokio::test]
async fn falls_back_for_a_repository_the_index_does_not_cover_under_a_checks_filter_keeping_only_the_matching_rows() {
    let (gh, cli, _clock) = setup();
    gh.once(ok("[]")).once(ok(&pull_requests(2, 1, |number| {
        json!({"statusCheckRollup": if number == 1 {
            json!([{"name": "lint", "status": "COMPLETED", "conclusion": "SUCCESS"}])
        } else {
            json!([{"name": "test", "status": "COMPLETED", "conclusion": "FAILURE"}])
        }})
    })));
    gh.always(ok(r#"{"data":{}}"#));
    let mut input = open_list(10);
    input.filters = Some(PullRequestListFilters {
        checks: Some(PullRequestListFiltersChecks::Passing),
        ..filters()
    });
    let batch = cli.list_pull_requests(input).await.unwrap();
    assert_eq!(gh.search_of(1), None);
    assert_eq!(batch.items.iter().map(|item| item.number).collect::<Vec<_>>(), [1]);
}

#[tokio::test]
async fn fails_a_checks_filter_for_a_row_whose_checks_are_still_pending() {
    let (gh, cli, _clock) = setup();
    gh.once(ok("[]")).once(ok(&pull_requests(
        1,
        1,
        |_| json!({"statusCheckRollup": [{"name": "build", "status": "IN_PROGRESS"}]}),
    )));
    let mut input = open_list(10);
    input.filters = Some(PullRequestListFilters {
        checks: Some(PullRequestListFiltersChecks::Passing),
        ..filters()
    });
    let batch = cli.list_pull_requests(input).await.unwrap();
    assert!(batch.items.is_empty());
}

#[tokio::test]
async fn falls_back_for_a_repository_the_index_does_not_cover_even_under_a_judgeable_filter() {
    let (gh, cli, _clock) = setup();
    gh.once(ok("[]")).once(ok(&pull_requests(2, 1, |number| json!({"isDraft": number == 1}))));
    gh.always(ok(r#"{"data":{}}"#));
    let mut input = open_list(10);
    input.filters = Some(PullRequestListFilters {
        draft: Some(PullRequestListFiltersDraft::Hide),
        ..filters()
    });
    let batch = cli.list_pull_requests(input).await.unwrap();
    assert_eq!(gh.search_of(1), None);
    assert_eq!(batch.items.iter().map(|item| item.number).collect::<Vec<_>>(), [2]);
}

#[tokio::test]
async fn carries_the_further_narrowings_into_a_batched_search() {
    let (gh, cli, _clock) = setup();
    gh.always(search_page(vec![], false));
    let mut input = search(&["acme/web"], PullRequestListState::Open, PullRequestInvolvement::All, 10);
    input.filters = Some(PullRequestListFilters {
        draft: Some(PullRequestListFiltersDraft::Only),
        review: Some(PullRequestListFiltersReview::None),
        labels: Some(vec![strings(&["bug"])]),
        ..filters()
    });
    cli.search_pull_requests(input).await.unwrap();
    assert_eq!(
        gh.search_query_of(0).as_deref(),
        Some(r#"is:pr is:open label:"bug" draft:true review:none sort:updated-desc repo:acme/web"#)
    );
}

#[tokio::test]
async fn quotes_a_search_so_it_cannot_add_a_qualifier_or_a_flag_of_its_own() {
    let (gh, cli, _clock) = setup();
    gh.always(ok("[]"));
    let mut input = open_list(10);
    input.query = Some(r#"-- is:merged label:secret "widen me""#.into());
    cli.list_pull_requests(input).await.unwrap();
    assert_eq!(
        gh.search_of(0).as_deref(),
        Some(r#""-- is:merged label:secret \"widen me\"" sort:updated-desc"#)
    );
    assert!(!gh.args(0).contains(&"is:merged".to_owned()));
}

#[tokio::test]
async fn escapes_a_backslash_before_the_quote_it_would_otherwise_let_out() {
    let (gh, cli, _clock) = setup();
    gh.always(ok("[]"));
    let mut input = open_list(10);
    input.query = Some(r#"a\" is:merged"#.into());
    cli.list_pull_requests(input).await.unwrap();
    assert_eq!(gh.search_of(0).as_deref(), Some(r#""a\\\" is:merged" sort:updated-desc"#));
}

#[tokio::test]
async fn asks_for_nothing_but_the_order_when_the_reader_typed_only_spaces() {
    let (gh, cli, _clock) = setup();
    gh.always(ok("[]"));
    let mut input = open_list(10);
    input.query = Some("   ".into());
    cli.list_pull_requests(input).await.unwrap();
    assert_eq!(gh.search_of(0).as_deref(), Some("sort:updated-desc"));
}

#[tokio::test]
async fn carries_on_from_the_instant_the_last_slice_ended_on() {
    let (gh, cli, _clock) = setup();
    gh.respond(|input| if is_membership_read(input) { ok(r#"{"data":{}}"#) } else { ok(&plain(3, 1)) });
    let mut input = open_list(10);
    input.cursor = Some(cursor());
    let batch = cli.list_pull_requests(input).await.unwrap();
    assert_eq!(gh.search_of(0).as_deref(), Some("updated:<=2026-07-02T00:00:00Z sort:updated-desc"));
    assert!(batch.continues);
}

#[tokio::test]
async fn answers_a_search_that_found_nothing_with_nothing_not_with_the_whole_repository() {
    let (gh, cli, _clock) = setup();
    gh.once(ok("[]"));
    let mut input = open_list(10);
    input.query = Some("fdsfklj".into());
    let batch = cli.list_pull_requests(input).await.unwrap();
    assert!(batch.items.is_empty());
    assert_eq!(gh.count(), 1);
}

#[tokio::test]
async fn reads_a_repository_github_will_not_search_the_way_gh_lists_one() {
    let (gh, cli, _clock) = setup();
    // GitHub answers for a repository outside its search index with no rows and no error.
    gh.once(ok("[]")).once(ok(&pull_requests(3, 1, |_| json!({"state": "CLOSED"}))));
    gh.always(ok(r#"{"data":{}}"#));
    let batch = cli
        .list_pull_requests(list("github.com", PullRequestListState::Closed, PullRequestInvolvement::All, 10))
        .await
        .unwrap();
    assert_eq!(batch.items.len(), 3);
    assert_eq!(gh.search_of(1), None);
    assert!(!batch.continues);
}

#[tokio::test]
async fn keeps_state_and_involvement_filters_on_the_search_free_fallback() {
    let (gh, cli, _clock) = setup();
    gh.once(ok("[]")).once(ok(&pull_requests(4, 1, |number| {
        let mut row = json!({
            "state": if number == 4 { "OPEN" } else { "CLOSED" },
            "reviewRequests": if number == 2 { json!([{"slug": "platform", "name": "Platform"}]) } else { json!([{"login": "bilal"}]) },
        });
        if number == 3 {
            row["mergedAt"] = json!("2026-07-03T00:00:00Z");
        }
        row
    })));
    gh.always(ok(r#"{"data":{}}"#));
    let batch = cli
        .list_pull_requests(list("github.com", PullRequestListState::Closed, PullRequestInvolvement::Reviewing, 10))
        .await
        .unwrap();
    // Individual requests for this viewer and team requests survive.
    assert_eq!(batch.items.iter().map(|item| item.number).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(gh.search_of(1), None);
    assert!(!batch.continues);
}

#[tokio::test]
async fn grows_the_search_free_fallback_until_it_fills_the_filtered_page() {
    let (gh, cli, _clock) = setup();
    let unrelated = || json!({"reviewRequests": [{"login": "somebody-else"}]});
    gh.once(ok("[]"))
        .once(ok(&pull_requests(3, 1, |_| unrelated())))
        .once(ok(&pull_requests(4, 1, |number| {
            if number == 4 {
                json!({"reviewRequests": [{"login": "bilal"}]})
            } else {
                unrelated()
            }
        })));
    gh.always(ok(r#"{"data":{}}"#));
    let batch = cli
        .list_pull_requests(list("github.com", PullRequestListState::Open, PullRequestInvolvement::Reviewing, 2))
        .await
        .unwrap();
    assert_eq!(batch.items.iter().map(|item| item.number).collect::<Vec<_>>(), [4]);
    assert_eq!(gh.limit_of(1), "3");
    assert_eq!(gh.limit_of(2), "6");
    assert!(!batch.truncated);
}

#[tokio::test]
async fn bounds_a_sparse_search_free_fallback_and_reports_the_unread_tail() {
    let (gh, cli, _clock) = setup();
    let reads = Arc::new(AtomicUsize::new(0));
    gh.respond(move |input| {
        if reads.fetch_add(1, Ordering::SeqCst) == 0 {
            return ok("[]");
        }
        let flag = input.args.iter().position(|arg| arg == "--limit").unwrap();
        let limit: i64 = input.args[flag + 1].parse().unwrap();
        ok(&pull_requests(limit, 1, |_| json!({"reviewRequests": [{"login": "somebody-else"}]})))
    });
    let batch = cli
        .list_pull_requests(list("github.com", PullRequestListState::Open, PullRequestInvolvement::Reviewing, 2))
        .await
        .unwrap();
    assert_eq!(gh.limit_of(gh.count() - 1), "1000");
    assert!(batch.items.is_empty());
    assert!(batch.truncated);
}

#[tokio::test]
async fn takes_an_empty_slice_for_a_repository_that_has_run_out_not_one_to_read_again() {
    let (gh, cli, _clock) = setup();
    gh.always(ok("[]"));
    let mut input = open_list(10);
    input.cursor = Some(cursor());
    cli.list_pull_requests(input).await.unwrap();
    assert_eq!(gh.count(), 1);
}

#[tokio::test]
async fn names_the_host_on_every_repository_it_addresses() {
    let (gh, cli, _clock) = setup();
    gh.always(ok("[]"));
    cli.list_pull_requests(list("github.acme.dev", PullRequestListState::Open, PullRequestInvolvement::All, 10))
        .await
        .unwrap();
    // A bare `owner/repo` resolves against github.com, which is a different repository.
    assert!(gh.args(0).contains(&"github.acme.dev/acme/web".to_owned()));
}

// ---------------------------------------------------------------------------------------------
// Actions and workflow approvals
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn updates_a_stale_branch_with_a_merge_commit_unless_asked_to_rebase() {
    let (gh, cli, _clock) = setup();
    gh.always(ok(""));
    cli.run_pull_request_action(action(PullRequestAction::UpdateBranch)).await.unwrap();
    assert_eq!(gh.args(0), strings(&["pr", "update-branch", "7", "--repo", "github.com/acme/web"]));
    let mut rebase = action(PullRequestAction::UpdateBranch);
    rebase.update_method = Some(PullRequestUpdateMethod::Rebase);
    cli.run_pull_request_action(rebase).await.unwrap();
    assert_eq!(gh.args(1), strings(&["pr", "update-branch", "7", "--repo", "github.com/acme/web", "--rebase"]));
}

#[tokio::test]
async fn merges_with_the_strategy_it_was_asked_for() {
    let (gh, cli, _clock) = setup();
    gh.always(ok(""));
    let mut merge = action(PullRequestAction::Merge);
    merge.merge_method = Some(PullRequestMergeMethod::Squash);
    cli.run_pull_request_action(merge).await.unwrap();
    assert_eq!(gh.args(0), strings(&["pr", "merge", "7", "--repo", "github.com/acme/web", "--squash"]));
}

#[tokio::test]
async fn arms_auto_merge_with_the_same_strategy_a_merge_would_have_used() {
    let (gh, cli, _clock) = setup();
    gh.always(ok(""));
    let mut arm = action(PullRequestAction::EnableAutoMerge);
    arm.merge_method = Some(PullRequestMergeMethod::Squash);
    cli.run_pull_request_action(arm).await.unwrap();
    assert_eq!(
        gh.args(0),
        strings(&["pr", "merge", "7", "--repo", "github.com/acme/web", "--auto", "--squash"])
    );
    cli.run_pull_request_action(action(PullRequestAction::EnableAutoMerge)).await.unwrap();
    assert_eq!(gh.args(1), strings(&["pr", "merge", "7", "--repo", "github.com/acme/web", "--auto", "--merge"]));
}

#[tokio::test]
async fn takes_auto_merge_back_off_without_naming_a_strategy() {
    let (gh, cli, _clock) = setup();
    gh.always(ok(""));
    let mut disarm = action(PullRequestAction::DisableAutoMerge);
    disarm.merge_method = Some(PullRequestMergeMethod::Squash);
    cli.run_pull_request_action(disarm).await.unwrap();
    assert_eq!(gh.args(0), strings(&["pr", "merge", "7", "--repo", "github.com/acme/web", "--disable-auto"]));
}

#[tokio::test]
async fn returns_a_pull_request_to_draft_by_undoing_ready() {
    let (gh, cli, _clock) = setup();
    gh.always(ok(""));
    cli.run_pull_request_action(action(PullRequestAction::Draft)).await.unwrap();
    assert_eq!(gh.args(0), strings(&["pr", "ready", "7", "--repo", "github.com/acme/web", "--undo"]));
}

#[tokio::test]
async fn opens_a_pull_request_that_reverts_a_merged_pull_request() {
    let (gh, cli, _clock) = setup();
    gh.once(node_id("PR_7")).once(ok("{}"));
    cli.run_pull_request_action(action(PullRequestAction::Revert)).await.unwrap();
    for expected in ["owner=acme", "name=web", "number=7"] {
        assert!(gh.args(0).contains(&expected.to_owned()));
    }
    assert_eq!(gh.args(1), strings(&["api", "graphql", "--hostname", "github.com", "--input", "-"]));
    assert!(gh.stdin(1).contains("revertPullRequest"));
    assert!(gh.stdin(1).contains(r#""pullRequestId":"PR_7""#));
}

fn fork_detail(head_sha: &str, cross_repository: bool, owner: Value) -> Reply {
    json(core_response(json!({
        "number": 7,
        "title": "Pull request 7",
        "url": "https://github.com/acme/web/pull/7",
        "headRefName": "feat/page",
        "headRefOid": head_sha,
        "isCrossRepository": cross_repository,
        "headRepositoryOwner": owner,
        "baseRefName": "main",
        "createdAt": "2026-07-01T00:00:00Z",
        "updatedAt": "2026-07-02T00:00:00Z",
    })))
}

fn heads(numbers: &[i64]) -> Reply {
    json(Value::Array(
        numbers
            .iter()
            .map(|number| json!({"number": number, "headRefOid": "abc123", "isCrossRepository": true, "headRepositoryOwner": {"login": "octocat"}}))
            .collect(),
    ))
}

#[tokio::test]
async fn does_not_approve_action_required_runs_for_a_same_repository_pull_request() {
    let (gh, cli, _clock) = setup();
    gh.once(fork_detail("abc123", false, json!({"login": "acme"})));
    cli.run_pull_request_action(action(PullRequestAction::ApproveWorkflows)).await.unwrap();
    assert_eq!(gh.count(), 1);
}

#[tokio::test]
async fn finds_and_approves_every_workflow_waiting_on_a_maintainer() {
    let (gh, cli, _clock) = setup();
    let detail = || fork_detail("abc123", true, json!({"login": "octocat"}));
    let runs = || {
        json(json!([
            {"databaseId": 10, "workflowName": "build", "url": "https://example.com/10"},
            {"databaseId": 11, "workflowName": "test", "url": "https://example.com/11"},
        ]))
    };
    // The heads and runs reads run concurrently, so each is answered by what it asks for.
    gh.respond(move |input| match (input.args[0].as_str(), input.args[1].as_str()) {
        ("api", "graphql") => detail(),
        ("pr", "list") => heads(&[7]),
        ("run", "list") => runs(),
        _ => ok(""),
    });
    cli.run_pull_request_action(action(PullRequestAction::ApproveWorkflows)).await.unwrap();
    let calls = gh.calls();
    let heads_read = calls.iter().find(|call| call.args[..2] == ["pr", "list"]).unwrap();
    assert_eq!(
        heads_read.args,
        strings(&[
            "pr",
            "list",
            "--repo",
            "github.com/acme/web",
            "--state",
            "open",
            "--head",
            "feat/page",
            "--limit",
            "1001",
            "--json",
            "number,headRefOid,isCrossRepository,headRepositoryOwner",
        ])
    );
    let runs_read = calls.iter().find(|call| call.args[..2] == ["run", "list"]).unwrap();
    assert_eq!(
        runs_read.args,
        strings(&[
            "run",
            "list",
            "--repo",
            "github.com/acme/web",
            "--commit",
            "abc123",
            "--branch",
            "feat/page",
            "--event",
            "pull_request",
            "--status",
            "action_required",
            "--limit",
            "1001",
            "--json",
            "databaseId,workflowName,url",
        ])
    );
    let approvals: Vec<Vec<String>> = calls
        .iter()
        .filter(|call| call.args.contains(&"--silent".to_owned()))
        .map(|call| call.args.clone())
        .collect();
    assert_eq!(
        approvals,
        [
            strings(&[
                "api",
                "--method",
                "POST",
                "--hostname",
                "github.com",
                "repos/acme/web/actions/runs/10/approve",
                "--silent"
            ]),
            strings(&[
                "api",
                "--method",
                "POST",
                "--hostname",
                "github.com",
                "repos/acme/web/actions/runs/11/approve",
                "--silent"
            ]),
        ]
    );
    // Each approval follows its own fresh detail, heads and runs reads.
    assert_eq!(gh.count(), 11);
    assert_eq!(gh.args(6), approvals[0]);
    assert_eq!(gh.args(10), approvals[1]);
}

#[tokio::test]
async fn refuses_a_stale_workflow_approval_after_the_pull_request_head_changes() {
    let (gh, cli, _clock) = setup();
    let details = Arc::new(Mutex::new(vec![
        fork_detail("abc123", true, json!({"login": "octocat"})),
        fork_detail("def456", true, json!({"login": "octocat"})),
    ]));
    gh.respond(move |input| match (input.args[0].as_str(), input.args[1].as_str()) {
        ("api", "graphql") => details.lock().unwrap().remove(0),
        ("pr", "list") => heads(&[7]),
        _ => json(json!([{"databaseId": 10, "workflowName": "build", "url": "https://example.com/10"}])),
    });
    let error = cli.run_pull_request_action(action(PullRequestAction::ApproveWorkflows)).await.unwrap_err();
    assert_eq!(error.tag(), "GitHubWorkflowApprovalHeadChangedError");
    assert!(matches!(error, GitHubPullRequestCliError::WorkflowApprovalHeadChanged { number: 7, .. }));
    assert_eq!(gh.count(), 4);
}

#[tokio::test]
async fn reads_workflow_runs_and_their_pull_request_scope_concurrently() {
    let (gh, cli, _clock) = setup();
    let heads_started = Arc::new(tokio::sync::Barrier::new(2));
    gh.respond_async(move |input| {
        let barrier = heads_started.clone();
        let heads = input.args[0] == "pr";
        async move {
            // Each read waits for the other to have started: run one after the other, they never would.
            barrier.wait().await;
            if heads {
                ok(r#"[{"number":7,"headRefOid":"abc123","isCrossRepository":true,"headRepositoryOwner":{"login":"octocat"}}]"#)
            } else {
                ok(r#"[{"databaseId":10,"workflowName":"build","url":"https://example.com/10"}]"#)
            }
        }
    });
    let runs = cli.list_workflow_runs_requiring_approval(approval_input()).await.unwrap();
    assert_eq!(
        serde_json::to_value(&runs).unwrap(),
        json!([{"id": 10, "name": "build", "url": "https://example.com/10"}])
    );
}

#[tokio::test]
async fn refuses_workflow_approval_when_one_head_belongs_to_several_pull_requests() {
    let (gh, cli, _clock) = setup();
    gh.respond(|input| if input.args[0] == "pr" { heads(&[7, 8]) } else { ok("[]") });
    let error = cli.list_workflow_runs_requiring_approval(approval_input()).await.unwrap_err();
    assert!(matches!(
        error,
        GitHubPullRequestCliError::WorkflowApprovalRefused {
            reason: WorkflowApprovalRefusal::HeadNotUnique,
            number: 7,
            observed_count: 2,
            limit: 1_000,
            ..
        }
    ));
    assert!(error.detail().contains("instead of uniquely matching #7"));
    assert_eq!(gh.count(), 2);
}

#[tokio::test]
async fn refuses_workflow_approval_when_github_omits_the_head_repository() {
    let (gh, cli, _clock) = setup();
    gh.once(fork_detail("abc123", true, Value::Null));
    let error = cli.run_pull_request_action(action(PullRequestAction::ApproveWorkflows)).await.unwrap_err();
    assert!(matches!(error, GitHubPullRequestCliError::WorkflowApprovalHeadUnavailable { number: 7, .. }));
    assert_eq!(gh.count(), 1);
}

#[tokio::test]
async fn surfaces_a_workflow_run_list_beyond_the_safe_approval_bound() {
    let (gh, cli, _clock) = setup();
    let runs: Vec<Value> = (1..=1_001).map(|id| json!({"databaseId": id})).collect();
    let runs = Value::Array(runs).to_string();
    gh.respond(move |input| if input.args[0] == "pr" { heads(&[7]) } else { ok(&runs) });
    let error = cli.list_workflow_runs_requiring_approval(approval_input()).await.unwrap_err();
    assert!(matches!(
        error,
        GitHubPullRequestCliError::WorkflowApprovalRefused {
            reason: WorkflowApprovalRefusal::RunListTruncated,
            number: 7,
            observed_count: 1_001,
            limit: 1_000,
            ..
        }
    ));
    assert!(error.detail().contains("more than 1000 workflow runs"));
    assert_eq!(gh.count(), 2);
}

// ---------------------------------------------------------------------------------------------
// Comments, reviews, threads, reactions and rewrites
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn sends_a_comment_body_over_stdin_never_in_argv() {
    let (gh, cli, _clock) = setup();
    gh.always(ok(""));
    cli.comment_on_pull_request(CommentInput {
        change_request: pr("github.com", 7),
        body: "Looks good.".into(),
    })
    .await
    .unwrap();
    assert_eq!(
        gh.args(0),
        strings(&["pr", "comment", "7", "--repo", "github.com/acme/web", "--body-file", "-"])
    );
    assert_eq!(gh.stdin(0), "Looks good.");
    assert!(!gh.args(0).contains(&"Looks good.".to_owned()));
}

#[tokio::test]
async fn sends_a_whole_review_as_one_request_body_over_stdin() {
    let (gh, cli, _clock) = setup();
    gh.always(ok("{}"));
    cli.submit_review(SubmitReviewInput {
        change_request: pr("github.com", 7),
        verdict: PullRequestReviewVerdict::Approve,
        body: "Looks right.".into(),
        comments: vec![PullRequestReviewCommentDraft {
            path: "src/a.ts".into(),
            old_path: None,
            position: PullRequestReviewPosition::Added(PullRequestReviewPositionAdded {
                kind: zc_contracts::LitAdded,
                new_line: 4,
            }),
            body: "nit".into(),
        }],
    })
    .await
    .unwrap();
    assert_eq!(
        gh.args(0),
        strings(&[
            "api",
            "--method",
            "POST",
            "--hostname",
            "github.com",
            "repos/acme/web/pulls/7/reviews",
            "--input",
            "-"
        ])
    );
    assert_eq!(gh.count(), 1);
    assert_eq!(
        gh.body(0),
        json!({"event": "APPROVE", "body": "Looks right.", "comments": [{"path": "src/a.ts", "line": 4, "side": "RIGHT", "body": "nit"}]})
    );
}

#[tokio::test]
async fn sends_a_reply_body_over_stdin_never_in_argv() {
    let (gh, cli, _clock) = setup();
    gh.always(ok("{}"));
    cli.reply_to_review_thread(ReplyToThreadInput {
        change_request: pr("github.com", 7),
        thread_id: "PRRT_1".into(),
        body: "Fixed in 42ff8ec.".into(),
    })
    .await
    .unwrap();
    assert_eq!(gh.args(0), strings(&["api", "graphql", "--hostname", "github.com", "--input", "-"]));
    let request = gh.body(0);
    assert!(request["query"].as_str().unwrap().contains("addPullRequestReviewThreadReply"));
    assert_eq!(request["variables"], json!({"threadId": "PRRT_1", "body": "Fixed in 42ff8ec."}));
    assert!(!gh.args(0).join(" ").contains("Fixed in 42ff8ec."));
}

#[tokio::test]
async fn resolves_and_unresolves_through_the_mutation_each_one_needs() {
    let (gh, cli, _clock) = setup();
    gh.always(ok("{}"));
    for resolved in [true, false] {
        cli.set_review_thread_resolution(SetThreadResolutionInput {
            change_request: pr("github.acme.dev", 7),
            thread_id: "PRRT_1".into(),
            resolved,
        })
        .await
        .unwrap();
    }
    assert!(gh.body(0)["query"].as_str().unwrap().contains("resolveReviewThread("));
    assert!(gh.body(1)["query"].as_str().unwrap().contains("unresolveReviewThread("));
    assert!(gh.args(0).contains(&"github.acme.dev".to_owned()));
}

#[tokio::test]
async fn confirms_a_given_subject_belongs_to_the_named_pull_request_then_reacts_to_it() {
    let (gh, cli, _clock) = setup();
    gh.once(subject("PR_kwDOA", "IC_1", "PR_kwDOA")).once(ok("{}"));
    cli.set_reaction(reaction(7, Some("IC_1"), PullRequestReactionContent::Heart, true))
        .await
        .unwrap();
    assert_eq!(gh.count(), 2);
    for expected in ["owner=acme", "name=web", "number=7", "subjectId=IC_1"] {
        assert!(gh.args(0).contains(&expected.to_owned()));
    }
    let request = gh.body(1);
    assert!(request["query"].as_str().unwrap().contains("addReaction("));
    assert_eq!(request["variables"], json!({"subjectId": "IC_1", "content": "HEART"}));
}

#[tokio::test]
async fn refuses_a_given_subject_that_belongs_to_a_different_pull_request() {
    let (gh, cli, _clock) = setup();
    gh.once(subject("PR_thisOne", "IC_99", "PR_someOtherOne"));
    let error = cli
        .set_reaction(reaction(7, Some("IC_99"), PullRequestReactionContent::Heart, true))
        .await
        .unwrap_err();
    assert_eq!(error.tag(), "GitHubSubjectScopeError");
    // Refused before any mutation was sent.
    assert_eq!(gh.count(), 1);
}

#[tokio::test]
async fn looks_up_the_pull_requests_own_node_id_when_no_subject_was_given() {
    let (gh, cli, _clock) = setup();
    gh.once(node_id("PR_kwDOA")).once(ok("{}"));
    cli.set_reaction(reaction(21, None, PullRequestReactionContent::Rocket, true)).await.unwrap();
    assert_eq!(gh.count(), 2);
    for expected in ["owner=acme", "name=web", "number=21"] {
        assert!(gh.args(0).contains(&expected.to_owned()));
    }
    let request = gh.body(1);
    assert!(request["query"].as_str().unwrap().contains("addReaction("));
    assert_eq!(request["variables"], json!({"subjectId": "PR_kwDOA", "content": "ROCKET"}));
}

#[tokio::test]
async fn takes_a_reaction_back_through_the_remove_mutation() {
    let (gh, cli, _clock) = setup();
    gh.once(subject("PR_kwDOA", "IC_1", "PR_kwDOA")).once(ok("{}"));
    cli.set_reaction(reaction(7, Some("IC_1"), PullRequestReactionContent::Heart, false))
        .await
        .unwrap();
    assert!(gh.body(1)["query"].as_str().unwrap().contains("removeReaction("));
}

#[tokio::test]
async fn rewrites_only_the_words_a_request_named() {
    let (gh, cli, _clock) = setup();
    gh.always(node_id("PR_kwDOA"));
    let rewrite = |title: Option<&str>, body: Option<&str>| UpdateChangeRequestInput {
        change_request: pr("github.com", 22),
        title: title.map(str::to_owned),
        body: body.map(str::to_owned),
    };
    cli.update_pull_request(rewrite(Some("A better title"), None)).await.unwrap();
    cli.update_pull_request(rewrite(None, Some("A better description."))).await.unwrap();
    cli.update_pull_request(rewrite(Some("Both"), Some("at once."))).await.unwrap();
    // One node id lookup for the pull request, then a mutation per rewrite.
    assert_eq!(gh.body(1)["variables"], json!({"pullRequestId": "PR_kwDOA", "title": "A better title"}));
    assert_eq!(gh.body(2)["variables"], json!({"pullRequestId": "PR_kwDOA", "body": "A better description."}));
    assert_eq!(
        gh.body(3)["variables"],
        json!({"pullRequestId": "PR_kwDOA", "title": "Both", "body": "at once."})
    );
    // Key order is the TS object's: the id, then the title, then the body.
    assert_eq!(
        gh.stdin(3).split("\"variables\":").nth(1).unwrap(),
        r#"{"pullRequestId":"PR_kwDOA","title":"Both","body":"at once."}}"#
    );
    assert!(!gh.args(3).join(" ").contains("at once."));
}

#[tokio::test]
async fn rewrites_a_remark_through_the_mutation_its_kind_needs() {
    let (gh, cli, _clock) = setup();
    gh.always(subject("PR_kwDOA", "IC_1", "PR_kwDOA"));
    for kind in [
        PullRequestCommentUpdateInputKind::IssueComment,
        PullRequestCommentUpdateInputKind::ReviewComment,
    ] {
        cli.update_comment(UpdateCommentInput {
            change_request: pr("github.com", 7),
            comment_id: "IC_1".into(),
            kind,
            body: "Reworded.".into(),
        })
        .await
        .unwrap();
    }
    assert!(gh.args(0).contains(&"subjectId=IC_1".to_owned()));
    assert!(gh.body(1)["query"].as_str().unwrap().contains("updateIssueComment("));
    assert_eq!(gh.body(1)["variables"], json!({"commentId": "IC_1", "body": "Reworded."}));
    assert!(gh.body(3)["query"].as_str().unwrap().contains("updatePullRequestReviewComment("));
    assert_eq!(gh.body(3)["variables"], json!({"commentId": "IC_1", "body": "Reworded."}));
}

#[tokio::test]
async fn refuses_a_comment_that_belongs_to_a_different_pull_request() {
    let (gh, cli, _clock) = setup();
    gh.once(subject("PR_thisOne", "IC_99", "PR_someOtherOne"));
    let error = cli
        .update_comment(UpdateCommentInput {
            change_request: pr("github.com", 7),
            comment_id: "IC_99".into(),
            kind: PullRequestCommentUpdateInputKind::IssueComment,
            body: "Reworded.".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.tag(), "GitHubSubjectScopeError");
    assert!(error.message().contains("updateComment"));
    assert_eq!(gh.count(), 1);
}

// ---------------------------------------------------------------------------------------------
// Review threads
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn asks_a_github_enterprise_host_for_its_own_review_threads() {
    let (gh, cli, _clock) = setup();
    gh.once(json(
        json!({"data": {"repository": {"pullRequest": {"reviewThreads": {"totalCount": 0, "nodes": []}}}}}),
    ));
    cli.list_review_thread_comments(pr("github.acme.dev", 7)).await.unwrap();
    for expected in ["--hostname", "github.acme.dev", "owner=acme", "name=web"] {
        assert!(gh.args(0).contains(&expected.to_owned()));
    }
}

#[tokio::test]
async fn follows_the_cursor_to_the_review_threads_the_first_page_left_behind() {
    let (gh, cli, _clock) = setup();
    gh.once(ok(&review_threads_page(vec![thread("PRRT_1", &["c1"])], Some("Y3Vyc29yOjE"))))
        .once(ok(&review_threads_page(vec![thread("PRRT_2", &["c2"])], None)));
    let conversation = cli.list_review_thread_comments(pr("github.com", 7)).await.unwrap();
    // The first page asks from the beginning, which gh only sends as a typed JSON null.
    assert!(gh.args(0).contains(&"cursor=null".to_owned()));
    assert!(gh.args(1).contains(&"cursor=Y3Vyc29yOjE".to_owned()));
    assert_eq!(
        conversation.comments.iter().map(|comment| comment.id.as_str()).collect::<Vec<_>>(),
        ["c1", "c2"]
    );
    assert!(!conversation.truncated);
}

#[tokio::test]
async fn stops_at_the_thread_bound_and_says_the_conversation_was_cut_short() {
    let (gh, cli, _clock) = setup();
    gh.always(ok(&review_threads_page(vec![thread("PRRT_1", &["c1"])], Some("Y3Vyc29yOjE"))));
    let conversation = cli.list_review_thread_comments(pr("github.com", 7)).await.unwrap();
    assert_eq!(gh.count(), 10);
    assert!(conversation.truncated);
}

#[tokio::test]
async fn leaves_a_long_thread_paged_until_the_reader_asks_for_more() {
    let (gh, cli, _clock) = setup();
    let mut long = thread("PRRT_1", &["c1"]);
    long["comments"] = thread_comments(&["c1"], Some("Y3Vyc29yOjI"), 3);
    gh.once(ok(&review_threads_page(vec![long], None)));
    let conversation = cli.list_review_thread_comments(pr("github.com", 7)).await.unwrap();
    assert_eq!(gh.count(), 1);
    assert_eq!(conversation.comments.iter().map(|comment| comment.id.as_str()).collect::<Vec<_>>(), ["c1"]);
    assert_eq!(conversation.review_threads[0].comment_count, Some(3));
    assert_eq!(conversation.review_threads[0].next_comments_cursor.as_deref(), Some("Y3Vyc29yOjI"));
    assert!(conversation.truncated);
}

#[tokio::test]
async fn reads_one_requested_page_from_a_review_thread_cursor() {
    let (gh, cli, _clock) = setup();
    gh.once(ok(&thread_comments_page(&["c2", "c3"], None, 3, "PR_7")));
    let page = cli
        .get_review_thread_comments(ReviewThreadCommentsInput {
            change_request: pr("github.com", 7),
            thread_id: "PRRT_1".into(),
            cursor: "Y3Vyc29yOjI".into(),
        })
        .await
        .unwrap();
    for expected in ["owner=acme", "name=web", "number=7", "threadId=PRRT_1", "cursor=Y3Vyc29yOjI"] {
        assert!(gh.args(0).contains(&expected.to_owned()));
    }
    assert_eq!(gh.count(), 1);
    assert_eq!(page.comments.iter().map(|comment| comment.id.as_str()).collect::<Vec<_>>(), ["c2", "c3"]);
    assert_eq!(page.next_cursor, None);
}

#[tokio::test]
async fn refuses_a_review_thread_from_another_pull_request() {
    let (gh, cli, _clock) = setup();
    gh.once(ok(&thread_comments_page(&["foreign"], None, 1, "PR_8")));
    let error = cli
        .get_review_thread_comments(ReviewThreadCommentsInput {
            change_request: pr("github.com", 7),
            thread_id: "PRRT_FOREIGN".into(),
            cursor: "Y3Vyc29yOjI".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.tag(), "GitHubSubjectScopeError");
}

// ---------------------------------------------------------------------------------------------
// Diffs and file contents
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn serves_a_diff_github_hands_over_whole_in_one_request_with_no_next_slice() {
    let (gh, cli, _clock) = setup();
    gh.once(ok("diff --git a/a b/a"));
    let slice = cli.get_pull_request_diff(diff(None, None)).await.unwrap();
    assert_eq!(slice.next_cursor, None);
    assert!(!slice.truncated);
    assert_eq!(gh.count(), 1);
    // The review needs GitHub's combined pull-request diff, not a format-patch stream.
    assert!(!gh.args(0).contains(&"--patch".to_owned()));
    assert_eq!(gh.args(0), strings(&["pr", "diff", "7", "--repo", "github.com/acme/web", "--color", "never"]));
}

#[tokio::test]
async fn reads_one_files_page_when_github_refuses_the_diff_and_says_it_is_the_last() {
    let (gh, cli, _clock) = setup();
    gh.once(command_failed()).once(ok(&pull_request_files(2, 1)));
    let mut input = diff(None, None);
    input.change_request.host = "github.acme.dev".into();
    let slice = cli.get_pull_request_diff(input).await.unwrap();
    assert!(!slice.truncated);
    assert_eq!(slice.next_cursor, None);
    assert!(slice.patch.contains("diff --git a/src/file1.ts b/src/file1.ts"));
    assert!(slice.patch.contains("diff --git a/src/file2.ts b/src/file2.ts"));
    for expected in ["--hostname", "github.acme.dev", "repos/acme/web/pulls/7/files?per_page=100&page=1"] {
        assert!(gh.args(1).contains(&expected.to_owned()));
    }
}

#[tokio::test]
async fn hands_back_a_cursor_for_the_next_page_rather_than_walking_on_by_itself() {
    let (gh, cli, _clock) = setup();
    gh.once(command_failed()).once(ok(&pull_request_files(100, 0)));
    let slice = cli.get_pull_request_diff(diff(None, None)).await.unwrap();
    assert!(!slice.truncated);
    assert!(slice.next_cursor.is_some());
    assert_eq!(gh.count(), 2);
}

#[tokio::test]
async fn carries_on_from_a_cursor_without_asking_gh_pr_diff_again() {
    let (gh, cli, _clock) = setup();
    gh.once(command_failed()).once(ok(&pull_request_files(100, 0)));
    let first = cli.get_pull_request_diff(diff(None, None)).await.unwrap();
    let next = first.next_cursor.unwrap();
    gh.once(ok(&pull_request_files(4, 100)));
    let second = cli.get_pull_request_diff(diff(Some(&next), None)).await.unwrap();
    assert_eq!(second.next_cursor, None);
    assert!(second.patch.contains("diff --git a/src/file100.ts b/src/file100.ts"));
    assert_eq!(gh.count(), 3);
    assert!(gh.args(2).contains(&"repos/acme/web/pulls/7/files?per_page=100&page=2".to_owned()));
}

#[tokio::test]
async fn refuses_a_cursor_it_never_handed_out_rather_than_reading_it_into_a_request() {
    let (gh, cli, _clock) = setup();
    let error = cli.get_pull_request_diff(diff(Some("1&per_page=1"), None)).await.unwrap_err();
    assert_eq!(error.tag(), "GitHubDiffCursorError");
    assert_eq!(gh.count(), 0);
}

#[tokio::test]
async fn reads_a_named_commit_from_the_commit_endpoint_rather_than_from_gh_pr_diff() {
    let (gh, cli, _clock) = setup();
    gh.once(ok(&pull_request_files(2, 1)));
    let slice = cli
        .get_pull_request_diff(diff(None, Some("a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0")))
        .await
        .unwrap();
    assert_eq!(gh.count(), 1);
    assert_eq!(slice.next_cursor, None);
    assert!(slice.patch.contains("diff --git a/src/file1.ts b/src/file1.ts"));
    assert!(gh
        .args(0)
        .contains(&"repos/acme/web/commits/a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0?per_page=100&page=1".to_owned()));
    // The commit endpoint wraps its files in an object, which jq unwraps for the decoder.
    assert!(gh.args(0).contains(&".files // []".to_owned()));
}

#[tokio::test]
async fn pages_inside_a_commit_the_way_it_pages_the_pull_requests_own_files() {
    let (gh, cli, _clock) = setup();
    gh.once(ok(&pull_request_files(100, 0)));
    let first = cli.get_pull_request_diff(diff(None, Some("a1b2c3d"))).await.unwrap();
    let next = first.next_cursor.unwrap();
    gh.once(ok(&pull_request_files(4, 100)));
    let second = cli.get_pull_request_diff(diff(Some(&next), Some("a1b2c3d"))).await.unwrap();
    assert_eq!(second.next_cursor, None);
    assert!(gh.args(1).contains(&"repos/acme/web/commits/a1b2c3d?per_page=100&page=2".to_owned()));
}

#[tokio::test]
async fn refuses_a_commit_that_is_not_a_sha_rather_than_reading_it_into_a_request() {
    let (gh, cli, _clock) = setup();
    let error = cli.get_pull_request_diff(diff(None, Some("../../pulls/8/files"))).await.unwrap_err();
    assert_eq!(error.tag(), "GitHubDiffCommitError");
    assert_eq!(gh.count(), 0);
}

#[tokio::test]
async fn ends_the_diff_on_a_page_with_no_files_rather_than_asking_for_it_again() {
    let (gh, cli, _clock) = setup();
    gh.always(ok("[]"));
    let slice = cli.get_pull_request_diff(diff(Some("4"), None)).await.unwrap();
    assert_eq!(slice.patch, "");
    assert_eq!(slice.next_cursor, None);
}

#[tokio::test]
async fn reports_the_refused_diff_when_the_files_api_cannot_answer_either() {
    let (gh, cli, _clock) = setup();
    gh.once(command_failed()).once(ok("not json"));
    let error = cli.get_pull_request_diff(diff(None, None)).await.unwrap_err();
    assert!(matches!(&error, GitHubPullRequestCliError::Cli(cli_error) if cli_error.kind == GitHubCliErrorKind::Command));
}

#[tokio::test]
async fn fails_a_files_page_too_large_to_read_rather_than_calling_the_diff_whole() {
    let (gh, cli, _clock) = setup();
    gh.once(command_failed()).once(truncated(&pull_request_files(1, 1)));
    let error = cli.get_pull_request_diff(diff(None, None)).await.unwrap_err();
    // The refusal that sent the read down this road is the one reported, by design.
    assert_eq!(error.tag(), "GitHubCliCommandError");
}

#[tokio::test]
async fn pages_an_oversized_patch_by_file_rather_than_handing_back_a_severed_one() {
    let (gh, cli, _clock) = setup();
    gh.once(truncated("diff --git a/a b/a\n@@ -1 +1 @@")).once(ok(&pull_request_files(1, 1)));
    let slice = cli.get_pull_request_diff(diff(None, None)).await.unwrap();
    assert!(gh.args(1).join(" ").contains("/pulls/7/files"));
    assert!(slice.patch.contains("src/file1.ts"));
    assert_eq!(gh.count(), 2);
}

#[tokio::test]
async fn expands_a_new_file_from_a_root_commit_without_requiring_a_parent() {
    let (gh, cli, _clock) = setup();
    gh.once(ok("\ta1b2c3d\n")).once(ok("root contents\n"));
    let contents = cli
        .get_pull_request_diff_file_contents(file_contents(Some("a1b2c3d"), PullRequestDiffFileContentsInputChangeType::New, "src/root.ts"))
        .await
        .unwrap();
    assert_eq!(
        contents,
        ProviderDiffFileContents {
            old_contents: String::new(),
            new_contents: "root contents\n".into(),
        }
    );
    assert_eq!(gh.count(), 2);
    assert!(gh.args(1).join(" ").contains("contents/src/root.ts?ref=a1b2c3d"));
}

#[tokio::test]
async fn reads_both_revisions_through_the_raw_contents_endpoint() {
    let (gh, cli, _clock) = setup();
    gh.once(ok("a1b2c3d\tb1c2d3e\n"));
    gh.respond(|input| {
        if input.args.last().unwrap().ends_with("ref=a1b2c3d") {
            ok("old\n")
        } else {
            ok("new\n")
        }
    });
    let contents = cli
        .get_pull_request_diff_file_contents(file_contents(None, PullRequestDiffFileContentsInputChangeType::Change, "src/a b/c.ts"))
        .await
        .unwrap();
    assert_eq!(contents.old_contents, "old\n");
    assert_eq!(contents.new_contents, "new\n");
    assert_eq!(
        gh.args(0),
        strings(&[
            "api",
            "--hostname",
            "github.com",
            "repos/acme/web/pulls/7",
            "--jq",
            "[.base.sha, .head.sha] | @tsv"
        ])
    );
    assert_eq!(
        gh.calls()[1..].iter().map(|call| call.args.clone()).collect::<Vec<_>>(),
        [
            strings(&[
                "api",
                "--hostname",
                "github.com",
                "--header",
                "Accept: application/vnd.github.raw+json",
                "repos/acme/web/contents/src/a%20b/c.ts?ref=a1b2c3d"
            ]),
            strings(&[
                "api",
                "--hostname",
                "github.com",
                "--header",
                "Accept: application/vnd.github.raw+json",
                "repos/acme/web/contents/src/a%20b/c.ts?ref=b1c2d3e"
            ]),
        ]
    );
}

#[tokio::test]
async fn reports_unusable_diff_revisions_as_a_structured_error() {
    let (gh, cli, _clock) = setup();
    gh.once(ok("not-a-sha\tstill-not-a-sha\n"));
    let error = cli
        .get_pull_request_diff_file_contents(file_contents(Some("a1b2c3d"), PullRequestDiffFileContentsInputChangeType::Change, "src/a.ts"))
        .await
        .unwrap_err();
    assert!(matches!(
        &error,
        GitHubPullRequestCliError::DiffRevisionsUnavailable { number: 7, commit: Some(commit), .. } if commit == "a1b2c3d"
    ));
}

#[tokio::test]
async fn reports_an_oversized_diff_file_with_its_path_and_reason() {
    let (gh, cli, _clock) = setup();
    gh.once(ok("a1b2c3d\tb1c2d3e\n")).once(truncated("partial"));
    let error = cli
        .get_pull_request_diff_file_contents(file_contents(None, PullRequestDiffFileContentsInputChangeType::Deleted, "src/large.ts"))
        .await
        .unwrap_err();
    assert!(matches!(
        &error,
        GitHubPullRequestCliError::DiffFileContentsUnavailable { path, reason: DiffFileUnavailableReason::Oversized, .. } if path == "src/large.ts"
    ));
}

#[tokio::test]
async fn reports_undecodable_diff_file_contents_as_binary() {
    let (gh, cli, _clock) = setup();
    gh.once(ok("a1b2c3d\tb1c2d3e\n")).once(invalid_utf8("binary\u{FFFD}contents"));
    let error = cli
        .get_pull_request_diff_file_contents(file_contents(None, PullRequestDiffFileContentsInputChangeType::Deleted, "assets/logo.png"))
        .await
        .unwrap_err();
    assert!(matches!(
        &error,
        GitHubPullRequestCliError::DiffFileContentsUnavailable { path, reason: DiffFileUnavailableReason::Binary, .. } if path == "assets/logo.png"
    ));
}

#[tokio::test]
async fn returns_valid_text_containing_a_literal_replacement_character() {
    let (gh, cli, _clock) = setup();
    gh.once(ok("a1b2c3d\tb1c2d3e\n")).once(ok("before\u{FFFD}after"));
    let contents = cli
        .get_pull_request_diff_file_contents(file_contents(None, PullRequestDiffFileContentsInputChangeType::Deleted, "docs/encoding.md"))
        .await
        .unwrap();
    assert_eq!(contents.old_contents, "before\u{FFFD}after");
}

// ---------------------------------------------------------------------------------------------
// Avatars, detail, access and comparisons
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn skips_the_avatar_lookup_when_a_listing_named_nobody() {
    let (gh, cli, _clock) = setup();
    let avatars = cli
        .list_actor_avatars(ActorAvatarsInput {
            cwd: "/w".into(),
            repository: "acme/web".into(),
            host: "github.com".into(),
            ids: vec![],
        })
        .await
        .unwrap();
    assert!(avatars.is_empty());
    assert_eq!(gh.count(), 0);
}

#[tokio::test]
async fn accounts_for_the_avatar_lookup_in_the_graphql_budget() {
    let (gh, cli, _clock) = setup();
    gh.once(json(json!({"data": {
        "nodes": [{"login": "octocat", "avatarUrl": "https://avatars/octocat"}],
        "rateLimit": {"cost": 1, "limit": 5_000, "remaining": 4_999, "resetAt": "2099-08-13T14:00:00Z"},
    }})));
    let avatars = cli
        .list_actor_avatars(ActorAvatarsInput {
            cwd: "/w".into(),
            repository: "acme/web".into(),
            host: "github.com".into(),
            ids: vec!["MDQ6VXNlcjE=".into()],
        })
        .await
        .unwrap();
    assert!(gh.args(0).contains(&"ids[]=MDQ6VXNlcjE=".to_owned()));
    assert!(gh.args(0).last().unwrap().contains("rateLimit { cost limit remaining resetAt }"));
    assert_eq!(avatars.get("octocat").map(String::as_str), Some("https://avatars/octocat"));
}

#[tokio::test]
async fn fails_the_read_when_gh_returns_something_unreadable() {
    let (gh, cli, _clock) = setup();
    gh.once(ok(r#"{"message":"not found"}"#));
    let error = cli.get_pull_request_detail(pr("github.com", 7)).await.unwrap_err();
    assert_eq!(error.tag(), "GitHubPullRequestReadError");
    assert_eq!(error.detail(), "GitHub CLI returned an unreadable getPullRequestDetail response.");
}

#[tokio::test]
async fn keeps_the_core_detail_read_separate_from_conversation_activity() {
    let (gh, cli, _clock) = setup();
    gh.once(json(core_response(json!({
        "number": 7,
        "title": "Progressive detail",
        "url": "https://github.com/acme/web/pull/7",
        "author": {"login": "octocat"},
        "headRefName": "feature",
        "baseRefName": "main",
        "createdAt": "2026-07-01T00:00:00Z",
        "updatedAt": "2026-07-02T00:00:00Z",
        "body": "Core body",
        "changedFiles": 2,
    }))))
    .once(json(json!({"author": {"login": "octocat"}, "comments": [], "reviews": [], "commits": []})));
    let detail = cli.get_pull_request_detail(pr("github.com", 7)).await.unwrap();
    let activity = cli.get_pull_request_activity(pr("github.com", 7)).await.unwrap();
    assert_eq!(detail.detail.body, "Core body");
    assert_eq!(activity.author.unwrap().login, "octocat");
    assert!(gh.args(0).contains(&"headRef=refs/pull/7/head".to_owned()));
    assert!(gh.args(0).last().unwrap().contains("viewerCanUpdateBranch"));
    assert_eq!(
        serde_json::to_value(&detail.viewer_access.merge_capabilities).unwrap(),
        json!({"merge": true, "squash": false, "rebase": true})
    );
    assert_eq!(
        serde_json::to_value(&detail.comparison).unwrap(),
        json!({"behindBy": 2, "viewerCanUpdate": true})
    );
    assert_eq!(gh.args(1).last().unwrap(), "author,comments,reviews,commits");
}

#[tokio::test]
async fn decodes_reviewers_labels_and_workflow_checks_without_another_detail_read() {
    let (gh, cli, _clock) = setup();
    gh.once(json(core_response(json!({
        "baseRef": null,
        "reviewRequests": {"nodes": [
            {"requestedReviewer": {"login": "reviewer"}},
            {"requestedReviewer": {"slug": "maintainers", "name": "Maintainers"}},
        ]},
        "labels": {"nodes": [{"name": "bug", "color": "ff0000"}]},
        "commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {
            "nodes": [
                {"__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS", "checkSuite": {"workflowRun": {"workflow": {"name": "linux"}}}},
                {"__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "FAILURE", "checkSuite": {"workflowRun": {"workflow": {"name": "windows"}}}},
            ],
            "pageInfo": {"hasNextPage": false},
        }}}}]},
    }))));
    let detail = cli.get_pull_request_detail(pr("github.com", 7)).await.unwrap();
    assert_eq!(gh.count(), 1);
    assert_eq!(detail.comparison, None);
    assert_eq!(detail.detail.item.review_request_logins, ["reviewer"]);
    assert!(detail.detail.item.has_team_review_request);
    assert_eq!(
        serde_json::to_value(&detail.detail.item.labels).unwrap(),
        json!([{"name": "bug", "color": "ff0000"}])
    );
    assert_eq!(detail.detail.checks.len(), 2);
    assert_eq!(detail.detail.item.checks_state, Some(zc_contracts::PullRequestChecksState::Failing));
}

fn paged_checks_core() -> Value {
    core_response(json!({"commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {
        "nodes": [{"name": "first", "status": "COMPLETED", "conclusion": "SUCCESS"}],
        "pageInfo": {"hasNextPage": true},
    }}}}]}}))
}

fn legacy_detail(core: &Value, head: &str, checks: Value) -> Reply {
    let mut legacy = core["data"]["repository"]["pullRequest"].clone();
    legacy["headRefOid"] = json!(head);
    legacy["reviewRequests"] = json!([]);
    legacy["labels"] = json!([]);
    legacy["statusCheckRollup"] = checks;
    json(legacy)
}

#[tokio::test]
async fn reads_every_check_when_the_combined_response_has_another_page() {
    let (gh, cli, _clock) = setup();
    let core = paged_checks_core();
    gh.once(json(core.clone())).once(legacy_detail(
        &core,
        "abc123",
        json!([
            {"name": "first", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"name": "last", "status": "COMPLETED", "conclusion": "FAILURE"},
        ]),
    ));
    let detail = cli.get_pull_request_detail(pr("github.com", 7)).await.unwrap();
    assert_eq!(detail.detail.checks.len(), 2);
    assert_eq!(detail.detail.item.checks_state, Some(zc_contracts::PullRequestChecksState::Failing));
    assert!(!detail.checks_truncated);
    assert_eq!(gh.args(1)[..2], strings(&["pr", "view"]));
}

#[tokio::test]
async fn refuses_to_combine_checks_from_different_head_revisions() {
    let (gh, cli, _clock) = setup();
    let core = paged_checks_core();
    gh.once(json(core.clone())).once(legacy_detail(&core, "new-head", json!([])));
    let error = cli.get_pull_request_detail(pr("github.com", 7)).await.unwrap_err();
    assert_eq!(error.tag(), "GitHubPullRequestReadError");
}

#[tokio::test]
async fn preserves_the_reserve_for_automatic_detail_reads_and_allows_manual_checks() {
    let (gh, cli, _clock) = setup();
    let mut response = core_response(json!({}));
    response["data"]["rateLimit"] = json!({"cost": 1, "limit": 5000, "remaining": 500, "resetAt": "2099-08-13T14:00:00Z"});
    gh.always(json(response));
    let input = pr("github.core-reserve.test", 7);
    cli.get_pull_request_detail(input.clone()).await.unwrap();
    let error = cli.get_pull_request_detail(input.clone()).await.unwrap_err();
    assert_eq!(error.tag(), "SourceControlRateLimitPausedError");
    assert_eq!(gh.count(), 1);
    with_github_reserve(cli.get_pull_request_detail(input)).await.unwrap();
    assert_eq!(gh.count(), 2);
}

fn viewer_permissions(permission: &str) -> Reply {
    json(json!({"data": {"repository": {
        "mergeCommitAllowed": true,
        "squashMergeAllowed": false,
        "rebaseMergeAllowed": true,
        "viewerPermission": permission,
        "pullRequest": {"viewerCanUpdate": true, "viewerDidAuthor": true},
    }}}))
}

fn read_only_author() -> Value {
    json!({
        "mergeCapabilities": {"merge": true, "squash": false, "rebase": true},
        "canWrite": false,
        "canTriage": false,
        "canUpdate": true,
        "didAuthor": true,
    })
}

#[tokio::test]
async fn asks_for_the_readers_standing_on_the_repository_and_on_the_pull_request_at_once() {
    let (gh, cli, _clock) = setup();
    gh.always(viewer_permissions("READ"));
    let access = cli
        .get_viewer_access(ViewerAccessInput {
            change_request: pr("github.com", 7),
            allow_reserve: None,
        })
        .await
        .unwrap();
    assert_eq!(gh.count(), 1);
    assert!(gh.args(0).contains(&"number=7".to_owned()));
    assert!(gh.args(0).last().unwrap().contains("mergeCommitAllowed squashMergeAllowed rebaseMergeAllowed"));
    assert_eq!(serde_json::to_value(&access).unwrap(), read_only_author());
}

fn comparison(rate: Option<Value>) -> Reply {
    let mut data = json!({"repository": {"pullRequest": {"viewerCanUpdateBranch": true, "baseRef": {"compare": {"behindBy": 4}}}}});
    if let Some(rate) = rate {
        data["rateLimit"] = rate;
    }
    json(json!({"data": data}))
}

fn base_comparison(allow_reserve: Option<bool>) -> BaseComparisonInput {
    BaseComparisonInput {
        change_request: pr("github.com", 7),
        head_ref: "fork:feat/page".into(),
        allow_reserve,
    }
}

#[tokio::test]
async fn sends_the_base_comparisons_variables_as_gh_flags_not_as_bare_words() {
    let (gh, cli, _clock) = setup();
    gh.always(comparison(None));
    let result = cli.get_pull_request_base_comparison(base_comparison(None)).await.unwrap();
    let args = gh.args(0);
    assert_eq!(
        args[..args.len() - 2],
        strings(&[
            "api",
            "graphql",
            "--hostname",
            "github.com",
            "-f",
            "owner=acme",
            "-f",
            "name=web",
            "-F",
            "number=7",
            "-f",
            "headRef=fork:feat/page"
        ])
    );
    assert_eq!(serde_json::to_value(&result).unwrap(), json!({"behindBy": 4, "viewerCanUpdate": true}));
    assert_eq!(args[args.len() - 2], "-f");
    assert!(args
        .last()
        .unwrap()
        .contains(&format!("query={}", &BASE_COMPARISON_GRAPHQL_QUERY[..BASE_COMPARISON_GRAPHQL_QUERY.len() - 2])));
}

#[tokio::test]
async fn stops_graphql_reads_at_the_protected_reserve_until_reset() {
    let (gh, cli, _clock) = setup();
    gh.always(comparison(Some(rate_limit(500))));
    cli.get_pull_request_base_comparison(base_comparison(None)).await.unwrap();
    assert!(gh.args(0).last().unwrap().contains("rateLimit { cost limit remaining resetAt }"));
    let error = cli.get_pull_request_base_comparison(base_comparison(None)).await.unwrap_err();
    let GitHubPullRequestCliError::RateLimitPaused(paused) = error else {
        panic!("expected a pause, got {error:?}");
    };
    assert_eq!(paused.host, "github.com");
    assert_eq!(paused.retry_at, date_parse_millis(RESET_AT).unwrap());
    assert_eq!(gh.count(), 1);
}

#[tokio::test]
async fn lets_an_interactive_permission_read_use_the_protected_reserve() {
    let (gh, cli, _clock) = setup();
    gh.once(comparison(Some(rate_limit(500)))).once(viewer_permissions("READ"));
    cli.get_pull_request_base_comparison(base_comparison(None)).await.unwrap();
    let access = cli
        .get_viewer_access(ViewerAccessInput {
            change_request: pr("github.com", 7),
            allow_reserve: Some(true),
        })
        .await
        .unwrap();
    assert_eq!(gh.count(), 2);
    assert_eq!(serde_json::to_value(&access).unwrap(), read_only_author());
}

// ---------------------------------------------------------------------------------------------
// Reviewers and labels
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn asks_github_to_review_naming_the_collection_a_request_is_added_to() {
    let (gh, cli, _clock) = setup();
    gh.always(ok("{}"));
    cli.set_reviewer_request(SetReviewerRequestInput {
        change_request: pr("github.com", 7),
        reviewers: vec![
            ReviewerRef {
                id: "octocat".into(),
                kind: PullRequestReviewerKind::User,
            },
            ReviewerRef {
                id: "reviewers".into(),
                kind: PullRequestReviewerKind::Team,
            },
        ],
        requested: true,
    })
    .await
    .unwrap();
    assert_eq!(
        gh.args(0),
        strings(&[
            "api",
            "--method",
            "POST",
            "--hostname",
            "github.com",
            "repos/acme/web/pulls/7/requested_reviewers",
            "--input",
            "-"
        ])
    );
    assert_eq!(gh.body(0), json!({"reviewers": ["octocat"], "team_reviewers": ["reviewers"]}));
}

#[tokio::test]
async fn takes_a_request_back_by_deleting_from_the_same_collection_it_was_added_to() {
    let (gh, cli, _clock) = setup();
    gh.always(ok("{}"));
    cli.set_reviewer_request(SetReviewerRequestInput {
        change_request: pr("github.com", 7),
        reviewers: vec![ReviewerRef {
            id: "octocat".into(),
            kind: PullRequestReviewerKind::User,
        }],
        requested: false,
    })
    .await
    .unwrap();
    assert!(gh.args(0).contains(&"DELETE".to_owned()));
    assert!(gh.args(0).contains(&"repos/acme/web/pulls/7/requested_reviewers".to_owned()));
    assert_eq!(gh.body(0), json!({"reviewers": ["octocat"], "team_reviewers": []}));
}

#[tokio::test]
async fn reads_who_may_review_and_who_already_has_in_one_request() {
    let (gh, cli, _clock) = setup();
    gh.always(json(json!({"data": {"repository": {
        "assignableUsers": {"pageInfo": {"hasNextPage": false}, "nodes": [{"login": "bilal"}, {"login": "octocat"}, {"login": "hubot"}]},
        "pullRequest": {"author": {"login": "bilal"}, "reviewRequests": {"nodes": [{"requestedReviewer": {"login": "octocat"}}]}},
    }}})));
    let list = cli.list_reviewer_candidates(pr("github.com", 7)).await.unwrap();
    assert_eq!(gh.count(), 1);
    assert!(gh.args(0).contains(&"number=7".to_owned()));
    let candidates: Vec<(String, bool)> = list
        .candidates
        .iter()
        .map(|candidate| (candidate.login.clone(), candidate.is_requested))
        .collect();
    assert_eq!(candidates, [("octocat".to_owned(), true), ("hubot".to_owned(), false)]);
}

#[tokio::test]
async fn puts_labels_on_by_posting_to_the_issues_own_collection_all_at_once() {
    let (gh, cli, _clock) = setup();
    gh.always(ok("[]"));
    cli.set_labels(SetLabelsInput {
        change_request: pr("github.com", 7),
        labels: strings(&["bug", "size:XL"]),
        applied: true,
    })
    .await
    .unwrap();
    assert_eq!(gh.count(), 1);
    assert_eq!(
        gh.args(0),
        strings(&[
            "api",
            "--method",
            "POST",
            "--hostname",
            "github.com",
            "repos/acme/web/issues/7/labels",
            "--input",
            "-"
        ])
    );
    assert_eq!(gh.body(0), json!({"labels": ["bug", "size:XL"]}));
}

#[tokio::test]
async fn takes_labels_off_one_at_a_time_naming_each_in_the_path_encoded() {
    let (gh, cli, _clock) = setup();
    gh.always(ok("[]"));
    cli.set_labels(SetLabelsInput {
        change_request: pr("github.com", 7),
        labels: strings(&["good first issue", "area/web"]),
        applied: false,
    })
    .await
    .unwrap();
    assert_eq!(gh.count(), 2);
    assert!(gh.args(0).contains(&"repos/acme/web/issues/7/labels/good%20first%20issue".to_owned()));
    assert!(gh.args(0).contains(&"DELETE".to_owned()));
    assert!(gh.args(1).contains(&"repos/acme/web/issues/7/labels/area%2Fweb".to_owned()));
}

// ---------------------------------------------------------------------------------------------
// Files viewed and node ids
// ---------------------------------------------------------------------------------------------

fn files_viewed_page(index: usize, has_next_page: bool) -> Reply {
    json(json!({"data": {"repository": {"pullRequest": {"files": {
        "pageInfo": {"hasNextPage": has_next_page, "endCursor": format!("cursor-{index}")},
        "nodes": [
            {"path": format!("src/file{index}.ts"), "viewerViewedState": "VIEWED"},
            {"path": format!("src/other{index}.ts"), "viewerViewedState": "UNVIEWED"},
        ],
    }}}}}))
}

#[tokio::test]
async fn reads_every_page_of_viewed_files_and_says_so_when_there_are_too_many() {
    let (gh, cli, _clock) = setup();
    gh.once(files_viewed_page(0, true))
        .once(files_viewed_page(1, true))
        .once(files_viewed_page(2, false));
    let viewed = cli.get_pull_request_files_viewed(pr("github.com", 7)).await.unwrap();
    assert_eq!(gh.count(), 3);
    // The first page asks from the start; each one after it carries the cursor before it.
    assert!(!gh.args(0).iter().any(|arg| arg.starts_with("after=")));
    assert!(gh.args(1).contains(&"after=cursor-0".to_owned()));
    assert!(gh.args(2).contains(&"after=cursor-1".to_owned()));
    assert!(!viewed.truncated);
    let files: Vec<(String, PullRequestFileViewedState)> = viewed.files.iter().map(|file| (file.path.clone(), file.state)).collect();
    let mut expected = Vec::new();
    for index in 0..3 {
        expected.push((format!("src/file{index}.ts"), PullRequestFileViewedState::Viewed));
        expected.push((format!("src/other{index}.ts"), PullRequestFileViewedState::Unviewed));
    }
    assert_eq!(files, expected);
}

#[tokio::test]
async fn stops_paging_viewed_files_rather_than_following_a_change_without_end() {
    let (gh, cli, _clock) = setup();
    gh.always(json(json!({"data": {"repository": {"pullRequest": {"files": {
        "pageInfo": {"hasNextPage": true, "endCursor": "cursor"},
        "nodes": [{"path": "src/file.ts", "viewerViewedState": "VIEWED"}],
    }}}}})));
    let viewed = cli.get_pull_request_files_viewed(pr("github.com", 7)).await.unwrap();
    assert_eq!(gh.count(), 5);
    assert!(viewed.truncated);
    assert_eq!(viewed.files.len(), 5);
}

#[tokio::test]
async fn clears_and_restores_a_burst_of_files_in_one_request() {
    let (gh, cli, _clock) = setup();
    gh.once(node_id("PR_1")).once(ok("{}"));
    cli.set_pull_request_files_viewed(set_viewed(23, &[("src/a.ts", true), ("src/b.ts", false)]))
        .await
        .unwrap();
    // One request to learn the pull request's node id, one for every press together.
    assert_eq!(gh.count(), 2);
    let sent = gh.body(1);
    assert!(sent["query"].as_str().unwrap().contains("f0: markFileAsViewed"));
    assert!(sent["query"].as_str().unwrap().contains("f1: unmarkFileAsViewed"));
    assert_eq!(sent["variables"], json!({"pullRequestId": "PR_1", "path0": "src/a.ts", "path1": "src/b.ts"}));
}

#[tokio::test]
async fn asks_the_host_nothing_when_nothing_was_pressed() {
    let (gh, cli, _clock) = setup();
    cli.set_pull_request_files_viewed(set_viewed(7, &[])).await.unwrap();
    assert_eq!(gh.count(), 0);
}

#[tokio::test]
async fn looks_a_pull_requests_node_id_up_once_however_often_it_is_written_to() {
    let (gh, cli, _clock) = setup();
    gh.once(node_id("PR_24"));
    gh.always(ok("{}"));
    cli.set_pull_request_files_viewed(set_viewed(24, &[("src/a.ts", true)])).await.unwrap();
    cli.set_pull_request_files_viewed(set_viewed(24, &[("src/b.ts", true)])).await.unwrap();
    cli.update_pull_request(UpdateChangeRequestInput {
        change_request: pr("github.com", 24),
        title: Some("Ticked through".into()),
        body: None,
    })
    .await
    .unwrap();
    // One lookup, then a mutation per write, every one of them addressed by the id it answered.
    assert_eq!(gh.count(), 4);
    assert!(gh.args(0).contains(&"number=24".to_owned()));
    let ids: Vec<Value> = (1..=3).map(|index| gh.body(index)["variables"]["pullRequestId"].clone()).collect();
    assert_eq!(ids, [json!("PR_24"), json!("PR_24"), json!("PR_24")]);
}

#[tokio::test]
async fn does_not_remember_a_node_id_lookup_that_failed() {
    let (gh, cli, _clock) = setup();
    gh.once(ok(r#"{"message":"not found"}"#)).once(node_id("PR_25")).once(ok("{}"));
    let write = || cli.set_pull_request_files_viewed(set_viewed(25, &[("src/a.ts", true)]));
    let error = write().await.unwrap_err();
    assert_eq!(error.tag(), "GitHubPullRequestReadError");
    write().await.unwrap();
    assert_eq!(gh.count(), 3);
    assert_eq!(gh.body(2)["variables"]["pullRequestId"], json!("PR_25"));
}

#[tokio::test]
async fn keeps_the_pull_request_being_ticked_through_not_the_one_looked_up_first() {
    let (gh, cli, _clock) = setup();
    const HOT: i64 = 9_000;
    let lookups: Arc<Mutex<HashMap<i64, usize>>> = Arc::default();
    let counted = lookups.clone();
    gh.respond(move |input| {
        let Some(asked) = input.args.iter().find_map(|arg| arg.strip_prefix("number=")) else {
            return ok("{}");
        };
        let number: i64 = asked.parse().unwrap();
        *counted.lock().unwrap().entry(number).or_default() += 1;
        node_id(&format!("PR_{number}"))
    });
    let tick = |number: i64| cli.set_pull_request_files_viewed(set_viewed(number, &[("src/a.ts", true)]));
    tick(HOT).await.unwrap();
    // A cache's worth of cold pull requests, with the open one pressed in between each of them.
    for filled in 0..NODE_ID_CACHE_CAPACITY as i64 {
        tick(HOT + 1 + filled).await.unwrap();
        tick(HOT).await.unwrap();
    }
    assert_eq!(lookups.lock().unwrap().get(&HOT), Some(&1));
}
