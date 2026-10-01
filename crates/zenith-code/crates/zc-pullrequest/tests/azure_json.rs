//! Port of `azureDevOpsPullRequestJson.test.ts`.

use serde_json::{json, Value};
use zc_contracts::{PullRequestActor, PullRequestCommentKind, PullRequestMergeMethod, PullRequestMergeability, PullRequestState};
use zc_pullrequest::azure::json::*;

const REST_URL: &str = "https://dev.azure.com/acme/_apis/git/repositories/6f9c9b7f-0000-0000-0000-000000000000/pullRequests/42";

/// Shaped after Azure's `GitPullRequest`, trimmed to the fields that are read.
fn pull_request(overrides: Value) -> Value {
    let mut base = json!({
        "pullRequestId": 42,
        "title": "Add the change requests page",
        "description": "Ships the page.",
        "status": "active",
        "isDraft": false,
        "mergeStatus": "succeeded",
        "createdBy": {"displayName": "Sam Example", "uniqueName": "sam@example.test"},
        "sourceRefName": "refs/heads/feat/page",
        "targetRefName": "refs/heads/main",
        "creationDate": "2026-07-01T00:00:00Z",
        "url": REST_URL,
        "repository": {"name": "web", "project": {"name": "platform"}},
    });
    for (key, value) in overrides.as_object().unwrap() {
        base[key] = value.clone();
    }
    base
}

fn list(rows: Value) -> AzureDevOpsPullRequestBatch {
    decode_pull_request_list_json(&rows.to_string()).expect("a successful decode")
}

fn one(row: Value) -> Option<AzureDevOpsPullRequest> {
    decode_pull_request_json(&row.to_string()).expect("a successful decode")
}

#[test]
fn reads_a_pull_request_as_a_change_request() {
    let batch = list(json!([pull_request(json!({}))]));
    assert_eq!(batch.items.len(), 1);
    let item = &batch.items[0];
    assert_eq!(item.number, 42);
    assert_eq!(item.title, "Add the change requests page");
    // The login is an email, because that is what `az account show` reports to compare with.
    assert_eq!(item.author.as_ref().unwrap().login, "sam@example.test");
    assert_eq!(item.author.as_ref().unwrap().name.as_deref(), Some("Sam Example"));
    // Azure prefixes its refs, which no other host does.
    assert_eq!(item.head_branch, "feat/page");
    assert_eq!(item.base_branch, "main");
    assert_eq!(item.state, PullRequestState::Open);
    assert!(!item.is_draft);
    assert_eq!(item.mergeability, PullRequestMergeability::Mergeable);
}

#[test]
fn assembles_a_browser_url_when_azure_reports_no_web_link() {
    let batch = list(json!([pull_request(json!({}))]));
    assert_eq!(batch.items[0].url, "https://dev.azure.com/acme/platform/_git/web/pullrequest/42");
}

#[test]
fn prefers_the_web_link_azure_sends_when_asked_for_one() {
    let batch = list(json!([pull_request(
        json!({"_links": {"web": {"href": "https://dev.azure.com/acme/platform/_git/web/pullrequest/42"}}})
    )]));
    assert_eq!(batch.items[0].url, "https://dev.azure.com/acme/platform/_git/web/pullrequest/42");
}

#[test]
fn reads_each_status() {
    for (status, expected) in [
        ("active", PullRequestState::Open),
        ("completed", PullRequestState::Merged),
        ("abandoned", PullRequestState::Closed),
        ("something new", PullRequestState::Open),
    ] {
        let batch = list(json!([pull_request(json!({"status": status}))]));
        assert_eq!(batch.items[0].state, expected, "{status}");
    }
}

#[test]
fn reads_each_merge_status() {
    for (merge_status, expected) in [
        ("succeeded", PullRequestMergeability::Mergeable),
        ("conflicts", PullRequestMergeability::Conflicting),
        ("rejectedByPolicy", PullRequestMergeability::Conflicting),
        ("queued", PullRequestMergeability::Unknown),
        ("notSet", PullRequestMergeability::Unknown),
    ] {
        let batch = list(json!([pull_request(json!({"mergeStatus": merge_status}))]));
        assert_eq!(batch.items[0].mergeability, expected, "{merge_status}");
    }
}

