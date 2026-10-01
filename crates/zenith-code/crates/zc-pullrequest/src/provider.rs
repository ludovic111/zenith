//! `PullRequestProvider.ts`: the neutral types every forge hands back and the
//! [`PullRequestProviderApi`] trait each forge implements. **Frozen**: forges and the service
//! code against this file.
//!
//! Mapping conventions from TS:
//! - `T | null` → `Option<T>`; an *optional* `T | null | undefined` whose absence means something
//!   different from `null` (e.g. `reviewDecision`: `undefined` = the host does not summarise,
//!   `null` = no decision yet) → `Option<Option<T>>` (outer `None` = absent).
//! - Optional TS methods (`api.getChangeRequestSummary === undefined`) are trait methods with a
//!   default body that fails with [`PullRequestProviderError::unsupported`]; whether a provider
//!   implements one is declared by [`PullRequestProviderApi::optional_methods`], which is what
//!   the service checks (never call an optional method the provider did not declare).
//! - Timestamps are the host's ISO strings, passed through as the TS code does.

use std::sync::Arc;

use async_trait::async_trait;
use futures::future::BoxFuture;
use zc_contracts::{
    PullRequestAction, PullRequestActor, PullRequestBaseComparison, PullRequestCapabilities, PullRequestCheck, PullRequestChecksState, PullRequestComment,
    PullRequestCommentUpdateInputKind, PullRequestCommit, PullRequestDiffFileContentsInputChangeType, PullRequestFileViewed, PullRequestInvolvement,
    PullRequestLabel, PullRequestLabelCandidateList, PullRequestListFilters, PullRequestListState, PullRequestMergeCapabilities, PullRequestMergeMethod,
    PullRequestMergeability, PullRequestOmittedFileStat, PullRequestReaction, PullRequestReactionContent, PullRequestReviewCommentDraft,
    PullRequestReviewDecision, PullRequestReviewThread, PullRequestReviewVerdict, PullRequestReviewerCandidateList, PullRequestReviewerKind,
    PullRequestStackHead, PullRequestStackMembership, PullRequestState, PullRequestThreadCommentsResult, PullRequestUpdateMethod, PullRequestViewerPermissions,
    SourceControlProviderKind,
};

pub use crate::error::{ProviderFailureReason, PullRequestProviderError};

/// Every provider method's result.
pub type ProviderResult<T> = Result<T, PullRequestProviderError>;

/// `ProviderChangeRequest`: a change request as the provider sees it, before the service
/// attaches project context.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderChangeRequest {
    pub stack: Option<PullRequestStackMembership>,
    pub number: i64,
    pub title: String,
    pub url: String,
    pub author: Option<PullRequestActor>,
    pub head_branch: String,
    /// `headRepositoryNameWithOwner?: string | null`.
    pub head_repository_name_with_owner: Option<Option<String>>,
    pub base_branch: String,
    pub state: PullRequestState,
    pub is_draft: bool,
    pub mergeability: PullRequestMergeability,
    pub additions: i64,
    pub deletions: i64,
    pub created_at: String,
    /// `closedAt?: string | null`.
    pub closed_at: Option<Option<String>>,
    /// `mergedAt?: string | null`.
    pub merged_at: Option<Option<String>>,
    pub updated_at: String,
    /// Accounts with a review requested. Team-level requests are excluded by each provider.
    pub review_request_logins: Vec<String>,
    pub labels: Vec<PullRequestLabel>,
    /// Absent (`None`) from a host that does not summarise its reviews (every host but GitHub);
    /// `Some(None)` is "no decision yet".
    pub review_decision: Option<Option<PullRequestReviewDecision>>,
    /// Absent from a host that reports no check rollup on its listings.
    pub checks_state: Option<Option<PullRequestChecksState>>,
}

