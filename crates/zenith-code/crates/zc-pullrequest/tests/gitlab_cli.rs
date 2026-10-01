//! `GitLabPullRequestCli.test.ts`: the `glab` invocations of the pull request feature, against a
//! scripted runner.

#![allow(clippy::result_large_err, clippy::too_many_arguments)]

mod support_gitlab;

use serde_json::{json, Value};
use support_gitlab::*;
use zc_contracts::{
    PullRequestAction, PullRequestDiffFileContentsInputChangeType as ChangeType, PullRequestDiffSide, PullRequestInvolvement, PullRequestListState,
    PullRequestMergeCapabilities, PullRequestMergeMethod, PullRequestReactionContent, PullRequestReviewCommentDraft, PullRequestReviewPosition,
    PullRequestReviewPositionAdded, PullRequestReviewPositionDeleted, PullRequestReviewVerdict,
};
use zc_pullrequest::gitlab::cli::{DiffFileContentsInput, FileContentsUnavailableReason, ListMergeRequestsInput, MergeRequestTarget};
use zc_pullrequest::gitlab::GitLabPullRequestCliError;
use zc_pullrequest::provider::ProviderListCursor;

const TARGET: MergeRequestTarget<'static> = MergeRequestTarget {
    cwd: "/w",
    repository: "acme/web",
    number: 7,
};

fn merge_requests(count: i64, first_number: i64) -> String {
    Value::Array(
        (0..count)
            .map(|index| {
                let number = first_number + index;
                json!({
                    "iid": number,
                    "title": format!("Merge request {number}"),
                    "web_url": format!("https://gitlab.example.test/acme/web/-/merge_requests/{number}"),
                    "source_branch": "feat/page",
                    "target_branch": "main",
                    "created_at": "2026-07-01T00:00:00Z",
                    "updated_at": "2026-07-02T00:00:00Z",
                })
            })
            .collect(),
    )
    .to_string()
}

/// A page of `/diffs` as GitLab serves it.
fn diff_page(first_index: i64, count: i64) -> String {
    Value::Array(
        (0..count)
            .map(|index| json!({"old_path": format!("src/{}.ts", first_index + index), "new_path": format!("src/{}.ts", first_index + index), "diff": "@@ -1 +1 @@\n-a\n+b\n"}))
            .collect(),
    )
    .to_string()
}

fn notes(count: i64, first_id: i64) -> String {
    Value::Array(
        (0..count)
            .map(|index| json!({"id": first_id + index, "body": format!("note {}", first_id + index), "author": {"username": "bilal"}, "created_at": "2026-07-01T00:00:00Z"}))
            .collect(),
    )
    .to_string()
}

fn author() -> Value {
    json!({"id": 1, "username": "bilal"})
}

fn reviewer() -> Value {
    json!({"id": 5, "username": "octocat"})
}

/// One merge request as `/merge_requests/:iid` answers with it.
fn merge_request_json(overrides: Value) -> String {
    let mut value = json!({
        "iid": 7,
        "title": "Merge request 7",
        "web_url": "https://gitlab.example.test/acme/web/-/merge_requests/7",
        "source_branch": "feat/page",
        "target_branch": "main",
        "created_at": "2026-07-01T00:00:00Z",
        "updated_at": "2026-07-02T00:00:00Z",
        "author": author(),
    });
    for (key, field) in overrides.as_object().unwrap() {
        value[key] = field.clone();
    }
    value.to_string()
}

fn with_diff_refs() -> String {
    merge_request_json(json!({"diff_refs": {"base_sha": "base", "head_sha": "head", "start_sha": "start"}}))
}

fn list_input(limit: i64) -> ListMergeRequestsInput<'static> {
    ListMergeRequestsInput {
        cwd: "/w",
        repository: "acme/web",
        state: PullRequestListState::Open,
        involvement: PullRequestInvolvement::All,
        viewer: "bilal",
        limit,
        query: None,
        cursor: None,
    }
}

#[tokio::test]
async fn asks_gitlab_for_one_row_more_than_the_page_to_probe_for_a_next_page() {
    let runner = ScriptedRunner::new();
    runner.once(out(&merge_requests(3, 1)));
    let batch = runner.cli().list_merge_requests(list_input(10)).await.unwrap();
    assert_eq!(batch.items.len(), 3);
    assert!(!batch.truncated);
    assert_eq!(batch.cursor_advance, 3);
    let path = runner.path(0);
    assert!(path.contains("projects/acme%2Fweb/merge_requests"));
    assert!(path.contains("per_page=11"));
    assert!(path.contains("state=opened"));
    assert_eq!(runner.calls()[0].command, "glab");
}

#[tokio::test]
async fn walks_pages_at_a_fixed_size_because_gitlab_pages_by_offset() {
    let runner = ScriptedRunner::new();
    runner.once(out(&merge_requests(100, 1))).once(out(&merge_requests(100, 101)));
    let batch = runner.cli().list_merge_requests(list_input(150)).await.unwrap();
    assert_eq!(batch.items.len(), 150);
    assert!(batch.truncated);
    for index in [0, 1] {
        assert!(runner.path(index).contains("per_page=100"));
    }
    assert!(runner.path(0).contains("page=1"));
    assert!(runner.path(1).contains("page=2"));
}

