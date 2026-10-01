//! `pullRequest/GitLabPullRequestProvider.ts`: GitLab merge requests as neutral change requests.

use async_trait::async_trait;
use zc_contracts::{
    PullRequestAction, PullRequestBaseComparison, PullRequestCapabilities, PullRequestEditCapabilities, PullRequestMergeMethod, PullRequestReviewCapabilities,
    PullRequestReviewVerdict, PullRequestReviewerCandidateList, PullRequestReviewerCapabilities, PullRequestUpdateMethod, PullRequestViewedFilesStore,
    PullRequestViewerPermissions, SourceControlProviderKind,
};
use zc_sourcecontrol::errors::Cause;
use zc_sourcecontrol::gitlab::{GitLabCli, GitLabCliErrorKind};

use super::cli::{GitLabPullRequestCli, GitLabPullRequestCliError, GitLabReactions, ListMergeRequestsInput, MergeRequestTarget};
use super::json::GitLabMergeRequestDetail;
use crate::error::{ProviderFailureReason, PullRequestProviderError};
use crate::provider::{
    ChangeRequestRef, CommentInput, GetDiffInput, ListChangeRequestsInput, OptionalMethods, ProviderChangeRequest, ProviderChangeRequestActivity,
    ProviderChangeRequestDetail, ProviderChangeRequestPage, ProviderDiffSlice, ProviderFileRevisions, ProviderHostRef, ProviderResult, PullRequestProviderApi,
    ReplyToThreadInput, RunActionInput, SetReactionInput, SetReviewerRequestInput, SetThreadResolutionInput, SubmitReviewInput, UpdateChangeRequestInput,
    UpdateCommentInput, ViewerPermissionsInput,
};

const KIND: SourceControlProviderKind = SourceControlProviderKind::Gitlab;

const ACTIONS: [PullRequestAction; 8] = [
    PullRequestAction::Merge,
    PullRequestAction::Ready,
    PullRequestAction::Draft,
    PullRequestAction::Close,
    PullRequestAction::Reopen,
    PullRequestAction::UpdateBranch,
    PullRequestAction::EnableAutoMerge,
    PullRequestAction::DisableAutoMerge,
];

/// No "changes requested": GitLab has approval and unresolved discussions, nothing that says a
/// merge request has been reviewed and rejected.
const VERDICTS: [PullRequestReviewVerdict; 2] = [PullRequestReviewVerdict::Comment, PullRequestReviewVerdict::Approve];

/// The actions `user.can_merge` answers for. Rebasing writes to the source branch, not the
/// target, but GitLab reports nothing narrower.
const MERGE_ACTIONS: [PullRequestAction; 4] = [
    PullRequestAction::Merge,
    PullRequestAction::UpdateBranch,
    PullRequestAction::EnableAutoMerge,
    PullRequestAction::DisableAutoMerge,
];

/// `CAPABILITIES`.
pub fn gitlab_capabilities() -> PullRequestCapabilities {
    PullRequestCapabilities {
        diff: true,
        comment: true,
        actions: ACTIONS.to_vec(),
        // GitLab offers all three, though a project settles on one; `mergeCapabilities` narrows it.
        merge_methods: vec![PullRequestMergeMethod::Merge, PullRequestMergeMethod::Squash, PullRequestMergeMethod::Rebase],
        // Rebase alone: GitLab moves a stale branch by replaying it and cannot merge the target in.
        update_methods: Some(vec![PullRequestUpdateMethod::Rebase]),
        search: true,
        reactions: Some(true),
        // GitLab keeps viewed files in one browser's local storage, so the marks are this
        // environment's own.
        viewed_files: Some(PullRequestViewedFilesStore::Environment),
        review: PullRequestReviewCapabilities {
            inline_comment: true,
            reply: true,
            resolve: true,
            verdicts: VERDICTS.to_vec(),
        },
        reviewers: PullRequestReviewerCapabilities {
            request: true,
            list_candidates: true,
        },
        edit: Some(PullRequestEditCapabilities {
            change_request: true,
            comment: true,
        }),
        stacks: None,
        stack_actions: None,
        labels: None,
    }
}