/// `ProviderChangeRequestSummary`: the fields needed to keep a linked thread's status live.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderChangeRequestSummary {
    pub number: i64,
    pub title: String,
    pub url: String,
    pub head_branch: String,
    pub base_branch: String,
    pub state: PullRequestState,
    /// Present when the host says an open pull request is still a draft.
    pub is_draft: Option<bool>,
    pub closed_at: Option<Option<String>>,
    pub merged_at: Option<Option<String>>,
    pub updated_at: String,
    /// Overview fields, present where the host's single read returns them at no extra cost.
    pub author: Option<Option<PullRequestActor>>,
    pub additions: Option<i64>,
    pub deletions: Option<i64>,
    pub changed_files: Option<i64>,
    pub review_decision: Option<Option<PullRequestReviewDecision>>,
    pub checks_state: Option<Option<PullRequestChecksState>>,
    pub mergeability: Option<PullRequestMergeability>,
}

/// `ProviderChangeRequestStackLayer`: one layer of a host-native stack (bottom to top).
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderChangeRequestStackLayer {
    pub title: Option<String>,
    pub is_draft: Option<bool>,
    pub head_sha: Option<String>,
    pub number: i64,
    pub head_branch: String,
    pub state: PullRequestState,
}

/// `ProviderChangeRequestStack`: a host-native stack (only GitHub has one).
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderChangeRequestStack {
    pub id: String,
    pub number: i64,
    pub url: String,
    pub base: String,
    pub layers: Vec<ProviderChangeRequestStackLayer>,
}

/// `ProviderChangeRequestPage`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderChangeRequestPage {
    pub items: Vec<ProviderChangeRequest>,
    /// The host has more rows than the page size asked for.
    pub truncated: bool,
    /// Optional count-based cursor advance (an offset-paged host may count malformed rows).
    pub cursor_advance: Option<i64>,
    /// This page can be carried on from.
    pub continues: bool,
}

/// `ProviderListCursor`: where a repository's next slice starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderListCursor {
    /// The instant of the oldest row already handed over (asked for inclusively).
    pub updated_before: String,
    /// How many provider rows this repository has consumed so far.
    pub delivered: i64,
}

/// `ProviderBatchedChangeRequest`: one repository's row inside an answer spanning several.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderBatchedChangeRequest {
    /// Provider-native identity, exactly as it was asked for.
    pub repository: String,
    pub change_request: ProviderChangeRequest,
}

/// `ProviderBatchedChangeRequestPage`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderBatchedChangeRequestPage {
    pub items: Vec<ProviderBatchedChangeRequest>,
    pub truncated: bool,
}

/// `ProviderChangeRequestStat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderChangeRequestStat {
    pub repository: String,
    pub number: i64,
    pub additions: i64,
    pub deletions: i64,
}

/// `ProviderChangeRequestDetail`: the listing row plus the detail fields.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderChangeRequestDetail {
    /// The `ProviderChangeRequest` part. Its `closed_at`/`merged_at` are ignored in favour of the
    /// required fields below.
    pub change_request: ProviderChangeRequest,
    pub body: String,
    pub changed_files: i64,
    pub merged_at: Option<String>,
    pub closed_at: Option<String>,
    pub reviewers: Vec<PullRequestActor>,
    pub checks: Vec<PullRequestCheck>,
    pub merge_capabilities: PullRequestMergeCapabilities,
    pub viewer_permissions: PullRequestViewerPermissions,
    pub base_comparison: Option<PullRequestBaseComparison>,
    pub behind_by: Option<i64>,
    pub auto_merge_enabled: Option<bool>,
    pub auto_merge_method: Option<PullRequestMergeMethod>,
    pub workflow_approvals_required: Option<i64>,
}

/// `ProviderChangeRequestActivity`: the conversation-shaped half of a detail.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProviderChangeRequestActivity {
    /// `author?: PullRequestActor | null`: an optional richer actor.
    pub author: Option<Option<PullRequestActor>>,
    pub reviewers: Option<Vec<PullRequestActor>>,
    pub comments: Vec<PullRequestComment>,
    pub comment_count: i64,
    pub comments_truncated: bool,
    pub review_threads: Vec<PullRequestReviewThread>,
    pub commits: Vec<PullRequestCommit>,
    pub reactions: Option<Vec<PullRequestReaction>>,
}

