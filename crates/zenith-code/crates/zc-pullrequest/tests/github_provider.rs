//! `GitHubPullRequestProvider.test.ts`: the provider over a mocked CLI service (the TS
//! `Layer.mock(GitHubPullRequestCli)`), plus `gitHubViewerPermissions` and `loginAvatarUrl`.

#![allow(clippy::result_large_err, clippy::type_complexity)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::FutureExt;
use serde_json::{json, Value};
use zc_contracts::{
    PullRequestAction, PullRequestActor, PullRequestCheck, PullRequestCheckStatus, PullRequestComment, PullRequestCommentKind,
    PullRequestCommentUpdateInputKind, PullRequestCommit, PullRequestLabelCandidateList, PullRequestMergeCapabilities, PullRequestMergeability,
    PullRequestReviewerCandidateList, PullRequestState, PullRequestThreadCommentsResult, PullRequestUpdateMethod,
};
use zc_pullrequest::github::cli::*;
use zc_pullrequest::github::json::*;
use zc_pullrequest::github::provider::{capabilities, git_hub_viewer_permissions, login_avatar_url, GitHubPullRequestProvider};
use zc_pullrequest::provider::*;
use zc_sourcecontrol::errors::Cause;
use zc_sourcecontrol::github::cli::current_pinned_github_credential;
use zc_sourcecontrol::github::{with_pinned_github_credential, GitHubCliError, GitHubCliErrorKind, PinnedGitHubCredential};

type Mock<I, O> = Option<Box<dyn Fn(I) -> BoxFuture<'static, CliResult<O>> + Send + Sync>>;

/// `Layer.mock(GitHubPullRequestCli)`: only what a test names answers; anything else is a bug.
#[derive(Default)]
struct MockCli {
    verified_credential: Mock<(), VerifiedCredential>,
    get_pull_request_summary: Mock<ChangeRequestRef, ProviderChangeRequestSummary>,
    get_pull_request_stack: Mock<ChangeRequestRef, Option<GitHubPullRequestStack>>,
    get_pull_request_detail: Mock<ChangeRequestRef, GitHubPullRequestCore>,
    list_workflow_runs_requiring_approval: Mock<WorkflowApprovalInput, Vec<GitHubWorkflowRunApproval>>,
    get_pull_request_base_comparison: Mock<BaseComparisonInput, GitHubBaseComparison>,
    get_viewer_access: Mock<ViewerAccessInput, GitHubViewerRepositoryAccess>,
    get_pull_request_activity: Mock<ChangeRequestRef, GitHubPullRequestActivity>,
    list_review_thread_comments: Mock<ChangeRequestRef, GitHubReviewThreadComments>,
    update_pull_request: Mock<UpdateChangeRequestInput, ()>,
    update_comment: Mock<UpdateCommentInput, ()>,
}

fn mocked<I, O>(mock: &Mock<I, O>, name: &str, input: I) -> BoxFuture<'static, CliResult<O>> {
    match mock {
        Some(mock) => mock(input),
        None => panic!("{name} was not mocked"),
    }
}

fn answer<I: 'static, O: Clone + Send + Sync + 'static>(value: O) -> Mock<I, O> {
    Some(Box::new(move |_| futures::future::ready(Ok(value.clone())).boxed()))
}

fn refuse<I: 'static, O: Send + 'static>(error: impl Fn() -> GitHubPullRequestCliError + Send + Sync + 'static) -> Mock<I, O> {
    Some(Box::new(move |_| futures::future::ready(Err(error())).boxed()))
}

