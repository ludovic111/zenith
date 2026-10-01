//! `githubStackActions.test.ts`: merging and rebasing a host-native stack against a fake `gh`
//! answering a script of responses in order.

#![allow(clippy::result_large_err)]

mod support_github;

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use support_github::*;
use zc_contracts::{PullRequestAction, PullRequestMergeMethod, PullRequestStackHead};
use zc_pullrequest::github::stack_actions::{run_github_stack_action, GitHubStackActionErrorKind, GitHubStackActionInput, StackActionFailure};
use zc_sourcecontrol::util::ManualClock;

fn stack() -> Value {
    json!([{
        "number": 50,
        "url": "https://api.github.com/repos/acme/web/stacks/50",
        "base": {"ref": "main"},
        "pull_requests": [
            {"number": 1, "title": "Base", "head": {"ref": "base", "sha": "aaa"}, "state": "closed", "merged_at": "2026-01-01T00:00:00Z"},
            {"number": 2, "title": "Middle", "head": {"ref": "middle", "sha": "bbb"}, "state": "open", "draft": false},
            {"number": 3, "title": "Top", "head": {"ref": "top", "sha": "ccc"}, "state": "open", "draft": false},
        ],
    }])
}

fn heads(heads: &[(i64, &str)]) -> Option<Vec<PullRequestStackHead>> {
    Some(
        heads
            .iter()
            .map(|(number, head_sha)| PullRequestStackHead {
                number: *number,
                head_sha: (*head_sha).into(),
            })
            .collect(),
    )
}

fn input() -> GitHubStackActionInput {
    GitHubStackActionInput {
        cwd: "/repo".into(),
        repository: "acme/web".into(),
        host: "github.com".into(),
        number: 3,
        stack_number: 50,
        expected_stack_heads: heads(&[(2, "bbb"), (3, "ccc")]),
        action: PullRequestAction::Merge,
        merge_method: None,
    }
}

fn rebase() -> GitHubStackActionInput {
    GitHubStackActionInput {
        action: PullRequestAction::UpdateBranch,
        ..input()
    }
}

fn access() -> Value {
    json!({"data": {"repository": {
        "pr2": {"headRepository": {"viewerPermission": "WRITE"}, "maintainerCanModify": false},
        "pr3": {"headRepository": {"viewerPermission": "WRITE"}, "maintainerCanModify": false},
    }}})
}

fn access_with_pr3(permission: &str, maintainer_can_modify: bool) -> Value {
    let mut access = access();
    access["data"]["repository"]["pr3"] = json!({"headRepository": {"viewerPermission": permission}, "maintainerCanModify": maintainer_can_modify});
    access
}

fn branch(number: i64, head_ref_oid: &str, behind_by: i64, processed: &[&str]) -> Value {
    json!({"data": {
        "processed": processed.iter().map(|oid| json!({"headRefOid": oid})).collect::<Vec<_>>(),
        "repository": {"pullRequest": {"id": format!("PR_{number}"), "headRefOid": head_ref_oid, "baseRef": {"compare": {"behindBy": behind_by}}}},
    }})
}

fn rebased() -> Value {
    json!({"data": {"updatePullRequestBranch": {"pullRequest": {"headRefOid": "rebased-sha"}}}})
}

fn rebase_responses() -> Vec<Value> {
    vec![branch(2, "bbb", 1, &[]), rebased(), branch(3, "ccc", 1, &["rebased-sha"]), rebased()]
}

/// `fake(responses)`: each call takes the next response; one past the script is a bug.
fn fake(responses: Vec<Value>) -> Arc<FakeGh> {
    let gh = FakeGh::new();
    let queue = Arc::new(Mutex::new(responses));
    gh.respond(move |_| {
        let mut queue = queue.lock().unwrap();
        assert!(!queue.is_empty(), "Unexpected GitHub request");
        json(queue.remove(0))
    });
    gh
}

async fn run(gh: &Arc<FakeGh>, input: GitHubStackActionInput) -> Result<(), StackActionFailure> {
    run_github_stack_action(&github(gh, &ManualClock::new(0)), input).await
}

fn stack_failure(result: Result<(), StackActionFailure>) -> (GitHubStackActionErrorKind, i64, String) {
    match result {
        Err(StackActionFailure::Stack(error)) => (error.kind, error.number, error.message()),
        other => panic!("expected a stack failure, got {other:?}"),
    }
}