/// `ProviderDiffSlice`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProviderDiffSlice {
    pub patch: String,
    /// Something in this slice could not be shown (as opposed to there being more slices).
    pub truncated: bool,
    pub next_cursor: Option<String>,
    pub omitted_file_stats: Option<Vec<PullRequestOmittedFileStat>>,
}

/// `ProviderDiffFileContents`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProviderDiffFileContents {
    pub old_contents: String,
    pub new_contents: String,
}

/// `ProviderFilesViewed`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProviderFilesViewed {
    pub files: Vec<PullRequestFileViewed>,
    pub truncated: bool,
}

/// `ProviderFileRevisions`: opaque per-path versions on the head; `""` for a deleted file. A path
/// is absent only where the read could not say.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProviderFileRevisions {
    /// Insertion-ordered, like the TS `Map`.
    pub revisions: Vec<(String, String)>,
    /// `complete?: boolean`.
    pub complete: Option<bool>,
}

/// `ProviderRepositoryRef`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProviderRepositoryRef {
    pub cwd: String,
    /// Provider-native repository identity, e.g. `owner/repo` or `group/subgroup/project`.
    pub repository: String,
    /// The host it lives on.
    pub host: String,
}

/// `ProviderRepositoryRef & { number }`: the input of most per-change-request methods.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChangeRequestRef {
    pub cwd: String,
    pub repository: String,
    pub host: String,
    pub number: i64,
}

impl ChangeRequestRef {
    pub fn repository_ref(&self) -> ProviderRepositoryRef {
        ProviderRepositoryRef {
            cwd: self.cwd.clone(),
            repository: self.repository.clone(),
            host: self.host.clone(),
        }
    }
}

/// `{cwd, host}` (`getViewer` takes an optional host).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProviderHostRef {
    pub cwd: String,
    pub host: Option<String>,
}

/// `listChangeRequests` input.
#[derive(Debug, Clone, PartialEq)]
pub struct ListChangeRequestsInput {
    pub cwd: String,
    pub repository: String,
    pub host: String,
    pub state: PullRequestListState,
    pub involvement: PullRequestInvolvement,
    pub viewer: String,
    pub limit: i64,
    pub query: Option<String>,
    pub cursor: Option<ProviderListCursor>,
    pub filters: Option<PullRequestListFilters>,
}

/// `listChangeRequestsAcross` input.
#[derive(Debug, Clone, PartialEq)]
pub struct ListChangeRequestsAcrossInput {
    /// Any checkout on the host.
    pub cwd: String,
    pub host: String,
    pub repositories: Vec<String>,
    pub state: PullRequestListState,
    pub involvement: PullRequestInvolvement,
    pub viewer: String,
    pub limit: i64,
    pub query: Option<String>,
    pub cursor: Option<ProviderListCursor>,
    pub filters: Option<PullRequestListFilters>,
}

/// `listChangeRequestStats` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListChangeRequestStatsInput {
    pub cwd: String,
    pub host: String,
    /// `(repository, number)`.
    pub change_requests: Vec<(String, i64)>,
}

/// `getChangeRequestStack` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetChangeRequestStackInput {
    pub change_request: ChangeRequestRef,
    pub include_details: Option<bool>,
}

/// `getReviewThreadComments` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewThreadCommentsInput {
    pub change_request: ChangeRequestRef,
    pub thread_id: String,
    pub cursor: String,
}

/// `getViewerPermissions` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewerPermissionsInput {
    pub change_request: ChangeRequestRef,
    /// Skip branch comparison when checking permission for an unrelated operation.
    pub include_update_branch: Option<bool>,
}

/// `getDiff` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetDiffInput {
    pub change_request: ChangeRequestRef,
    pub cursor: Option<String>,
    /// One commit's own changes.
    pub commit: Option<String>,
}

/// `getDiffFileContents` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFileContentsInput {
    pub change_request: ChangeRequestRef,
    pub commit: Option<String>,
    pub change_type: PullRequestDiffFileContentsInputChangeType,
    pub old_path: String,
    pub new_path: String,
}

