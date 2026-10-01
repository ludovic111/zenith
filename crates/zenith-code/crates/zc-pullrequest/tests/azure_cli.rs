//! Port of `AzureDevOpsPullRequestCli.test.ts`: the CLI (and the provider over it) against a
//! scripted `az`, through the real `AzureDevOpsCli` and `VcsProcess`.

#![allow(clippy::result_large_err)]

mod support_azure;

use std::sync::Arc;

use serde_json::{json, Value};
use support_azure::*;
use zc_contracts::{
    PullRequestAction, PullRequestEditCapabilities, PullRequestInvolvement, PullRequestListState, PullRequestMergeMethod, PullRequestViewedFilesStore,
};
use zc_pullrequest::azure::cli::{AzureDevOpsPullRequestCli, AzureDevOpsPullRequestCliApi, ListPullRequestsInput};
use zc_pullrequest::azure::json::AzureDevOpsRepositoryLocation;
use zc_pullrequest::azure::AzureDevOpsPullRequestProvider;
use zc_pullrequest::provider::*;

fn cli(runner: &Arc<QueuedRunner>) -> AzureDevOpsPullRequestCli {
    AzureDevOpsPullRequestCli::new(runner.cli())
}

fn provider(runner: &Arc<QueuedRunner>) -> AzureDevOpsPullRequestProvider {
    AzureDevOpsPullRequestProvider::new(runner.cli())
}

fn location() -> AzureDevOpsRepositoryLocation {
    AzureDevOpsRepositoryLocation {
        project: "platform".into(),
        repository: "web".into(),
    }
}

fn change_request(number: i64) -> ChangeRequestRef {
    ChangeRequestRef {
        cwd: "/w".into(),
        repository: "web".into(),
        host: "dev.azure.com".into(),
        number,
    }
}

fn pull_request_row() -> Value {
    json!({
        "pullRequestId": 42,
        "title": "Add the page",
        "status": "active",
        "sourceRefName": "refs/heads/feat/page",
        "targetRefName": "refs/heads/main",
        "creationDate": "2026-07-01T00:00:00Z",
        "url": "https://dev.azure.com/acme/_apis/git/repositories/web/pullRequests/42",
        "repository": {"name": "web", "project": {"name": "platform"}},
    })
}

fn one_iteration() -> Value {
    json!({"value": [{"id": 1, "sourceRefCommit": {"commitId": "a".repeat(40)}, "commonRefCommit": {"commitId": "b".repeat(40)}}]})
}

fn pull_request_rows(count: usize, first_number: usize) -> Vec<Value> {
    (0..count)
        .map(|index| {
            let number = first_number + index;
            json!({
                "pullRequestId": number,
                "title": format!("Pull request {number}"),
                "status": "active",
                "sourceRefName": "refs/heads/feat/page",
                "targetRefName": "refs/heads/main",
                "creationDate": "2026-07-01T00:00:00Z",
                "repository": {"name": "web", "project": {"name": "platform"}},
                "url": format!("https://dev.azure.com/acme/_apis/git/repositories/web/pullRequests/{number}"),
            })
        })
        .collect()
}

fn pull_requests(count: usize, first_number: usize) -> String {
    Value::Array(pull_request_rows(count, first_number)).to_string()
}