/// `gitLabViewerPermissions`: GitLab answers one question per viewer (`user.can_merge`), so
/// merging, now or later, is the only thing narrowed. The author may close, reopen and move a
/// merge request in and out of draft whatever their role, and GitLab never says who the author
/// is, so the rest stay granted and GitLab explains any refusal itself.
pub fn gitlab_viewer_permissions(viewer_can_merge: bool) -> PullRequestViewerPermissions {
    PullRequestViewerPermissions {
        stack_rebase: None,
        actions: ACTIONS
            .iter()
            .copied()
            .filter(|action| !MERGE_ACTIONS.contains(action) || viewer_can_merge)
            .collect(),
        comment: true,
        resolve: true,
        verdicts: VERDICTS.to_vec(),
        request_reviewers: true,
        update_methods: viewer_can_merge.then(|| vec![PullRequestUpdateMethod::Rebase]),
        labels: None,
    }
}

/// `gitLabProviderFailure`: the CLI tags that mean the tool itself is unusable.
pub fn gitlab_provider_failure(error: &GitLabPullRequestCliError) -> ProviderFailureReason {
    match error {
        GitLabPullRequestCliError::Cli(error) => match error.kind {
            GitLabCliErrorKind::Unavailable => ProviderFailureReason::MissingTool,
            GitLabCliErrorKind::Authentication => ProviderFailureReason::Unauthenticated,
            GitLabCliErrorKind::RateLimit => ProviderFailureReason::RateLimited,
            _ => ProviderFailureReason::Failed,
        },
        _ => ProviderFailureReason::Failed,
    }
}

fn fail(operation: &'static str) -> impl Fn(GitLabPullRequestCliError) -> PullRequestProviderError {
    move |error| PullRequestProviderError::new(KIND, operation, gitlab_provider_failure(&error), error.detail()).with_cause(Cause::new(error))
}

fn target(input: &ChangeRequestRef) -> MergeRequestTarget<'_> {
    MergeRequestTarget {
        cwd: &input.cwd,
        repository: &input.repository,
        number: input.number,
    }
}

fn to_detail(merge_request: GitLabMergeRequestDetail, merge_capabilities: zc_contracts::PullRequestMergeCapabilities) -> ProviderChangeRequestDetail {
    let item = merge_request.item;
    ProviderChangeRequestDetail {
        change_request: ProviderChangeRequest {
            stack: None,
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
            review_decision: None,
            checks_state: None,
        },
        body: merge_request.body,
        changed_files: merge_request.changed_files,
        merged_at: merge_request.merged_at,
        closed_at: merge_request.closed_at,
        reviewers: merge_request.reviewers,
        checks: merge_request.checks,
        merge_capabilities,
        viewer_permissions: gitlab_viewer_permissions(merge_request.viewer_can_merge),
        // A GitLab too old to count the divergence says nothing rather than "up to date".
        base_comparison: Some(match merge_request.diverged_commits {
            None => PullRequestBaseComparison::Unknown,
            Some(behind) if behind > 0 => PullRequestBaseComparison::Behind,
            Some(_) => PullRequestBaseComparison::UpToDate,
        }),
        behind_by: merge_request.diverged_commits,
        auto_merge_enabled: merge_request.auto_merge_enabled,
        auto_merge_method: merge_request.auto_merge_method,
        workflow_approvals_required: None,
    }
}

/// The GitLab pull request provider (`GitLabPullRequestProvider.make`).
#[derive(Clone)]
pub struct GitLabPullRequestProvider {
    cli: GitLabPullRequestCli,
    capabilities: PullRequestCapabilities,
}

impl GitLabPullRequestProvider {
    /// Built over the shared `glab` client ([`zc_sourcecontrol::SourceControl::gitlab`]).
    pub fn new(gitlab: GitLabCli) -> Self {
        Self::from_cli(GitLabPullRequestCli::new(gitlab))
    }

    pub fn from_cli(cli: GitLabPullRequestCli) -> Self {
        Self {
            cli,
            capabilities: gitlab_capabilities(),
        }
    }

    pub fn cli(&self) -> &GitLabPullRequestCli {
        &self.cli
    }
}