/// `setFilesViewed` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetFilesViewedInput {
    pub change_request: ChangeRequestRef,
    /// `(path, viewed)`.
    pub files: Vec<(String, bool)>,
}

/// `getFileRevisions` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRevisionsInput {
    pub change_request: ChangeRequestRef,
    pub paths: Vec<String>,
}

/// `runAction` input.
#[derive(Debug, Clone, PartialEq)]
pub struct RunActionInput {
    pub change_request: ChangeRequestRef,
    pub action: PullRequestAction,
    pub stack_number: Option<i64>,
    pub expected_stack_heads: Option<Vec<PullRequestStackHead>>,
    pub merge_method: Option<PullRequestMergeMethod>,
    pub update_method: Option<PullRequestUpdateMethod>,
}

/// `updateChangeRequest` input (never both `None`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateChangeRequestInput {
    pub change_request: ChangeRequestRef,
    pub title: Option<String>,
    pub body: Option<String>,
}

/// `comment` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentInput {
    pub change_request: ChangeRequestRef,
    pub body: String,
}

/// `updateComment` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCommentInput {
    pub change_request: ChangeRequestRef,
    pub comment_id: String,
    pub kind: PullRequestCommentUpdateInputKind,
    pub body: String,
}

/// `submitReview` input.
#[derive(Debug, Clone, PartialEq)]
pub struct SubmitReviewInput {
    pub change_request: ChangeRequestRef,
    pub verdict: PullRequestReviewVerdict,
    pub body: String,
    pub comments: Vec<PullRequestReviewCommentDraft>,
}

/// One reviewer of `setReviewerRequest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewerRef {
    pub id: String,
    pub kind: PullRequestReviewerKind,
}

/// `setReviewerRequest` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetReviewerRequestInput {
    pub change_request: ChangeRequestRef,
    pub reviewers: Vec<ReviewerRef>,
    pub requested: bool,
}

/// `setLabels` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetLabelsInput {
    pub change_request: ChangeRequestRef,
    pub labels: Vec<String>,
    pub applied: bool,
}

/// `replyToThread` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyToThreadInput {
    pub change_request: ChangeRequestRef,
    pub thread_id: String,
    pub body: String,
}

/// `setReaction` input. `subject_id: None` is the change request itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetReactionInput {
    pub change_request: ChangeRequestRef,
    pub subject_id: Option<String>,
    pub content: PullRequestReactionContent,
    pub reacted: bool,
}

/// `setThreadResolution` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetThreadResolutionInput {
    pub change_request: ChangeRequestRef,
    pub thread_id: String,
    pub resolved: bool,
}

/// The routing identity of `getRoutingIdentity`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingIdentity {
    pub account_id: String,
    pub viewer: String,
}

/// The identity handed to `withVerifiedCredential`'s `use`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedIdentity {
    pub account_id: String,
    pub viewer: String,
    pub credential_fingerprint: String,
}

/// Runs a future with the verified credential in scope (the TS `Effect.provideService(
/// PinnedGitHubCredential, …)` around `use`). Type-erased so the trait stays dyn-compatible:
/// callers write their result into a slot captured by the future.
pub trait CredentialScope: Send + Sync {
    fn run<'a>(&'a self, future: BoxFuture<'a, ()>) -> BoxFuture<'a, ()>;
}

/// A verified credential: who it is, and the scope that pins every host call to it.
#[derive(Clone)]
pub struct VerifiedCredential {
    pub identity: VerifiedIdentity,
    pub scope: Arc<dyn CredentialScope>,
}

impl std::fmt::Debug for VerifiedCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifiedCredential").field("identity", &self.identity).finish_non_exhaustive()
    }
}

/// Which optional methods of [`PullRequestProviderApi`] a provider implements (the TS
/// `api.method !== undefined` checks).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OptionalMethods {
    pub with_verified_credential: bool,
    pub get_routing_identity: bool,
    pub list_change_requests_across: bool,
    pub list_change_request_stats: bool,
    pub get_change_request_preview: bool,
    pub get_change_request_summary: bool,
    pub get_change_request_stack: bool,
    pub get_review_thread_comments: bool,
    pub get_diff_file_contents: bool,
    pub get_files_viewed: bool,
    pub set_files_viewed: bool,
    pub get_file_revisions: bool,
    pub update_change_request: bool,
    pub update_comment: bool,
    pub list_label_candidates: bool,
    pub set_labels: bool,
}

