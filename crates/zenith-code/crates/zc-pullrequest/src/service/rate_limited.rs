//! `withRateLimitBackoff(api, host, limits, options)`: a provider whose every host call first
//! asks the host's rate limit, records how it went, and is refused while the host is paused.
//!
//! Interactive calls (the ones a reader is waiting on: writes, permission reads, the viewer lookup
//! when asked for) pass a pause without clearing it and may spend GitHub's reserved quota. The
//! routing identity and credential verification are handed through untouched.

use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use zc_contracts::{
    PullRequestCapabilities, PullRequestLabelCandidateList, PullRequestReviewerCandidateList, PullRequestThreadCommentsResult, PullRequestViewerPermissions,
    SourceControlProviderKind,
};
use zc_sourcecontrol::github::cli::with_github_reserve;
use zc_sourcecontrol::rate_limit::{RateLimitKey, SourceControlRateLimit};

use crate::error::{Cause, ProviderFailureReason, PullRequestProviderError};
use crate::provider::*;

/// The wrapped provider.
pub(crate) struct RateLimited {
    inner: SharedProvider,
    host: String,
    limits: SourceControlRateLimit,
    viewer_allows_pause: bool,
}

/// `withRateLimitBackoff(api, host, limits, {viewerAllowsPause})`.
pub(crate) fn with_rate_limit_backoff(inner: SharedProvider, host: &str, limits: &SourceControlRateLimit, viewer_allows_pause: bool) -> SharedProvider {
    Arc::new(RateLimited {
        inner,
        host: host.to_owned(),
        limits: limits.clone(),
        viewer_allows_pause,
    })
}

impl RateLimited {
    async fn protect<T>(&self, operation: &str, allow_paused: bool, call: impl Future<Output = ProviderResult<T>>) -> ProviderResult<T> {
        let kind = self.inner.kind();
        let key = RateLimitKey::new(kind, self.host.clone());
        let lease = self.limits.check(&key, allow_paused).map_err(|paused| {
            PullRequestProviderError::new(kind, operation, ProviderFailureReason::RateLimited, paused.detail())
                .with_retry_at(Some(paused.retry_at))
                .with_cause(Cause::new(paused))
        })?;
        let result = if allow_paused { with_github_reserve(call).await } else { call.await };
        match &result {
            Ok(_) => self.limits.record_success(&key, lease),
            Err(error) if error.reason == ProviderFailureReason::RateLimited => self.limits.record_rate_limit(&key, lease, error.retry_at),
            Err(_) => {}
        }
        result
    }
}

#[async_trait]
impl PullRequestProviderApi for RateLimited {
    fn kind(&self) -> SourceControlProviderKind {
        self.inner.kind()
    }

    fn capabilities(&self) -> &PullRequestCapabilities {
        self.inner.capabilities()
    }

    fn optional_methods(&self) -> OptionalMethods {
        self.inner.optional_methods()
    }

    async fn verified_credential(&self, cwd: &str, host: &str) -> ProviderResult<VerifiedCredential> {
        self.inner.verified_credential(cwd, host).await
    }

    async fn get_routing_identity(&self, cwd: &str, host: &str) -> ProviderResult<RoutingIdentity> {
        self.inner.get_routing_identity(cwd, host).await
    }

    // Refused during a pause like any other read, except for the caller that asks for the
    // bypass: a lookup that failed is not held, so letting every background read through would
    // spawn this host's CLI on each of them and re-extend the pause it was already in.
    async fn get_viewer(&self, input: ProviderHostRef) -> ProviderResult<String> {
        self.protect("getViewer", self.viewer_allows_pause, self.inner.get_viewer(input)).await
    }

    async fn list_change_requests(&self, input: ListChangeRequestsInput) -> ProviderResult<ProviderChangeRequestPage> {
        self.protect("listChangeRequests", false, self.inner.list_change_requests(input)).await
    }

    async fn list_change_requests_across(&self, input: ListChangeRequestsAcrossInput) -> ProviderResult<ProviderBatchedChangeRequestPage> {
        self.protect("listChangeRequestsAcross", false, self.inner.list_change_requests_across(input))
            .await
    }

    async fn list_change_request_stats(&self, input: ListChangeRequestStatsInput) -> ProviderResult<Vec<ProviderChangeRequestStat>> {
        self.protect("listChangeRequestStats", false, self.inner.list_change_request_stats(input)).await
    }

    async fn get_change_request(&self, input: ChangeRequestRef) -> ProviderResult<ProviderChangeRequestDetail> {
        self.protect("getChangeRequest", false, self.inner.get_change_request(input)).await
    }

