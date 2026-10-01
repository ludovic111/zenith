//! `pullRequest/BitbucketPullRequestProvider.ts`: Bitbucket Cloud as a [`PullRequestProviderApi`].

use async_trait::async_trait;
use zc_contracts::{
    PullRequestAction, PullRequestCapabilities, PullRequestEditCapabilities, PullRequestMergeCapabilities, PullRequestMergeMethod, PullRequestMergeability,
    PullRequestReviewCapabilities, PullRequestReviewVerdict, PullRequestReviewerCandidateList, PullRequestReviewerCapabilities, PullRequestViewedFilesStore,
    PullRequestViewerPermissions, SourceControlProviderKind,
};
use zc_sourcecontrol::bitbucket::{BitbucketApi, BitbucketApiError};
use zc_sourcecontrol::errors::Cause;
use zc_sourcecontrol::util::SharedClock;

use super::api::{BitbucketConversation, BitbucketPullRequestApi, BitbucketPullRequestApiError, ListPullRequestsInput};
use super::json::BitbucketPullRequest;
use crate::provider::{
    ChangeRequestRef, CommentInput, FileRevisionsInput, GetDiffInput, ListChangeRequestsInput, OptionalMethods, ProviderChangeRequest,
    ProviderChangeRequestActivity, ProviderChangeRequestDetail, ProviderChangeRequestPage, ProviderDiffSlice, ProviderFailureReason, ProviderFileRevisions,
    ProviderHostRef, ProviderResult, PullRequestProviderApi, PullRequestProviderError, ReplyToThreadInput, RunActionInput, SetReactionInput,
    SetReviewerRequestInput, SetThreadResolutionInput, SubmitReviewInput, UpdateChangeRequestInput, UpdateCommentInput, ViewerPermissionsInput,
};

const KIND: SourceControlProviderKind = SourceControlProviderKind::Bitbucket;