/// `getChangeRequestPreview`'s answer: `Omit<PullRequestPreview, "projectId" | "repository">`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderChangeRequestPreview {
    pub number: i64,
    pub title: String,
    pub url: String,
    pub author: Option<PullRequestActor>,
    pub state: PullRequestState,
    pub is_draft: bool,
    pub created_at: String,
}

/// `PullRequestProviderApi`: one host's change requests. Implementations own their tool and JSON
/// shapes and hand back the neutral types above; anything a host cannot do is declared in
/// `capabilities` rather than failing at call time.
#[async_trait]
pub trait PullRequestProviderApi: Send + Sync {
    fn kind(&self) -> SourceControlProviderKind;
    fn capabilities(&self) -> &PullRequestCapabilities;
    /// Which optional methods below are implemented.
    fn optional_methods(&self) -> OptionalMethods;

    /// `withVerifiedCredential` (optional): resolves and verifies the host credential.
    async fn verified_credential(&self, cwd: &str, host: &str) -> ProviderResult<VerifiedCredential> {
        let _ = (cwd, host);
        Err(PullRequestProviderError::unsupported(self.kind(), "withVerifiedCredential"))
    }

    /// `getRoutingIdentity` (optional).
    async fn get_routing_identity(&self, cwd: &str, host: &str) -> ProviderResult<RoutingIdentity> {
        let _ = (cwd, host);
        Err(PullRequestProviderError::unsupported(self.kind(), "getRoutingIdentity"))
    }

    /// `getViewer`: the signed-in account, which involvement filtering compares against.
    async fn get_viewer(&self, input: ProviderHostRef) -> ProviderResult<String>;

    async fn list_change_requests(&self, input: ListChangeRequestsInput) -> ProviderResult<ProviderChangeRequestPage>;

    /// `listChangeRequestsAcross` (optional): one host-wide search.
    async fn list_change_requests_across(&self, input: ListChangeRequestsAcrossInput) -> ProviderResult<ProviderBatchedChangeRequestPage> {
        let _ = input;
        Err(PullRequestProviderError::unsupported(self.kind(), "listChangeRequestsAcross"))
    }

    /// `listChangeRequestStats` (optional).
    async fn list_change_request_stats(&self, input: ListChangeRequestStatsInput) -> ProviderResult<Vec<ProviderChangeRequestStat>> {
        let _ = input;
        Err(PullRequestProviderError::unsupported(self.kind(), "listChangeRequestStats"))
    }

    async fn get_change_request(&self, input: ChangeRequestRef) -> ProviderResult<ProviderChangeRequestDetail>;

    /// `getChangeRequestPreview` (optional).
    async fn get_change_request_preview(&self, input: ChangeRequestRef) -> ProviderResult<ProviderChangeRequestPreview> {
        let _ = input;
        Err(PullRequestProviderError::unsupported(self.kind(), "getChangeRequestPreview"))
    }

    /// `getChangeRequestSummary` (optional).
    async fn get_change_request_summary(&self, input: ChangeRequestRef) -> ProviderResult<ProviderChangeRequestSummary> {
        let _ = input;
        Err(PullRequestProviderError::unsupported(self.kind(), "getChangeRequestSummary"))
    }

    /// `getChangeRequestStack` (optional): `None` when not stacked.
    async fn get_change_request_stack(&self, input: GetChangeRequestStackInput) -> ProviderResult<Option<ProviderChangeRequestStack>> {
        let _ = input;
        Err(PullRequestProviderError::unsupported(self.kind(), "getChangeRequestStack"))
    }

    async fn get_change_request_activity(&self, input: ChangeRequestRef) -> ProviderResult<ProviderChangeRequestActivity>;

