//! `pullRequest/GitHubPullRequestProvider.ts`: the GitHub [`PullRequestProviderApi`], over
//! [`GitHubPullRequestCli`]. It declares what GitHub can do ([`capabilities`]), derives what the
//! signed-in account may do ([`git_hub_viewer_permissions`]), puts faces on actors `gh` reports
//! without one, and maps every CLI failure to a [`PullRequestProviderError`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use regex::Regex;
use zc_contracts::{
    PullRequestAction, PullRequestActor, PullRequestBaseComparison, PullRequestCapabilities, PullRequestCheck, PullRequestCheckStatus, PullRequestCommentKind,
    PullRequestEditCapabilities, PullRequestLabelCandidateList, PullRequestMergeMethod, PullRequestReviewCapabilities, PullRequestReviewVerdict,
    PullRequestReviewerCandidateList, PullRequestReviewerCapabilities, PullRequestState, PullRequestThreadCommentsResult, PullRequestUpdateMethod,
    PullRequestViewedFilesStore, PullRequestViewerPermissions, SourceControlProviderKind,
};
use zc_sourcecontrol::errors::Cause;
use zc_sourcecontrol::github::GitHubCliErrorKind;

use crate::error::{ProviderFailureReason, PullRequestProviderError};
use crate::github::cli::{
    ActorAvatarsInput, BaseComparisonInput, GitHubPullRequestCli, GitHubPullRequestCliApi, GitHubPullRequestCliError, ViewerAccessInput, WorkflowApprovalInput,
};
use crate::github::json as gh_json;
use crate::provider::*;

/// `CAPABILITIES`.
pub fn capabilities() -> PullRequestCapabilities {
    PullRequestCapabilities {
        diff: true,
        comment: true,
        actions: vec![
            PullRequestAction::Merge,
            PullRequestAction::Ready,
            PullRequestAction::Draft,
            PullRequestAction::Close,
            PullRequestAction::Reopen,
            PullRequestAction::UpdateBranch,
            PullRequestAction::EnableAutoMerge,
            PullRequestAction::DisableAutoMerge,
            PullRequestAction::Revert,
            PullRequestAction::ApproveWorkflows,
        ],
        merge_methods: vec![PullRequestMergeMethod::Merge, PullRequestMergeMethod::Squash, PullRequestMergeMethod::Rebase],
        update_methods: Some(vec![PullRequestUpdateMethod::Merge, PullRequestUpdateMethod::Rebase]),
        search: true,
        reactions: Some(true),
        viewed_files: Some(PullRequestViewedFilesStore::Host),
        review: PullRequestReviewCapabilities {
            inline_comment: true,
            reply: true,
            resolve: true,
            verdicts: all_verdicts(),
        },
        reviewers: PullRequestReviewerCapabilities {
            request: true,
            list_candidates: true,
        },
        edit: Some(PullRequestEditCapabilities {
            change_request: true,
            comment: true,
        }),
        stacks: Some(true),
        stack_actions: Some(true),
        labels: Some(true),
    }
}

fn all_verdicts() -> Vec<PullRequestReviewVerdict> {
    vec![
        PullRequestReviewVerdict::Comment,
        PullRequestReviewVerdict::Approve,
        PullRequestReviewVerdict::RequestChanges,
    ]
}

/// `gitHubViewerPermissions`: what the signed-in account may do here, from what GitHub says
/// about it.
///
/// Merging (now or armed for later), reverting and approving workflows need a role that can
/// push; ready/draft/close/reopen go by `viewerCanUpdate`, which an author has with read access
/// alone; updating the branch is GitHub's own `viewerCanUpdateBranch`, offered to nobody when it
/// was not read. Commenting and reviewing are not gated, except that an author may only comment
/// on their own change (GitHub refuses their approval). Resolving needs write or authorship,
/// asking for a review needs write, and labelling needs triage.
pub fn git_hub_viewer_permissions(access: &gh_json::GitHubViewerAccess) -> PullRequestViewerPermissions {
    let mut actions = Vec::new();
    if access.can_write {
        actions.extend([
            PullRequestAction::Merge,
            PullRequestAction::EnableAutoMerge,
            PullRequestAction::DisableAutoMerge,
            PullRequestAction::Revert,
            PullRequestAction::ApproveWorkflows,
        ]);
    }
    if access.can_update {
        actions.extend([
            PullRequestAction::Ready,
            PullRequestAction::Draft,
            PullRequestAction::Close,
            PullRequestAction::Reopen,
        ]);
    }
    let can_update_branch = access.can_update_branch == Some(true);
    if can_update_branch {
        actions.push(PullRequestAction::UpdateBranch);
    }
    PullRequestViewerPermissions {
        stack_rebase: access.can_write.then_some(true),
        actions,
        comment: true,
        resolve: access.can_write || access.did_author,
        verdicts: if access.did_author {
            vec![PullRequestReviewVerdict::Comment]
        } else {
            all_verdicts()
        },
        request_reviewers: access.can_write,
        update_methods: can_update_branch.then(|| vec![PullRequestUpdateMethod::Merge, PullRequestUpdateMethod::Rebase]),
        labels: Some(access.can_triage),
    }
}