fn is_mutation(args: &[String]) -> bool {
    args.iter().any(|arg| arg.starts_with("query=mutation"))
}

#[tokio::test]
async fn submits_one_atomic_merge_with_the_reviewed_head_and_respects_the_merge_queue() {
    let gh = fake(vec![stack(), json!({"status": "enqueued", "details": {}})]);
    run(
        &gh,
        GitHubStackActionInput {
            merge_method: Some(PullRequestMergeMethod::Squash),
            ..input()
        },
    )
    .await
    .unwrap();
    assert_eq!(gh.count(), 2);
    let args = gh.args(1);
    for expected in ["repos/acme/web/pulls/3/merge-async", "sha=ccc", "merge_action=default", "merge_method=squash"] {
        assert!(args.contains(&expected.to_owned()), "{expected}");
    }
    assert_eq!(
        args,
        strings(&[
            "api",
            "--hostname",
            "github.com",
            "--method",
            "PUT",
            "repos/acme/web/pulls/3/merge-async",
            "-f",
            "merge_method=squash",
            "-f",
            "merge_action=default",
            "-f",
            "sha=ccc",
        ])
    );
    assert_eq!(
        gh.args(0),
        strings(&["api", "--hostname", "github.com", "repos/acme/web/stacks?pull_request=3"])
    );
}

#[tokio::test]
async fn merges_through_the_selected_layer_without_including_later_draft_layers() {
    let mut five = stack();
    five[0]["pull_requests"] = Value::Array(
        (0..5)
            .map(|index| json!({"number": index + 1, "head": {"ref": format!("layer-{}", index + 1), "sha": format!("sha-{}", index + 1)}, "state": "open", "draft": index >= 3}))
            .collect(),
    );
    let gh = fake(vec![five, json!({"status": "merged", "details": {}})]);
    run(
        &gh,
        GitHubStackActionInput {
            number: 3,
            expected_stack_heads: heads(&[(1, "sha-1"), (2, "sha-2"), (3, "sha-3")]),
            ..input()
        },
    )
    .await
    .unwrap();
    assert_eq!(gh.count(), 2);
    for expected in ["repos/acme/web/pulls/3/merge-async", "sha=sha-3", "merge_action=default"] {
        assert!(gh.args(1).contains(&expected.to_owned()));
    }
}

#[tokio::test]
async fn rejects_stale_reviewed_heads_below_a_selected_middle_layer() {
    let gh = fake(vec![stack()]);
    let result = run(
        &gh,
        GitHubStackActionInput {
            number: 2,
            expected_stack_heads: heads(&[(2, "old-head")]),
            ..input()
        },
    )
    .await;
    assert!(matches!(stack_failure(result).0, GitHubStackActionErrorKind::Changed { .. }));
    assert_eq!(gh.count(), 1);
}

#[tokio::test]
async fn does_not_merge_from_an_already_merged_layer() {
    let gh = fake(vec![stack()]);
    let result = run(
        &gh,
        GitHubStackActionInput {
            number: 1,
            expected_stack_heads: heads(&[]),
            ..input()
        },
    )
    .await;
    assert_eq!(stack_failure(result).0, GitHubStackActionErrorKind::Unsupported);
    assert_eq!(gh.count(), 1);
}

#[tokio::test(start_paused = true)]
async fn polls_an_accepted_merge_and_reports_a_later_rule_rejection() {
    let gh = fake(vec![
        stack(),
        json!({"status": "pending", "details": {"uuid": "operation"}}),
        json!({"status": "failed", "details": {"message": "Required checks have not passed"}}),
    ]);
    let result = run(&gh, input()).await;
    assert_eq!(stack_failure(result).0, GitHubStackActionErrorKind::MergeRejected);
    assert!(gh.args(2).contains(&"repos/acme/web/pulls/3/merge-async/operation".to_owned()));
}

#[tokio::test]
async fn retains_stack_identity_and_a_rejection_response_without_a_message() {
    let gh = fake(vec![stack(), json!({"status": "failed", "details": {}})]);
    let Err(StackActionFailure::Stack(error)) = run(&gh, input()).await else {
        panic!("expected a stack failure");
    };
    assert_eq!(error.tag(), "GitHubStackMergeRejectedError");
    assert_eq!(error.repository, "acme/web");
    assert_eq!(error.number, 3);
    assert_eq!(error.stack_number, 50);
    assert_eq!(error.cause.unwrap().defect(), json!({"status": "failed", "details": {}}));
}