#[test]
fn stands_the_closing_time_in_for_a_last_touched_time() {
    let batch = list(json!([pull_request(json!({"status": "completed", "closedDate": "2026-07-05T00:00:00Z"}))]));
    assert_eq!(batch.items[0].created_at, "2026-07-01T00:00:00Z");
    assert_eq!(batch.items[0].updated_at, "2026-07-05T00:00:00Z");
}

#[test]
fn skips_a_malformed_row_but_still_counts_it() {
    let batch = list(json!([{"pullRequestId": "nope"}, pull_request(json!({}))]));
    assert_eq!(batch.items.len(), 1);
    assert_eq!(batch.raw_count, 2);
    assert_eq!(batch.raw_indexes, vec![1]);
}

#[test]
fn reads_reviewers_as_review_requests() {
    let detail = one(pull_request(
        json!({"reviewers": [{"displayName": "Riley", "uniqueName": "riley@example.test", "vote": 10}]}),
    ))
    .unwrap();
    assert_eq!(detail.review_request_logins, vec!["riley@example.test"]);
    assert_eq!(
        detail.reviewers,
        vec![PullRequestActor {
            is_bot: None,
            login: "riley@example.test".into(),
            name: Some("Riley".into()),
            avatar_url: None,
        }]
    );
}

#[test]
fn reads_auto_complete_from_whoever_armed_it() {
    let armed = one(pull_request(json!({"autoCompleteSetBy": {"displayName": "Sam Example"}}))).unwrap();
    assert!(armed.auto_merge_enabled);
    // Azure leaves the field out entirely rather than sending it empty.
    assert!(!one(pull_request(json!({}))).unwrap().auto_merge_enabled);
}

#[test]
fn keeps_the_strategy_stored_with_auto_complete() {
    let armed = one(pull_request(
        json!({"autoCompleteSetBy": {"displayName": "Sam Example"}, "completionOptions": {"mergeStrategy": "squash"}}),
    ))
    .unwrap();
    assert!(armed.auto_merge_enabled);
    assert_eq!(armed.auto_merge_method, Some(PullRequestMergeMethod::Squash));

    let unspecified = one(pull_request(
        json!({"autoCompleteSetBy": {"displayName": "Sam Example"}, "completionOptions": {"squashMerge": false}}),
    ))
    .unwrap();
    assert!(unspecified.auto_merge_enabled);
    assert_eq!(unspecified.auto_merge_method, None);
}

#[test]
fn works_out_where_the_repository_lives() {
    let detail = one(pull_request(json!({}))).unwrap();
    assert_eq!(
        detail.location,
        Some(AzureDevOpsRepositoryLocation {
            project: "platform".into(),
            repository: "web".into()
        })
    );
}

#[test]
fn reports_no_repository_location_when_azure_said_too_little() {
    let detail = one(pull_request(json!({
        "url": null,
        "repository": null,
        "_links": {"web": {"href": "https://dev.azure.com/acme/platform/_git/web/pullrequest/42"}},
    })))
    .unwrap();
    assert_eq!(detail.location, None);
}

#[test]
fn returns_nothing_when_azure_gave_no_way_to_place_the_pull_request() {
    assert_eq!(one(pull_request(json!({"url": null, "repository": null}))), None);
}

#[test]
fn reads_the_signed_in_account_name() {
    assert_eq!(
        decode_viewer_json(&json!({"user": {"name": "sam@example.test"}}).to_string())
            .unwrap()
            .as_deref(),
        Some("sam@example.test")
    );
}

#[test]
fn returns_no_viewer_when_nobody_is_signed_in() {
    assert_eq!(decode_viewer_json(&json!({"user": null}).to_string()).unwrap(), None);
}

fn threads(value: Value) -> Vec<zc_contracts::PullRequestComment> {
    decode_threads_json(&value.to_string()).expect("a successful decode")
}

