//! `GitLabPullRequestProvider.test.ts`, plus the provider's error mapping, optional methods and
//! activity fallbacks, against a scripted `glab`.

#![allow(clippy::result_large_err, clippy::too_many_arguments)]

mod support_gitlab;

use serde_json::json;
use support_gitlab::*;
use zc_contracts::{PullRequestAction, PullRequestBaseComparison, PullRequestCommentUpdateInputKind, PullRequestReviewVerdict, PullRequestUpdateMethod};
use zc_core::defect::Defect;
use zc_core::process::{ProcessInvocation, ProcessRunError};
use zc_pullrequest::gitlab::provider::{gitlab_capabilities, gitlab_viewer_permissions};
use zc_pullrequest::provider::{ChangeRequestRef, OptionalMethods, UpdateChangeRequestInput, UpdateCommentInput, ViewerPermissionsInput};
use zc_pullrequest::{ProviderFailureReason, PullRequestProviderApi};

fn reference() -> ChangeRequestRef {
    ChangeRequestRef {
        cwd: "/w".into(),
        repository: "acme/web".into(),
        host: "gitlab.example.test".into(),
        number: 7,
    }
}

mod viewer_permissions {
    use super::*;

    #[test]
    fn offers_everything_to_a_viewer_gitlab_says_can_merge() {
        let permissions = serde_json::to_value(gitlab_viewer_permissions(true)).unwrap();
        assert_eq!(
            permissions,
            json!({
                "actions": ["merge", "ready", "draft", "close", "reopen", "update-branch", "enable-auto-merge", "disable-auto-merge"],
                "comment": true,
                "resolve": true,
                "verdicts": ["comment", "approve"],
                "requestReviewers": true,
                "updateMethods": ["rebase"],
            })
        );
    }

    #[test]
    fn keeps_merge_now_and_later_from_a_viewer_gitlab_says_cannot() {
        let permissions = serde_json::to_value(gitlab_viewer_permissions(false)).unwrap();
        assert_eq!(
            permissions,
            json!({
                "actions": ["ready", "draft", "close", "reopen"],
                "comment": true,
                "resolve": true,
                "verdicts": ["comment", "approve"],
                "requestReviewers": true,
            })
        );
    }

    #[test]
    fn names_no_way_of_updating_a_branch_it_will_not_let_this_viewer_update() {
        assert_eq!(gitlab_viewer_permissions(false).update_methods, None);
        assert_eq!(gitlab_viewer_permissions(true).update_methods, Some(vec![PullRequestUpdateMethod::Rebase]));
    }

    #[test]
    fn treats_an_author_with_read_access_as_any_other_reader() {
        assert_eq!(
            gitlab_viewer_permissions(false).actions,
            vec![
                PullRequestAction::Ready,
                PullRequestAction::Draft,
                PullRequestAction::Close,
                PullRequestAction::Reopen
            ]
        );
    }
}

mod base_freshness {
    use super::*;

    async fn read_with(divergence: serde_json::Value) -> zc_pullrequest::provider::ProviderChangeRequestDetail {
        let runner = ScriptedRunner::new();
        let mut merge_request = json!({
            "iid": 7,
            "title": "Merge request 7",
            "web_url": "https://gitlab.example.test/acme/web/-/merge_requests/7",
            "source_branch": "feat/page",
            "target_branch": "main",
            "merge_status": "can_be_merged",
            "changes_count": "1",
            "created_at": "2026-07-01T00:00:00Z",
            "updated_at": "2026-07-02T00:00:00Z",
        });
        for (key, value) in divergence.as_object().unwrap() {
            merge_request[key] = value.clone();
        }
        let detail = merge_request.to_string();
        runner.implement(move |request| {
            if request.args[1].starts_with("projects/acme%2Fweb?license=false") {
                out(r#"{"merge_method":"merge","squash_option":"default_on"}"#)
            } else {
                out(&detail)
            }
        });
        runner.provider().get_change_request(reference()).await.unwrap()
    }

    #[tokio::test]
    async fn reads_a_counted_divergence_as_a_branch_that_has_fallen_behind() {
        let detail = read_with(json!({"diverged_commits_count": 3})).await;
        assert_eq!(detail.base_comparison, Some(PullRequestBaseComparison::Behind));
        assert_eq!(detail.behind_by, Some(3));
    }

    #[tokio::test]
    async fn reads_a_divergence_of_none_as_a_branch_that_is_current() {
        let detail = read_with(json!({"diverged_commits_count": 0})).await;
        assert_eq!(detail.base_comparison, Some(PullRequestBaseComparison::UpToDate));
        assert_eq!(detail.behind_by, Some(0));
    }

    #[tokio::test]
    async fn says_nothing_at_all_where_gitlab_counted_nothing() {
        let detail = read_with(json!({})).await;
        assert_eq!(detail.base_comparison, Some(PullRequestBaseComparison::Unknown));
        assert_eq!(detail.behind_by, None);
        assert_eq!(detail.changed_files, 1);
        assert!(detail.merge_capabilities.merge && detail.merge_capabilities.squash);
    }
}

mod rewriting {
    use super::*;