#[async_trait]
impl PullRequestProviderApi for GitLabPullRequestProvider {
    fn kind(&self) -> SourceControlProviderKind {
        KIND
    }

    fn capabilities(&self) -> &PullRequestCapabilities {
        &self.capabilities
    }

    fn optional_methods(&self) -> OptionalMethods {
        OptionalMethods {
            get_file_revisions: true,
            update_change_request: true,
            update_comment: true,
            ..OptionalMethods::default()
        }
    }

    async fn get_viewer(&self, input: ProviderHostRef) -> ProviderResult<String> {
        self.cli.get_viewer_username(&input.cwd).await.map_err(fail("getViewer"))
    }

    async fn list_change_requests(&self, input: ListChangeRequestsInput) -> ProviderResult<ProviderChangeRequestPage> {
        let batch = self
            .cli
            .list_merge_requests(ListMergeRequestsInput {
                cwd: &input.cwd,
                repository: &input.repository,
                state: input.state,
                involvement: input.involvement,
                viewer: &input.viewer,
                limit: input.limit,
                query: input.query.as_deref(),
                cursor: input.cursor.as_ref(),
            })
            .await
            .map_err(fail("listChangeRequests"))?;
        Ok(ProviderChangeRequestPage {
            items: batch
                .items
                .into_iter()
                .map(|item| ProviderChangeRequest {
                    stack: None,
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
                    review_decision: None,
                    checks_state: None,
                })
                .collect(),
            truncated: batch.truncated,
            cursor_advance: Some(batch.cursor_advance),
            // GitLab is always asked by update, newest first, so every page can be continued.
            continues: true,
        })
    }

    async fn get_change_request(&self, input: ChangeRequestRef) -> ProviderResult<ProviderChangeRequestDetail> {
        let (merge_request, merge_capabilities) = futures::try_join!(
            self.cli.get_merge_request_detail(target(&input)),
            self.cli.get_project_merge_capabilities(&input.cwd, &input.repository)
        )
        .map_err(fail("getChangeRequest"))?;
        Ok(to_detail(merge_request, merge_capabilities))
    }

    async fn get_change_request_activity(&self, input: ChangeRequestRef) -> ProviderResult<ProviderChangeRequestActivity> {
        let target = target(&input);
        // Each read falls back on its own: a failed read costs the conversation that part only.
        let (notes, commits, discussions, awards) = futures::join!(
            async { self.cli.list_notes(target).await.unwrap_or_else(|_| (Vec::new(), true)) },
            async { self.cli.list_commits(target).await.unwrap_or_default() },
            async { self.cli.list_discussions(target).await.unwrap_or_else(|_| (Vec::new(), true)) },
            // The notes endpoint carries no award, so they are read alongside it.
            async { self.cli.list_reactions(target).await.unwrap_or_else(|_| GitLabReactions::default()) },
        );
        let (comments, notes_truncated) = notes;
        let (threads, discussions_truncated) = discussions;
        let reactions_of = |id: &str| Some(awards.reactions_by_note_id.get(id).cloned().unwrap_or_default());
        Ok(ProviderChangeRequestActivity {
            author: None,
            reviewers: None,
            // GitLab reports no count of its own; the notes walk carries every comment, including
            // the ones written under a discussion, until GitLab runs out.
            comment_count: comments.len() as i64,
            comments: comments
                .into_iter()
                .map(|mut comment| {
                    comment.reactions = reactions_of(&comment.id);
                    comment
                })
                .collect(),
            comments_truncated: notes_truncated || discussions_truncated,
            review_threads: threads
                .into_iter()
                .map(|mut thread| {
                    for comment in &mut thread.comments {
                        comment.reactions = reactions_of(&comment.id);
                    }
                    thread
                })
                .collect(),
            commits,
            reactions: Some(awards.reactions.clone()),
        })
    }

    /// The same read the detail takes `user.can_merge` from: there is nothing cheaper to ask.
    async fn get_viewer_permissions(&self, input: ViewerPermissionsInput) -> ProviderResult<PullRequestViewerPermissions> {
        let merge_request = self
            .cli
            .get_merge_request_detail(target(&input.change_request))
            .await
            .map_err(fail("getViewerPermissions"))?;
        Ok(gitlab_viewer_permissions(merge_request.viewer_can_merge))
    }