#[async_trait]
impl GitHubPullRequestCliApi for MockCli {
    async fn verified_credential(&self, _cwd: &str, _host: &str) -> CliResult<VerifiedCredential> {
        mocked(&self.verified_credential, "verifiedCredential", ()).await
    }
    async fn get_routing_identity(&self, _cwd: &str, _host: &str) -> CliResult<RoutingIdentity> {
        unimplemented!("getRoutingIdentity")
    }
    async fn get_viewer_login(&self, _cwd: &str, _host: &str) -> CliResult<String> {
        unimplemented!("getViewerLogin")
    }
    async fn list_pull_requests(&self, _input: ListChangeRequestsInput) -> CliResult<zc_pullrequest::github::cli::GitHubPullRequestListBatch> {
        unimplemented!("listPullRequests")
    }
    async fn search_pull_requests(&self, _input: ListChangeRequestsAcrossInput) -> CliResult<zc_pullrequest::github::cli::GitHubPullRequestSearchBatch> {
        unimplemented!("searchPullRequests")
    }
    async fn list_pull_request_stats(&self, _input: ListChangeRequestStatsInput) -> CliResult<Vec<GitHubPullRequestStat>> {
        unimplemented!("listPullRequestStats")
    }
    async fn get_pull_request_summary(&self, input: ChangeRequestRef) -> CliResult<ProviderChangeRequestSummary> {
        mocked(&self.get_pull_request_summary, "getPullRequestSummary", input).await
    }
    async fn get_pull_request_detail(&self, input: ChangeRequestRef) -> CliResult<GitHubPullRequestCore> {
        mocked(&self.get_pull_request_detail, "getPullRequestDetail", input).await
    }
    async fn get_pull_request_preview(&self, _input: ChangeRequestRef) -> CliResult<ProviderChangeRequestPreview> {
        unimplemented!("getPullRequestPreview")
    }
    async fn list_workflow_runs_requiring_approval(&self, input: WorkflowApprovalInput) -> CliResult<Vec<GitHubWorkflowRunApproval>> {
        mocked(&self.list_workflow_runs_requiring_approval, "listWorkflowRunsRequiringApproval", input).await
    }
    async fn get_pull_request_stack(&self, input: ChangeRequestRef, _include_details: bool) -> CliResult<Option<GitHubPullRequestStack>> {
        mocked(&self.get_pull_request_stack, "getPullRequestStack", input).await
    }
    async fn get_pull_request_base_comparison(&self, input: BaseComparisonInput) -> CliResult<GitHubBaseComparison> {
        mocked(&self.get_pull_request_base_comparison, "getPullRequestBaseComparison", input).await
    }
    async fn get_pull_request_activity(&self, input: ChangeRequestRef) -> CliResult<GitHubPullRequestActivity> {
        mocked(&self.get_pull_request_activity, "getPullRequestActivity", input).await
    }
    async fn get_pull_request_diff(&self, _input: GetDiffInput) -> CliResult<ProviderDiffSlice> {
        unimplemented!("getPullRequestDiff")
    }
    async fn get_pull_request_diff_file_contents(&self, _input: DiffFileContentsInput) -> CliResult<ProviderDiffFileContents> {
        unimplemented!("getPullRequestDiffFileContents")
    }
    async fn get_pull_request_files_viewed(&self, _input: ChangeRequestRef) -> CliResult<ProviderFilesViewed> {
        unimplemented!("getPullRequestFilesViewed")
    }
    async fn set_pull_request_files_viewed(&self, _input: SetFilesViewedInput) -> CliResult<()> {
        unimplemented!("setPullRequestFilesViewed")
    }
    async fn list_review_thread_comments(&self, input: ChangeRequestRef) -> CliResult<GitHubReviewThreadComments> {
        mocked(&self.list_review_thread_comments, "listReviewThreadComments", input).await
    }
    async fn list_actor_avatars(&self, _input: ActorAvatarsInput) -> CliResult<BTreeMap<String, String>> {
        unimplemented!("listActorAvatars")
    }
    async fn get_review_thread_comments(&self, _input: ReviewThreadCommentsInput) -> CliResult<PullRequestThreadCommentsResult> {
        unimplemented!("getReviewThreadComments")
    }
    async fn get_viewer_access(&self, input: ViewerAccessInput) -> CliResult<GitHubViewerRepositoryAccess> {
        mocked(&self.get_viewer_access, "getViewerAccess", input).await
    }
    async fn list_reviewer_candidates(&self, _input: ChangeRequestRef) -> CliResult<PullRequestReviewerCandidateList> {
        unimplemented!("listReviewerCandidates")
    }
    async fn set_reviewer_request(&self, _input: SetReviewerRequestInput) -> CliResult<()> {
        unimplemented!("setReviewerRequest")
    }
    async fn list_label_candidates(&self, _input: ChangeRequestRef) -> CliResult<PullRequestLabelCandidateList> {
        unimplemented!("listLabelCandidates")
    }
    async fn set_labels(&self, _input: SetLabelsInput) -> CliResult<()> {
        unimplemented!("setLabels")
    }
    async fn run_pull_request_action(&self, _input: RunActionInput) -> CliResult<()> {
        unimplemented!("runPullRequestAction")
    }
    async fn comment_on_pull_request(&self, _input: CommentInput) -> CliResult<()> {
        unimplemented!("commentOnPullRequest")
    }
    async fn submit_review(&self, _input: SubmitReviewInput) -> CliResult<()> {
        unimplemented!("submitReview")
    }
    async fn reply_to_review_thread(&self, _input: ReplyToThreadInput) -> CliResult<()> {
        unimplemented!("replyToReviewThread")
    }
    async fn set_review_thread_resolution(&self, _input: SetThreadResolutionInput) -> CliResult<()> {
        unimplemented!("setReviewThreadResolution")
    }
    async fn set_reaction(&self, _input: SetReactionInput) -> CliResult<()> {
        unimplemented!("setReaction")
    }
    async fn update_pull_request(&self, input: UpdateChangeRequestInput) -> CliResult<()> {
        mocked(&self.update_pull_request, "updatePullRequest", input).await
    }
    async fn update_comment(&self, input: UpdateCommentInput) -> CliResult<()> {
        mocked(&self.update_comment, "updateComment", input).await
    }
}

fn provider(mock: MockCli) -> GitHubPullRequestProvider {
    GitHubPullRequestProvider::with_cli(Arc::new(mock))
}

fn pr(number: i64) -> ChangeRequestRef {
    ChangeRequestRef {
        cwd: "/w".into(),
        repository: "acme/web".into(),
        host: "github.com".into(),
        number,
    }
}

fn read_error(operation: &str) -> GitHubPullRequestCliError {
    GitHubPullRequestCliError::Read {
        cwd: "/w".into(),
        operation: operation.into(),
        cause: Cause::message("unreadable"),
    }
}

fn access(can_write: bool, can_triage: bool, can_update: bool, did_author: bool) -> GitHubViewerAccess {
    GitHubViewerAccess {
        can_write,
        can_triage,
        can_update,
        did_author,
        can_update_branch: None,
    }
}

fn writer_access() -> GitHubViewerRepositoryAccess {
    GitHubViewerRepositoryAccess {
        viewer: access(true, true, true, false),
        merge_capabilities: PullRequestMergeCapabilities {
            merge: true,
            squash: true,
            rebase: true,
        },
    }
}

fn check(name: &str, status: PullRequestCheckStatus, description: Option<&str>, url: Option<&str>) -> PullRequestCheck {
    PullRequestCheck {
        name: name.into(),
        status,
        description: description.map(str::to_owned),
        url: url.map(str::to_owned),
    }
}