#[tokio::test]
async fn hands_a_search_to_gitlabs_own_search_parameter() {
    let runner = ScriptedRunner::new();
    runner.once(out("[]"));
    runner
        .cli()
        .list_merge_requests(ListMergeRequestsInput {
            query: Some("page"),
            ..list_input(10)
        })
        .await
        .unwrap();
    assert!(runner.path(0).contains("search=page"));
}

#[tokio::test]
async fn carries_on_from_the_number_of_rows_already_delivered() {
    let runner = ScriptedRunner::new();
    runner.once(out(&merge_requests(3, 1)));
    let cursor = ProviderListCursor {
        updated_before: "2026-07-02T00:00:00Z".into(),
        delivered: 10,
    };
    runner
        .cli()
        .list_merge_requests(ListMergeRequestsInput {
            cursor: Some(&cursor),
            ..list_input(10)
        })
        .await
        .unwrap();
    let path = runner.path(0);
    assert!(!path.contains("updated_before="));
    assert!(path.contains("order_by=updated_at"));
    assert!(path.contains("per_page=11"));
    assert!(path.contains("page=1"));
}

#[tokio::test]
async fn advances_beyond_several_pages_sharing_the_cursor_timestamp() {
    let runner = ScriptedRunner::new();
    runner.once(out(&merge_requests(11, 144))).once(out(&merge_requests(11, 155)));
    let cursor = ProviderListCursor {
        updated_before: "2026-07-02T00:00:00Z".into(),
        delivered: 150,
    };
    let batch = runner
        .cli()
        .list_merge_requests(ListMergeRequestsInput {
            cursor: Some(&cursor),
            ..list_input(10)
        })
        .await
        .unwrap();
    assert!(runner.path(0).contains("per_page=11"));
    assert!(runner.path(0).contains("page=14"));
    assert!(runner.path(1).contains("page=15"));
    assert_eq!(batch.items.iter().map(|item| item.number).collect::<Vec<_>>(), (151..=160).collect::<Vec<_>>());
    assert!(batch.truncated);
}