/// `gitHubProviderFailure`: the CLI failures that mean the tool itself is unusable (or paused),
/// rather than one request failing.
pub fn git_hub_provider_failure(error: &GitHubPullRequestCliError) -> (ProviderFailureReason, Option<i64>) {
    match error {
        GitHubPullRequestCliError::Cli(error) => match error.kind {
            GitHubCliErrorKind::Unavailable => (ProviderFailureReason::MissingTool, None),
            GitHubCliErrorKind::Authentication => (ProviderFailureReason::Unauthenticated, None),
            GitHubCliErrorKind::RateLimit { retry_at } => (ProviderFailureReason::RateLimited, retry_at),
            _ => (ProviderFailureReason::Failed, None),
        },
        GitHubPullRequestCliError::RateLimitPaused(paused) => (ProviderFailureReason::RateLimited, Some(paused.retry_at)),
        _ => (ProviderFailureReason::Failed, None),
    }
}

/// `fail(operation)`.
fn fail(operation: &str, error: GitHubPullRequestCliError) -> PullRequestProviderError {
    let (reason, retry_at) = git_hub_provider_failure(&error);
    PullRequestProviderError::new(SourceControlProviderKind::Github, operation, reason, error.detail())
        .with_retry_at(retry_at)
        .with_cause(Cause::new(error))
}

/// `loginAvatarUrl`: the picture every GitHub install serves at `/<login>.png`, `None` for
/// anything that is not a plain user login (an app's `dependabot[bot]` names no page).
pub fn login_avatar_url(login: &str, host: &str) -> Option<String> {
    static LOGIN: OnceLock<Regex> = OnceLock::new();
    LOGIN
        .get_or_init(|| Regex::new(r"(?i)^[a-z0-9][a-z0-9-]{0,38}$").expect("valid regex"))
        .is_match(login)
        .then(|| format!("https://{host}/{login}.png?size=80"))
}

/// `withAvatar`: `gh pr view --json` reports no avatar, so the ones the GraphQL read collected
/// are applied by login, falling back to the login-shaped URL. An actor with one keeps it.
fn with_avatar(
    actor: Option<PullRequestActor>,
    avatars_by_login: &BTreeMap<String, String>,
    host: &str,
    bot_logins: Option<&BTreeSet<String>>,
) -> Option<PullRequestActor> {
    let mut actor = actor?;
    if bot_logins.is_some_and(|bots| bots.contains(&actor.login)) {
        actor.is_bot = Some(true);
    }
    if actor.avatar_url.is_some() {
        return Some(actor);
    }
    if let Some(avatar_url) = avatars_by_login.get(&actor.login).cloned().or_else(|| login_avatar_url(&actor.login, host)) {
        actor.avatar_url = Some(avatar_url);
    }
    Some(actor)
}