    /// `getReviewThreadComments` (optional).
    async fn get_review_thread_comments(&self, input: ReviewThreadCommentsInput) -> ProviderResult<PullRequestThreadCommentsResult> {
        let _ = input;
        Err(PullRequestProviderError::unsupported(self.kind(), "getReviewThreadComments"))
    }

    async fn get_viewer_permissions(&self, input: ViewerPermissionsInput) -> ProviderResult<PullRequestViewerPermissions>;

    /// Only called when `capabilities.diff` is true.
    async fn get_diff(&self, input: GetDiffInput) -> ProviderResult<ProviderDiffSlice>;

    /// `getDiffFileContents` (optional).
    async fn get_diff_file_contents(&self, input: DiffFileContentsInput) -> ProviderResult<ProviderDiffFileContents> {
        let _ = input;
        Err(PullRequestProviderError::unsupported(self.kind(), "getDiffFileContents"))
    }

    /// `getFilesViewed` (optional; `capabilities.viewedFiles == "host"`).
    async fn get_files_viewed(&self, input: ChangeRequestRef) -> ProviderResult<ProviderFilesViewed> {
        let _ = input;
        Err(PullRequestProviderError::unsupported(self.kind(), "getFilesViewed"))
    }

    /// `setFilesViewed` (optional; `capabilities.viewedFiles == "host"`).
    async fn set_files_viewed(&self, input: SetFilesViewedInput) -> ProviderResult<()> {
        let _ = input;
        Err(PullRequestProviderError::unsupported(self.kind(), "setFilesViewed"))
    }

    /// `getFileRevisions` (optional; required when `capabilities.viewedFiles == "environment"`).
    async fn get_file_revisions(&self, input: FileRevisionsInput) -> ProviderResult<ProviderFileRevisions> {
        let _ = input;
        Err(PullRequestProviderError::unsupported(self.kind(), "getFileRevisions"))
    }

    async fn run_action(&self, input: RunActionInput) -> ProviderResult<()>;

    /// `updateChangeRequest` (optional; `capabilities.edit.changeRequest`).
    async fn update_change_request(&self, input: UpdateChangeRequestInput) -> ProviderResult<()> {
        let _ = input;
        Err(PullRequestProviderError::unsupported(self.kind(), "updateChangeRequest"))
    }

    async fn comment(&self, input: CommentInput) -> ProviderResult<()>;

    /// `updateComment` (optional; `capabilities.edit.comment`).
    async fn update_comment(&self, input: UpdateCommentInput) -> ProviderResult<()> {
        let _ = input;
        Err(PullRequestProviderError::unsupported(self.kind(), "updateComment"))
    }

    async fn submit_review(&self, input: SubmitReviewInput) -> ProviderResult<()>;

    async fn list_reviewer_candidates(&self, input: ChangeRequestRef) -> ProviderResult<PullRequestReviewerCandidateList>;

    async fn set_reviewer_request(&self, input: SetReviewerRequestInput) -> ProviderResult<()>;

    /// `listLabelCandidates` (optional; with `setLabels` where `capabilities.labels`).
    async fn list_label_candidates(&self, input: ChangeRequestRef) -> ProviderResult<PullRequestLabelCandidateList> {
        let _ = input;
        Err(PullRequestProviderError::unsupported(self.kind(), "listLabelCandidates"))
    }

    /// `setLabels` (optional).
    async fn set_labels(&self, input: SetLabelsInput) -> ProviderResult<()> {
        let _ = input;
        Err(PullRequestProviderError::unsupported(self.kind(), "setLabels"))
    }

    /// Only called when `capabilities.review.reply`.
    async fn reply_to_thread(&self, input: ReplyToThreadInput) -> ProviderResult<()>;

    /// Only called when `capabilities.reactions`.
    async fn set_reaction(&self, input: SetReactionInput) -> ProviderResult<()>;

    /// Only called when `capabilities.review.resolve`.
    async fn set_thread_resolution(&self, input: SetThreadResolutionInput) -> ProviderResult<()>;
}

/// A shared provider.
pub type SharedProvider = Arc<dyn PullRequestProviderApi>;