#[tokio::test]
async fn refuses_a_changed_stack_before_performing_any_mutation() {
    let gh = fake(vec![stack()]);
    let result = run(
        &gh,
        GitHubStackActionInput {
            expected_stack_heads: heads(&[(2, "old"), (3, "ccc")]),
            ..input()
        },
    )
    .await;
    assert!(matches!(stack_failure(result).0, GitHubStackActionErrorKind::Changed { .. }));
    assert_eq!(gh.count(), 1);
}

#[tokio::test]
async fn rebases_unmerged_layers_bottom_to_top_without_local_git_commands() {
    let mut responses = vec![stack(), access()];
    responses.extend(rebase_responses());
    let gh = fake(responses);
    run(&gh, rebase()).await.unwrap();
    let mutations: Vec<Vec<String>> = gh.calls().into_iter().map(|call| call.args).filter(|args| is_mutation(args)).collect();
    assert_eq!(mutations.len(), 2);
    assert!(mutations[0].contains(&"id=PR_2".to_owned()));
    assert!(mutations[0].contains(&"sha=bbb".to_owned()));
    assert!(mutations[1].contains(&"id=PR_3".to_owned()));
    assert!(mutations[1].contains(&"sha=ccc".to_owned()));
    assert!(gh.calls().iter().all(|call| call.command == "gh" && call.args[0] == "api"));
    // The documents, verbatim.
    assert_eq!(
        gh.args(1).last().unwrap(),
        "query=query($owner:String!,$name:String!){repository(owner:$owner,name:$name){pr2:pullRequest(number:2){headRepository{viewerPermission} maintainerCanModify} pr3:pullRequest(number:3){headRepository{viewerPermission} maintainerCanModify}}}"
    );
    assert_eq!(
        gh.args(2),
        strings(&[
            "api",
            "--hostname",
            "github.com",
            "graphql",
            "-f",
            "owner=acme",
            "-f",
            "name=web",
            "-F",
            "number=2",
            "-f",
            "sha=bbb",
            "-f",
            "query=query($owner:String!,$name:String!,$number:Int!,$sha:String!){ repository(owner:$owner,name:$name){pullRequest(number:$number){id headRefOid baseRef{compare(headRef:$sha){behindBy}}}}}",
        ])
    );
    assert_eq!(
        gh.args(3).last().unwrap(),
        "query=mutation($id:ID!,$sha:GitObjectID!){updatePullRequestBranch(input:{pullRequestId:$id,expectedHeadOid:$sha,updateMethod:REBASE}){pullRequest{headRefOid}}}"
    );
    assert!(gh.args(4).last().unwrap().starts_with(
        r#"query=query($owner:String!,$name:String!,$number:Int!,$sha:String!){processed:nodes(ids:["PR_2"]){... on PullRequest{headRefOid}} repository("#
    ));
}

#[tokio::test]
async fn does_not_update_later_layers_after_a_rebase_failure() {
    let gh = FakeGh::new();
    let queue = Arc::new(Mutex::new(vec![stack(), access(), branch(2, "bbb", 1, &[])]));
    gh.respond(move |input| {
        if is_mutation(&input.args) {
            return unauthenticated();
        }
        json(queue.lock().unwrap().remove(0))
    });
    let (kind, number, _) = stack_failure(run(&gh, rebase()).await);
    assert_eq!(kind, GitHubStackActionErrorKind::RebaseFailed { completed: 0 });
    assert_eq!(number, 2);
}

#[tokio::test]
async fn refuses_the_entire_rebase_before_mutation_when_a_later_fork_denies_write_access() {
    let gh = fake(vec![stack(), access_with_pr3("READ", false)]);
    let (kind, _, _) = stack_failure(run(&gh, rebase()).await);
    assert_eq!(kind, GitHubStackActionErrorKind::Permission);
    assert_eq!(gh.count(), 2);
    assert!(gh.calls().iter().all(|call| call.args[0] == "api"));
}

#[tokio::test]
async fn allows_a_fork_that_explicitly_permits_maintainer_updates() {
    let mut responses = vec![stack(), access_with_pr3("READ", true)];
    responses.extend(rebase_responses());
    let gh = fake(responses);
    run(&gh, rebase()).await.unwrap();
    assert!(gh.args(gh.count() - 1).contains(&"id=PR_3".to_owned()));
}