/// `openDetail` with `coreFields`: an open, cross-repository pull request.
fn open_detail() -> GitHubPullRequestCore {
    GitHubPullRequestCore {
        detail: GitHubPullRequestDetail {
            item: GitHubPullRequestListItem {
                stack: None,
                author_id: None,
                number: 7,
                title: "Pull request 7".into(),
                url: "https://github.com/acme/web/pull/7".into(),
                author: None,
                head_branch: "feat/page".into(),
                base_branch: "main".into(),
                state: PullRequestState::Open,
                is_draft: false,
                mergeability: PullRequestMergeability::Mergeable,
                review_decision: None,
                additions: 1,
                deletions: 1,
                created_at: "2026-07-01T00:00:00Z".into(),
                updated_at: "2026-07-02T00:00:00Z".into(),
                review_request_logins: vec![],
                has_team_review_request: false,
                labels: vec![],
                checks_state: None,
            },
            is_cross_repository: Some(true),
            head_repository_owner: Some("acme".into()),
            head_sha: Some("abc123".into()),
            body: String::new(),
            changed_files: 1,
            merged_at: None,
            closed_at: None,
            checks: vec![],
            auto_merge_enabled: None,
            auto_merge_method: None,
        },
        viewer_access: writer_access(),
        comparison: Some(GitHubBaseComparison {
            behind_by: Some(0_i64.into()),
            viewer_can_update: true,
        }),
        checks_truncated: false,
    }
}

fn comparison(behind_by: i64) -> GitHubBaseComparison {
    GitHubBaseComparison {
        behind_by: Some(behind_by.into()),
        viewer_can_update: true,
    }
}

fn approval(id: i64, name: &str, url: &str) -> GitHubWorkflowRunApproval {
    GitHubWorkflowRunApproval {
        id,
        name: name.into(),
        url: Some(url.into()),
    }
}

fn unknown_approvals() -> PullRequestCheck {
    check(
        "Workflow approval status",
        PullRequestCheckStatus::ActionRequired,
        Some("GitHub could not determine whether workflows are awaiting approval."),
        None,
    )
}

// ---------------------------------------------------------------------------------------------

struct Identity;

impl CredentialScope for Identity {
    fn run<'a>(&'a self, future: BoxFuture<'a, ()>) -> BoxFuture<'a, ()> {
        future
    }
}

#[tokio::test]
async fn maps_credential_verification_failures_without_relabeling_operation_failures() {
    let fails = Arc::new(Mutex::new(true));
    let switch = fails.clone();
    let provider = provider(MockCli {
        verified_credential: Some(Box::new(move |_| {
            let fails = *switch.lock().unwrap();
            async move {
                if fails {
                    Err(GitHubPullRequestCliError::ViewerLoginUnavailable { cwd: "/w".into() })
                } else {
                    Ok(VerifiedCredential {
                        identity: VerifiedIdentity {
                            account_id: "123".into(),
                            viewer: "viewer".into(),
                            credential_fingerprint: "fingerprint".into(),
                        },
                        scope: Arc::new(Identity),
                    })
                }
            }
            .boxed()
        })),
        ..MockCli::default()
    });
    assert!(provider.optional_methods().with_verified_credential);
    let operations = Arc::new(AtomicUsize::new(0));
    // The use of the credential, as the service runs it: inside the scope, its result in a slot.
    let verify = || async {
        let credential = provider.verified_credential("/w", "github.com").await?;
        let slot: Arc<Mutex<Option<Result<(), &'static str>>>> = Arc::default();
        let (operations, result) = (operations.clone(), slot.clone());
        credential
            .scope
            .run(Box::pin(async move {
                operations.fetch_add(1, Ordering::SeqCst);
                *result.lock().unwrap() = Some(Err("operation-failed"));
            }))
            .await;
        let outcome = slot.lock().unwrap().take().unwrap();
        Ok::<_, zc_pullrequest::PullRequestProviderError>(outcome)
    };
    let error = verify().await.unwrap_err();
    assert_eq!(error.operation, "routeIdentity");
    assert_eq!(error.to_wire()["_tag"], "PullRequestProviderError");
    assert_eq!(operations.load(Ordering::SeqCst), 0);
    *fails.lock().unwrap() = false;
    assert_eq!(verify().await.unwrap(), Err("operation-failed"));
    assert_eq!(operations.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn uses_one_narrow_read_for_a_linked_pull_request_summary() {
    let reads = Arc::new(AtomicUsize::new(0));
    let counted = reads.clone();
    let provider = provider(MockCli {
        get_pull_request_summary: Some(Box::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            futures::future::ready(Ok(ProviderChangeRequestSummary {
                number: 7,
                title: "Summary".into(),
                url: "https://github.com/acme/web/pull/7".into(),
                head_branch: "feat/summary".into(),
                base_branch: "main".into(),
                state: PullRequestState::Open,
                is_draft: None,
                closed_at: None,
                merged_at: None,
                updated_at: "2026-08-24T12:34:56.000Z".into(),
                author: Some(Some(PullRequestActor {
                    is_bot: None,
                    login: "octocat".into(),
                    name: None,
                    avatar_url: None,
                })),
                additions: None,
                deletions: None,
                changed_files: None,
                review_decision: None,
                checks_state: None,
                mergeability: None,
            }))
            .boxed()
        })),
        ..MockCli::default()
    });
    let summary = provider.get_change_request_summary(pr(7)).await.unwrap();
    assert_eq!(summary.state, PullRequestState::Open);
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    // The author's avatar comes from the login-shaped URL, not a second request.
    assert_eq!(
        summary.author.flatten().unwrap().avatar_url.as_deref(),
        Some("https://github.com/octocat.png?size=80")
    );
}