    async fn get_diff(&self, input: GetDiffInput) -> ProviderResult<ProviderDiffSlice> {
        let slice = self
            .cli
            .get_merge_request_diff(target(&input.change_request), input.cursor.as_deref(), input.commit.as_deref())
            .await
            .map_err(fail("getDiff"))?;
        Ok(ProviderDiffSlice {
            patch: slice.patch,
            truncated: slice.truncated,
            next_cursor: slice.next_cursor,
            omitted_file_stats: None,
        })
    }

    /// What each marked file is at the head; GitLab's own local-storage marks are keyed on the
    /// blob id too, so this stales when its web UI would.
    async fn get_file_revisions(&self, input: crate::provider::FileRevisionsInput) -> ProviderResult<ProviderFileRevisions> {
        let revisions = self
            .cli
            .get_file_revisions(target(&input.change_request), &input.paths)
            .await
            .map_err(fail("getFileRevisions"))?;
        Ok(ProviderFileRevisions {
            revisions: revisions.into_entries(),
            complete: None,
        })
    }

    async fn run_action(&self, input: RunActionInput) -> ProviderResult<()> {
        self.cli
            .run_merge_request_action(target(&input.change_request), input.action, input.merge_method)
            .await
            .map_err(fail("runAction"))
    }

    async fn update_change_request(&self, input: UpdateChangeRequestInput) -> ProviderResult<()> {
        self.cli
            .update_merge_request(target(&input.change_request), input.title.as_deref(), input.body.as_deref())
            .await
            .map_err(fail("updateChangeRequest"))
    }

    async fn comment(&self, input: CommentInput) -> ProviderResult<()> {
        self.cli
            .comment_on_merge_request(target(&input.change_request), &input.body)
            .await
            .map_err(fail("comment"))
    }

    /// The kind is not read: every comment carries a plain REST note id, and one endpoint
    /// rewrites both.
    async fn update_comment(&self, input: UpdateCommentInput) -> ProviderResult<()> {
        self.cli
            .update_note(target(&input.change_request), &input.comment_id, &input.body)
            .await
            .map_err(fail("updateComment"))
    }

    async fn submit_review(&self, input: SubmitReviewInput) -> ProviderResult<()> {
        self.cli
            .submit_review(target(&input.change_request), input.verdict, &input.body, &input.comments)
            .await
            .map_err(fail("submitReview"))
    }

    /// Users only: GitLab requests a review of a person.
    async fn list_reviewer_candidates(&self, input: ChangeRequestRef) -> ProviderResult<PullRequestReviewerCandidateList> {
        self.cli.list_reviewer_candidates(target(&input)).await.map_err(fail("listReviewerCandidates"))
    }

    async fn set_reviewer_request(&self, input: SetReviewerRequestInput) -> ProviderResult<()> {
        let ids: Vec<String> = input.reviewers.iter().map(|reviewer| reviewer.id.clone()).collect();
        self.cli
            .set_reviewer_request(target(&input.change_request), &ids, input.requested)
            .await
            .map_err(fail("setReviewerRequest"))
    }

    async fn reply_to_thread(&self, input: ReplyToThreadInput) -> ProviderResult<()> {
        self.cli
            .reply_to_discussion(target(&input.change_request), &input.thread_id, &input.body)
            .await
            .map_err(fail("replyToThread"))
    }

    async fn set_reaction(&self, input: SetReactionInput) -> ProviderResult<()> {
        self.cli
            .set_reaction(target(&input.change_request), input.subject_id.as_deref(), input.content, input.reacted)
            .await
            .map_err(fail("setReaction"))
    }

    async fn set_thread_resolution(&self, input: SetThreadResolutionInput) -> ProviderResult<()> {
        self.cli
            .set_discussion_resolution(target(&input.change_request), &input.thread_id, input.resolved)
            .await
            .map_err(fail("setThreadResolution"))
    }
}
