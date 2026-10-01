//! The real [`PullRequestService`] behind the RPC handlers ([`PullRequestServiceApi`]) and the
//! diff route ([`DiffSource`]).
//!
//! The RPCs pass no options, so `summary` may recover a transient failure from its last good
//! answer and `stack` reads the layer details (the TS defaults: `recoverTransientFailure !==
//! false`, `includeDetails !== false`); `invalidate` notifies readers (`{notifyReaders: true}`).

use async_trait::async_trait;
use futures::future::BoxFuture;
use serde_json::Value;
use zc_contracts::*;
use zc_ports::EventStream;

use crate::error::PullRequestError;
use crate::http::DiffSource;
use crate::rpc::PullRequestServiceApi;
use crate::service::PullRequestService as Service;

#[async_trait]
impl PullRequestServiceApi for Service {
    async fn list(&self, input: PullRequestListInput) -> Result<PullRequestListResult, PullRequestError> {
        Service::list(self, input).await
    }
    async fn list_stats(&self, input: PullRequestListStatsInput) -> Result<PullRequestListStatsResult, PullRequestError> {
        Service::list_stats(self, input).await
    }
    async fn routing(&self, input: PullRequestRef) -> Result<PullRequestRoutingResult, PullRequestError> {
        Service::routing(self, input).await
    }
    async fn routing_identity(&self, input: PullRequestRoutingIdentityInput) -> Result<PullRequestRoutingIdentityResult, PullRequestError> {
        Service::routing_identity(self, input).await
    }
    async fn with_routing_credential(
        &self,
        input: PullRequestRef,
        operation: BoxFuture<'static, Result<Value, PullRequestError>>,
    ) -> Result<Value, PullRequestError> {
        Service::with_routing_credential(self, &input, operation).await
    }
    async fn summary(&self, input: PullRequestRef) -> Result<PullRequestSummary, PullRequestError> {
        Service::summary(self, input, true).await
    }
    async fn stack(&self, input: PullRequestRef) -> Result<Option<PullRequestStack>, PullRequestError> {
        Service::stack(self, input, true).await
    }
    async fn detail(&self, input: PullRequestRef) -> Result<PullRequestDetail, PullRequestError> {
        Service::detail(self, input).await
    }
    async fn preview(&self, input: PullRequestRef) -> Result<PullRequestPreview, PullRequestError> {
        Service::preview(self, input).await
    }
    async fn activity(&self, input: PullRequestRef) -> Result<PullRequestActivity, PullRequestError> {
        Service::activity(self, input).await
    }
    async fn thread_comments(&self, input: PullRequestThreadCommentsInput) -> Result<PullRequestThreadCommentsResult, PullRequestError> {
        Service::thread_comments(self, input).await
    }
    async fn diff_file_contents(&self, input: PullRequestDiffFileContentsInput) -> Result<PullRequestDiffFileContentsResult, PullRequestError> {
        Service::diff_file_contents(self, input).await
    }
    async fn files_viewed(&self, input: PullRequestRef) -> Result<PullRequestFilesViewedResult, PullRequestError> {
        Service::files_viewed(self, input).await
    }
    async fn set_files_viewed(&self, input: PullRequestSetFilesViewedInput) -> Result<(), PullRequestError> {
        Service::set_files_viewed(self, input).await
    }
    async fn run_action(&self, input: PullRequestActionInput) -> Result<(), PullRequestError> {
        Service::run_action(self, input).await
    }
    async fn update(&self, input: PullRequestUpdateInput) -> Result<(), PullRequestError> {
        Service::update(self, input).await
    }
    async fn comment(&self, input: PullRequestCommentInput) -> Result<(), PullRequestError> {
        Service::comment(self, input).await
    }
    async fn update_comment(&self, input: PullRequestCommentUpdateInput) -> Result<(), PullRequestError> {
        Service::update_comment(self, input).await
    }
    async fn submit_review(&self, input: PullRequestSubmitReviewInput) -> Result<(), PullRequestError> {
        Service::submit_review(self, input).await
    }
    async fn reply_to_thread(&self, input: PullRequestThreadReplyInput) -> Result<(), PullRequestError> {
        Service::reply_to_thread(self, input).await
    }
    async fn set_thread_resolution(&self, input: PullRequestThreadResolutionInput) -> Result<(), PullRequestError> {
        Service::set_thread_resolution(self, input).await
    }
    async fn set_reaction(&self, input: PullRequestReactionInput) -> Result<(), PullRequestError> {
        Service::set_reaction(self, input).await
    }
    async fn reviewer_candidates(&self, input: PullRequestRef) -> Result<PullRequestReviewerCandidateList, PullRequestError> {
        Service::reviewer_candidates(self, input).await
    }
    async fn request_reviewers(&self, input: PullRequestReviewerRequestInput) -> Result<(), PullRequestError> {
        Service::request_reviewers(self, input).await
    }
    async fn label_candidates(&self, input: PullRequestRef) -> Result<PullRequestLabelCandidateList, PullRequestError> {
        Service::label_candidates(self, input).await
    }
    async fn set_labels(&self, input: PullRequestLabelChangeInput) -> Result<(), PullRequestError> {
        Service::set_labels(self, input).await
    }
    async fn invalidate(&self, input: PullRequestInvalidateInput) {
        Service::invalidate(self, input, true).await
    }
    fn subscribe_refreshes(&self) -> EventStream<u64> {
        Service::subscribe_refreshes(self)
    }
}

#[async_trait]
impl DiffSource for Service {
    async fn diff(&self, input: PullRequestDiffInput) -> Result<PullRequestDiffResult, PullRequestError> {
        Service::diff(self, input).await
    }
}