#[tokio::test]
async fn declares_host_native_stacks_and_passes_the_one_the_cli_reads_through() {
    let stack = GitHubPullRequestStack {
        id: "42".into(),
        number: 3,
        url: "https://github.com/acme/web/stacks/3".into(),
        base: "main".into(),
        layers: vec![
            GitHubPullRequestStackLayer {
                title: None,
                is_draft: None,
                head_sha: None,
                number: 6,
                head_branch: "feat/one".into(),
                state: PullRequestState::Merged,
            },
            GitHubPullRequestStackLayer {
                title: None,
                is_draft: None,
                head_sha: None,
                number: 7,
                head_branch: "feat/two".into(),
                state: PullRequestState::Open,
            },
        ],
    };
    let answered = stack.clone();
    let provider = provider(MockCli {
        get_pull_request_stack: Some(Box::new(move |input: ChangeRequestRef| {
            futures::future::ready(Ok((input.number == 7).then(|| answered.clone()))).boxed()
        })),
        ..MockCli::default()
    });
    assert_eq!(provider.capabilities().stacks, Some(true));
    assert!(provider.optional_methods().get_change_request_stack);
    let read = |number| {
        provider.get_change_request_stack(GetChangeRequestStackInput {
            change_request: pr(number),
            include_details: None,
        })
    };
    let seven = read(7).await.unwrap().unwrap();
    assert_eq!(seven.id, "42");
    assert_eq!(seven.number, 3);
    assert_eq!(seven.url, stack.url);
    assert_eq!(seven.base, "main");
    assert_eq!(
        seven
            .layers
            .iter()
            .map(|layer| (layer.number, layer.head_branch.clone(), layer.state))
            .collect::<Vec<_>>(),
        [
            (6, "feat/one".to_owned(), PullRequestState::Merged),
            (7, "feat/two".to_owned(), PullRequestState::Open)
        ]
    );
    assert_eq!(read(8).await.unwrap(), None);
}

#[tokio::test]
async fn reports_a_failed_stack_read_against_its_own_operation() {
    let provider = provider(MockCli {
        get_pull_request_stack: refuse(|| read_error("getPullRequestStack")),
        ..MockCli::default()
    });
    let error = provider
        .get_change_request_stack(GetChangeRequestStackInput {
            change_request: pr(7),
            include_details: None,
        })
        .await
        .unwrap_err();
    assert_eq!(error.operation, "getChangeRequestStack");
    assert_eq!(error.reason, zc_pullrequest::ProviderFailureReason::Failed);
    assert_eq!(error.detail, "GitHub CLI returned an unreadable getPullRequestStack response.");
}

// ---------------------------------------------------------------------------------------------
// gitHubViewerPermissions
// ---------------------------------------------------------------------------------------------

#[test]
fn offers_everything_to_a_viewer_who_can_write_to_the_repository() {
    assert_eq!(
        serde_json::to_value(git_hub_viewer_permissions(&access(true, true, true, false))).unwrap(),
        json!({
            "actions": ["merge", "enable-auto-merge", "disable-auto-merge", "revert", "approve-workflows", "ready", "draft", "close", "reopen"],
            "comment": true,
            "resolve": true,
            "stackRebase": true,
            "verdicts": ["comment", "approve", "request-changes"],
            "requestReviewers": true,
            "labels": true,
        })
    );
}

#[test]
fn leaves_a_passer_by_on_a_repository_they_can_only_read_nothing_but_the_review() {
    assert_eq!(
        serde_json::to_value(git_hub_viewer_permissions(&access(false, false, false, false))).unwrap(),
        json!({
            "actions": [],
            "comment": true,
            "resolve": false,
            "verdicts": ["comment", "approve", "request-changes"],
            "requestReviewers": false,
            "labels": false,
        })
    );
}

#[test]
fn lets_a_triager_label_without_letting_them_merge_or_ask_for_a_review() {
    let permissions = git_hub_viewer_permissions(&access(false, true, false, false));
    assert_eq!(permissions.labels, Some(true));
    assert!(!permissions.request_reviewers);
    assert!(permissions.actions.is_empty());
}

#[test]
fn keeps_an_authors_own_pull_request_theirs_to_close_with_read_access_and_no_more() {
    assert_eq!(
        serde_json::to_value(git_hub_viewer_permissions(&access(false, false, true, true))).unwrap(),
        json!({
            "actions": ["ready", "draft", "close", "reopen"],
            "comment": true,
            "resolve": true,
            "verdicts": ["comment"],
            "requestReviewers": false,
            "labels": false,
        })
    );
}