#[test]
fn takes_every_real_comment_of_every_thread_oldest_first() {
    let comments = threads(json!({"value": [
        {"id": 2, "comments": [{"id": 1, "content": "Second remark.", "author": {"displayName": "Riley", "uniqueName": "riley@example.test"}, "publishedDate": "2026-07-03T00:00:00Z"}]},
        {"id": 1, "comments": [
            // Azure's own activity notes are events rather than remarks.
            {"id": 1, "content": "Sam voted", "commentType": "system", "publishedDate": "x"},
            {"id": 2, "content": "First remark.", "author": {"displayName": "Sam", "uniqueName": "sam@example.test"}, "publishedDate": "2026-07-02T00:00:00Z"},
        ]},
    ]}));
    assert_eq!(
        comments.iter().map(|c| c.body.as_str()).collect::<Vec<_>>(),
        ["First remark.", "Second remark."]
    );
    assert_eq!(comments[0].kind, PullRequestCommentKind::IssueComment);
    assert_eq!(comments[0].author.as_ref().unwrap().login, "sam@example.test");
}

#[test]
fn reads_a_thread_pinned_to_a_file_as_a_review_comment() {
    let comments = threads(
        json!({"value": [{"id": 3, "threadContext": {"filePath": "/src/app.ts"}, "comments": [{"id": 1, "content": "Rename this.", "publishedDate": "2026-07-02T00:00:00Z"}]}]}),
    );
    assert_eq!(comments[0].kind, PullRequestCommentKind::ReviewComment);
    assert_eq!(comments[0].path.as_deref(), Some("/src/app.ts"));
}

#[test]
fn keeps_the_replies_under_a_thread() {
    let comments = threads(json!({"value": [{"id": 4, "threadContext": {"filePath": "/src/app.ts"}, "comments": [
        {"id": 1, "content": "Rename this.", "publishedDate": "2026-07-02T00:00:00Z"},
        {"id": 2, "content": "Renamed.", "publishedDate": "2026-07-02T01:00:00Z"},
        {"id": 3, "content": "Thanks.", "publishedDate": "2026-07-02T02:00:00Z"},
    ]}]}));
    assert_eq!(comments.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["4:1", "4:2", "4:3"]);
}

#[test]
fn drops_deleted_threads_and_threads_with_nothing_to_show() {
    let comments = threads(json!({"value": [
        {"id": 1, "isDeleted": true, "comments": [{"id": 1, "content": "gone", "publishedDate": "2026-07-02T00:00:00Z"}]},
        {"id": 2, "comments": []},
        {"id": 3, "comments": [{"id": 1, "content": "   ", "publishedDate": "2026-07-02T00:00:00Z"}]},
    ]}));
    assert!(comments.is_empty());
}

fn iteration(id: i64, head: &str, base: &str) -> Value {
    json!({"id": id, "sourceRefCommit": {"commitId": head}, "commonRefCommit": {"commitId": base}, "targetRefCommit": {"commitId": base}})
}