/// `withWorkflowApprovals`: fork workflows awaiting approval, which the check rollup leaves out,
/// as checks of their own (unless a check already points at the run).
fn with_workflow_approvals(checks: Vec<PullRequestCheck>, runs: &[gh_json::GitHubWorkflowRunApproval], unavailable: bool) -> Vec<PullRequestCheck> {
    static RUN: OnceLock<Regex> = OnceLock::new();
    let run_url = RUN.get_or_init(|| Regex::new(r"/actions/runs/(\d+)(?:/|$)").expect("valid regex"));
    let represented: Vec<i64> = checks
        .iter()
        .filter(|check| check.status == PullRequestCheckStatus::ActionRequired)
        .filter_map(|check| check.url.as_deref())
        .filter_map(|url| run_url.captures(url).and_then(|captures| captures[1].parse().ok()))
        .collect();
    let mut all = checks;
    all.extend(runs.iter().filter(|run| !represented.contains(&run.id)).map(|run| PullRequestCheck {
        name: run.name.clone(),
        status: PullRequestCheckStatus::ActionRequired,
        description: Some("A maintainer must approve this workflow before it can run.".into()),
        url: run.url.clone(),
    }));
    if unavailable {
        all.push(PullRequestCheck {
            name: "Workflow approval status".into(),
            status: PullRequestCheckStatus::ActionRequired,
            description: Some("GitHub could not determine whether workflows are awaiting approval.".into()),
            url: None,
        });
    }
    all
}

/// `rendersEmpty`: true where markdown would render nothing (whitespace, or only HTML comments).
fn renders_empty(body: &str) -> bool {
    static COMMENT: OnceLock<Regex> = OnceLock::new();
    let stripped = COMMENT
        .get_or_init(|| Regex::new(r"(?s)<!--.*?-->").expect("valid regex"))
        .replace_all(body, "");
    zc_sourcecontrol::util::js_trim(&stripped).is_empty()
}

/// A listing row as the neutral provider type.
fn change_request_from_item(item: gh_json::GitHubPullRequestListItem) -> ProviderChangeRequest {
    ProviderChangeRequest {
        stack: item.stack,
        number: item.number,
        title: item.title,
        url: item.url,
        author: item.author,
        head_branch: item.head_branch,
        head_repository_name_with_owner: None,
        base_branch: item.base_branch,
        state: item.state,
        is_draft: item.is_draft,
        mergeability: item.mergeability,
        additions: item.additions,
        deletions: item.deletions,
        created_at: item.created_at,
        closed_at: None,
        merged_at: None,
        updated_at: item.updated_at,
        review_request_logins: item.review_request_logins,
        labels: item.labels,
        review_decision: Some(item.review_decision),
        checks_state: Some(item.checks_state),
    }
}

/// The GitHub pull request provider.
#[derive(Clone)]
pub struct GitHubPullRequestProvider {
    cli: Arc<dyn GitHubPullRequestCliApi>,
    capabilities: PullRequestCapabilities,
}

impl std::fmt::Debug for GitHubPullRequestProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubPullRequestProvider").finish_non_exhaustive()
    }
}

impl GitHubPullRequestProvider {
    /// `GitHubPullRequestProvider.make` over the real CLI service.
    pub fn new(cli: GitHubPullRequestCli) -> Self {
        Self::with_cli(Arc::new(cli))
    }

    /// The provider over any implementation of the CLI service (tests pass a mock).
    pub fn with_cli(cli: Arc<dyn GitHubPullRequestCliApi>) -> Self {
        Self {
            cli,
            capabilities: capabilities(),
        }
    }
}

#[async_trait]
impl PullRequestProviderApi for GitHubPullRequestProvider {
    fn kind(&self) -> SourceControlProviderKind {
        SourceControlProviderKind::Github
    }

    fn capabilities(&self) -> &PullRequestCapabilities {
        &self.capabilities
    }

    fn optional_methods(&self) -> OptionalMethods {
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
    }

    async fn verified_credential(&self, cwd: &str, host: &str) -> ProviderResult<VerifiedCredential> {
        self.cli.verified_credential(cwd, host).await.map_err(|error| fail("routeIdentity", error))
    }

    async fn get_routing_identity(&self, cwd: &str, host: &str) -> ProviderResult<RoutingIdentity> {
        self.cli.get_routing_identity(cwd, host).await.map_err(|error| fail("routeIdentity", error))
    }

    async fn get_viewer(&self, input: ProviderHostRef) -> ProviderResult<String> {
        let host = input.host.unwrap_or_else(|| "github.com".into());
        self.cli.get_viewer_login(&input.cwd, &host).await.map_err(|error| fail("getViewer", error))
    }

