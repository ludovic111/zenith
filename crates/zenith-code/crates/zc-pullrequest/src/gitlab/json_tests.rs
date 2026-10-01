//! `gitLabMergeRequestJson.test.ts`.

use serde_json::{json, Value};
use zc_contracts::{
    PullRequestActor, PullRequestCheck, PullRequestCheckStatus, PullRequestCommentKind, PullRequestLabel, PullRequestMergeMethod, PullRequestMergeability,
    PullRequestReaction, PullRequestReactionContent, PullRequestState,
};

use super::*;

fn base() -> Value {
    json!({
        "iid": 1,
        "title": "Add the merge requests page",
        "web_url": "https://gitlab.example.test/acme/web/-/merge_requests/1",
        "source_branch": "feat/page",
        "target_branch": "main",
        "created_at": "2026-07-01T00:00:00Z",
        "updated_at": "2026-07-02T00:00:00Z",
    })
}

fn with(entry: Value) -> Value {
    let mut value = base();
    for (key, field) in entry.as_object().unwrap() {
        value[key] = field.clone();
    }
    value
}

fn list_json(entries: Vec<Value>) -> String {
    Value::Array(entries.into_iter().map(with).collect()).to_string()
}

fn detail_json(entry: Value) -> String {
    with(entry).to_string()
}

fn list(entries: Vec<Value>) -> GitLabMergeRequestListPage {
    decode_merge_request_list_json(&list_json(entries)).unwrap()
}

fn detail(entry: Value) -> GitLabMergeRequestDetail {
    decode_merge_request_detail_json(&detail_json(entry)).unwrap()
}

fn actor(login: &str, name: Option<&str>) -> PullRequestActor {
    PullRequestActor {
        is_bot: None,
        login: login.into(),
        name: name.map(Into::into),
        avatar_url: None,
    }
}

mod merge_request_list {
    use super::*;

    #[test]
    fn reads_a_merge_request_as_a_change_request() {
        let batch = list(vec![json!({
            "iid": 42,
            "author": {"username": "bilal", "name": "Bilal"},
            "state": "opened",
            "merge_status": "can_be_merged",
            "draft": false,
            "reviewers": [{"username": "julius"}],
            "labels": ["backend", "  "],
        })]);
        assert_eq!(batch.items.len(), 1);
        let item = &batch.items[0];
        assert_eq!(item.number, 42);
        assert_eq!(item.author, Some(actor("bilal", Some("Bilal"))));
        assert_eq!(item.head_branch, "feat/page");
        assert_eq!(item.base_branch, "main");
        assert_eq!(item.state, PullRequestState::Open);
        assert!(!item.is_draft);
        assert_eq!(item.mergeability, PullRequestMergeability::Mergeable);
        assert_eq!(item.review_request_logins, vec!["julius".to_owned()]);
        assert_eq!(
            item.labels,
            vec![PullRequestLabel {
                name: "backend".into(),
                color: None
            }]
        );
    }

    #[test]
    fn reports_no_line_counts_which_gitlab_does_not_expose() {
        let batch = list(vec![json!({})]);
        assert_eq!((batch.items[0].additions, batch.items[0].deletions), (0, 0));
    }

    #[test]
    fn treats_a_merged_timestamp_as_merged_whatever_the_state_says() {
        let batch = list(vec![json!({"state": "opened", "merged_at": "2026-07-03T00:00:00Z"})]);
        assert_eq!(batch.items[0].state, PullRequestState::Merged);
    }

    #[test]
    fn keeps_a_locked_merge_request_open() {
        assert_eq!(list(vec![json!({"state": "locked"})]).items[0].state, PullRequestState::Open);
    }

    #[test]
    fn reads_the_legacy_draft_flag() {
        assert!(list(vec![json!({"work_in_progress": true})]).items[0].is_draft);
    }

    #[test]
    fn calls_a_conflicted_merge_request_conflicting_even_while_the_check_is_pending() {
        let batch = list(vec![json!({"merge_status": "checking", "has_conflicts": true})]);
        assert_eq!(batch.items[0].mergeability, PullRequestMergeability::Conflicting);
    }

