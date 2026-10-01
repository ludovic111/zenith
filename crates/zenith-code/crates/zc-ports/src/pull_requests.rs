//! The pull request port: the part of `PullRequestService`
//! (`apps/server/src/pullRequest/PullRequestService.ts`) that the orchestration reactors
//! (`ThreadPullRequestReactor`, `PullRequestSyncReactor`, `ThreadSettlementReactor`,
//! `CheckpointReactor`) and the `/api/pull-requests/diff` endpoint use.
//!
//! The RPC handlers use the whole service from zc-pullrequest directly; only this slice crosses
//! the orchestration ↔ pull request cycle.

use async_trait::async_trait;

use crate::contracts::{
    ProjectId, PullRequestDiffInput, PullRequestDiffResult, PullRequestError, PullRequestInvalidateInput, PullRequestRef, PullRequestStack, PullRequestSummary,
};
use crate::EventStream;

/// `PullRequestMergeEvent`: a pull request this server saw merge.
#[derive(Debug, Clone, PartialEq)]
pub struct PullRequestMergeEvent {
    pub reference: PullRequestRef,
    /// ISO timestamp.
    pub merged_at: String,
}

#[async_trait]
pub trait PullRequests: Send + Sync {
    /// `summary(ref, {recoverTransientFailure?})`.
    async fn summary(&self, reference: PullRequestRef, recover_transient_failure: bool) -> Result<PullRequestSummary, PullRequestError>;

    /// `stack(ref, {includeDetails?})`: the host-native stack, or `None`.
    async fn stack(&self, reference: PullRequestRef, include_details: bool) -> Result<Option<PullRequestStack>, PullRequestError>;

    /// `diff(input)` (also served as `POST /api/pull-requests/diff`).
    async fn diff(&self, input: PullRequestDiffInput) -> Result<PullRequestDiffResult, PullRequestError>;

    /// `invalidate(input, {notifyReaders?})`. Never fails.
    async fn invalidate(&self, input: PullRequestInvalidateInput, notify_readers: bool);

    /// `refreshAfterTurn(projectId)`: refresh the project's PRs after an agent turn. Never fails.
    async fn refresh_after_turn(&self, project_id: &ProjectId);

    /// `subscribeMerges`: merges seen from now on.
    fn subscribe_merges(&self) -> EventStream<PullRequestMergeEvent>;

    /// `subscribeRefreshes`: a counter bumped whenever cached PR data should be re-read
    /// (`pullRequests.subscribeRefreshes`).
    fn subscribe_refreshes(&self) -> EventStream<u64>;
}