#[tokio::test]
async fn uses_the_permissions_carried_by_the_core_read_without_a_second_request() {
    let provider = provider(MockCli {
        get_pull_request_detail: Some(Box::new(|_| {
            let mut detail = open_detail();
            detail.comparison = None;
            detail.viewer_access.viewer.can_write = false;
            detail.viewer_access.viewer.can_triage = false;
            detail.detail.head_repository_owner = None;
            detail.detail.head_sha = None;
            // The answer depends on the credential the read is pinned to.
            detail.viewer_access.merge_capabilities.squash = current_pinned_github_credential()
                .map(|credential| credential.credential_fingerprint)
                .as_deref()
                != Some("restricted");
            futures::future::ready(Ok(detail)).boxed()
        })),
        ..MockCli::default()
    });
    let detail = provider.get_change_request(pr(7)).await.unwrap();
    assert_eq!(
        serde_json::to_value(&detail.viewer_permissions).unwrap(),
        json!({
            "actions": ["ready", "draft", "close", "reopen"],
            "comment": true,
            "resolve": false,
            "verdicts": ["comment", "approve", "request-changes"],
            "requestReviewers": false,
            "labels": false,
        })
    );
    assert_eq!(detail.workflow_approvals_required, None);
    assert!(detail.checks.contains(&unknown_approvals()));
    for fingerprint in ["broad", "restricted", "broad"] {
        let scoped = with_pinned_github_credential(
            PinnedGitHubCredential {
                host: "github.com".into(),
                token: "credential".into(),
                credential_fingerprint: fingerprint.into(),
            },
            provider.get_change_request(pr(7)),
        )
        .await
        .unwrap();
        assert_eq!(scoped.merge_capabilities.squash, fingerprint != "restricted");
    }
}

#[tokio::test]
async fn keeps_fork_workflows_awaiting_approval_out_of_the_passing_state() {
    let provider = provider(MockCli {
        get_pull_request_detail: Some(Box::new(|_| {
            let mut detail = open_detail();
            detail.detail.head_repository_owner = Some("octocat".into());
            detail.detail.item.checks_state = Some(zc_contracts::PullRequestChecksState::Passing);
            detail.detail.checks = vec![
                check(
                    "manual gate",
                    PullRequestCheckStatus::ActionRequired,
                    None,
                    Some("https://example.com/manual-gate"),
                ),
                check("build", PullRequestCheckStatus::Success, None, None),
            ];
            futures::future::ready(Ok(detail)).boxed()
        })),
        list_workflow_runs_requiring_approval: answer(vec![approval(123, "contributor tests", "https://github.com/acme/web/actions/runs/123")]),
        get_pull_request_base_comparison: answer(comparison(0)),
        get_viewer_access: answer(writer_access()),
        ..MockCli::default()
    });
    let detail = provider.get_change_request(pr(7)).await.unwrap();
    assert_eq!(detail.workflow_approvals_required, Some(1));
    assert_eq!(
        detail.checks,
        [
            check(
                "manual gate",
                PullRequestCheckStatus::ActionRequired,
                None,
                Some("https://example.com/manual-gate")
            ),
            check("build", PullRequestCheckStatus::Success, None, None),
            check(
                "contributor tests",
                PullRequestCheckStatus::ActionRequired,
                Some("A maintainer must approve this workflow before it can run."),
                Some("https://github.com/acme/web/actions/runs/123"),
            ),
        ]
    );
}

#[tokio::test]
async fn does_not_repeat_a_workflow_run_a_check_already_points_at() {
    let provider = provider(MockCli {
        get_pull_request_detail: Some(Box::new(|_| {
            let mut detail = open_detail();
            detail.detail.checks = vec![check(
                "tests",
                PullRequestCheckStatus::ActionRequired,
                None,
                Some("https://github.com/acme/web/actions/runs/123/job/9"),
            )];
            futures::future::ready(Ok(detail)).boxed()
        })),
        list_workflow_runs_requiring_approval: answer(vec![approval(123, "tests", "https://github.com/acme/web/actions/runs/123")]),
        ..MockCli::default()
    });
    let detail = provider.get_change_request(pr(7)).await.unwrap();
    assert_eq!(detail.workflow_approvals_required, Some(1));
    assert_eq!(detail.checks.len(), 1);
}

#[tokio::test]
async fn uses_the_core_comparison_and_permissions_while_preserving_workflow_approval_checks() {
    let provider = provider(MockCli {
        get_pull_request_detail: Some(Box::new(|_| {
            let mut detail = open_detail();
            detail.comparison = Some(comparison(2));
            futures::future::ready(Ok(detail)).boxed()
        })),
        list_workflow_runs_requiring_approval: answer(vec![approval(123, "tests", "https://example.com/runs/123")]),
        ..MockCli::default()
    });
    let detail = provider.get_change_request(pr(7)).await.unwrap();
    assert_eq!(detail.behind_by, Some(2));
    assert_eq!(detail.base_comparison, Some(zc_contracts::PullRequestBaseComparison::Behind));
    assert_eq!(detail.workflow_approvals_required, Some(1));
    assert!(detail.checks.contains(&check(
        "tests",
        PullRequestCheckStatus::ActionRequired,
        Some("A maintainer must approve this workflow before it can run."),
        Some("https://example.com/runs/123"),
    )));
    assert!(detail.viewer_permissions.actions.contains(&PullRequestAction::UpdateBranch));
}

#[tokio::test]
async fn does_not_classify_same_repository_gates_as_fork_workflow_approvals() {
    let provider = provider(MockCli {
        get_pull_request_detail: Some(Box::new(|_| {
            let mut detail = open_detail();
            detail.detail.is_cross_repository = Some(false);
            futures::future::ready(Ok(detail)).boxed()
        })),
        get_pull_request_base_comparison: answer(comparison(0)),
        list_workflow_runs_requiring_approval: Some(Box::new(|_| panic!("same-repository pull requests must not probe fork workflow approvals"))),
        get_viewer_access: answer(writer_access()),
        ..MockCli::default()
    });
    let detail = provider.get_change_request(pr(7)).await.unwrap();
    assert_eq!(detail.workflow_approvals_required, Some(0));
    assert!(detail.checks.is_empty());
}