    #[test]
    fn leaves_an_unfinished_merge_check_unknown() {
        assert_eq!(
            list(vec![json!({"merge_status": "checking"})]).items[0].mergeability,
            PullRequestMergeability::Unknown
        );
    }

    #[test]
    fn skips_a_malformed_row_but_still_counts_it() {
        let raw = Value::Array(vec![json!({"iid": "not a number"}), base()]).to_string();
        let batch = decode_merge_request_list_json(&raw).unwrap();
        assert_eq!(batch.items.len(), 1);
        assert_eq!(batch.raw_indexes, vec![1]);
        assert_eq!(batch.raw_count, 2);
    }

    #[test]
    fn fails_a_row_whose_optional_field_is_null_where_the_schema_takes_none() {
        // `draft: Schema.optional(Schema.Boolean)` has no `NullOr`, so a `null` fails the row.
        let batch = list(vec![json!({"draft": null}), json!({"iid": 2, "has_conflicts": null})]);
        assert_eq!(batch.items.iter().map(|item| item.number).collect::<Vec<_>>(), vec![2]);
    }
}

mod merge_request_detail {
    use super::*;

    #[test]
    fn reads_the_description_file_count_and_pipeline() {
        let detail = detail(json!({
            "description": "Ships the page.",
            "changes_count": "3",
            "reviewers": [{"username": "julius", "name": "Julius"}],
            "head_pipeline": {
                "status": "success",
                "web_url": "https://gitlab.example.test/acme/web/-/pipelines/9",
                "source": "merge_request_event",
            },
        }));
        assert_eq!(detail.body, "Ships the page.");
        assert_eq!(detail.changed_files, 3);
        assert_eq!(detail.reviewers, vec![actor("julius", Some("Julius"))]);
        assert_eq!(
            detail.checks,
            vec![PullRequestCheck {
                name: "Pipeline".into(),
                status: PullRequestCheckStatus::Success,
                description: Some("merge_request_event".into()),
                url: Some("https://gitlab.example.test/acme/web/-/pipelines/9".into()),
            }]
        );
    }

    #[test]
    fn reads_an_uncounted_change_set_as_its_floor() {
        assert_eq!(detail(json!({"changes_count": "1000+"})).changed_files, 1000);
    }

    #[test]
    fn falls_back_to_no_file_count_when_gitlab_omits_one() {
        assert_eq!(detail(json!({})).changed_files, 0);
    }

    #[test]
    fn reads_either_auto_merge_field_and_says_nothing_where_gitlab_named_neither() {
        assert_eq!(detail(json!({"merge_when_pipeline_succeeds": true})).auto_merge_enabled, Some(true));
        assert_eq!(detail(json!({"auto_merge_enabled": true})).auto_merge_enabled, Some(true));
        assert_eq!(detail(json!({"merge_when_pipeline_succeeds": false})).auto_merge_enabled, Some(false));
        assert_eq!(detail(json!({})).auto_merge_enabled, None);
    }

    #[test]
    fn keeps_the_squash_choice_stored_with_auto_merge() {
        let armed = detail(json!({"auto_merge_enabled": true, "squash_on_merge": true}));
        assert_eq!(
            (armed.auto_merge_enabled, armed.auto_merge_method),
            (Some(true), Some(PullRequestMergeMethod::Squash))
        );
        assert_eq!(
            detail(json!({"auto_merge_enabled": true, "squash": true, "squash_on_merge": false})).auto_merge_method,
            None
        );
    }

    #[test]
    fn keeps_a_divergence_gitlab_did_not_count_apart_from_a_divergence_of_none() {
        assert_eq!(detail(json!({"diverged_commits_count": 3})).diverged_commits, Some(3));
        assert_eq!(detail(json!({"diverged_commits_count": 0})).diverged_commits, Some(0));
        assert_eq!(detail(json!({})).diverged_commits, None);
        assert_eq!(detail(json!({"diverged_commits_count": null})).diverged_commits, None);
    }