#[test]
fn reads_every_push_in_order_oldest_first() {
    let iterations = decode_iterations_json(&json!({"value": [iteration(2, "bbb", "base"), iteration(1, "aaa", "base")]}).to_string()).unwrap();
    assert_eq!(iterations.iter().map(|i| i.id).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(
        iterations.last(),
        Some(&AzureDevOpsIteration {
            id: 2,
            head_commit: "bbb".into(),
            merge_base_commit: "base".into()
        })
    );
}

#[test]
fn skips_a_push_azure_could_not_place_both_ends_of() {
    let iterations =
        decode_iterations_json(&json!({"value": [{"id": 1, "sourceRefCommit": {"commitId": "aaa"}}, iteration(2, "bbb", "base")]}).to_string()).unwrap();
    assert_eq!(iterations.iter().map(|i| i.id).collect::<Vec<_>>(), [2]);
}

fn changes(value: Value) -> AzureDevOpsChangePage {
    decode_iteration_changes_json(&value.to_string()).expect("a successful decode")
}

fn entry(path: &str, old_path: &str, kind: AzureDevOpsChangeKind, object_id: Option<&str>, original: Option<&str>) -> AzureDevOpsChangeEntry {
    AzureDevOpsChangeEntry {
        path: path.into(),
        old_path: old_path.into(),
        change_kind: kind,
        object_id: object_id.map(Into::into),
        original_object_id: original.map(Into::into),
    }
}

#[test]
fn names_each_changed_file_without_the_leading_slash() {
    let page = changes(json!({"changeEntries": [
        {"changeType": "add", "item": {"path": "/DEMO.md", "objectId": "ec00"}},
        {"changeType": "edit", "item": {"path": "/README.md", "objectId": "8f80", "originalObjectId": "0ca4"}},
        {"changeType": "delete", "item": {"path": "/OLD.md", "originalObjectId": "1111"}},
    ]}));
    let kinds: Vec<_> = page.changes.iter().map(|c| (c.path.as_str(), c.change_kind)).collect();
    assert_eq!(
        kinds,
        [
            ("DEMO.md", AzureDevOpsChangeKind::New),
            ("README.md", AzureDevOpsChangeKind::Change),
            ("OLD.md", AzureDevOpsChangeKind::Deleted)
        ]
    );
}

#[test]
fn keeps_a_space_at_the_end_of_a_file_name() {
    let page = changes(json!({"changeEntries": [
        {"changeType": "edit", "item": {"path": "/docs/readme.md ", "objectId": "ec00"}},
        {"changeType": "rename", "sourceServerItem": "/docs/old.md ", "item": {"path": "/docs/moved.md", "objectId": "aaaa", "originalObjectId": "aaaa"}},
    ]}));
    let paths: Vec<_> = page.changes.iter().map(|c| (c.path.as_str(), c.old_path.as_str())).collect();
    assert_eq!(paths, [("docs/readme.md ", "docs/readme.md "), ("docs/moved.md", "docs/old.md ")]);
}

#[test]
fn reads_a_rename_as_one_file_that_moved() {
    let page = changes(json!({"changeEntries": [
        {"changeType": "rename", "sourceServerItem": "/docs/old.md", "item": {"path": "/docs/new.md", "objectId": "aaaa", "originalObjectId": "aaaa"}},
        {"changeType": "edit, rename", "sourceServerItem": "/src/old.ts", "item": {"path": "/src/new.ts", "objectId": "bbbb", "originalObjectId": "cccc"}},
    ]}));
    assert_eq!(
        page.changes,
        vec![
            entry("docs/new.md", "docs/old.md", AzureDevOpsChangeKind::RenamePure, Some("aaaa"), Some("aaaa")),
            entry("src/new.ts", "src/old.ts", AzureDevOpsChangeKind::RenameChanged, Some("bbbb"), Some("cccc")),
        ]
    );
}

#[test]
fn drops_the_folders_azure_lists_alongside_the_files() {
    let page = changes(json!({"changeEntries": [
        {"changeType": "add", "item": {"path": "/docs", "isFolder": true, "gitObjectType": "tree"}},
        {"changeType": "add", "item": {"path": "/docs/page.md", "objectId": "dddd"}},
    ]}));
    assert_eq!(page.changes.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(), ["docs/page.md"]);
}

#[test]
fn carries_where_the_next_page_starts() {
    let page = changes(json!({"changeEntries": [{"changeType": "add", "item": {"path": "/DEMO.md", "objectId": "ec00"}}], "nextSkip": 2000}));
    assert_eq!(page.next_skip, Some(2000));
}

#[test]
fn reads_the_last_page_as_the_end_of_the_change() {
    let page = changes(json!({"changeEntries": [{"changeType": "add", "item": {"path": "/DEMO.md", "objectId": "ec00"}}]}));
    assert_eq!(page.next_skip, None);
}

#[test]
fn reads_where_a_rename_came_from_out_of_either_place() {
    let page = changes(
        json!({"changeEntries": [{"changeType": "rename", "originalPath": "/docs/old.md", "item": {"path": "/docs/new.md", "objectId": "aaaa", "originalObjectId": "aaaa"}}]}),
    );
    assert_eq!(page.changes[0].old_path, "docs/old.md");
}

#[test]
fn reads_the_text_out_of_the_envelope() {
    assert_eq!(
        decode_item_content_json(&json!({"path": "/a.md", "content": "one\ntwo"}).to_string()).unwrap(),
        AzureDevOpsItemContent {
            contents: "one\ntwo".into(),
            is_binary: false
        }
    );
}

#[test]
fn reads_an_empty_file_as_empty() {
    assert_eq!(
        decode_item_content_json(&json!({"path": "/a.md"}).to_string()).unwrap(),
        AzureDevOpsItemContent::default()
    );
}

#[test]
fn keeps_azures_word_that_a_file_is_binary() {
    assert_eq!(
        decode_item_content_json(&json!({"path": "/logo.png", "content": "b2xk", "contentMetadata": {"isBinary": true}}).to_string()).unwrap(),
        AzureDevOpsItemContent {
            contents: "b2xk".into(),
            is_binary: true
        }
    );
}

// What `az devops invoke` really answers with: the route's own body with one key of its own
// added, and live repositories leaving out fields the contract documents.

#[test]
fn reads_the_envelope_the_extension_adds_its_continuation_token_to() {
    assert!(threads(json!({"value": [], "count": 0, "continuation_token": null})).is_empty());
    let iterations = decode_iterations_json(
        &json!({"count": 1, "continuation_token": null, "value": [{"id": 1, "sourceRefCommit": {"commitId": "f4031105213c68f197cd46ea303bb5e72acc889a"}, "commonRefCommit": {"commitId": "acbc805382edd6c38b8d6de6ba9f9bba9da4ad35"}}]})
            .to_string(),
    )
    .unwrap();
    assert_eq!(
        iterations,
        vec![AzureDevOpsIteration {
            id: 1,
            head_commit: "f4031105213c68f197cd46ea303bb5e72acc889a".into(),
            merge_base_commit: "acbc805382edd6c38b8d6de6ba9f9bba9da4ad35".into()
        }]
    );
}

#[test]
fn keeps_the_files_of_a_change_that_names_neither_object_type_nor_next_page() {
    let page = changes(json!({"continuation_token": null, "changeEntries": [
        {"changeType": "add", "item": {"path": "/DEMO.md", "objectId": "EC005DB24"}},
        {"changeType": "edit", "item": {"path": "/README.md", "objectId": "8F8047A49"}},
    ]}));
    assert_eq!(page.next_skip, None);
    assert_eq!(
        page.changes,
        vec![
            entry("DEMO.md", "DEMO.md", AzureDevOpsChangeKind::New, Some("EC005DB24"), None),
            entry("README.md", "README.md", AzureDevOpsChangeKind::Change, Some("8F8047A49"), None),
        ]
    );
}

#[test]
fn reads_a_text_file_whose_content_type_azure_calls_a_stream() {
    let item = decode_item_content_json(
        &json!({
            "path": "/README.md", "objectId": "8f8047a49", "gitObjectType": "blob", "content": "# Demo\n",
            "contentMetadata": {"contentType": "application/octet-stream", "encoding": 65001, "extension": "md", "fileName": "README.md"},
            "continuation_token": null,
        })
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        item,
        AzureDevOpsItemContent {
            contents: "# Demo\n".into(),
            is_binary: false
        }
    );
}

#[test]
fn fails_on_a_payload_that_is_not_the_envelope() {
    assert!(decode_threads_json("not json").is_err());
    assert!(decode_threads_json("[]").is_err());
    assert!(decode_iteration_changes_json(r#"{"changeEntries": [], "nextSkip": "x"}"#).is_err());
    assert!(decode_pull_request_json(r#"{"message":"not found"}"#).is_err());
    assert!(decode_viewer_json(r#"{"user":"someone"}"#).is_err());
}