    async fn list_change_requests(&self, input: ListChangeRequestsInput) -> ProviderResult<ProviderChangeRequestPage> {
        let (cwd, repository, host) = (input.cwd.clone(), input.repository.clone(), input.host.clone());
        let page = self.cli.list_pull_requests(input).await.map_err(|error| fail("listChangeRequests", error))?;
        let mut ids: Vec<String> = Vec::new();
        for id in page.items.iter().filter_map(|item| item.author_id.clone()) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        // A listing without faces is still a listing: a failed lookup falls back to initials.
        let avatars_by_login = self
            .cli
            .list_actor_avatars(ActorAvatarsInput {
                cwd,
                repository,
                host: host.clone(),
                ids,
            })
            .await
            .unwrap_or_default();
        Ok(ProviderChangeRequestPage {
            items: page
                .items
                .into_iter()
                .map(|item| {
                    let mut change_request = change_request_from_item(item);
                    change_request.author = with_avatar(change_request.author.take(), &avatars_by_login, &host, None);
                    change_request
                })
                .collect(),
            truncated: page.truncated,
            cursor_advance: None,
            continues: page.continues,
        })
    }

    /// A search reports an author's picture itself, so no lookup of its own; `withAvatar` still
    /// stands behind it for a login GitHub answered nothing for.
    async fn list_change_requests_across(&self, input: ListChangeRequestsAcrossInput) -> ProviderResult<ProviderBatchedChangeRequestPage> {
        let host = input.host.clone();
        let batch = self
            .cli
            .search_pull_requests(input)
            .await
            .map_err(|error| fail("listChangeRequestsAcross", error))?;
        Ok(ProviderBatchedChangeRequestPage {
            truncated: batch.truncated,
            items: batch
                .items
                .into_iter()
                .map(|item| {
                    let repository = item.repository.clone();
                    let mut change_request = change_request_from_item(item.item);
                    change_request.author = with_avatar(change_request.author.take(), &BTreeMap::new(), &host, None);
                    ProviderBatchedChangeRequest { repository, change_request }
                })
                .collect(),
        })
    }

    async fn list_change_request_stats(&self, input: ListChangeRequestStatsInput) -> ProviderResult<Vec<ProviderChangeRequestStat>> {
        let stats = self
            .cli
            .list_pull_request_stats(input)
            .await
            .map_err(|error| fail("listChangeRequestStats", error))?;
        Ok(stats
            .into_iter()
            .map(|stat| ProviderChangeRequestStat {
                repository: stat.repository,
                number: stat.number,
                additions: stat.additions,
                deletions: stat.deletions,
            })
            .collect())
    }

    async fn get_change_request(&self, input: ChangeRequestRef) -> ProviderResult<ProviderChangeRequestDetail> {
        let host = input.host.clone();
        let pull_request = self
            .cli
            .get_pull_request_detail(input.clone())
            .await
            .map_err(|error| fail("getChangeRequest", error))?;
        // Fork workflows awaiting approval are absent from the normal check rollup.
        let (runs, unavailable) = if pull_request.detail.item.state != PullRequestState::Open || pull_request.detail.is_cross_repository != Some(true) {
            (Vec::new(), false)
        } else {
            match (pull_request.detail.head_sha.clone(), pull_request.detail.head_repository_owner.clone()) {
                (Some(head_sha), Some(head_repository_owner)) => {
                    let approvals = self
                        .cli
                        .list_workflow_runs_requiring_approval(WorkflowApprovalInput {
                            change_request: input.clone(),
                            head_sha,
                            head_branch: pull_request.detail.item.head_branch.clone(),
                            head_repository_owner,
                        })
                        .await;
                    match approvals {
                        Ok(runs) => (runs, false),
                        Err(error) if is_rate_limited(&error) => return Err(fail("getChangeRequest", error)),
                        Err(_) => (Vec::new(), true),
                    }
                }
                _ => (Vec::new(), true),
            }
        };
        let core = pull_request;
        let detail = core.detail;
        let comparison = core.comparison;
        let mut change_request = change_request_from_item(detail.item);
        change_request.author = with_avatar(change_request.author.take(), &BTreeMap::new(), &host, None);
        let reviewers = change_request
            .review_request_logins
            .iter()
            .map(|login| PullRequestActor {
                is_bot: None,
                login: login.clone(),
                name: None,
                avatar_url: None,
            })
            .collect();
        let mut access = core.viewer_access.viewer.clone();
        access.can_update_branch = Some(comparison.as_ref().is_some_and(|comparison| comparison.viewer_can_update));
        // GitHub counts commits, so the number is always whole.
        let behind_by = comparison
            .as_ref()
            .and_then(|comparison| comparison.behind_by)
            .map(|behind| behind.get() as i64);
        Ok(ProviderChangeRequestDetail {
            body: detail.body,
            changed_files: detail.changed_files,
            merged_at: detail.merged_at,
            closed_at: detail.closed_at,
            reviewers,
            checks: with_workflow_approvals(detail.checks, &runs, unavailable),
            merge_capabilities: core.viewer_access.merge_capabilities.clone(),
            viewer_permissions: git_hub_viewer_permissions(&access),
            base_comparison: Some(match behind_by {
                None => PullRequestBaseComparison::Unknown,
                Some(behind) if behind > 0 => PullRequestBaseComparison::Behind,
                Some(_) => PullRequestBaseComparison::UpToDate,
            }),
            behind_by,
            auto_merge_enabled: detail.auto_merge_enabled,
            auto_merge_method: detail.auto_merge_method,
            workflow_approvals_required: (!unavailable).then_some(runs.len() as i64),
            change_request,
        })
    }