    #[test]
    fn maps_a_pipeline_waiting_on_a_person_to_neutral_not_failure() {
        assert_eq!(
            detail(json!({"head_pipeline": {"status": "manual"}})).checks[0].status,
            PullRequestCheckStatus::Neutral
        );
    }

    #[test]
    fn carries_reviewer_ids_and_fails_on_a_reviewer_without_a_username() {
        assert_eq!(
            detail(json!({"reviewers": [{"id": 5, "username": "octo"}, {"username": "kit"}]})).reviewer_ids,
            vec![5]
        );
        assert!(decode_merge_request_detail_json(&detail_json(json!({"reviewers": [{"id": 5}]}))).is_err());
    }
}

mod viewer {
    use super::*;

    #[test]
    fn reads_the_signed_in_username() {
        assert_eq!(decode_viewer_json(r#"{"username":"bilal"}"#).unwrap(), Some("bilal".into()));
    }

    #[test]
    fn returns_nothing_when_the_account_has_no_username() {
        assert_eq!(decode_viewer_json(r#"{"username":"  "}"#).unwrap(), None);
    }
}

mod notes {
    use super::*;

    #[test]
    fn keeps_comments_and_drops_gitlabs_own_activity_notes() {
        let notes = decode_notes_json(
            &json!([
                {"id": 1, "body": "assigned to @bilal", "system": true, "created_at": "2026-07-01T00:00:00Z"},
                {"id": 2, "body": "Looks good.", "author": {"username": "julius"}, "created_at": "2026-07-02T00:00:00Z"},
                {"id": 3, "body": "   ", "created_at": "2026-07-03T00:00:00Z"},
            ])
            .to_string(),
        )
        .unwrap();
        assert_eq!(notes.comments.len(), 1);
        assert_eq!(notes.comments[0].id, "2");
        assert_eq!(notes.comments[0].kind, PullRequestCommentKind::IssueComment);
        assert_eq!(notes.comments[0].body, "Looks good.");
        assert_eq!(notes.raw_count, 3);
    }

    #[test]
    fn reads_a_line_note_as_a_review_comment_on_its_file() {
        let notes = decode_notes_json(
            &json!([{
                "id": 7, "type": "DiffNote", "body": "Rename this.", "created_at": "2026-07-02T00:00:00Z",
                "position": {"new_path": "src/app.ts", "old_path": "src/old.ts"},
            }])
            .to_string(),
        )
        .unwrap();
        assert_eq!(notes.comments[0].kind, PullRequestCommentKind::ReviewComment);
        assert_eq!(notes.comments[0].path.as_deref(), Some("src/app.ts"));
    }

    #[test]
    fn falls_back_to_the_old_path_for_a_note_on_a_deleted_line() {
        let notes = decode_notes_json(
            &json!([{
                "id": 8, "type": "DiffNote", "body": "Gone.", "created_at": "2026-07-02T00:00:00Z",
                "position": {"new_path": null, "old_path": "src/old.ts"},
            }])
            .to_string(),
        )
        .unwrap();
        assert_eq!(notes.comments[0].path.as_deref(), Some("src/old.ts"));
    }
}

mod commits {
    use super::*;

    fn oids(raw: Value) -> Vec<String> {
        decode_commits_json(&raw.to_string()).unwrap().into_iter().map(|commit| commit.oid).collect()
    }

    #[test]
    fn returns_commits_oldest_first() {
        assert_eq!(
            oids(json!([
                {"id": "bbb", "title": "second", "committed_date": "2026-07-02T00:00:00Z"},
                {"id": "aaa", "title": "first", "committed_date": "2026-07-01T00:00:00Z"},
            ])),
            vec!["aaa", "bbb"]
        );
    }

    #[test]
    fn skips_commits_whose_id_is_empty() {
        assert_eq!(
            oids(json!([
                {"id": "   ", "title": "invalid", "committed_date": "2026-07-02T00:00:00Z"},
                {"id": "aaa", "committed_date": "2026-07-01T00:00:00Z"},
            ])),
            vec!["aaa"]
        );
    }

    #[test]
    fn falls_back_to_the_creation_timestamp_when_there_is_no_commit_date() {
        let commits = decode_commits_json(
            &json!([{"id": "aaa", "created_at": "2026-07-01T00:00:00+08:00", "author_name": "Ada Example", "author_email": "ada@example.test"}]).to_string(),
        )
        .unwrap();
        assert_eq!(commits[0].oid, "aaa");
        assert_eq!(commits[0].committed_date, "2026-07-01T00:00:00+08:00");
        assert_eq!(commits[0].authors, Some(vec![actor("Ada Example", Some("Ada Example"))]));
    }

    #[test]
    fn carries_commit_additions_and_deletions_when_gitlab_returns_stats() {
        let commits = decode_commits_json(
            &json!([{"id": "aaa", "committed_date": "2026-07-01T00:00:00Z", "stats": {"additions": 21, "deletions": 8, "total": 29}}]).to_string(),
        )
        .unwrap();
        assert_eq!((commits[0].additions, commits[0].deletions), (Some(21), Some(8)));
    }
}

mod merge_request_diffs {
    use super::*;

    fn patch(raw: Value) -> GitLabMergeRequestPatch {
        decode_merge_request_diffs_json(&raw.to_string()).unwrap()
    }

    #[test]
    fn quotes_literal_backslashes_without_interpreting_them_as_escapes() {
        let result = patch(json!([{"old_path": r"src\notes.ts", "new_path": r"src\notes.ts", "diff": "@@ -1 +1 @@\n-old\n+new"}]));
        assert_eq!(
            result.patch,
            [
                r#"diff --git "a/src\\notes.ts" "b/src\\notes.ts""#,
                r#"--- "a/src\\notes.ts""#,
                r#"+++ "b/src\\notes.ts""#,
                "@@ -1 +1 @@",
                "-old",
                "+new",
                "",
            ]
            .join("\n")
        );
    }

    #[test]
    fn preserves_spaces_and_literal_backslashes_in_both_rename_paths() {
        let result = patch(json!([{"old_path": r" old\name.ts ", "new_path": r" new\name.ts ", "renamed_file": true, "diff": ""}]));
        assert_eq!(
            result.patch,
            [
                r#"diff --git "a/ old\\name.ts " "b/ new\\name.ts ""#,
                r#"rename from " old\\name.ts ""#,
                r#"rename to " new\\name.ts ""#,
                r#"--- "a/ old\\name.ts ""#,
                r#"+++ "b/ new\\name.ts ""#,
            ]
            .join("\n")
        );
    }

    #[test]
    fn assembles_a_unified_patch_gitlab_does_not_return() {
        let result = patch(json!([{"old_path": "src/app.ts", "new_path": "src/app.ts", "diff": "@@ -1 +1 @@\n-old\n+new\n"}]));
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
    }

    #[test]
    fn points_a_new_file_at_dev_null_on_the_left_and_a_deleted_file_on_the_right() {
        let result = patch(json!([
            {"old_path": "src/new.ts", "new_path": "src/new.ts", "new_file": true, "b_mode": "100755", "diff": "@@ -0,0 +1 @@\n+hello\n"},
            {"old_path": "src/gone.ts", "new_path": "src/gone.ts", "deleted_file": true, "diff": "@@ -1 +0,0 @@\n-bye\n"},
        ]));
        for expected in ["new file mode 100755", "--- /dev/null", "deleted file mode 100644", "+++ /dev/null"] {
            assert!(result.patch.contains(expected), "{expected}");
        }
    }

    #[test]
    fn records_a_rename_so_the_patch_names_both_paths() {
        let result = patch(json!([{"old_path": "src/old.ts", "new_path": "src/new.ts", "renamed_file": true, "diff": ""}]));
        assert!(result.patch.contains("rename from src/old.ts"));
        assert!(result.patch.contains("rename to src/new.ts"));
    }

    #[test]
    fn reports_truncation_for_a_file_gitlab_refused_to_inline() {
        let result = patch(json!([{"old_path": "big.bin", "new_path": "big.bin", "diff": "", "too_large": true}]));
        assert!(result.truncated);
        assert!(result.patch.contains("diff --git a/big.bin b/big.bin"));
    }

    #[test]
    fn reports_how_many_files_gitlab_returned_so_the_caller_can_page() {
        let files: Vec<Value> = (0..3)
            .map(|index| json!({"old_path": format!("src/{index}.ts"), "new_path": format!("src/{index}.ts"), "diff": "@@ -1 +1 @@\n-a\n+b\n"}))
            .collect();
        let result = patch(Value::Array(files));
        assert_eq!(result.raw_count, 3);
        assert!(!result.truncated);
        assert!(result.patch.contains("src/2.ts"));
    }

    #[test]
    fn fails_when_gitlab_did_not_return_a_list() {
        assert!(decode_merge_request_diffs_json(r#"{"message":"404"}"#).is_err());
    }
}

mod merge_request_viewer_fields {
    use super::*;

    #[test]
    fn carries_gitlabs_own_answer_for_whether_this_viewer_can_merge() {
        assert!(!detail(json!({"user": {"can_merge": false}})).viewer_can_merge);
        assert!(detail(json!({"user": {"can_merge": true}})).viewer_can_merge);
    }

    #[test]
    fn leaves_merging_permitted_where_gitlab_answered_without_the_field() {
        assert!(detail(json!({})).viewer_can_merge);
        assert!(detail(json!({"user": null})).viewer_can_merge);
    }
}

mod award_emoji {
    use super::*;

    fn reaction(content: PullRequestReactionContent, count: i64, actors: &[&str], viewer_has_reacted: bool) -> PullRequestReaction {
        PullRequestReaction {
            content,
            count,
            actors: actors.iter().map(|actor| (*actor).to_owned()).collect(),
            viewer_has_reacted,
        }
    }

    #[test]
    fn reads_mr_awards_keys_note_awards_by_rest_id_and_ignores_an_award_outside_the_eight() {
        let result = decode_award_emoji_json(
            &json!({"data": {
                "currentUser": {"username": "bilal"},
                "project": {"mergeRequest": {
                    "awardEmoji": {"nodes": [
                        {"name": "thumbsup", "user": {"username": "bilal"}},
                        {"name": "thumbsup", "user": {"username": "julius"}},
                    ]},
                    "notes": {
                        "pageInfo": {"hasNextPage": false, "endCursor": null},
                        "nodes": [
                            {"id": "gid://gitlab/DiffNote/42", "awardEmoji": {"nodes": [{"name": "heart", "user": {"username": "julius"}}]}},
                            {"id": "gid://gitlab/Note/7", "awardEmoji": {"nodes": [{"name": "partyparrot", "user": {"username": "bilal"}}]}},
                        ],
                    },
                }},
            }})
            .to_string(),
        )
        .unwrap();
        assert_eq!(result.reactions, vec![reaction(PullRequestReactionContent::ThumbsUp, 2, &["julius"], true)]);
        assert_eq!(
            result.reactions_by_note_id.into_entries(),
            vec![("42".to_owned(), vec![reaction(PullRequestReactionContent::Heart, 1, &["julius"], false)])]
        );
        assert_eq!(result.next_cursor, None);
    }

    #[test]
    fn hands_back_a_cursor_when_gitlab_has_more_notes_to_page() {
        let result = decode_award_emoji_json(
            &json!({"data": {"currentUser": null, "project": {"mergeRequest": {
                "awardEmoji": {"nodes": []},
                "notes": {"pageInfo": {"hasNextPage": true, "endCursor": "Y3Vyc29yOjE"}, "nodes": []},
            }}}})
            .to_string(),
        )
        .unwrap();
        assert_eq!(result.next_cursor.as_deref(), Some("Y3Vyc29yOjE"));
    }

    #[test]
    fn matches_the_viewers_username_case_insensitively() {
        let result = decode_award_emoji_json(
            &json!({"data": {"currentUser": {"username": "Bilal"}, "project": {"mergeRequest": {
                "awardEmoji": {"nodes": [
                    {"name": "heart", "user": {"username": "bilal"}},
                    {"name": "heart", "user": {"username": "julius"}},
                ]},
                "notes": {"pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": []},
            }}}})
            .to_string(),
        )
        .unwrap();
        assert_eq!(result.reactions, vec![reaction(PullRequestReactionContent::Heart, 2, &["julius"], true)]);
    }
}

mod own_award {
    use super::*;

    const AWARDS: &str = r#"[{"id":101,"name":"thumbsup","user":{"username":"julius"}},{"id":102,"name":"thumbsup","user":{"username":"bilal"}}]"#;

    #[test]
    fn finds_the_readers_own_award_of_that_name() {
        assert_eq!(
            decode_own_award_id_json(AWARDS, PullRequestReactionContent::ThumbsUp, "bilal").unwrap(),
            Some(102)
        );
    }

    #[test]
    fn returns_nothing_where_the_reader_has_no_award_of_that_name() {
        assert_eq!(decode_own_award_id_json(AWARDS, PullRequestReactionContent::Heart, "bilal").unwrap(), None);
    }
}

#[test]
fn award_names_spell_the_contents_whose_gitlab_name_is_not_their_own() {
    assert_eq!(gitlab_award_name(PullRequestReactionContent::ThumbsUp), "thumbsup");
    assert_eq!(gitlab_award_name(PullRequestReactionContent::Laugh), "laughing");
    assert_eq!(gitlab_award_name(PullRequestReactionContent::Hooray), "tada");
}

mod repository_blobs {
    use super::*;

    fn blobs(nodes: Value) -> Vec<(String, String)> {
        decode_repository_blobs_json(&json!({"data": {"project": {"repository": {"blobs": {"nodes": nodes}}}}}).to_string())
            .unwrap()
            .expect("an answered blobs query")
            .into_entries()
    }

    fn pairs(entries: &[(&str, &str)]) -> Vec<(String, String)> {
        entries.iter().map(|(path, oid)| ((*path).to_owned(), (*oid).to_owned())).collect()
    }

    #[test]
    fn reads_a_blob_id_per_path() {
        assert_eq!(
            blobs(json!([{"path": "src/a.ts", "oid": "aaa111"}, {"path": "src/b.ts", "oid": "bbb222"}])),
            pairs(&[("src/a.ts", "aaa111"), ("src/b.ts", "bbb222")])
        );
    }

    #[test]
    fn leaves_out_a_node_missing_either_half() {
        assert_eq!(
            blobs(json!([{"path": "src/a.ts", "oid": null}, {"path": null, "oid": "bbb222"}, null, {"path": "src/c.ts", "oid": "ccc333"}])),
            pairs(&[("src/c.ts", "ccc333")])
        );
    }

    #[test]
    fn keys_a_blob_by_the_path_the_host_spelled_spaces_and_all() {
        assert_eq!(
            blobs(json!([
                {"path": " leading.ts", "oid": "aaa111"},
                {"path": "trailing.ts ", "oid": "bbb222"},
                {"path": "   ", "oid": "ccc333"},
                {"path": "", "oid": "ddd444"},
            ])),
            pairs(&[(" leading.ts", "aaa111"), ("trailing.ts ", "bbb222"), ("   ", "ccc333")])
        );
    }

    #[test]
    fn tells_a_project_the_reader_cannot_see_from_a_revision_with_none_of_the_files() {
        assert_eq!(decode_repository_blobs_json(r#"{"data":{"project":null}}"#).unwrap(), None);
        assert_eq!(decode_repository_blobs_json(r#"{"data":{"project":{"repository":null}}}"#).unwrap(), None);
        assert_eq!(
            decode_repository_blobs_json(r#"{"data":{"project":{"repository":{"blobs":{"nodes":[]}}}}}"#).unwrap(),
            Some(OrderedMap::new())
        );
    }

    #[test]
    fn fails_on_output_that_is_not_the_querys_shape() {
        assert!(decode_repository_blobs_json("not json").is_err());
        assert!(decode_repository_blobs_json(r#"{"errors":[]}"#).is_err());
    }
}