#[tokio::test]
async fn keeps_an_unsafe_workflow_approval_scope_visible_as_unknown() {
    let provider = provider(MockCli {
        get_pull_request_detail: answer(open_detail()),
        get_pull_request_base_comparison: answer(comparison(0)),
        list_workflow_runs_requiring_approval: refuse(|| GitHubPullRequestCliError::WorkflowApprovalRefused {
            cwd: "/w".into(),
            number: 7,
            reason: WorkflowApprovalRefusal::HeadNotUnique,
            observed_count: 2,
            limit: 1_000,
        }),
        get_viewer_access: answer(writer_access()),
        ..MockCli::default()
    });
    let detail = provider.get_change_request(pr(7)).await.unwrap();
    assert_eq!(detail.workflow_approvals_required, None);
    assert_eq!(detail.checks, [unknown_approvals()]);
}

#[tokio::test]
async fn propagates_workflow_discovery_rate_limits() {
    let provider = provider(MockCli {
        get_pull_request_detail: answer(open_detail()),
        get_pull_request_base_comparison: answer(comparison(0)),
        list_workflow_runs_requiring_approval: refuse(|| {
            GitHubPullRequestCliError::Cli(GitHubCliError::new(
                GitHubCliErrorKind::RateLimit { retry_at: None },
                "/w",
                Cause::message("rate limited"),
            ))
        }),
        get_viewer_access: answer(writer_access()),
        ..MockCli::default()
    });
    let error = provider.get_change_request(pr(7)).await.unwrap_err();
    assert_eq!(error.operation, "getChangeRequest");
    assert_eq!(error.reason, zc_pullrequest::ProviderFailureReason::RateLimited);
}

// ---------------------------------------------------------------------------------------------
// getViewerPermissions
// ---------------------------------------------------------------------------------------------

fn permissions_input(include_update_branch: Option<bool>) -> ViewerPermissionsInput {
    ViewerPermissionsInput {
        change_request: pr(7),
        include_update_branch,
    }
}

#[tokio::test]
async fn checks_fresh_access_without_reading_branch_details_for_unrelated_operations() {
    let reads = Arc::new(AtomicUsize::new(0));
    let counted = reads.clone();
    let provider = provider(MockCli {
        get_pull_request_detail: Some(Box::new(|_| panic!("Unexpected detail read"))),
        get_pull_request_base_comparison: Some(Box::new(|_| panic!("Unexpected comparison read"))),
        get_viewer_access: Some(Box::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            futures::future::ready(Ok(writer_access())).boxed()
        })),
        ..MockCli::default()
    });
    let permissions = provider.get_viewer_permissions(permissions_input(Some(false))).await.unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    assert!(permissions.actions.contains(&PullRequestAction::Merge));
    assert!(!permissions.actions.contains(&PullRequestAction::UpdateBranch));
}

#[tokio::test]
async fn offers_update_branch_when_the_comparison_grants_it() {
    let provider = provider(MockCli {
        get_pull_request_detail: answer(open_detail()),
        get_pull_request_base_comparison: answer(comparison(3)),
        get_viewer_access: answer(writer_access()),
        ..MockCli::default()
    });
    let permissions = provider.get_viewer_permissions(permissions_input(None)).await.unwrap();
    assert!(permissions.actions.contains(&PullRequestAction::UpdateBranch));
    assert_eq!(
        permissions.update_methods,
        Some(vec![PullRequestUpdateMethod::Merge, PullRequestUpdateMethod::Rebase])
    );
}

#[tokio::test]
async fn uses_the_graphql_reserve_for_manual_permission_checks() {
    let viewer_reserve = Arc::new(Mutex::new(None));
    let comparison_reserve = Arc::new(Mutex::new(None));
    let (viewer_slot, comparison_slot) = (viewer_reserve.clone(), comparison_reserve.clone());
    let provider = provider(MockCli {
        get_pull_request_detail: answer(open_detail()),
        get_pull_request_base_comparison: Some(Box::new(move |input: BaseComparisonInput| {
            *comparison_slot.lock().unwrap() = input.allow_reserve;
            // A fork's head only resolves qualified by its owner.
            assert_eq!(input.head_ref, "acme:feat/page");
            futures::future::ready(Ok(comparison(3))).boxed()
        })),
        get_viewer_access: Some(Box::new(move |input: ViewerAccessInput| {
            *viewer_slot.lock().unwrap() = input.allow_reserve;
            futures::future::ready(Ok(writer_access())).boxed()
        })),
        ..MockCli::default()
    });
    provider.get_viewer_permissions(permissions_input(None)).await.unwrap();
    assert_eq!(*viewer_reserve.lock().unwrap(), Some(true));
    assert_eq!(*comparison_reserve.lock().unwrap(), Some(true));
}

#[tokio::test]
async fn withholds_update_branch_when_the_comparison_cannot_be_read() {
    let provider = provider(MockCli {
        get_pull_request_detail: answer(open_detail()),
        get_pull_request_base_comparison: refuse(|| read_error("getPullRequestBaseComparison")),
        get_viewer_access: answer(writer_access()),
        ..MockCli::default()
    });
    let permissions = provider.get_viewer_permissions(permissions_input(None)).await.unwrap();
    assert!(!permissions.actions.contains(&PullRequestAction::UpdateBranch));
    assert_eq!(permissions.update_methods, None);
    // The rest of the answer survives a comparison nobody could make.
    assert!(permissions.actions.contains(&PullRequestAction::Merge));
}

// ---------------------------------------------------------------------------------------------
// getChangeRequestActivity
// ---------------------------------------------------------------------------------------------