    async fn get_change_request_preview(&self, input: ChangeRequestRef) -> ProviderResult<ProviderChangeRequestPreview> {
        self.cli
            .get_pull_request_preview(input)
            .await
            .map_err(|error| fail("getChangeRequestPreview", error))
    }

    /// `gh pr view` names the author without an avatar; the login-shaped URL stands in, without
    /// the second request the listing spends on it.
    async fn get_change_request_summary(&self, input: ChangeRequestRef) -> ProviderResult<ProviderChangeRequestSummary> {
        let host = input.host.clone();
        let mut summary = self
            .cli
            .get_pull_request_summary(input)
            .await
            .map_err(|error| fail("getChangeRequestSummary", error))?;
        if let Some(author) = summary.author.take() {
            summary.author = Some(with_avatar(author, &BTreeMap::new(), &host, None));
        }
        Ok(summary)
    }

    async fn get_change_request_stack(&self, input: GetChangeRequestStackInput) -> ProviderResult<Option<ProviderChangeRequestStack>> {
        let stack = self
            .cli
            .get_pull_request_stack(input.change_request, input.include_details == Some(true))
            .await
            .map_err(|error| fail("getChangeRequestStack", error))?;
        Ok(stack.map(provider_stack))
    }

    async fn get_change_request_activity(&self, input: ChangeRequestRef) -> ProviderResult<ProviderChangeRequestActivity> {
        let host = input.host.clone();
        let (pull_request, threads) = futures::join!(self.cli.get_pull_request_activity(input.clone()), async {
            // Line comments live on review threads, which `gh pr view --json` cannot reach. A
            // GraphQL hiccup degrades to a truncated conversation rather than blanking activity.
            self.cli
                .list_review_thread_comments(input.clone())
                .await
                .unwrap_or_else(|_| degraded_review_threads())
        });
        let pull_request = pull_request.map_err(|error| fail("getChangeRequestActivity", error))?;
        let actor = |actor: Option<PullRequestActor>| with_avatar(actor, &threads.avatars_by_login, &host, Some(&threads.bot_logins));
        let commits = if threads.commits.is_empty() {
            pull_request.commits.clone()
        } else {
            threads.commits.clone()
        };
        let commits = commits
            .into_iter()
            .map(|mut commit| {
                if let Some(stat) = threads.commit_stats.get(&commit.oid) {
                    commit.additions = Some(stat.additions);
                    commit.deletions = Some(stat.deletions);
                }
                commit.authors = commit
                    .authors
                    .map(|authors| authors.into_iter().map(|author| actor(Some(author.clone())).unwrap_or(author)).collect());
                commit
            })
            .collect();
        let comment_count = pull_request.comments.len() as i64 + threads.comment_count;
        let mut comments: Vec<_> = pull_request
            .comments
            .into_iter()
            .chain(threads.comments.iter().cloned())
            .map(|mut comment| {
                // GitHub keeps a dismissal's reason on the timeline event, so a dismissed review
                // with nothing visible of its own reads its words from there ("visible": a bot
                // review often carries only an HTML marker, which renders as nothing).
                if comment.kind == PullRequestCommentKind::Review
                    && comment.review_state.as_deref().is_some_and(|state| state.to_uppercase() == "DISMISSED")
                    && renders_empty(&comment.body)
                {
                    if let Some(reason) = threads.dismissals_by_review_id.get(&comment.id).cloned() {
                        comment.body = reason;
                    }
                }
                comment.author = actor(comment.author.take());
                // A comment out of `gh pr view --json` carries no reaction: they arrive from the
                // GraphQL page by node id.
                if comment.reactions.is_none() {
                    comment.reactions = Some(threads.reactions_by_id.get(&comment.id).cloned().unwrap_or_default());
                }
                comment
            })
            .collect();
        comments.sort_by(|left, right| left.created_at.cmp(&right.created_at));
        let review_threads = threads
            .review_threads
            .iter()
            .cloned()
            .map(|mut thread| {
                for comment in &mut thread.comments {
                    comment.author = actor(comment.author.take());
                }
                thread
            })
            .collect();
        Ok(ProviderChangeRequestActivity {
            author: Some(actor(pull_request.author)),
            reviewers: Some(threads.reviewers.clone()),
            comments,
            // `gh pr view --json comments,reviews` follows GitHub's cursors itself, so only the
            // thread walk can stop short of the host.
            comment_count,
            comments_truncated: threads.truncated,
            review_threads,
            commits,
            reactions: Some(threads.reactions.clone()),
        })
    }