#[tokio::test(start_paused = true)]
async fn bounds_polling_and_reports_a_still_running_merge_without_claiming_success() {
    let mut responses = vec![stack()];
    responses.extend((0..40).map(|_| json!({"status": "pending", "details": {"uuid": "operation"}})));
    let gh = fake(responses);
    let (kind, _, _) = stack_failure(run(&gh, input()).await);
    assert_eq!(kind, GitHubStackActionErrorKind::MergePending);
    assert!(gh.count() < 40);
}

#[tokio::test]
async fn rejects_a_push_after_preflight_without_rebasing_the_new_revision() {
    let gh = fake(vec![stack(), access(), branch(2, "new-head", 1, &[])]);
    let (kind, number, _) = stack_failure(run(&gh, rebase()).await);
    assert_eq!(kind, GitHubStackActionErrorKind::Changed { completed: 0 });
    assert_eq!(number, 2);
    assert_eq!(gh.count(), 3);
}

#[tokio::test]
async fn skips_current_layers_without_submitting_a_rebase_mutation() {
    let gh = fake(vec![stack(), access(), branch(2, "bbb", 0, &[]), branch(3, "ccc", 0, &["bbb"])]);
    run(&gh, rebase()).await.unwrap();
    assert!(!gh.calls().iter().any(|call| is_mutation(&call.args)));
}

#[tokio::test]
async fn keeps_earlier_progress_and_stops_after_a_later_layer_fails() {
    let gh = fake(vec![
        stack(),
        access(),
        branch(2, "bbb", 1, &[]),
        rebased(),
        branch(3, "ccc", 1, &["rebased-sha"]),
        json!({"data": {"updatePullRequestBranch": null}}),
    ]);
    let (kind, number, _) = stack_failure(run(&gh, rebase()).await);
    assert_eq!(kind, GitHubStackActionErrorKind::RebaseFailed { completed: 1 });
    assert_eq!(number, 3);
}

#[tokio::test]
async fn reports_partial_progress_when_a_later_head_changes_during_the_rebase() {
    let gh = fake(vec![
        stack(),
        access(),
        branch(2, "bbb", 1, &[]),
        rebased(),
        branch(3, "concurrent-head", 1, &["rebased-sha"]),
    ]);
    let (kind, number, message) = stack_failure(run(&gh, rebase()).await);
    assert_eq!(kind, GitHubStackActionErrorKind::Changed { completed: 1 });
    assert_eq!(number, 3);
    assert!(message.contains("Earlier updates remain on GitHub"));
    assert_eq!(gh.calls().iter().filter(|call| is_mutation(&call.args)).count(), 1);
}

async fn rejects_a_push_to_a_processed_layer(rebased_parent: bool) {
    let mut responses = vec![stack(), access(), branch(2, "bbb", if rebased_parent { 1 } else { 0 }, &[])];
    if rebased_parent {
        responses.push(rebased());
    }
    responses.push(branch(3, "ccc", 1, &["concurrent-parent-head"]));
    let gh = fake(responses);
    let (kind, number, _) = stack_failure(run(&gh, rebase()).await);
    assert_eq!(kind, GitHubStackActionErrorKind::Changed { completed: 1 });
    assert_eq!(number, 2);
    assert!(gh.args(gh.count() - 1).iter().any(|arg| arg.contains(r#"processed:nodes(ids:["PR_2"])"#)));
    assert_eq!(gh.calls().iter().filter(|call| is_mutation(&call.args)).count(), usize::from(rebased_parent));
}

#[tokio::test]
async fn rejects_a_push_to_a_processed_layer_rebased_false() {
    rejects_a_push_to_a_processed_layer(false).await;
}

#[tokio::test]
async fn rejects_a_push_to_a_processed_layer_rebased_true() {
    rejects_a_push_to_a_processed_layer(true).await;
}

#[tokio::test]
async fn refuses_an_action_a_stack_cannot_take_before_reading_anything() {
    let gh = FakeGh::new();
    let (kind, _, message) = stack_failure(
        run(
            &gh,
            GitHubStackActionInput {
                action: PullRequestAction::Close,
                ..input()
            },
        )
        .await,
    );
    assert_eq!(kind, GitHubStackActionErrorKind::Unsupported);
    assert_eq!(message, "This operation is not supported for this stack.");
    assert_eq!(gh.count(), 0);
}