fn thread_comments(commits: Vec<PullRequestCommit>) -> GitHubReviewThreadComments {
    GitHubReviewThreadComments {
        comments: vec![],
        dismissals_by_review_id: BTreeMap::new(),
        review_threads: vec![],
        comment_count: 0,
        truncated: false,
        reactions: vec![],
        reactions_by_id: BTreeMap::new(),
        reviewers: vec![],
        avatars_by_login: BTreeMap::new(),
        bot_logins: BTreeSet::new(),
        commit_stats: BTreeMap::new(),
        commits,
        viewer: GitHubPullRequestViewerFields {
            can_update: true,
            did_author: false,
        },
    }
}

fn commit(oid: &str, headline: &str, date: &str) -> PullRequestCommit {
    PullRequestCommit {
        oid: oid.into(),
        message_headline: headline.into(),
        committed_date: date.into(),
        additions: None,
        deletions: None,
        authors: Some(vec![]),
    }
}

fn activity_provider(threads: GitHubReviewThreadComments, comments: Vec<PullRequestComment>, commits: Vec<PullRequestCommit>) -> GitHubPullRequestProvider {
    provider(MockCli {
        get_pull_request_activity: answer(GitHubPullRequestActivity {
            author: None,
            comments,
            commits,
        }),
        list_review_thread_comments: answer(threads),
        ..MockCli::default()
    })
}

#[tokio::test]
async fn prefers_the_graphql_commits_which_are_the_newest_over_the_gh_view_list() {
    let provider = activity_provider(
        thread_comments(vec![commit("graphql-newest", "the newest commit gh pr view drops", "2026-07-06T00:00:00Z")]),
        vec![],
        vec![commit("view-oldest", "gh pr view's oldest commit", "2026-01-01T00:00:00Z")],
    );
    let activity = provider.get_change_request_activity(pr(7)).await.unwrap();
    assert_eq!(
        activity.commits.iter().map(|commit| commit.oid.as_str()).collect::<Vec<_>>(),
        ["graphql-newest"]
    );
}

#[tokio::test]
async fn falls_back_to_the_gh_view_list_when_the_graphql_read_has_no_commits() {
    let provider = activity_provider(
        thread_comments(vec![]),
        vec![],
        vec![commit("view-oldest", "gh pr view's oldest commit", "2026-01-01T00:00:00Z")],
    );
    let activity = provider.get_change_request_activity(pr(7)).await.unwrap();
    assert_eq!(activity.commits.iter().map(|commit| commit.oid.as_str()).collect::<Vec<_>>(), ["view-oldest"]);
}

#[tokio::test]
async fn degrades_a_failed_thread_read_to_a_truncated_conversation() {
    let provider = provider(MockCli {
        get_pull_request_activity: answer(GitHubPullRequestActivity {
            author: None,
            comments: vec![],
            commits: vec![],
        }),
        list_review_thread_comments: refuse(|| read_error("listReviewThreadComments")),
        ..MockCli::default()
    });
    let activity = provider.get_change_request_activity(pr(7)).await.unwrap();
    assert!(activity.comments_truncated);
    assert_eq!(activity.comment_count, 0);
}

fn dismissed_review(body: &str) -> PullRequestComment {
    PullRequestComment {
        id: "PRR_1".into(),
        kind: PullRequestCommentKind::Review,
        author: Some(PullRequestActor {
            is_bot: None,
            login: "macroscopeapp".into(),
            name: None,
            avatar_url: None,
        }),
        body: body.into(),
        created_at: "2026-07-03T00:00:00Z".into(),
        url: None,
        path: None,
        review_state: Some("DISMISSED".into()),
        reactions: None,
    }
}

fn dismissal_threads() -> GitHubReviewThreadComments {
    let mut threads = thread_comments(vec![]);
    threads
        .dismissals_by_review_id
        .insert("PRR_1".into(), "Dismissing prior approval to re-evaluate 9b66581".into());
    threads.bot_logins.insert("macroscopeapp".into());
    threads
}

#[tokio::test]
async fn fills_a_marker_only_dismissed_review_with_the_timelines_reason() {
    let provider = activity_provider(
        dismissal_threads(),
        vec![dismissed_review("<!-- Macroscope (Approvability) review body marker -->")],
        vec![],
    );
    let activity = provider.get_change_request_activity(pr(7)).await.unwrap();
    assert_eq!(activity.comments[0].body, "Dismissing prior approval to re-evaluate 9b66581");
    assert_eq!(activity.comments[0].author.as_ref().unwrap().is_bot, Some(true));
}

#[tokio::test]
async fn keeps_the_words_of_a_dismissed_review_that_has_its_own() {
    let provider = activity_provider(dismissal_threads(), vec![dismissed_review("These findings still stand.")], vec![]);
    let activity = provider.get_change_request_activity(pr(7)).await.unwrap();
    assert_eq!(activity.comments[0].body, "These findings still stand.");
}