    async fn get_review_thread_comments(&self, input: ReviewThreadCommentsInput) -> ProviderResult<PullRequestThreadCommentsResult> {
        self.cli
            .get_review_thread_comments(input)
            .await
            .map_err(|error| fail("getReviewThreadComments", error))
    }

    async fn get_viewer_permissions(&self, input: ViewerPermissionsInput) -> ProviderResult<PullRequestViewerPermissions> {
        let target = input.change_request.clone();
        let access = self.cli.get_viewer_access(ViewerAccessInput {
            change_request: target.clone(),
            allow_reserve: Some(true),
        });
        // Whether this viewer may update the branch is only on the comparison, which resolves
        // through the head ref the detail carries. A failure withholds that one action.
        let can_update_branch = async {
            if input.include_update_branch == Some(false) {
                return false;
            }
            let comparison = async {
                let pull_request = self.cli.get_pull_request_detail(target.clone()).await?;
                let detail = &pull_request.detail;
                let Some(owner) = detail.head_repository_owner.as_ref().filter(|_| detail.item.state == PullRequestState::Open) else {
                    return Ok(false);
                };
                let comparison = self
                    .cli
                    .get_pull_request_base_comparison(BaseComparisonInput {
                        change_request: target.clone(),
                        head_ref: format!("{owner}:{}", detail.item.head_branch),
                        allow_reserve: Some(true),
                    })
                    .await?;
                Ok::<_, GitHubPullRequestCliError>(comparison.viewer_can_update)
            };
            comparison.await.unwrap_or(false)
        };
        let (access, can_update_branch) = futures::join!(access, can_update_branch);
        let mut access = access.map_err(|error| fail("getViewerPermissions", error))?.viewer;
        access.can_update_branch = Some(can_update_branch);
        Ok(git_hub_viewer_permissions(&access))
    }

    async fn get_diff(&self, input: GetDiffInput) -> ProviderResult<ProviderDiffSlice> {
        self.cli.get_pull_request_diff(input).await.map_err(|error| fail("getDiff", error))
    }

    async fn get_diff_file_contents(&self, input: DiffFileContentsInput) -> ProviderResult<ProviderDiffFileContents> {
        self.cli
            .get_pull_request_diff_file_contents(input)
            .await
            .map_err(|error| fail("getDiffFileContents", error))
    }

    async fn get_files_viewed(&self, input: ChangeRequestRef) -> ProviderResult<ProviderFilesViewed> {
        self.cli
            .get_pull_request_files_viewed(input)
            .await
            .map_err(|error| fail("getFilesViewed", error))
    }

    async fn set_files_viewed(&self, input: SetFilesViewedInput) -> ProviderResult<()> {
        self.cli
            .set_pull_request_files_viewed(input)
            .await
            .map_err(|error| fail("setFilesViewed", error))
    }