#[tokio::test]
async fn advances_the_cursor_through_malformed_raw_rows() {
    let runner = ScriptedRunner::new();
    let mut rows: Vec<Value> = serde_json::from_str(&merge_requests(2, 1)).unwrap();
    rows.insert(0, json!({"iid": "malformed"}));
    runner.once(out(&Value::Array(rows).to_string()));
    let batch = runner.cli().list_merge_requests(list_input(2)).await.unwrap();
    assert_eq!(batch.items.iter().map(|item| item.number).collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(batch.cursor_advance, 3);
    assert!(batch.truncated);
}

#[tokio::test]
async fn url_encodes_a_search_so_it_cannot_add_a_parameter_of_its_own() {
    let runner = ScriptedRunner::new();
    runner.once(out("[]"));
    runner
        .cli()
        .list_merge_requests(ListMergeRequestsInput {
            query: Some(r#"-a&per_page=1 "b""#),
            ..list_input(10)
        })
        .await
        .unwrap();
    let path = runner.path(0);
    assert!(path.contains("search=-a%26per_page%3D1%20%22b%22"));
    assert_eq!(path.matches("per_page=").count(), 1);
}

#[tokio::test]
async fn asks_for_no_search_at_all_when_the_reader_typed_only_spaces() {
    let runner = ScriptedRunner::new();
    runner.once(out("[]"));
    runner
        .cli()
        .list_merge_requests(ListMergeRequestsInput {
            query: Some("   "),
            ..list_input(10)
        })
        .await
        .unwrap();
    assert!(!runner.path(0).contains("search="));
}

#[tokio::test]
async fn stops_walking_on_a_short_page() {
    let runner = ScriptedRunner::new();
    runner.once(out(&merge_requests(40, 1)));
    let batch = runner.cli().list_merge_requests(list_input(150)).await.unwrap();
    assert_eq!(batch.items.len(), 40);
    assert!(!batch.truncated);
    assert_eq!(runner.count(), 1);
}

#[tokio::test]
async fn stops_walking_when_every_row_on_a_page_fails_to_decode() {
    let runner = ScriptedRunner::new();
    let unusable = Value::Array((0..100).map(|_| json!({"iid": "nope"})).collect()).to_string();
    runner.always(out(&unusable));
    let batch = runner.cli().list_merge_requests(list_input(150)).await.unwrap();
    assert_eq!(batch.items.len(), 0);
    assert_eq!(runner.count(), 2);
}

#[tokio::test]
async fn asks_gitlab_for_every_state_on_the_all_tab() {
    let runner = ScriptedRunner::new();
    runner.once(out("[]"));
    runner
        .cli()
        .list_merge_requests(ListMergeRequestsInput {
            state: PullRequestListState::All,
            ..list_input(10)
        })
        .await
        .unwrap();
    assert!(runner.path(0).contains("state=all"));
}

#[tokio::test]
async fn filters_by_the_reviewer_when_the_viewer_is_reviewing() {
    let runner = ScriptedRunner::new();
    runner.once(out("[]"));
    runner
        .cli()
        .list_merge_requests(ListMergeRequestsInput {
            involvement: PullRequestInvolvement::Reviewing,
            ..list_input(10)
        })
        .await
        .unwrap();
    assert!(runner.path(0).contains("reviewer_username=bilal"));
}

#[tokio::test]
async fn addresses_a_nested_group_project_by_its_encoded_full_path() {
    let runner = ScriptedRunner::new();
    runner.once(out("[]"));
    runner
        .cli()
        .list_merge_requests(ListMergeRequestsInput {
            repository: "acme/platform/web",
            ..list_input(10)
        })
        .await
        .unwrap();
    assert!(runner.path(0).contains("projects/acme%2Fplatform%2Fweb/merge_requests"));
}

#[tokio::test]
async fn merges_immediately_rather_than_leaving_auto_merge_armed() {
    let runner = ScriptedRunner::new();
    runner.once(out(""));
    runner
        .cli()
        .run_merge_request_action(TARGET, PullRequestAction::Merge, Some(PullRequestMergeMethod::Squash))
        .await
        .unwrap();
    assert_eq!(
        runner.args(0),
        ["mr", "merge", "7", "--repo", "acme/web", "--auto-merge=false", "--yes", "--squash"]
    );
}

#[tokio::test]
async fn arms_auto_merge_with_the_same_strategy_a_merge_would_have_used() {
    let runner = ScriptedRunner::new();
    runner.once(out(""));
    runner
        .cli()
        .run_merge_request_action(TARGET, PullRequestAction::EnableAutoMerge, Some(PullRequestMergeMethod::Squash))
        .await
        .unwrap();
    assert_eq!(
        runner.args(0),
        ["mr", "merge", "7", "--repo", "acme/web", "--auto-merge=true", "--yes", "--squash"]
    );
}

#[tokio::test]
async fn cancels_an_armed_auto_merge_through_the_api_glab_has_no_flag_for() {
    let runner = ScriptedRunner::new();
    runner.once(out("{}"));
    let target = MergeRequestTarget {
        repository: "acme/platform/web",
        ..TARGET
    };
    runner
        .cli()
        .run_merge_request_action(target, PullRequestAction::DisableAutoMerge, None)
        .await
        .unwrap();
    assert_eq!(
        runner.args(0),
        [
            "api",
            "projects/acme%2Fplatform%2Fweb/merge_requests/7/cancel_merge_when_pipeline_succeeds",
            "--method",
            "POST"
        ]
    );
}

#[tokio::test]
async fn brings_a_stale_branch_up_to_date_by_rebasing_it() {
    let runner = ScriptedRunner::new();
    runner.once(out(""));
    runner
        .cli()
        .run_merge_request_action(TARGET, PullRequestAction::UpdateBranch, None)
        .await
        .unwrap();
    assert_eq!(runner.args(0), ["mr", "rebase", "7", "--repo", "acme/web"]);
}

#[tokio::test]
async fn moves_a_merge_request_back_to_draft_through_glab() {
    let runner = ScriptedRunner::new();
    runner.once(out(""));
    runner.cli().run_merge_request_action(TARGET, PullRequestAction::Draft, None).await.unwrap();
    assert_eq!(runner.args(0), ["mr", "update", "7", "--repo", "acme/web", "--draft"]);
}

#[tokio::test]
async fn refuses_an_action_this_host_does_not_declare_without_running_anything() {
    let runner = ScriptedRunner::new();
    let error = runner
        .cli()
        .run_merge_request_action(TARGET, PullRequestAction::Revert, None)
        .await
        .unwrap_err();
    assert_eq!(error.detail(), "GitLab merge request action revert is unsupported");
    assert_eq!(runner.count(), 0);
}

#[tokio::test]
async fn sends_a_comment_body_over_stdin_never_in_argv() {
    let runner = ScriptedRunner::new();
    runner.once(out(""));
    runner.cli().comment_on_merge_request(TARGET, "true").await.unwrap();
    assert_eq!(
        runner.args(0),
        [
            "api",
            "projects/acme%2Fweb/merge_requests/7/notes",
            "--method",
            "POST",
            "--input",
            "-",
            "--header",
            "Content-Type: application/json"
        ]
    );
    assert_eq!(runner.stdin(0).as_deref(), Some(r#"{"body":"true"}"#));
}

#[tokio::test]
async fn reads_one_diff_page_and_hands_back_the_cursor_for_the_next() {
    let runner = ScriptedRunner::new();
    runner.once(out(&diff_page(0, 100)));
    let diff = runner.cli().get_merge_request_diff(TARGET, None, None).await.unwrap();
    assert_eq!(runner.count(), 1);
    assert!(diff.next_cursor.is_some());
    assert!(!diff.truncated);
    assert!(runner.path(0).contains("merge_requests/7/diffs?per_page=100&page=1"));
    let call = &runner.calls()[0];
    assert_eq!(call.max_output_bytes, Some(8 * 1024 * 1024));
    assert_eq!(call.timeout, Some(std::time::Duration::from_millis(60_000)));
}

#[tokio::test]
async fn carries_on_from_a_cursor_at_the_page_it_names() {
    let runner = ScriptedRunner::new();
    runner.once(out(&diff_page(0, 100))).once(out(&diff_page(100, 3)));
    let cli = runner.cli();
    let first = cli.get_merge_request_diff(TARGET, None, None).await.unwrap();
    let second = cli.get_merge_request_diff(TARGET, first.next_cursor.as_deref(), None).await.unwrap();
    assert!(runner.path(1).contains("page=2"));
    assert_eq!(second.next_cursor, None);
    assert!(second.patch.contains("diff --git a/src/100.ts b/src/100.ts"));
}

#[tokio::test]
async fn refuses_a_cursor_it_never_handed_out_rather_than_reading_it_into_a_query() {
    let runner = ScriptedRunner::new();
    let error = runner.cli().get_merge_request_diff(TARGET, Some("1&per_page=1"), None).await.unwrap_err();
    assert_eq!(error.tag(), "GitLabDiffCursorError");
    assert_eq!(runner.count(), 0);
}

#[tokio::test]
async fn reads_a_named_commit_from_its_own_diff_and_pages_inside_it() {
    let runner = ScriptedRunner::new();
    runner.once(out(&diff_page(0, 100))).once(out(&diff_page(100, 3)));
    let cli = runner.cli();
    let commit = Some("a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0");
    let first = cli.get_merge_request_diff(TARGET, None, commit).await.unwrap();
    let second = cli.get_merge_request_diff(TARGET, first.next_cursor.as_deref(), commit).await.unwrap();
    let commit_path = "projects/acme%2Fweb/repository/commits/a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0/diff";
    assert_eq!(runner.path(0), format!("{commit_path}?per_page=100&page=1"));
    assert_eq!(runner.path(1), format!("{commit_path}?per_page=100&page=2"));
    assert_eq!(second.next_cursor, None);
}

#[tokio::test]
async fn refuses_a_commit_that_is_not_a_sha_rather_than_reading_it_into_a_path() {
    let runner = ScriptedRunner::new();
    let error = runner
        .cli()
        .get_merge_request_diff(TARGET, None, Some("../../merge_requests/8/diffs"))
        .await
        .unwrap_err();
    assert_eq!(error.tag(), "GitLabDiffCommitError");
    assert_eq!(runner.count(), 0);
}

fn contents_input(commit: Option<&'static str>, change_type: ChangeType, path: &'static str) -> DiffFileContentsInput<'static> {
    DiffFileContentsInput {
        target: TARGET,
        commit,
        change_type,
        old_path: path,
        new_path: path,
    }
}

#[tokio::test]
async fn reports_a_commit_with_no_parent_as_a_structured_error() {
    let runner = ScriptedRunner::new();
    runner.once(out(r#"{"id":"a1b2c3d","parent_ids":[]}"#));
    let error = runner
        .cli()
        .get_merge_request_diff_file_contents(contents_input(Some("a1b2c3d"), ChangeType::Change, "src/a.ts"))
        .await
        .unwrap_err();
    assert!(matches!(&error, GitLabPullRequestCliError::DiffCommitParentUnavailable { commit, .. } if commit == "a1b2c3d"));
}

#[tokio::test]
async fn expands_a_new_file_from_a_root_commit_without_requiring_a_parent() {
    let runner = ScriptedRunner::new();
    runner.once(out(r#"{"id":"a1b2c3d","parent_ids":[]}"#)).once(out("first contents\n"));
    let contents = runner
        .cli()
        .get_merge_request_diff_file_contents(contents_input(Some("a1b2c3d"), ChangeType::New, "src/first.ts"))
        .await
        .unwrap();
    assert_eq!(contents, (String::new(), "first contents\n".to_owned()));
    assert!(runner.path(1).contains("raw?ref=a1b2c3d"));
}

const DIFF_REFS: &str = r#"{"diff_refs":{"base_sha":"a1b2c3d","head_sha":"b1c2d3e","start_sha":"a1b2c3d"}}"#;

#[tokio::test]
async fn reports_an_oversized_diff_file_with_its_path_and_reason() {
    let runner = ScriptedRunner::new();
    runner.once(out(DIFF_REFS)).once(out_truncated("partial"));
    let error = runner
        .cli()
        .get_merge_request_diff_file_contents(contents_input(None, ChangeType::Deleted, "src/large.ts"))
        .await
        .unwrap_err();
    assert!(matches!(
        &error,
        GitLabPullRequestCliError::DiffFileContentsUnavailable { path, reason: FileContentsUnavailableReason::Oversized, .. } if path == "src/large.ts"
    ));
    assert_eq!(runner.calls()[1].max_output_bytes, Some(1024 * 1024));
}

#[tokio::test]
async fn reports_undecodable_diff_file_contents_as_binary() {
    let runner = ScriptedRunner::new();
    runner.once(out(DIFF_REFS)).once(out_invalid_utf8("binary\u{FFFD}contents"));
    let error = runner
        .cli()
        .get_merge_request_diff_file_contents(contents_input(None, ChangeType::Deleted, "assets/logo.png"))
        .await
        .unwrap_err();
    assert!(matches!(
        &error,
        GitLabPullRequestCliError::DiffFileContentsUnavailable { path, reason: FileContentsUnavailableReason::Binary, .. } if path == "assets/logo.png"
    ));
}

#[tokio::test]
async fn returns_valid_text_containing_a_literal_replacement_character() {
    let runner = ScriptedRunner::new();
    runner.once(out(DIFF_REFS)).once(out("before\u{FFFD}after"));
    let (old, _) = runner
        .cli()
        .get_merge_request_diff_file_contents(contents_input(None, ChangeType::Deleted, "docs/encoding.md"))
        .await
        .unwrap();
    assert_eq!(old, "before\u{FFFD}after");
}

#[tokio::test]
async fn ends_the_diff_on_a_page_with_no_files_rather_than_asking_for_it_again() {
    let runner = ScriptedRunner::new();
    runner.once(out("[]"));
    let diff = runner.cli().get_merge_request_diff(TARGET, Some("4"), None).await.unwrap();
    assert_eq!(diff.patch, "");
    assert_eq!(diff.next_cursor, None);
}

#[tokio::test]
async fn fails_a_diff_page_cut_off_mid_json_rather_than_calling_the_diff_whole() {
    let runner = ScriptedRunner::new();
    runner.once(out_truncated(r#"[{"old_path":"src/x.ts","new_p"#));
    let error = runner.cli().get_merge_request_diff(TARGET, None, None).await.unwrap_err();
    assert_eq!(error.tag(), "GitLabMergeRequestReadError");
}

#[tokio::test]
async fn offers_no_squash_when_the_project_does_not_say_it_allows_one() {
    let runner = ScriptedRunner::new();
    runner.once(out(r#"{"merge_method":"merge"}"#));
    let capabilities = runner.cli().get_project_merge_capabilities("/w", "acme/web").await.unwrap();
    assert_eq!(
        capabilities,
        PullRequestMergeCapabilities {
            merge: true,
            squash: false,
            rebase: false
        }
    );
}

#[tokio::test]
async fn reads_the_projects_merge_settings_as_its_merge_capabilities() {
    let runner = ScriptedRunner::new();
    runner.once(out(r#"{"merge_method":"ff","squash_option":"never"}"#));
    let capabilities = runner.cli().get_project_merge_capabilities("/w", "acme/web").await.unwrap();
    assert_eq!(
        capabilities,
        PullRequestMergeCapabilities {
            merge: false,
            squash: false,
            rebase: true
        }
    );
    assert_eq!(runner.path(0), "projects/acme%2Fweb?license=false");
}

#[tokio::test]
async fn asks_the_detail_read_for_the_divergence_gitlab_withholds_by_default() {
    let runner = ScriptedRunner::new();
    runner.once(out(r#"{"message":"404 Not Found"}"#));
    let _ = runner.cli().get_merge_request_detail(TARGET).await;
    assert_eq!(runner.path(0), "projects/acme%2Fweb/merge_requests/7?include_diverged_commits_count=true");
}

#[tokio::test]
async fn fails_the_read_when_gitlab_returns_something_unreadable() {
    let runner = ScriptedRunner::new();
    runner.once(out(r#"{"message":"404 Not Found"}"#));
    let error = runner.cli().get_merge_request_detail(TARGET).await.unwrap_err();
    assert_eq!(error.tag(), "GitLabMergeRequestReadError");
    assert_eq!(error.detail(), "GitLab CLI returned an unreadable getMergeRequestDetail response.");
}

#[tokio::test]
async fn fails_when_the_authenticated_account_has_no_username() {
    let runner = ScriptedRunner::new();
    runner.once(out(r#"{"username":""}"#));
    let error = runner.cli().get_viewer_username("/w").await.unwrap_err();
    assert_eq!(error.tag(), "GitLabViewerUnavailableError");
}

#[tokio::test]
async fn walks_the_notes_until_gitlab_answers_with_a_short_page() {
    let runner = ScriptedRunner::new();
    runner.once(out(&notes(100, 1))).once(out(&notes(2, 101)));
    let (comments, truncated) = runner.cli().list_notes(TARGET).await.unwrap();
    assert!(runner.args(0).join(" ").contains("page=1"));
    assert!(runner.args(1).join(" ").contains("page=2"));
    assert_eq!(comments.len(), 102);
    assert!(!truncated);
}

#[tokio::test]
async fn stops_the_note_walk_at_its_bound_and_says_the_conversation_was_cut_short() {
    let runner = ScriptedRunner::new();
    runner.always(out(&notes(100, 1)));
    let (_, truncated) = runner.cli().list_notes(TARGET).await.unwrap();
    assert_eq!(runner.count(), 10);
    assert!(truncated);
}

#[tokio::test]
async fn reads_a_positioned_discussion_as_a_thread_anchored_to_its_line() {
    let runner = ScriptedRunner::new();
    runner.once(out(&json!([
        {"id": "abc123", "notes": [
            {
                "id": 1, "body": "rename this",
                "author": {"username": "bilal", "avatar_url": "https://avatars.example.test/b.png"},
                "created_at": "2026-07-01T00:00:00Z", "resolvable": true, "resolved": true,
                "position": {"position_type": "text", "new_path": "src/a.ts", "old_path": "src/a.ts", "new_line": 12, "old_line": null},
            },
            {"id": 2, "body": "done", "author": {"username": "julius"}, "created_at": "2026-07-01T01:00:00Z"},
        ]},
        {"id": "def456", "notes": [{"id": 3, "body": "ship it", "created_at": "2026-07-01Z"}]},
    ])
    .to_string()));
    let (threads, _) = runner.cli().list_discussions(TARGET).await.unwrap();
    assert_eq!(threads.len(), 1);
    let thread = &threads[0];
    assert_eq!(
        (thread.id.as_str(), thread.path.as_str(), thread.line, thread.side, thread.is_resolved),
        ("abc123", "src/a.ts", Some(12), PullRequestDiffSide::Right, true)
    );
    assert_eq!(thread.comments.len(), 2);
}

fn draft(path: &str, old_path: Option<&str>, position: PullRequestReviewPosition, body: &str) -> PullRequestReviewCommentDraft {
    PullRequestReviewCommentDraft {
        path: path.into(),
        old_path: old_path.map(Into::into),
        position,
        body: body.into(),
    }
}

#[tokio::test]
async fn sends_a_review_as_its_comments_then_its_summary_then_the_verdict() {
    let runner = ScriptedRunner::new();
    runner
        .once(out(&merge_request_json(
            json!({"diff_refs": {"base_sha": "base", "head_sha": "head", "start_sha": "start"}}),
        )))
        .always(out("{}"));
    let comments = [draft(
        "src/b.ts",
        Some("src/a.ts"),
        PullRequestReviewPosition::Deleted(PullRequestReviewPositionDeleted {
            kind: Default::default(),
            old_line: 4,
        }),
        "why remove?",
    )];
    runner
        .cli()
        .submit_review(TARGET, PullRequestReviewVerdict::Approve, "Looks right.", &comments)
        .await
        .unwrap();
    assert!(runner.path(0).contains("merge_requests/7"));
    assert!(runner.path(1).contains("/discussions"));
    assert_eq!(
        runner.body(1),
        json!({
            "body": "why remove?",
            "position": {
                "base_sha": "base", "head_sha": "head", "start_sha": "start", "position_type": "text",
                "old_path": "src/a.ts", "new_path": "src/b.ts", "old_line": 4,
            },
        })
    );
    assert!(runner.path(2).contains("/notes"));
    assert!(runner.path(3).contains("/approve"));
}

#[tokio::test]
async fn does_not_ask_for_diff_revisions_when_a_review_carries_no_line_comments() {
    let runner = ScriptedRunner::new();
    runner.always(out("{}"));
    runner
        .cli()
        .submit_review(TARGET, PullRequestReviewVerdict::Comment, "One thought.", &[])
        .await
        .unwrap();
    assert_eq!(runner.count(), 1);
    assert!(runner.path(0).contains("/notes"));
}

#[tokio::test]
async fn resolves_a_discussion_in_place_rather_than_posting_to_it() {
    let runner = ScriptedRunner::new();
    runner.always(out("{}"));
    runner.cli().set_discussion_resolution(TARGET, "abc123", true).await.unwrap();
    assert!(runner.args(0).contains(&"--method".to_owned()));
    assert!(runner.args(0).contains(&"PUT".to_owned()));
    assert!(runner.path(0).contains("/discussions/abc123"));
    assert_eq!(runner.body(0), json!({"resolved": true}));
}

#[tokio::test]
async fn awards_an_emoji_through_a_post_naming_it_not_a_body() {
    let runner = ScriptedRunner::new();
    runner.always(out("{}"));
    runner
        .cli()
        .set_reaction(TARGET, None, PullRequestReactionContent::ThumbsUp, true)
        .await
        .unwrap();
    assert_eq!(runner.count(), 1);
    assert_eq!(
        runner.args(0),
        ["api", "projects/acme%2Fweb/merge_requests/7/award_emoji?name=thumbsup", "--method", "POST"]
    );
}

#[tokio::test]
async fn removes_an_award_by_listing_them_and_deleting_the_readers_own_id() {
    let runner = ScriptedRunner::new();
    runner
        .once(out(r#"{"username":"bilal"}"#))
        .once(out(
            &json!([{"id": 5, "name": "thumbsup", "user": {"username": "bilal"}}, {"id": 6, "name": "thumbsup", "user": {"username": "julius"}}]).to_string(),
        ))
        .once(out("{}"));
    runner
        .cli()
        .set_reaction(TARGET, None, PullRequestReactionContent::ThumbsUp, false)
        .await
        .unwrap();
    assert_eq!(runner.count(), 3);
    assert_eq!(
        runner.args(2),
        ["api", "projects/acme%2Fweb/merge_requests/7/award_emoji/5", "--method", "DELETE"]
    );
}

#[tokio::test]
async fn does_nothing_when_the_reader_has_no_award_of_that_name_to_take_back() {
    let runner = ScriptedRunner::new();
    runner.once(out(r#"{"username":"bilal"}"#)).once(out("[]"));
    runner
        .cli()
        .set_reaction(TARGET, None, PullRequestReactionContent::ThumbsUp, false)
        .await
        .unwrap();
    assert_eq!(runner.count(), 2);
}

#[tokio::test]
async fn addresses_an_award_on_a_note_through_the_note() {
    let runner = ScriptedRunner::new();
    runner.always(out("{}"));
    runner
        .cli()
        .set_reaction(TARGET, Some("42"), PullRequestReactionContent::Hooray, true)
        .await
        .unwrap();
    assert_eq!(runner.path(0), "projects/acme%2Fweb/merge_requests/7/notes/42/award_emoji?name=tada");
}

#[tokio::test]
async fn names_a_merge_request_with_no_diff_revisions_rather_than_calling_it_unreadable() {
    let runner = ScriptedRunner::new();
    runner.once(out(&merge_request_json(json!({"diff_refs": null}))));
    let comments = [draft(
        "src/a.ts",
        None,
        PullRequestReviewPosition::Added(PullRequestReviewPositionAdded {
            kind: Default::default(),
            new_line: 4,
        }),
        "nit",
    )];
    let error = runner
        .cli()
        .submit_review(TARGET, PullRequestReviewVerdict::Comment, "", &comments)
        .await
        .unwrap_err();
    assert_eq!(error.tag(), "GitLabDiffRefsUnavailableError");
}

#[tokio::test]
async fn reads_who_has_access_to_the_project_and_who_is_already_on_the_merge_request() {
    let runner = ScriptedRunner::new();
    runner
        .once(out(&merge_request_json(json!({"reviewers": [reviewer()]}))))
        .once(out(&json!([author(), reviewer(), {"id": 9, "username": "hubot"}]).to_string()));
    let list = runner.cli().list_reviewer_candidates(TARGET).await.unwrap();
    assert_eq!(runner.path(1), "projects/acme%2Fweb/users?per_page=100");
    assert_eq!(
        list.candidates
            .iter()
            .map(|candidate| (candidate.id.as_str(), candidate.is_requested))
            .collect::<Vec<_>>(),
        vec![("5", true), ("9", false)]
    );
    assert!(!list.truncated);
}

#[tokio::test]
async fn writes_the_reviewer_set_back_with_the_one_being_asked_added_to_it() {
    let runner = ScriptedRunner::new();
    runner.once(out(&merge_request_json(json!({"reviewers": [reviewer()]})))).once(out("{}"));
    runner.cli().set_reviewer_request(TARGET, &["9".to_owned()], true).await.unwrap();
    assert!(runner.args(1).contains(&"PUT".to_owned()));
    assert_eq!(runner.body(1), json!({"reviewer_ids": [5, 9]}));
}

#[tokio::test]
async fn takes_a_reviewer_out_of_the_set_rather_than_clearing_it() {
    let runner = ScriptedRunner::new();
    runner
        .once(out(&merge_request_json(json!({"reviewers": [reviewer(), {"id": 9, "username": "hubot"}]}))))
        .once(out("{}"));
    runner.cli().set_reviewer_request(TARGET, &["9".to_owned()], false).await.unwrap();
    assert_eq!(runner.body(1), json!({"reviewer_ids": [5]}));
}

#[tokio::test]
async fn ignores_an_id_gitlab_could_not_have_handed_out_which_names_nobody() {
    let runner = ScriptedRunner::new();
    runner.once(out(&merge_request_json(json!({"reviewers": [reviewer()]})))).once(out("{}"));
    runner.cli().set_reviewer_request(TARGET, &["octocat".to_owned()], true).await.unwrap();
    assert_eq!(runner.body(1), json!({"reviewer_ids": [5]}));
}

const JSON_ARGS_PUT: [&str; 8] = [
    "api",
    "projects/acme%2Fweb/merge_requests/7",
    "--method",
    "PUT",
    "--input",
    "-",
    "--header",
    "Content-Type: application/json",
];

#[tokio::test]
async fn rewrites_a_title_without_touching_the_description() {
    let runner = ScriptedRunner::new();
    runner.always(out("{}"));
    runner.cli().update_merge_request(TARGET, Some("A better title"), None).await.unwrap();
    assert_eq!(runner.args(0), JSON_ARGS_PUT);
    assert_eq!(runner.body(0), json!({"title": "A better title"}));
}

#[tokio::test]
async fn sends_a_rewritten_body_as_gitlabs_description_and_nothing_else() {
    let runner = ScriptedRunner::new();
    runner.always(out("{}"));
    runner.cli().update_merge_request(TARGET, None, Some("What this changes.")).await.unwrap();
    assert_eq!(runner.body(0), json!({"description": "What this changes."}));
}

#[tokio::test]
async fn rewrites_title_and_description_together_in_one_request() {
    let runner = ScriptedRunner::new();
    runner.always(out("{}"));
    runner
        .cli()
        .update_merge_request(TARGET, Some("A better title"), Some("What this changes."))
        .await
        .unwrap();
    assert_eq!(runner.count(), 1);
    assert_eq!(
        runner.stdin(0).as_deref(),
        Some(r#"{"title":"A better title","description":"What this changes."}"#)
    );
}

#[tokio::test]
async fn rewrites_a_note_in_place_through_the_note_it_names() {
    let runner = ScriptedRunner::new();
    runner.always(out("{}"));
    runner.cli().update_note(TARGET, "42", "true").await.unwrap();
    assert_eq!(
        runner.args(0),
        [
            "api",
            "projects/acme%2Fweb/merge_requests/7/notes/42",
            "--method",
            "PUT",
            "--input",
            "-",
            "--header",
            "Content-Type: application/json"
        ]
    );
    assert_eq!(runner.stdin(0).as_deref(), Some(r#"{"body":"true"}"#));
}

fn pairs(entries: &[(&str, &str)]) -> Vec<(String, String)> {
    entries.iter().map(|(path, oid)| ((*path).to_owned(), (*oid).to_owned())).collect()
}

#[tokio::test]
async fn reads_blob_ids_for_the_marked_paths_at_the_merge_requests_head() {
    let runner = ScriptedRunner::new();
    runner.once(out(&with_diff_refs())).once(out(
        r#"{"data":{"project":{"repository":{"blobs":{"nodes":[{"path":"src/a.ts","oid":"aaa"}]}}}}}"#,
    ));
    let revisions = runner
        .cli()
        .get_file_revisions(TARGET, &["src/a.ts".to_owned(), "src/gone.ts".to_owned()])
        .await
        .unwrap();
    assert_eq!(revisions.into_entries(), pairs(&[("src/a.ts", "aaa"), ("src/gone.ts", "")]));
    assert_eq!(
        runner.body(1)["variables"],
        json!({"fullPath": "acme/web", "ref": "head", "paths": ["src/a.ts", "src/gone.ts"]})
    );
}

#[tokio::test]
async fn leaves_the_paths_out_when_gitlab_did_not_answer_the_blobs_query() {
    let runner = ScriptedRunner::new();
    runner.once(out(&with_diff_refs())).once(out(r#"{"data":{"project":null}}"#));
    let revisions = runner
        .cli()
        .get_file_revisions(TARGET, &["src/a.ts".to_owned(), "src/b.ts".to_owned()])
        .await
        .unwrap();
    assert!(revisions.is_empty());
}

#[tokio::test]
async fn asks_gitlab_nothing_when_no_file_is_marked() {
    let runner = ScriptedRunner::new();
    let revisions = runner.cli().get_file_revisions(TARGET, &[]).await.unwrap();
    assert!(revisions.is_empty());
    assert_eq!(runner.count(), 0);
}

#[tokio::test]
async fn splits_the_paths_across_requests_because_gitlab_charges_the_query_by_how_many() {
    let runner = ScriptedRunner::new();
    let paths: Vec<String> = (0..150).map(|index| format!("src/{index}.ts")).collect();
    runner.once(out(&with_diff_refs())).implement(|request| {
        let body: Value = serde_json::from_str(request.stdin.as_deref().unwrap_or("{}")).unwrap();
        let nodes: Vec<Value> = body["variables"]["paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|path| json!({"path": path, "oid": format!("oid-{}", path.as_str().unwrap())}))
            .collect();
        out(&json!({"data": {"project": {"repository": {"blobs": {"nodes": nodes}}}}}).to_string())
    });
    let revisions = runner.cli().get_file_revisions(TARGET, &paths).await.unwrap();
    assert_eq!(revisions.len(), 150);
    assert_eq!(revisions.get("src/149.ts").map(String::as_str), Some("oid-src/149.ts"));
    assert_eq!(runner.count(), 3);
}

#[tokio::test]
async fn reads_awards_a_page_of_notes_at_a_time_through_graphql() {
    let runner = ScriptedRunner::new();
    let page = |cursor: Option<&str>, has_next: bool, note: &str| {
        json!({"data": {"currentUser": {"username": "bilal"}, "project": {"mergeRequest": {
            "awardEmoji": {"nodes": [{"name": "rocket", "user": {"username": "julius"}}]},
            "notes": {"pageInfo": {"hasNextPage": has_next, "endCursor": cursor}, "nodes": [
                {"id": format!("gid://gitlab/Note/{note}"), "awardEmoji": {"nodes": [{"name": "eyes", "user": {"username": "bilal"}}]}},
            ]},
        }}}})
        .to_string()
    };
    runner.once(out(&page(Some("c1"), true, "1"))).once(out(&page(None, false, "2")));
    let reactions = runner.cli().list_reactions(TARGET).await.unwrap();
    assert_eq!(
        runner.args(0),
        [
            "api",
            "graphql",
            "--method",
            "POST",
            "--input",
            "-",
            "--header",
            "Content-Type: application/json"
        ]
    );
    assert_eq!(runner.body(0)["variables"], json!({"fullPath": "acme/web", "iid": "7", "cursor": null}));
    assert_eq!(runner.body(1)["variables"]["cursor"], json!("c1"));
    assert_eq!(reactions.reactions.len(), 1);
    assert_eq!(
        reactions.reactions_by_note_id.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        vec!["1", "2"]
    );
}

#[tokio::test]
async fn classifies_a_missing_glab_as_the_shared_cli_error() {
    let runner = ScriptedRunner::new();
    runner.once(exit(1, "", "To get started with GitLab CLI, please run: glab auth login"));
    let error = runner.cli().get_viewer_username("/w").await.unwrap_err();
    assert_eq!(error.tag(), "GitLabCliAuthenticationError");
}