// ---------------------------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn hands_a_rewrite_to_the_cli_as_the_request_named_it() {
    let rewrites: Arc<Mutex<Vec<Value>>> = Arc::default();
    let (pull_requests, comments) = (rewrites.clone(), rewrites.clone());
    let provider = provider(MockCli {
        update_pull_request: Some(Box::new(move |input: UpdateChangeRequestInput| {
            pull_requests.lock().unwrap().push(json!({
                "number": input.change_request.number,
                "title": input.title,
                "body": input.body,
            }));
            futures::future::ready(Ok(())).boxed()
        })),
        update_comment: Some(Box::new(move |input: UpdateCommentInput| {
            comments.lock().unwrap().push(json!({
                "number": input.change_request.number,
                "commentId": input.comment_id,
                "kind": input.kind.as_str(),
                "body": input.body,
            }));
            futures::future::ready(Ok(())).boxed()
        })),
        ..MockCli::default()
    });
    assert_eq!(
        serde_json::to_value(&provider.capabilities().edit).unwrap(),
        json!({"changeRequest": true, "comment": true})
    );
    provider
        .update_change_request(UpdateChangeRequestInput {
            change_request: pr(7),
            title: Some("A better title".into()),
            body: None,
        })
        .await
        .unwrap();
    provider
        .update_comment(UpdateCommentInput {
            change_request: pr(7),
            comment_id: "IC_1".into(),
            kind: PullRequestCommentUpdateInputKind::ReviewComment,
            body: "Reworded.".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        *rewrites.lock().unwrap(),
        [
            json!({"number": 7, "title": "A better title", "body": null}),
            json!({"number": 7, "commentId": "IC_1", "kind": "review-comment", "body": "Reworded."}),
        ]
    );
}

// ---------------------------------------------------------------------------------------------
// loginAvatarUrl, capabilities and error mapping
// ---------------------------------------------------------------------------------------------

#[test]
fn serves_a_users_picture_from_the_host_they_belong_to() {
    assert_eq!(
        login_avatar_url("octocat", "github.com").as_deref(),
        Some("https://github.com/octocat.png?size=80")
    );
    assert_eq!(
        login_avatar_url("octocat", "ghe.example.com").as_deref(),
        Some("https://ghe.example.com/octocat.png?size=80")
    );
}

#[test]
fn has_nothing_for_an_app_which_names_no_page() {
    assert_eq!(login_avatar_url("dependabot[bot]", "github.com"), None);
}

#[test]
fn refuses_anything_that_is_not_a_login_rather_than_building_a_url_out_of_it() {
    let long = "x".repeat(40);
    for login in ["../../etc", "a b", "-leading", long.as_str(), ""] {
        assert_eq!(login_avatar_url(login, "github.com"), None, "{login}");
    }
}

#[test]
fn declares_every_capability_and_optional_method_the_ts_provider_has() {
    assert_eq!(
        serde_json::to_value(capabilities()).unwrap(),
        json!({
            "diff": true,
            "comment": true,
            "actions": ["merge", "ready", "draft", "close", "reopen", "update-branch", "enable-auto-merge", "disable-auto-merge", "revert", "approve-workflows"],
            "mergeMethods": ["merge", "squash", "rebase"],
            "updateMethods": ["merge", "rebase"],
            "search": true,
            "reactions": true,
            "viewedFiles": "host",
            "review": {"inlineComment": true, "reply": true, "resolve": true, "verdicts": ["comment", "approve", "request-changes"]},
            "reviewers": {"request": true, "listCandidates": true},
            "edit": {"changeRequest": true, "comment": true},
            "stacks": true,
            "stackActions": true,
            "labels": true,
        })
    );
    let provider = provider(MockCli::default());
    assert_eq!(
        provider.optional_methods(),
        OptionalMethods {
            with_verified_credential: true,
            get_routing_identity: true,
            list_change_requests_across: true,
            list_change_request_stats: true,
            get_change_request_preview: true,
            get_change_request_summary: true,
            get_change_request_stack: true,
            get_review_thread_comments: true,
            get_diff_file_contents: true,
            get_files_viewed: true,
            set_files_viewed: true,
            get_file_revisions: false,
            update_change_request: true,
            update_comment: true,
            list_label_candidates: true,
            set_labels: true,
        }
    );
}

#[tokio::test]
async fn reports_a_missing_or_signed_out_gh_as_unusable_and_a_pause_as_rate_limited() {
    let cases = [
        (GitHubCliErrorKind::Unavailable, zc_pullrequest::ProviderFailureReason::MissingTool),
        (GitHubCliErrorKind::Authentication, zc_pullrequest::ProviderFailureReason::Unauthenticated),
        (GitHubCliErrorKind::Command, zc_pullrequest::ProviderFailureReason::Failed),
    ];
    for (kind, reason) in cases {
        let provider = provider(MockCli {
            get_pull_request_stack: refuse(move || GitHubPullRequestCliError::Cli(GitHubCliError::new(kind.clone(), "/w", Cause::message("gh")))),
            ..MockCli::default()
        });
        let error = provider
            .get_change_request_stack(GetChangeRequestStackInput {
                change_request: pr(7),
                include_details: None,
            })
            .await
            .unwrap_err();
        assert_eq!(error.reason, reason);
    }
    let provider = provider(MockCli {
        get_pull_request_stack: refuse(|| {
            GitHubPullRequestCliError::RateLimitPaused(zc_sourcecontrol::rate_limit::SourceControlRateLimitPausedError {
                provider: zc_contracts::SourceControlProviderKind::Github,
                host: "github.com".into(),
                retry_at: 1_234,
            })
        }),
        ..MockCli::default()
    });
    let error = provider
        .get_change_request_stack(GetChangeRequestStackInput {
            change_request: pr(7),
            include_details: None,
        })
        .await
        .unwrap_err();
    assert_eq!(error.reason, zc_pullrequest::ProviderFailureReason::RateLimited);
    assert_eq!(error.retry_at, Some(1_234));
    assert_eq!(
        error.to_wire(),
        json!({
            "_tag": "PullRequestProviderError",
            "provider": "github",
            "operation": "getChangeRequestStack",
            "reason": "rate-limited",
            "detail": "github requests to github.com are paused until the rate limit resets.",
            "retryAt": 1_234,
            "cause": {"name": "SourceControlRateLimitPausedError", "message": "github requests to github.com are paused until the rate limit resets."},
        })
    );
}