fn list_input(state: PullRequestListState, involvement: PullRequestInvolvement, limit: i64) -> ListPullRequestsInput {
    ListPullRequestsInput {
        cwd: "/w".into(),
        repository: "web".into(),
        state,
        involvement,
        viewer: "sam@example.test".into(),
        limit,
        cursor: None,
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

fn arg_after(args: &[String], flag: &str) -> String {
    args[args.iter().position(|arg| arg == flag).unwrap() + 1].clone()
}

/// A page of change entries the size Azure really answers with.
fn change_entries(count: usize) -> Vec<Value> {
    let commit = "c".repeat(40);
    (0..count)
        .map(|index| {
            let path = format!("/apps/server/src/generated/module-{index}/persisted-projection-{index}.ts");
            json!({"changeType": "edit", "item": {
                "path": path, "objectId": "a".repeat(40), "originalObjectId": "b".repeat(40), "commitId": commit, "gitObjectType": "blob",
                "url": format!("https://dev.azure.com/acme/platform/_apis/git/repositories/6f9c9b7f-0000-0000-0000-000000000000/items{path}?versionType=Commit&version={commit}"),
            }})
        })
        .collect()
}

/// An Azure identity, which rides along with every comment and every push.
fn identity(name: &str) -> Value {
    let id = "6f9c9b7f-0000-0000-0000-000000000000";
    json!({
        "displayName": name, "id": id, "uniqueName": format!("{}@example.test", name.to_lowercase().replace(' ', ".")),
        "descriptor": format!("aad.{}", "z".repeat(52)),
        "imageUrl": format!("https://dev.azure.com/acme/_api/_common/identityImage?id={id}"),
        "url": format!("https://identities.example.test/A{id}/_apis/Identities/{id}"),
        "_links": {"avatar": {"href": format!("https://dev.azure.com/acme/_apis/GraphProfile/MemberAvatars/aad.{}", "z".repeat(52))}},
    })
}

fn thread_rows(count: usize) -> Vec<Value> {
    (0..count)
        .map(|index| {
            json!({
                "id": index + 1, "publishedDate": "2026-07-02T00:00:00Z", "lastUpdatedDate": "2026-07-02T00:00:00Z", "status": "active",
                "threadContext": {"filePath": format!("/apps/server/src/generated/module-{index}.ts")},
                "identities": {"1": identity("Reviewer One")}, "isDeleted": false,
                "comments": [{
                    "id": 1, "parentCommentId": 0, "author": identity("Reviewer One"),
                    "content": format!("Comment {index}: {}", "this needs another look. ".repeat(20)),
                    "publishedDate": "2026-07-02T00:00:00Z", "lastUpdatedDate": "2026-07-02T00:00:00Z", "commentType": "text", "usersLiked": [],
                }],
            })
        })
        .collect()
}

fn iteration_rows(count: usize) -> Vec<Value> {
    (0..count)
        .map(|index| {
            json!({
                "id": index + 1, "description": format!("Pushed {index} commits"), "author": identity("Author One"),
                "createdDate": "2026-07-02T00:00:00Z", "updatedDate": "2026-07-02T00:00:00Z",
                "sourceRefCommit": {"commitId": format!("{index:a>40}")}, "targetRefCommit": {"commitId": "b".repeat(40)},
                "commonRefCommit": {"commitId": "c".repeat(40)}, "hasMultipleCommits": true, "reason": "push",
                "push": {"pushId": index + 1, "date": "2026-07-02T00:00:00Z", "pushedBy": identity("Author One")},
            })
        })
        .collect()
}

#[tokio::test]
async fn asks_for_one_row_more_than_the_page() {
    let runner = QueuedRunner::new();
    runner.once(pull_requests(3, 1));
    let batch = cli(&runner)
        .list_pull_requests(list_input(PullRequestListState::Open, PullRequestInvolvement::All, 10))
        .await
        .unwrap();
    assert_eq!(batch.items.len(), 3);
    assert!(!batch.truncated);
    assert_eq!(
        runner.args(0),
        strings(&[
            "repos",
            "pr",
            "list",
            "--detect",
            "true",
            "--repository",
            "web",
            "--status",
            "active",
            "--include-links",
            "--top",
            "11",
            "--only-show-errors",
            "--output",
            "json"
        ])
    );
}

#[tokio::test]
async fn reads_a_page_larger_than_the_vcs_default_output_limit() {
    let rows: Vec<Value> = pull_request_rows(100, 1)
        .into_iter()
        .map(|mut row| {
            row["description"] = json!("x".repeat(10_000));
            row
        })
        .collect();
    let response = Value::Array(rows).to_string();
    assert!(response.len() > VCS_DEFAULT_MAX_OUTPUT_BYTES);
    let runner = QueuedRunner::new();
    runner.once(response);
    let batch = cli(&runner)
        .list_pull_requests(list_input(PullRequestListState::Merged, PullRequestInvolvement::All, 99))
        .await
        .unwrap();
    assert_eq!(batch.items.len(), 99);
    assert!(batch.truncated);
}

#[tokio::test]
async fn reads_the_page_unnarrowed_when_asked_to_search() {
    let runner = QueuedRunner::new();
    runner.once(pull_requests(3, 1));
    let page = provider(&runner)
        .list_change_requests(ListChangeRequestsInput {
            cwd: "/w".into(),
            repository: "web".into(),
            host: "dev.azure.com".into(),
            state: PullRequestListState::Open,
            involvement: PullRequestInvolvement::All,
            viewer: "sam@example.test".into(),
            limit: 10,
            query: Some("page".into()),
            cursor: None,
            filters: None,
        })
        .await
        .unwrap();
    // Nothing of the search reaches the command, where it could only mean the wrong thing.
    assert_eq!(page.items.len(), 3);
    assert_eq!(
        runner.args(0),
        strings(&[
            "repos",
            "pr",
            "list",
            "--detect",
            "true",
            "--repository",
            "web",
            "--status",
            "active",
            "--include-links",
            "--top",
            "11",
            "--only-show-errors",
            "--output",
            "json"
        ])
    );
}

#[tokio::test]
async fn steps_over_what_it_has_already_handed_over() {
    let runner = QueuedRunner::new();
    runner.once(pull_requests(3, 1));
    let mut input = list_input(PullRequestListState::Open, PullRequestInvolvement::All, 10);
    input.cursor = Some(ProviderListCursor {
        updated_before: "2026-07-02T00:00:00Z".into(),
        delivered: 20,
    });
    cli(&runner).list_pull_requests(input).await.unwrap();
    let args = runner.args(0);
    assert_eq!(arg_after(&args, "--skip"), "20");
    assert!(!args.contains(&"2026-07-02T00:00:00Z".to_owned()));
}

#[tokio::test]
async fn reports_truncation_from_the_extra_row() {
    let runner = QueuedRunner::new();
    runner.once(pull_requests(11, 1));
    let batch = cli(&runner)
        .list_pull_requests(list_input(PullRequestListState::Open, PullRequestInvolvement::All, 10))
        .await
        .unwrap();
    assert_eq!(batch.items.len(), 10);
    assert!(batch.truncated);
    assert_eq!(batch.cursor_advance, 10);
}

#[tokio::test]
async fn advances_by_malformed_raw_rows_and_keeps_reading() {
    let runner = QueuedRunner::new();
    runner
        .once(json!([{"pullRequestId": "malformed"}, pull_request_rows(1, 1)[0], {"pullRequestId": "also malformed"}]).to_string())
        .once(pull_requests(2, 2));
    let batch = cli(&runner)
        .list_pull_requests(list_input(PullRequestListState::Open, PullRequestInvolvement::All, 2))
        .await
        .unwrap();
    assert_eq!(batch.items.iter().map(|item| item.number).collect::<Vec<_>>(), [1, 2]);
    assert!(batch.truncated);
    // Three raw rows from the first request and one from the second produced this page.
    assert_eq!(batch.cursor_advance, 4);
    let second = runner.args(1);
    assert_eq!(arg_after(&second, "--skip"), "3");
    assert_eq!(arg_after(&second, "--top"), "2");
}

#[tokio::test]
async fn narrows_to_the_author_on_the_authored_tab() {
    let runner = QueuedRunner::new();
    runner.once("[]");
    cli(&runner)
        .list_pull_requests(list_input(PullRequestListState::Closed, PullRequestInvolvement::Authored, 10))
        .await
        .unwrap();
    let args = runner.args(0);
    assert!(args.contains(&"--creator".to_owned()));
    assert!(args.contains(&"sam@example.test".to_owned()));
    // Azure calls a closed pull request abandoned.
    assert!(args.contains(&"abandoned".to_owned()));
}

#[tokio::test]
async fn asks_azure_for_every_status_on_the_all_tab() {
    let runner = QueuedRunner::new();
    runner.once("[]");
    cli(&runner)
        .list_pull_requests(list_input(PullRequestListState::All, PullRequestInvolvement::All, 10))
        .await
        .unwrap();
    assert_eq!(arg_after(&runner.args(0), "--status"), "all");
}

#[tokio::test]
async fn narrows_to_the_reviewer_on_the_reviewing_tab() {
    let runner = QueuedRunner::new();
    runner.once("[]");
    cli(&runner)
        .list_pull_requests(list_input(PullRequestListState::Open, PullRequestInvolvement::Reviewing, 10))
        .await
        .unwrap();
    assert!(runner.args(0).contains(&"--reviewer".to_owned()));
}

#[tokio::test]
async fn reads_the_signed_in_account_which_az_reports_as_a_bare_value() {
    let runner = QueuedRunner::new();
    runner.once(json!({"name": "sam@example.test", "type": "user"}).to_string());
    let viewer = cli(&runner).get_viewer("/w").await.unwrap();
    assert_eq!(viewer, "sam@example.test");
    assert_eq!(
        runner.args(0),
        strings(&["account", "show", "--query", "user", "--only-show-errors", "--output", "json"])
    );
}

#[tokio::test]
async fn fails_when_nobody_is_signed_in() {
    let runner = QueuedRunner::new();
    runner.once("");
    let error = cli(&runner).get_viewer("/w").await.unwrap_err();
    assert_eq!(error.tag(), "AzureDevOpsViewerUnavailableError");
}

fn update_args(rest: &[&str]) -> Vec<String> {
    let mut args = strings(&["repos", "pr", "update", "--detect", "true", "--id", "42"]);
    args.extend(strings(rest));
    args.extend(strings(&["--only-show-errors", "--output", "json"]));
    args
}

#[tokio::test]
async fn completes_a_pull_request_to_merge_it_squashing_only_when_asked() {
    let runner = QueuedRunner::new();
    runner.always(|_| output("{}"));
    cli(&runner)
        .run_pull_request_action("/w", 42, PullRequestAction::Merge, Some(PullRequestMergeMethod::Squash))
        .await
        .unwrap();
    assert_eq!(runner.args(0), update_args(&["--status", "completed", "--squash", "true"]));
}

#[tokio::test]
async fn stores_the_squash_choice_with_an_auto_completion() {
    let runner = QueuedRunner::new();
    runner.always(|_| output("{}"));
    cli(&runner)
        .run_pull_request_action("/w", 42, PullRequestAction::EnableAutoMerge, Some(PullRequestMergeMethod::Squash))
        .await
        .unwrap();
    assert_eq!(runner.args(0), update_args(&["--auto-complete", "true", "--squash", "true"]));
}

#[tokio::test]
async fn moves_a_pull_request_with_each_action() {
    for (action, expected) in [
        (PullRequestAction::EnableAutoMerge, ["--auto-complete", "true"]),
        (PullRequestAction::DisableAutoMerge, ["--auto-complete", "false"]),
        (PullRequestAction::Draft, ["--draft", "true"]),
        (PullRequestAction::Ready, ["--draft", "false"]),
        (PullRequestAction::Close, ["--status", "abandoned"]),
        (PullRequestAction::Reopen, ["--status", "active"]),
    ] {
        let runner = QueuedRunner::new();
        runner.always(|_| output("{}"));
        cli(&runner).run_pull_request_action("/w", 42, action, None).await.unwrap();
        assert_eq!(runner.args(0), update_args(&expected), "{action:?}");
    }
}

#[tokio::test]
async fn rewrites_sending_nothing_it_was_not_given() {
    let cases: [(Option<&str>, Option<&str>, Vec<&str>); 3] = [
        (Some("Add the page"), None, vec!["--title=Add the page"]),
        (None, Some("Why the page changed"), vec!["--description=Why the page changed"]),
        (
            Some("Add the page"),
            Some("Why the page changed"),
            vec!["--title=Add the page", "--description=Why the page changed"],
        ),
    ];
    for (title, body, expected) in cases {
        let runner = QueuedRunner::new();
        runner.always(|_| output("{}"));
        cli(&runner).update_pull_request("/w", 42, title, body).await.unwrap();
        assert_eq!(runner.args(0), update_args(&expected));
    }
}

#[tokio::test]
async fn sends_a_description_that_starts_with_a_dash_as_one_value() {
    let runner = QueuedRunner::new();
    runner.always(|_| output("{}"));
    cli(&runner)
        .update_pull_request("/w", 42, None, Some("- rewrote the page\n- kept the rest"))
        .await
        .unwrap();
    assert!(runner.args(0).contains(&"--description=- rewrote the page\n- kept the rest".to_owned()));
}

#[tokio::test]
async fn rewrites_through_the_provider_which_says_it_takes_one() {
    let runner = QueuedRunner::new();
    runner.always(|_| output("{}"));
    let provider = provider(&runner);
    assert_eq!(
        provider.capabilities().edit,
        Some(PullRequestEditCapabilities {
            change_request: true,
            comment: false
        })
    );
    assert!(provider.optional_methods().update_change_request);
    provider
        .update_change_request(UpdateChangeRequestInput {
            change_request: change_request(42),
            title: Some("Add the page".into()),
            body: None,
        })
        .await
        .unwrap();
    let args = runner.args(0);
    assert!(args.contains(&"--title=Add the page".to_owned()));
    assert!(!args.iter().any(|arg| arg.starts_with("--description")));
}

fn revisions(input_paths: &[&str]) -> FileRevisionsInput {
    FileRevisionsInput {
        change_request: change_request(42),
        paths: input_paths.iter().map(|path| (*path).to_owned()).collect(),
    }
}

fn pairs(revisions: &ProviderFileRevisions) -> Vec<(&str, &str)> {
    revisions.revisions.iter().map(|(path, revision)| (path.as_str(), revision.as_str())).collect()
}

#[tokio::test]
async fn names_the_heads_blob_as_what_a_cleared_file_was_cleared_at() {
    let runner = QueuedRunner::new();
    runner
        .once(pull_request_row().to_string())
        .once(
            json!({"value": [
                {"id": 1, "sourceRefCommit": {"commitId": "a".repeat(40)}, "commonRefCommit": {"commitId": "b".repeat(40)}},
                {"id": 2, "sourceRefCommit": {"commitId": "c".repeat(40)}, "commonRefCommit": {"commitId": "b".repeat(40)}},
            ]})
            .to_string(),
        )
        .once(
            json!({"changeEntries": [
                {"changeType": "edit", "item": {"path": "/README.md", "objectId": "8f80"}},
                {"changeType": "add", "item": {"path": "/DEMO.md", "objectId": "0ca4"}},
            ]})
            .to_string(),
        );
    let provider = provider(&runner);
    // Kept here rather than on Azure, so the marks need a revision of their own.
    assert_eq!(provider.capabilities().viewed_files, Some(PullRequestViewedFilesStore::Environment));
    assert!(provider.optional_methods().get_file_revisions);
    let answer = provider.get_file_revisions(revisions(&["README.md"])).await.unwrap();
    // The latest push, since an iteration's changes are reported against the merge base.
    assert!(runner.args(2).contains(&"iterationId=2".to_owned()));
    // Only what was asked for.
    assert_eq!(pairs(&answer), [("README.md", "8f80")]);
}

#[tokio::test]
async fn reads_where_a_pull_request_lives_once() {
    let iterations = one_iteration().to_string();
    let changes = json!({"changeEntries": [{"changeType": "edit", "item": {"path": "/README.md", "objectId": "8f80"}}]}).to_string();
    let runner = QueuedRunner::new();
    runner
        .once(pull_request_row().to_string())
        .once(iterations.clone())
        .once(changes.clone())
        .once(iterations)
        .once(changes);
    let provider = provider(&runner);
    provider.get_file_revisions(revisions(&["README.md"])).await.unwrap();
    let again = provider.get_file_revisions(revisions(&["README.md"])).await.unwrap();
    assert_eq!(runner.count(), 5);
    // The second read goes straight to the pushes.
    assert!(runner.args(3).contains(&"pullRequestIterations".to_owned()));
    assert_eq!(pairs(&again), [("README.md", "8f80")]);
}

#[tokio::test]
async fn answers_for_a_marked_file_the_pull_request_no_longer_changes_as_empty() {
    let runner = QueuedRunner::new();
    runner
        .once(pull_request_row().to_string())
        .once(one_iteration().to_string())
        .once(r#"{"changeEntries":[]}"#);
    let answer = provider(&runner).get_file_revisions(revisions(&["GONE.md"])).await.unwrap();
    assert_eq!(pairs(&answer), [("GONE.md", "")]);
}

fn edit_entries(from: usize, count: usize) -> Vec<Value> {
    (0..count)
        .map(|index| json!({"changeType": "edit", "item": {"path": format!("/src/f{}.ts", from + index), "objectId": format!("blob-{}", from + index)}}))
        .collect()
}

#[tokio::test]
async fn says_nothing_about_the_files_past_the_end_of_a_change_it_gave_up_following() {
    let runner = QueuedRunner::new();
    runner
        .once(pull_request_row().to_string())
        .once(one_iteration().to_string())
        .once(json!({"changeEntries": edit_entries(0, 5_000), "nextSkip": 5_000}).to_string())
        .once(json!({"changeEntries": edit_entries(5_000, 5_000), "nextSkip": 10_000}).to_string());
    let answer = provider(&runner)
        .get_file_revisions(revisions(&["src/f1.ts", "src/f5001.ts", "src/past-the-cut.ts"]))
        .await
        .unwrap();
    // The second page picks up where the first said it ended.
    assert!(runner.args(3).contains(&"$skip=5000".to_owned()));
    assert_eq!(pairs(&answer), [("src/f1.ts", "blob-1"), ("src/f5001.ts", "blob-5001")]);
}

#[tokio::test]
async fn stops_following_pages_by_what_azure_counts() {
    let runner = QueuedRunner::new();
    runner.once(pull_request_row().to_string()).once(one_iteration().to_string()).always(|input| {
        // Nothing a review can show, so every page decodes to nothing at all.
        let skip = input
            .args
            .iter()
            .find_map(|arg| arg.strip_prefix("$skip="))
            .map_or(0, |skip| skip.parse::<i64>().unwrap());
        output(json!({"changeEntries": [{"changeType": "add", "item": {"path": "/src", "isFolder": true}}], "nextSkip": skip + 2_000}).to_string())
    });
    let answer = provider(&runner).get_file_revisions(revisions(&["src/page.ts"])).await.unwrap();
    // The pull request, its pushes, and five pages.
    assert_eq!(runner.count(), 7);
    // It read part of a change, so it says nothing about the file it never saw.
    assert!(answer.revisions.is_empty());
}

fn diff_input(number: i64) -> GetDiffInput {
    GetDiffInput {
        change_request: change_request(number),
        cursor: None,
        commit: None,
    }
}

#[tokio::test]
async fn leaves_one_file_the_host_would_not_hand_over_listed_without_its_hunks() {
    let runner = QueuedRunner::new();
    runner
        .once(pull_request_row().to_string())
        .once(one_iteration().to_string())
        .once(
            json!({"changeEntries": [
                {"changeType": "add", "item": {"path": "/huge.bin", "objectId": "8f80"}},
                {"changeType": "add", "item": {"path": "/DEMO.md", "objectId": "0ca4"}},
            ]})
            .to_string(),
        )
        // Both files are read at once, so the answer goes by path rather than by order.
        .always(|input| {
            if input.args.iter().any(|arg| arg == "path=/huge.bin") {
                failure("the blob is past what the route will carry")
            } else {
                output(json!({"content": "hello\n"}).to_string())
            }
        });
    let slice = provider(&runner).get_diff(diff_input(42)).await.unwrap();
    assert!(slice.truncated);
    assert!(slice.patch.contains("diff --git a/huge.bin b/huge.bin"));
    // The file behind it still renders.
    assert!(slice.patch.contains("+hello"));
}

#[tokio::test]
async fn fails_the_whole_read_when_it_is_the_connection_that_would_not_answer() {
    let runner = QueuedRunner::new();
    runner
        .once(pull_request_row().to_string())
        .once(one_iteration().to_string())
        .once(json!({"changeEntries": [{"changeType": "add", "item": {"path": "/DEMO.md", "objectId": "0ca4"}}]}).to_string())
        .once_with(|_| failure("HTTP 429: Too Many Requests"));
    let error = provider(&runner).get_diff(diff_input(42)).await.unwrap_err();
    assert_eq!(error.reason, ProviderFailureReason::RateLimited);
}

#[tokio::test]
async fn takes_azures_own_word_on_a_file_it_will_not_spell_out() {
    let runner = QueuedRunner::new();
    runner
        .once(pull_request_row().to_string())
        .once(one_iteration().to_string())
        .once(json!({"changeEntries": [{"changeType": "add", "item": {"path": "/logo.png", "objectId": "8f80"}}]}).to_string())
        .once(json!({"content": "iVBORw0KGgo=", "contentMetadata": {"isBinary": true}}).to_string());
    let slice = provider(&runner).get_diff(diff_input(42)).await.unwrap();
    // Without the metadata every file reads as text however it was stored.
    assert!(runner.args(3).contains(&"includeContentMetadata=true".to_owned()));
    assert!(slice.patch.contains("Binary files a/logo.png and b/logo.png differ"));
    assert!(slice.truncated);
}

#[tokio::test]
async fn stops_following_pages_when_one_does_not_move_the_cursor_on() {
    let runner = QueuedRunner::new();
    runner
        .once(pull_request_row().to_string())
        .once(one_iteration().to_string())
        .once(json!({"changeEntries": [{"changeType": "edit", "item": {"path": "/a.ts", "objectId": "8f80"}}], "nextSkip": 2_000}).to_string())
        .once(json!({"changeEntries": [{"changeType": "edit", "item": {"path": "/b.ts", "objectId": "0ca4"}}], "nextSkip": 2_000}).to_string());
    let answer = provider(&runner).get_file_revisions(revisions(&["a.ts", "b.ts", "unlisted.ts"])).await.unwrap();
    // Four reads and no more.
    assert_eq!(runner.count(), 4);
    assert_eq!(pairs(&answer), [("a.ts", "8f80"), ("b.ts", "0ca4")]);
}

#[tokio::test]
async fn asks_azure_nothing_when_no_file_has_been_ticked_off() {
    let runner = QueuedRunner::new();
    runner.always(|_| output(r#"{"changeEntries":[]}"#));
    let answer = provider(&runner).get_file_revisions(revisions(&[])).await.unwrap();
    assert!(answer.revisions.is_empty());
    assert_eq!(runner.count(), 0);
}

#[tokio::test]
async fn reads_the_conversation_through_the_rest_api_pinned_to_a_version() {
    let runner = QueuedRunner::new();
    runner.once(json!({"value": [{"id": 1, "comments": [{"id": 1, "content": "Looks good.", "publishedDate": "2026-07-02T00:00:00Z"}]}]}).to_string());
    let comments = cli(&runner).list_threads("/w", &location(), 42).await.unwrap();
    assert_eq!(comments.len(), 1);
    // `az devops invoke` rather than `az rest`.
    let args = runner.args(0);
    for expected in ["invoke", "pullRequestThreads", "project=platform", "repositoryId=web", "pullRequestId=42"] {
        assert!(args.contains(&expected.to_owned()), "{expected}");
    }
    assert_eq!(
        args,
        strings(&[
            "devops",
            "invoke",
            "--detect",
            "true",
            "--area",
            "git",
            "--resource",
            "pullRequestThreads",
            "--api-version",
            "7.1",
            "--route-parameters",
            "project=platform",
            "repositoryId=web",
            "pullRequestId=42",
            "--only-show-errors",
            "--output",
            "json"
        ])
    );
    // This route does not page, so the read asks for more than the process default.
    assert!(runner.max_output_bytes(0).unwrap() > VCS_DEFAULT_MAX_OUTPUT_BYTES);
}

#[tokio::test]
async fn reads_a_long_reviews_threads_past_the_default_output_limit() {
    let response = json!({"value": thread_rows(800)}).to_string();
    assert!(response.len() > VCS_DEFAULT_MAX_OUTPUT_BYTES);
    let runner = QueuedRunner::new();
    runner.once(response);
    let comments = cli(&runner).list_threads("/w", &location(), 42).await.unwrap();
    assert_eq!(comments.len(), 800);
}

#[tokio::test]
async fn reads_a_long_reviews_iterations_past_the_default_output_limit() {
    let response = json!({"value": iteration_rows(1_200)}).to_string();
    assert!(response.len() > VCS_DEFAULT_MAX_OUTPUT_BYTES);
    let runner = QueuedRunner::new();
    runner.once(response);
    let iterations = cli(&runner).list_iterations("/w", &location(), 42).await.unwrap();
    assert_eq!(iterations.len(), 1_200);
    assert_eq!(iterations.last().unwrap().id, 1_200);
}

#[tokio::test]
async fn reads_a_full_page_of_change_entries_past_the_default_output_limit() {
    let response = json!({"changeEntries": change_entries(2_000)}).to_string();
    assert!(response.len() > VCS_DEFAULT_MAX_OUTPUT_BYTES);
    let runner = QueuedRunner::new();
    runner.once(response);
    let page = cli(&runner).list_iteration_changes("/w", &location(), 42, 1).await.unwrap();
    assert_eq!(page.changes.len(), 2_000);
    assert!(!page.truncated);
    assert_eq!(
        runner.args(0),
        strings(&[
            "devops",
            "invoke",
            "--detect",
            "true",
            "--area",
            "git",
            "--resource",
            "pullRequestIterationChanges",
            "--api-version",
            "7.1",
            "--route-parameters",
            "project=platform",
            "repositoryId=web",
            "pullRequestId=42",
            "iterationId=1",
            "--query-parameters",
            "$top=2000",
            "$skip=0",
            "--only-show-errors",
            "--output",
            "json"
        ])
    );
}

#[tokio::test]
async fn reads_a_file_whose_json_envelope_is_past_the_default_output_limit() {
    let file = "const value = 1;\n".repeat(57_000);
    let response = json!({"content": file}).to_string();
    assert!(file.len() < VCS_DEFAULT_MAX_OUTPUT_BYTES);
    assert!(response.len() > VCS_DEFAULT_MAX_OUTPUT_BYTES);
    let runner = QueuedRunner::new();
    runner.once(response);
    let item = cli(&runner)
        .read_item_content("/w", &location(), "src/generated/schema.ts", &"a".repeat(40))
        .await
        .unwrap();
    assert_eq!(item.contents, file);
    assert!(!item.is_binary);
}

#[tokio::test]
async fn asks_for_a_file_by_azures_own_spelling_of_its_path() {
    let runner = QueuedRunner::new();
    runner.once(json!({"content": "const a = 1;"}).to_string());
    cli(&runner).read_item_content("/w", &location(), "src/app.ts", &"a".repeat(40)).await.unwrap();
    assert_eq!(
        runner.args(0),
        strings(&[
            "devops",
            "invoke",
            "--detect",
            "true",
            "--area",
            "git",
            "--resource",
            "items",
            "--api-version",
            "7.1",
            "--route-parameters",
            "project=platform",
            "repositoryId=web",
            "--query-parameters",
            "path=/src/app.ts",
            "versionDescriptor.versionType=commit",
            &format!("versionDescriptor.version={}", "a".repeat(40)),
            "includeContent=true",
            "includeContentMetadata=true",
            "$format=json",
            "--only-show-errors",
            "--output",
            "json"
        ])
    );
}

#[tokio::test]
async fn reports_a_pull_request_it_cannot_place_as_its_own_outcome() {
    let runner = QueuedRunner::new();
    runner.once(
        json!({"pullRequestId": 42, "title": "Add the page", "sourceRefName": "refs/heads/feat/page", "targetRefName": "refs/heads/main", "creationDate": "2026-07-01T00:00:00Z"})
            .to_string(),
    );
    let error = cli(&runner).get_pull_request("/w", 42).await.unwrap_err();
    assert_eq!(error.tag(), "AzureDevOpsPullRequestIncompleteError");
}

#[tokio::test]
async fn fails_the_read_when_az_returns_something_unreadable() {
    let runner = QueuedRunner::new();
    runner.once(r#"{"message":"not found"}"#);
    let error = cli(&runner).get_pull_request("/w", 42).await.unwrap_err();
    assert_eq!(error.tag(), "AzureDevOpsPullRequestReadError");
    assert_eq!(
        error.message(),
        "Azure CLI failed in getPullRequest: Azure CLI returned an unreadable getPullRequest response."
    );
}

#[tokio::test]
async fn adds_reviewers_with_the_one_command_azure_has_for_it() {
    let runner = QueuedRunner::new();
    runner.once("[]");
    cli(&runner)
        .set_pull_request_reviewers("/w", 42, &["octocat@example.test".into(), "hubot@example.test".into()], true)
        .await
        .unwrap();
    assert_eq!(
        runner.args(0),
        strings(&[
            "repos",
            "pr",
            "reviewer",
            "add",
            "--detect",
            "true",
            "--id",
            "42",
            "--reviewers",
            "octocat@example.test",
            "hubot@example.test",
            "--only-show-errors",
            "--output",
            "json"
        ])
    );
}

#[tokio::test]
async fn takes_a_reviewer_off_with_the_same_commands_counterpart() {
    let runner = QueuedRunner::new();
    runner.once("[]");
    cli(&runner)
        .set_pull_request_reviewers("/w", 42, &["octocat@example.test".into()], false)
        .await
        .unwrap();
    assert!(runner.args(0).contains(&"remove".to_owned()));
}

#[tokio::test]
async fn refuses_a_reviewer_az_would_read_as_a_flag_before_running_anything() {
    let runner = QueuedRunner::new();
    let error = cli(&runner).set_pull_request_reviewers("/w", 42, &["--query".into()], true).await.unwrap_err();
    assert_eq!(error.tag(), "AzureDevOpsReviewerNameError");
    assert_eq!(runner.count(), 0);
}