/// `CAPABILITIES`.
pub fn bitbucket_capabilities() -> PullRequestCapabilities {
    PullRequestCapabilities {
        diff: true,
        comment: true,
        // Bitbucket has no endpoint that reopens a declined pull request, and nothing documented
        // that moves one in or out of draft, so neither is offered.
        actions: vec![PullRequestAction::Merge, PullRequestAction::Close],
        merge_methods: vec![PullRequestMergeMethod::Merge, PullRequestMergeMethod::Squash, PullRequestMergeMethod::Rebase],
        update_methods: None,
        search: true,
        // Bitbucket Cloud's API exposes no reaction on a pull request or on a comment.
        reactions: Some(false),
        // Bitbucket Cloud states nothing about what a reviewer has already read, so the marks
        // are kept in the environment.
        viewed_files: Some(PullRequestViewedFilesStore::Environment),
        review: PullRequestReviewCapabilities {
            inline_comment: true,
            reply: true,
            resolve: true,
            verdicts: vec![
                PullRequestReviewVerdict::Comment,
                PullRequestReviewVerdict::Approve,
                PullRequestReviewVerdict::RequestChanges,
            ],
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

/// `bitbucketViewerPermissions`: what the configured account may do here, from the one thing
/// Bitbucket states per viewer — the repository permission. Merging needs `write` or `admin`, so
/// that is what narrows. Declining stays offered (an author may decline their own with read
/// access, and nothing here says who opened this one); commenting, reviewing and asking for a
/// review are not narrowed either.
pub fn bitbucket_viewer_permissions(can_write: bool) -> PullRequestViewerPermissions {
    let capabilities = bitbucket_capabilities();
    PullRequestViewerPermissions {
        stack_rebase: None,
        actions: capabilities
            .actions
            .into_iter()
            .filter(|action| *action != PullRequestAction::Merge || can_write)
            .collect(),
        comment: true,
        resolve: true,
        verdicts: capabilities.review.verdicts,
        request_reviewers: true,
        update_methods: None,
        labels: None,
    }
}

/// `bitbucketProviderFailure`: the failures that mean the credentials are the problem, rather
/// than one request. Bitbucket is read over HTTP with configured credentials, so there is no tool
/// to be missing: unusable always means the credentials are absent or refused.
pub fn bitbucket_provider_failure(error: &BitbucketPullRequestApiError) -> (ProviderFailureReason, Option<i64>) {
    match error.api() {
        Some(BitbucketApiError::Response { status: 401, .. }) => (ProviderFailureReason::Unauthenticated, None),
        Some(BitbucketApiError::Response { status: 429, retry_at, .. } | BitbucketApiError::ResponseBodyRead { status: 429, retry_at, .. }) => {
            (ProviderFailureReason::RateLimited, *retry_at)
        }
        _ => (ProviderFailureReason::Failed, None),
    }
}

/// `fail(operation)`: every Bitbucket failure states its own fact; this names the operation
/// around it, so the two do not stack into "failed in x: failed in y: …".
fn fail(operation: &str) -> impl Fn(BitbucketPullRequestApiError) -> PullRequestProviderError + '_ {
    move |error| {
        let (reason, retry_at) = bitbucket_provider_failure(&error);
        PullRequestProviderError::new(KIND, operation, reason, error.detail())
            .with_retry_at(retry_at)
            .with_cause(Cause::new(error))
    }
}

/// `recoverRead`: an optional read that failed answers its fallback, except for a rate limit,
/// which is passed on so the host is paused.
fn recover_read<A>(read: Result<A, BitbucketPullRequestApiError>, fallback: A) -> Result<A, BitbucketPullRequestApiError> {
    match read {
        Err(error) if error.is_rate_limited() => Err(error),
        Err(_) => Ok(fallback),
        ok => ok,
    }
}

/// `toChangeRequest`.
fn to_change_request(pull_request: &BitbucketPullRequest) -> ProviderChangeRequest {
    ProviderChangeRequest {
        stack: None,
        number: pull_request.number,
        title: pull_request.title.clone(),
        url: pull_request.url.clone(),
        author: pull_request.author.clone(),
        head_branch: pull_request.head_branch.clone(),
        head_repository_name_with_owner: pull_request.head_repository_name_with_owner.clone().filter(|name| !name.is_empty()).map(Some),
        base_branch: pull_request.base_branch.clone(),
        state: pull_request.state,
        is_draft: pull_request.is_draft,
        mergeability: pull_request.mergeability,
        // Line counts are a separate read, which only the detail is worth spending on.
        additions: 0,
        deletions: 0,
        created_at: pull_request.created_at.clone(),
        closed_at: None,
        merged_at: None,
        updated_at: pull_request.updated_at.clone(),
        review_request_logins: pull_request.review_request_logins.clone(),
        // Bitbucket has no labels on a pull request.
        labels: Vec::new(),
        review_decision: None,
        checks_state: None,
    }
}

/// `BitbucketPullRequestProvider`.
#[derive(Debug, Clone)]
pub struct BitbucketPullRequestProvider {
    api: BitbucketPullRequestApi,
    capabilities: PullRequestCapabilities,
}

impl BitbucketPullRequestProvider {
    /// Over zc-sourcecontrol's shared client (`SourceControl::bitbucket`); `clock` times the
    /// file revision cache.
    pub fn new(bitbucket: BitbucketApi, clock: SharedClock) -> Self {
        Self::from_api(BitbucketPullRequestApi::new(bitbucket, clock))
    }

    pub fn from_api(api: BitbucketPullRequestApi) -> Self {
        Self {
            api,
            capabilities: bitbucket_capabilities(),
        }
    }

    pub fn api(&self) -> &BitbucketPullRequestApi {
        &self.api
    }
}

#[async_trait]
impl PullRequestProviderApi for BitbucketPullRequestProvider {
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

    /// Bitbucket credentials come from the server rather than a checkout, so the account is the
    /// same whichever workspace asks.
    async fn get_viewer(&self, _input: ProviderHostRef) -> ProviderResult<String> {
        self.api.get_viewer().await.map_err(fail("getViewer"))
    }

    async fn list_change_requests(&self, input: ListChangeRequestsInput) -> ProviderResult<ProviderChangeRequestPage> {
        let batch = self
            .api
            .list_pull_requests(ListPullRequestsInput {
                repository: input.repository,
                state: input.state,
                limit: input.limit,
                query: input.query,
                cursor: input.cursor,
            })
            .await
            .map_err(fail("listChangeRequests"))?;
        Ok(ProviderChangeRequestPage {
            items: batch.items.iter().map(to_change_request).collect(),
            truncated: batch.truncated,
            cursor_advance: None,
            // Bitbucket is asked for `-updated_on` whether or not it is being carried on from, so
            // every page it answers is one a cursor can continue.
            continues: true,
        })
    }

    async fn get_change_request(&self, input: ChangeRequestRef) -> ProviderResult<ProviderChangeRequestDetail> {
        let (repository, number) = (input.repository.as_str(), input.number);
        let (pull_request, diff_stat, mergeability, checks, can_write) = futures::try_join!(
            self.api.get_pull_request(repository, number),
            self.api.get_diff_stat(repository, number),
            async { recover_read(self.api.get_mergeability(repository, number).await, PullRequestMergeability::Unknown) },
            async { recover_read(self.api.list_checks(repository, number).await, Vec::new()) },
            // A permission that could not be read is an unknown one, which is granted: a hidden
            // Merge leaves someone entitled to it with no way through, and one Bitbucket refuses
            // at least says why.
            async { recover_read(self.api.get_repository_permission(repository).await, true) },
        )
        .map_err(fail("getChangeRequest"))?;
        let mut change_request = to_change_request(&pull_request);
        change_request.mergeability = mergeability;
        change_request.additions = diff_stat.additions;
        change_request.deletions = diff_stat.deletions;
        Ok(ProviderChangeRequestDetail {
            change_request,
            body: pull_request.body.clone(),
            changed_files: diff_stat.changed_files,
            merged_at: None,
            closed_at: None,
            reviewers: pull_request.reviewers.clone(),
            checks,
            // Bitbucket publishes no per-repository list of allowed strategies, so all are
            // offered and a strategy the repository forbids fails on merge.
            merge_capabilities: PullRequestMergeCapabilities {
                merge: true,
                squash: true,
                rebase: true,
            },
            viewer_permissions: bitbucket_viewer_permissions(can_write),
            base_comparison: None,
            behind_by: None,
            auto_merge_enabled: None,
            auto_merge_method: None,
            workflow_approvals_required: None,
        })
    }

    async fn get_change_request_activity(&self, input: ChangeRequestRef) -> ProviderResult<ProviderChangeRequestActivity> {
        let (repository, number) = (input.repository.as_str(), input.number);
        let unread = BitbucketConversation {
            comments: Vec::new(),
            threads: Vec::new(),
            truncated: true,
        };
        let (pull_request, conversation, commits) = futures::try_join!(
            // Reviews ride on the pull request itself, so this inexpensive core read is repeated
            // here rather than making the core response wait for the conversation endpoints.
            self.api.get_pull_request(repository, number),
            async { recover_read(self.api.list_comments(repository, number).await, unread) },
            async { recover_read(self.api.list_commits(repository, number).await, Vec::new()) },
        )
        .map_err(fail("getChangeRequestActivity"))?;
        let comment_count = (conversation.comments.len() + pull_request.reviews.len()) as i64;
        let mut comments = conversation.comments;
        comments.extend(pull_request.reviews);
        comments.sort_by(|left, right| left.created_at.cmp(&right.created_at));
        Ok(ProviderChangeRequestActivity {
            author: None,
            reviewers: None,
            comments,
            comment_count,
            comments_truncated: conversation.truncated,
            review_threads: conversation.threads,
            commits,
            reactions: None,
        })
    }

    async fn get_viewer_permissions(&self, input: ViewerPermissionsInput) -> ProviderResult<PullRequestViewerPermissions> {
        let can_write = self
            .api
            .get_repository_permission(&input.change_request.repository)
            .await
            .map_err(fail("getViewerPermissions"))?;
        Ok(bitbucket_viewer_permissions(can_write))
    }

    /// `/diff` answers with the whole patch and pages nothing, so the first slice is the last.
    async fn get_diff(&self, input: GetDiffInput) -> ProviderResult<ProviderDiffSlice> {
        let reference = &input.change_request;
        let diff = self
            .api
            .get_pull_request_diff(&reference.repository, reference.number, input.commit.as_deref())
            .await
            .map_err(fail("getDiff"))?;
        Ok(ProviderDiffSlice {
            patch: diff.patch,
            truncated: diff.truncated,
            next_cursor: None,
            omitted_file_stats: None,
        })
    }

    async fn get_file_revisions(&self, input: FileRevisionsInput) -> ProviderResult<ProviderFileRevisions> {
        let reference = &input.change_request;
        let revisions = self
            .api
            .get_file_revisions(&reference.repository, reference.number, &input.paths)
            .await
            .map_err(fail("getFileRevisions"))?;
        Ok(ProviderFileRevisions {
            revisions: revisions.revisions,
            complete: Some(revisions.complete),
        })
    }

    async fn run_action(&self, input: RunActionInput) -> ProviderResult<()> {
        let reference = &input.change_request;
        self.api
            .run_action(&reference.repository, reference.number, input.action, input.merge_method)
            .await
            .map_err(fail("runAction"))
    }

    async fn update_change_request(&self, input: UpdateChangeRequestInput) -> ProviderResult<()> {
        let reference = &input.change_request;
        self.api
            .update_change_request(&reference.repository, reference.number, input.title.as_deref(), input.body.as_deref())
            .await
            .map_err(fail("updateChangeRequest"))
    }

    async fn comment(&self, input: CommentInput) -> ProviderResult<()> {
        let reference = &input.change_request;
        self.api
            .comment(&reference.repository, reference.number, &input.body)
            .await
            .map_err(fail("comment"))
    }

    async fn update_comment(&self, input: UpdateCommentInput) -> ProviderResult<()> {
        let reference = &input.change_request;
        self.api
            .update_comment(&reference.repository, reference.number, &input.comment_id, &input.body)
            .await
            .map_err(fail("updateComment"))
    }

    async fn submit_review(&self, input: SubmitReviewInput) -> ProviderResult<()> {
        let reference = &input.change_request;
        self.api
            .submit_review(&reference.repository, reference.number, input.verdict, &input.body, &input.comments)
            .await
            .map_err(fail("submitReview"))
    }

    /// Users only: Bitbucket requests a review of an account, and has no group that stands in
    /// for one on a pull request.
    async fn list_reviewer_candidates(&self, input: ChangeRequestRef) -> ProviderResult<PullRequestReviewerCandidateList> {
        self.api
            .list_reviewer_candidates(&input.repository, input.number)
            .await
            .map_err(fail("listReviewerCandidates"))
    }

    async fn set_reviewer_request(&self, input: SetReviewerRequestInput) -> ProviderResult<()> {
        let reference = &input.change_request;
        let ids: Vec<String> = input.reviewers.iter().map(|reviewer| reviewer.id.clone()).collect();
        self.api
            .set_reviewer_request(&reference.repository, reference.number, &ids, input.requested)
            .await
            .map_err(fail("setReviewerRequest"))
    }

    async fn reply_to_thread(&self, input: ReplyToThreadInput) -> ProviderResult<()> {
        let reference = &input.change_request;
        self.api
            .reply_to_comment(&reference.repository, reference.number, &input.thread_id, &input.body)
            .await
            .map_err(fail("replyToThread"))
    }

    /// Never called: `capabilities.reactions` is false, and the service refuses without it.
    async fn set_reaction(&self, _input: SetReactionInput) -> ProviderResult<()> {
        Err(PullRequestProviderError::failed(KIND, "setReaction", "Bitbucket does not support reactions."))
    }

    async fn set_thread_resolution(&self, input: SetThreadResolutionInput) -> ProviderResult<()> {
        let reference = &input.change_request;
        self.api
            .set_comment_resolution(&reference.repository, reference.number, &input.thread_id, input.resolved)
            .await
            .map_err(fail("setThreadResolution"))
    }
}

#[cfg(test)]
mod tests {
    //! `BitbucketPullRequestProvider.test.ts` (the pure helpers; the recovery tests run against a
    //! scripted requester in `tests/bitbucket_provider.rs`).

    use super::*;

    #[test]
    fn treats_only_an_http_401_as_unusable_credentials() {
        let response_error = |status| {
            BitbucketPullRequestApiError::Api(BitbucketApiError::Response {
                operation: "request",
                status,
                response_body_length: 0,
                retry_at: None,
            })
        };
        assert_eq!(bitbucket_provider_failure(&response_error(401)).0, ProviderFailureReason::Unauthenticated);
        assert_eq!(bitbucket_provider_failure(&response_error(403)).0, ProviderFailureReason::Failed);
    }

    #[test]
    fn offers_both_actions_to_credentials_with_write_access() {
        assert_eq!(
            serde_json::to_value(bitbucket_viewer_permissions(true)).unwrap(),
            serde_json::json!({
                "actions": ["merge", "close"],
                "comment": true,
                "resolve": true,
                "verdicts": ["comment", "approve", "request-changes"],
                // Bitbucket says nothing about who may set a reviewer, and an unreported
                // permission is granted.
                "requestReviewers": true,
            })
        );
    }

    #[test]
    fn keeps_merge_from_credentials_that_can_only_read_the_repository() {
        assert_eq!(
            serde_json::to_value(bitbucket_viewer_permissions(false)).unwrap(),
            serde_json::json!({
                "actions": ["close"],
                "comment": true,
                "resolve": true,
                "verdicts": ["comment", "approve", "request-changes"],
                "requestReviewers": true,
            })
        );
    }

    #[test]
    fn treats_an_author_with_read_access_as_any_other_reader() {
        // The repository permission says nothing about who opened this pull request, and its
        // author may decline it with read access alone — so declining stays offered.
        assert_eq!(bitbucket_viewer_permissions(false).actions, vec![PullRequestAction::Close]);
    }
}