    async fn get_change_request_preview(&self, input: ChangeRequestRef) -> ProviderResult<ProviderChangeRequestPreview> {
        self.protect("getChangeRequestPreview", false, self.inner.get_change_request_preview(input))
            .await
    }

    async fn get_change_request_summary(&self, input: ChangeRequestRef) -> ProviderResult<ProviderChangeRequestSummary> {
        self.protect("getChangeRequestSummary", false, self.inner.get_change_request_summary(input))
            .await
    }

    async fn get_change_request_stack(&self, input: GetChangeRequestStackInput) -> ProviderResult<Option<ProviderChangeRequestStack>> {
        self.protect("getChangeRequestStack", false, self.inner.get_change_request_stack(input)).await
    }

    async fn get_change_request_activity(&self, input: ChangeRequestRef) -> ProviderResult<ProviderChangeRequestActivity> {
        self.protect("getChangeRequestActivity", false, self.inner.get_change_request_activity(input))
            .await
    }

    async fn get_review_thread_comments(&self, input: ReviewThreadCommentsInput) -> ProviderResult<PullRequestThreadCommentsResult> {
        self.protect("getReviewThreadComments", false, self.inner.get_review_thread_comments(input))
            .await
    }

    async fn get_viewer_permissions(&self, input: ViewerPermissionsInput) -> ProviderResult<PullRequestViewerPermissions> {
        self.protect("getViewerPermissions", true, self.inner.get_viewer_permissions(input)).await
    }

    async fn get_diff(&self, input: GetDiffInput) -> ProviderResult<ProviderDiffSlice> {
        self.protect("getDiff", false, self.inner.get_diff(input)).await
    }

    async fn get_diff_file_contents(&self, input: DiffFileContentsInput) -> ProviderResult<ProviderDiffFileContents> {
        self.protect("getDiffFileContents", false, self.inner.get_diff_file_contents(input)).await
    }

    async fn get_files_viewed(&self, input: ChangeRequestRef) -> ProviderResult<ProviderFilesViewed> {
        self.protect("getFilesViewed", false, self.inner.get_files_viewed(input)).await
    }

    async fn set_files_viewed(&self, input: SetFilesViewedInput) -> ProviderResult<()> {
        self.protect("setFilesViewed", true, self.inner.set_files_viewed(input)).await
    }

    async fn get_file_revisions(&self, input: FileRevisionsInput) -> ProviderResult<ProviderFileRevisions> {
        self.protect("getFileRevisions", false, self.inner.get_file_revisions(input)).await
    }

    async fn run_action(&self, input: RunActionInput) -> ProviderResult<()> {
        self.protect("runAction", true, self.inner.run_action(input)).await
    }

    async fn update_change_request(&self, input: UpdateChangeRequestInput) -> ProviderResult<()> {
        self.protect("updateChangeRequest", true, self.inner.update_change_request(input)).await
    }

    async fn comment(&self, input: CommentInput) -> ProviderResult<()> {
        self.protect("comment", true, self.inner.comment(input)).await
    }

    async fn update_comment(&self, input: UpdateCommentInput) -> ProviderResult<()> {
        self.protect("updateComment", true, self.inner.update_comment(input)).await
    }

    async fn submit_review(&self, input: SubmitReviewInput) -> ProviderResult<()> {
        self.protect("submitReview", true, self.inner.submit_review(input)).await
    }

    async fn list_reviewer_candidates(&self, input: ChangeRequestRef) -> ProviderResult<PullRequestReviewerCandidateList> {
        self.protect("listReviewerCandidates", true, self.inner.list_reviewer_candidates(input)).await
    }

    async fn set_reviewer_request(&self, input: SetReviewerRequestInput) -> ProviderResult<()> {
        self.protect("setReviewerRequest", true, self.inner.set_reviewer_request(input)).await
    }

    async fn list_label_candidates(&self, input: ChangeRequestRef) -> ProviderResult<PullRequestLabelCandidateList> {
        self.protect("listLabelCandidates", true, self.inner.list_label_candidates(input)).await
    }

    async fn set_labels(&self, input: SetLabelsInput) -> ProviderResult<()> {
        self.protect("setLabels", true, self.inner.set_labels(input)).await
    }

    async fn reply_to_thread(&self, input: ReplyToThreadInput) -> ProviderResult<()> {
        self.protect("replyToThread", true, self.inner.reply_to_thread(input)).await
    }

    async fn set_reaction(&self, input: SetReactionInput) -> ProviderResult<()> {
        self.protect("setReaction", true, self.inner.set_reaction(input)).await
    }

    async fn set_thread_resolution(&self, input: SetThreadResolutionInput) -> ProviderResult<()> {
        self.protect("setThreadResolution", true, self.inner.set_thread_resolution(input)).await
    }
}