    async fn run_action(&self, input: RunActionInput) -> ProviderResult<()> {
        self.cli.run_pull_request_action(input).await.map_err(|error| fail("runAction", error))
    }

    async fn update_change_request(&self, input: UpdateChangeRequestInput) -> ProviderResult<()> {
        self.cli.update_pull_request(input).await.map_err(|error| fail("updateChangeRequest", error))
    }

    async fn comment(&self, input: CommentInput) -> ProviderResult<()> {
        self.cli.comment_on_pull_request(input).await.map_err(|error| fail("comment", error))
    }

    async fn update_comment(&self, input: UpdateCommentInput) -> ProviderResult<()> {
        self.cli.update_comment(input).await.map_err(|error| fail("updateComment", error))
    }

    async fn submit_review(&self, input: SubmitReviewInput) -> ProviderResult<()> {
        self.cli.submit_review(input).await.map_err(|error| fail("submitReview", error))
    }

    async fn list_reviewer_candidates(&self, input: ChangeRequestRef) -> ProviderResult<PullRequestReviewerCandidateList> {
        self.cli
            .list_reviewer_candidates(input)
            .await
            .map_err(|error| fail("listReviewerCandidates", error))
    }

    async fn set_reviewer_request(&self, input: SetReviewerRequestInput) -> ProviderResult<()> {
        self.cli.set_reviewer_request(input).await.map_err(|error| fail("setReviewerRequest", error))
    }

    async fn list_label_candidates(&self, input: ChangeRequestRef) -> ProviderResult<PullRequestLabelCandidateList> {
        self.cli.list_label_candidates(input).await.map_err(|error| fail("listLabelCandidates", error))
    }

    async fn set_labels(&self, input: SetLabelsInput) -> ProviderResult<()> {
        self.cli.set_labels(input).await.map_err(|error| fail("setLabels", error))
    }

    async fn reply_to_thread(&self, input: ReplyToThreadInput) -> ProviderResult<()> {
        self.cli.reply_to_review_thread(input).await.map_err(|error| fail("replyToThread", error))
    }

    async fn set_reaction(&self, input: SetReactionInput) -> ProviderResult<()> {
        self.cli.set_reaction(input).await.map_err(|error| fail("setReaction", error))
    }

    async fn set_thread_resolution(&self, input: SetThreadResolutionInput) -> ProviderResult<()> {
        self.cli
            .set_review_thread_resolution(input)
            .await
            .map_err(|error| fail("setThreadResolution", error))
    }
}

/// What `listReviewThreadComments` degrades to when the GraphQL read fails: a truncated
/// conversation rather than a blank activity.
fn degraded_review_threads() -> gh_json::GitHubReviewThreadComments {
    gh_json::GitHubReviewThreadComments {
        comments: Vec::new(),
        dismissals_by_review_id: BTreeMap::new(),
        review_threads: Vec::new(),
        comment_count: 0,
        truncated: true,
        reactions: Vec::new(),
        reactions_by_id: BTreeMap::new(),
        reviewers: Vec::new(),
        avatars_by_login: BTreeMap::new(),
        bot_logins: BTreeSet::new(),
        commit_stats: BTreeMap::new(),
        commits: Vec::new(),
        viewer: gh_json::GitHubPullRequestViewerFields {
            can_update: true,
            did_author: false,
        },
    }
}

/// The CLI rate-limit failures, which workflow discovery propagates rather than degrading.
fn is_rate_limited(error: &GitHubPullRequestCliError) -> bool {
    match error {
        GitHubPullRequestCliError::Cli(error) => error.is_rate_limit(),
        GitHubPullRequestCliError::RateLimitPaused(_) => true,
        _ => false,
    }
}

/// A stack as the neutral provider type.
fn provider_stack(stack: gh_json::GitHubPullRequestStack) -> ProviderChangeRequestStack {
    ProviderChangeRequestStack {
        id: stack.id,
        number: stack.number,
        url: stack.url,
        base: stack.base,
        layers: stack
            .layers
            .into_iter()
            .map(|layer| ProviderChangeRequestStackLayer {
                title: layer.title,
                is_draft: layer.is_draft,
                head_sha: layer.head_sha,
                number: layer.number,
                head_branch: layer.head_branch,
                state: layer.state,
            })
            .collect(),
    }
}