    #[tokio::test]
    async fn sends_only_the_half_of_the_merge_request_the_reader_rewrote() {
        let runner = ScriptedRunner::new();
        runner.always(out("{}"));
        let provider = runner.provider();
        assert!(provider.optional_methods().update_change_request);
        provider
            .update_change_request(UpdateChangeRequestInput {
                change_request: reference(),
                title: None,
                body: Some("What this changes.".into()),
            })
            .await
            .unwrap();
        assert_eq!(runner.path(0), "projects/acme%2Fweb/merge_requests/7");
        assert_eq!(runner.body(0), json!({"description": "What this changes."}));
    }

    #[tokio::test]
    async fn rewrites_a_positioned_comment_through_the_same_note_as_any_other() {
        let runner = ScriptedRunner::new();
        runner.always(out("{}"));
        let provider = runner.provider();
        assert!(provider.optional_methods().update_comment);
        provider
            .update_comment(UpdateCommentInput {
                change_request: reference(),
                comment_id: "42".into(),
                kind: PullRequestCommentUpdateInputKind::ReviewComment,
                body: "Reworded.".into(),
            })
            .await
            .unwrap();
        assert_eq!(runner.path(0), "projects/acme%2Fweb/merge_requests/7/notes/42");
        assert_eq!(runner.body(0), json!({"body": "Reworded."}));
    }
}

#[test]
fn declares_the_capabilities_and_optional_methods_of_the_ts_provider() {
    let capabilities = serde_json::to_value(gitlab_capabilities()).unwrap();
    assert_eq!(
        capabilities,
        json!({
            "diff": true,
            "comment": true,
            "actions": ["merge", "ready", "draft", "close", "reopen", "update-branch", "enable-auto-merge", "disable-auto-merge"],
            "mergeMethods": ["merge", "squash", "rebase"],
            "updateMethods": ["rebase"],
            "search": true,
            "reactions": true,
            "viewedFiles": "environment",
            "review": {"inlineComment": true, "reply": true, "resolve": true, "verdicts": ["comment", "approve"]},
            "reviewers": {"request": true, "listCandidates": true},
            "edit": {"changeRequest": true, "comment": true},
        })
    );
    let runner = ScriptedRunner::new();
    assert_eq!(
        runner.provider().optional_methods(),
        OptionalMethods {
            get_file_revisions: true,
            update_change_request: true,
            update_comment: true,
            ..OptionalMethods::default()
        }
    );
}

#[tokio::test]
async fn reports_a_missing_glab_as_a_missing_tool() {
    let runner = ScriptedRunner::new();
    runner.always(Err(ProcessRunError::Spawn {
        invocation: ProcessInvocation {
            command: "glab".into(),
            argument_count: 2,
            cwd: Some("/w".into()),
            spawn_cwd: None,
        },
        resolved_command: Some("glab".into()),
        resolved_argument_count: Some(2),
        shell: Some(false),
        cause: Defect::error("Error", "No such file or directory (os error 2)"),
    }));
    let error = runner
        .provider()
        .get_viewer(zc_pullrequest::provider::ProviderHostRef { cwd: "/w".into(), host: None })
        .await
        .unwrap_err();
    assert_eq!(error.reason, ProviderFailureReason::MissingTool);
    assert_eq!(error.operation, "getViewer");
    assert_eq!(error.detail, "GitLab CLI (`glab`) is required but not available on PATH.");
    assert_eq!(
        error.cause.as_ref().and_then(|cause| cause.name()).as_deref(),
        Some("GitLabCliUnavailableError")
    );
}

#[tokio::test]
async fn reports_an_unauthenticated_glab_and_a_rate_limit_by_reason() {
    let runner = ScriptedRunner::new();
    runner.once(exit(1, "", "401 Unauthorized")).once(exit(1, "", "429 Too Many Requests"));
    let provider = runner.provider();
    let unauthenticated = provider
        .get_viewer_permissions(ViewerPermissionsInput {
            change_request: reference(),
            include_update_branch: None,
        })
        .await
        .unwrap_err();
    assert_eq!(unauthenticated.reason, ProviderFailureReason::Unauthenticated);
    let limited = provider
        .get_viewer_permissions(ViewerPermissionsInput {
            change_request: reference(),
            include_update_branch: None,
        })
        .await
        .unwrap_err();
    assert_eq!(limited.reason, ProviderFailureReason::RateLimited);
}

#[tokio::test]
async fn reports_an_unreadable_answer_as_a_failed_read_named_after_it() {
    let runner = ScriptedRunner::new();
    runner.always(out("not json"));
    let error = runner
        .provider()
        .get_viewer_permissions(ViewerPermissionsInput {
            change_request: reference(),
            include_update_branch: None,
        })
        .await
        .unwrap_err();
    assert_eq!(error.reason, ProviderFailureReason::Failed);
    assert_eq!(
        error.message(),
        "gitlab failed in getViewerPermissions: GitLab CLI returned an unreadable getMergeRequestDetail response."
    );
    let cause = error.cause.unwrap().defect();
    assert_eq!(cause["name"], "GitLabMergeRequestReadError");
    assert_eq!(cause["cause"]["name"], "SchemaError");
}

#[tokio::test]
async fn keeps_the_conversation_when_one_of_its_reads_fails() {
    let runner = ScriptedRunner::new();
    runner.implement(|request| {
        let path = request.args[1].as_str();
        if path.contains("/notes?") {
            out(r#"[{"id":11,"body":"First.","author":{"username":"julius"},"created_at":"2026-07-01T00:00:00Z"}]"#)
        } else if path == "graphql" {
            out(r#"{"data":{"currentUser":{"username":"bilal"},"project":{"mergeRequest":{"awardEmoji":{"nodes":[]},"notes":{"pageInfo":{"hasNextPage":false},"nodes":[{"id":"gid://gitlab/Note/11","awardEmoji":{"nodes":[{"name":"heart","user":{"username":"bilal"}}]}}]}}}}}"#)
        } else {
            exit(1, "", "boom")
        }
    });
    let activity = runner.provider().get_change_request_activity(reference()).await.unwrap();
    assert_eq!(activity.comment_count, 1);
    // The discussions read failed, so the conversation is reported as cut short.
    assert!(activity.comments_truncated);
    assert!(activity.commits.is_empty());
    assert_eq!(activity.comments[0].reactions.as_ref().map(Vec::len), Some(1));
    assert_eq!(activity.reactions, Some(Vec::new()));
}

#[tokio::test]
async fn submits_an_approval_through_the_provider() {
    let runner = ScriptedRunner::new();
    runner.always(out("{}"));
    runner
        .provider()
        .submit_review(zc_pullrequest::provider::SubmitReviewInput {
            change_request: reference(),
            verdict: PullRequestReviewVerdict::Approve,
            body: "  ".into(),
            comments: Vec::new(),
        })
        .await
        .unwrap();
    assert_eq!(runner.count(), 1);
    assert_eq!(runner.path(0), "projects/acme%2Fweb/merge_requests/7/approve");
}
