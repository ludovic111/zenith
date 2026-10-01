//! Shared fixtures of the `service_*` tests (the port of `PullRequestService.test.ts`'s
//! helpers): projects, provider rows, a fake provider whose every call the test supplies, a fake
//! projection reader, and a service built over them with a manual clock, a temp database and an
//! in-memory read cache.

#![allow(dead_code)]

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::future::{BoxFuture, FutureExt};
use serde_json::json;
use zc_contracts::*;
use zc_core::vcs_process::VcsProcess;
use zc_ports::contracts::TaggedError;
use zc_ports::orchestration::{
    DeletedWorktreeThread, FullThreadDiffContext, ImportedAgentSessionSource, ReplayStats, SnapshotCounts, ThreadCheckpointContext, ThreadDetailQuery,
    ThreadPullRequests, ThreadRuntimeContext, TurnStartMessage,
};
use zc_ports::ProjectionReads;
use zc_pullrequest::provider::*;
use zc_pullrequest::{PullRequestError, PullRequestProviderError, PullRequestReadCache, PullRequestService, PullRequestServiceDeps};
use zc_sourcecontrol::discovery::{DiscoverySpec, ManagedCliDiscovery};
use zc_sourcecontrol::registry::{SourceControlProviderRegistration, UnsupportedProvider};
use zc_sourcecontrol::util::{ManualClock, SharedClock};
use zc_sourcecontrol::{SourceControlProviderContext, SourceControlProviderRegistry};

pub use zc_contracts::SourceControlProviderKind as Kind;

// -------------------------------------------------------------------------------------------
// Fixtures
// -------------------------------------------------------------------------------------------

/// `project({id, title, workspaceRoot, repository?, provider?, host?, remoteUrl?})`.
#[derive(Debug, Clone)]
pub struct ProjectSpec {
    pub id: String,
    pub title: String,
    pub workspace_root: String,
    pub repository: Option<String>,
    pub provider: Option<String>,
    pub host: Option<String>,
    pub remote_url: Option<String>,
}

impl ProjectSpec {
    pub fn provider(mut self, provider: &str) -> Self {
        self.provider = Some(provider.into());
        self
    }

    pub fn host(mut self, host: &str) -> Self {
        self.host = Some(host.into());
        self
    }

    pub fn remote_url(mut self, remote_url: &str) -> Self {
        self.remote_url = Some(remote_url.into());
        self
    }

    pub fn build(&self) -> OrchestrationProjectShell {
        // The host defaults from the provider, so a fixture only names one when the point of the
        // test is two hosts of the same kind.
        let host = self.host.clone().unwrap_or_else(|| {
            if self.provider.as_deref() == Some("gitlab") {
                "gitlab.com".into()
            } else {
                "github.com".into()
            }
        });
        let mut value = json!({
            "id": self.id,
            "title": self.title,
            "workspaceRoot": self.workspace_root,
            "defaultModelSelection": null,
            "scripts": [],
            "createdAt": "2026-07-01T00:00:00Z",
            "updatedAt": "2026-07-01T00:00:00Z",
        });
        if let Some(repository) = &self.repository {
            value["repositoryIdentity"] = json!({
                "canonicalKey": format!("{host}/{repository}"),
                "locator": {
                    "source": "git-remote",
                    "remoteName": "origin",
                    "remoteUrl": self.remote_url.clone().unwrap_or_else(|| format!("https://{host}/{repository}.git")),
                },
                "provider": self.provider.clone().unwrap_or_else(|| "github".into()),
                "displayName": repository,
            });
        }
        serde_json::from_value(value).expect("a valid project shell")
    }
}

impl From<ProjectSpec> for OrchestrationProjectShell {
    fn from(spec: ProjectSpec) -> Self {
        spec.build()
    }
}

pub fn project(id: &str, title: &str, workspace_root: &str, repository: Option<&str>) -> ProjectSpec {
    ProjectSpec {
        id: id.into(),
        title: title.into(),
        workspace_root: workspace_root.into(),
        repository: repository.map(Into::into),
        provider: None,
        host: None,
        remote_url: None,
    }
}

/// A project with a repository on github.com.
pub fn gh(id: &str, title: &str, workspace_root: &str, repository: &str) -> ProjectSpec {
    project(id, title, workspace_root, Some(repository))
}

pub fn actor(login: &str) -> PullRequestActor {
    PullRequestActor {
        is_bot: None,
        login: login.into(),
        name: None,
        avatar_url: None,
    }
}

/// `changeRequest(number, updatedAt)`.
pub fn change_request(number: i64, updated_at: &str) -> ProviderChangeRequest {
    ProviderChangeRequest {
        stack: None,
        number,
        title: format!("Change request {number}"),
        url: format!("https://host/pull/{number}"),
        author: Some(actor("octocat")),
        head_branch: format!("feat/{number}"),
        head_repository_name_with_owner: None,
        base_branch: "main".into(),
        state: PullRequestState::Open,
        is_draft: false,
        mergeability: PullRequestMergeability::Mergeable,
        additions: 1,
        deletions: 0,
        created_at: "2026-07-01T00:00:00Z".into(),
        closed_at: None,
        merged_at: None,
        updated_at: updated_at.into(),
        review_request_logins: Vec::new(),
        labels: Vec::new(),
        review_decision: None,
        checks_state: None,
    }
}

/// A host's summary read of `changeRequest(number, updatedAt)`.
pub fn summary_row(number: i64, updated_at: &str) -> ProviderChangeRequestSummary {
    let row = change_request(number, updated_at);
    ProviderChangeRequestSummary {
        number: row.number,
        title: row.title,
        url: row.url,
        head_branch: row.head_branch,
        base_branch: row.base_branch,
        state: row.state,
        is_draft: Some(row.is_draft),
        closed_at: None,
        merged_at: None,
        updated_at: row.updated_at,
        author: Some(row.author),
        additions: Some(row.additions),
        deletions: Some(row.deletions),
        changed_files: None,
        review_decision: None,
        checks_state: None,
        mergeability: Some(row.mergeability),
    }
}

/// A host's narrow preview read of `changeRequest(number, updatedAt)`.
pub fn preview_row(number: i64, updated_at: &str) -> ProviderChangeRequestPreview {
    let row = change_request(number, updated_at);
    ProviderChangeRequestPreview {
        number: row.number,
        title: row.title,
        url: row.url,
        author: row.author,
        state: row.state,
        is_draft: row.is_draft,
        created_at: row.created_at,
    }
}

/// `{...changeRequest(number, repository, updatedAt), repository}` (a batched row).
pub fn batched(number: i64, repository: &str, updated_at: &str) -> ProviderBatchedChangeRequest {
    ProviderBatchedChangeRequest {
        repository: repository.into(),
        change_request: change_request(number, updated_at),
    }
}

pub fn all_permissions() -> PullRequestViewerPermissions {
    PullRequestViewerPermissions {
        stack_rebase: None,
        actions: vec![
            PullRequestAction::Merge,
            PullRequestAction::Ready,
            PullRequestAction::Draft,
            PullRequestAction::Close,
            PullRequestAction::Reopen,
        ],
        comment: true,
        resolve: true,
        verdicts: vec![
            PullRequestReviewVerdict::Comment,
            PullRequestReviewVerdict::Approve,
            PullRequestReviewVerdict::RequestChanges,
        ],
        request_reviewers: true,
        update_methods: None,
        labels: None,
    }
}

/// `hostedChangeRequest(body, additions)`.
pub fn hosted(body: &str, additions: i64) -> ProviderChangeRequestDetail {
    detail_of(
        ProviderChangeRequest {
            additions,
            ..change_request(1, "2026-07-02T00:00:00Z")
        },
        body,
    )
}

/// The detail of a listing row, with nothing else to say.
pub fn detail_of(change_request: ProviderChangeRequest, body: &str) -> ProviderChangeRequestDetail {
    ProviderChangeRequestDetail {
        change_request,
        body: body.into(),
        changed_files: 2,
        merged_at: None,
        closed_at: None,
        reviewers: Vec::new(),
        checks: Vec::new(),
        merge_capabilities: PullRequestMergeCapabilities {
            merge: true,
            squash: true,
            rebase: true,
        },
        viewer_permissions: PullRequestViewerPermissions {
            actions: vec![PullRequestAction::Merge],
            ..all_permissions()
        },
        base_comparison: None,
        behind_by: None,
        auto_merge_enabled: None,
        auto_merge_method: None,
        workflow_approvals_required: None,
    }
}

pub fn empty_activity() -> ProviderChangeRequestActivity {
    ProviderChangeRequestActivity::default()
}

pub fn page(items: Vec<ProviderChangeRequest>, truncated: bool, continues: bool) -> ProviderChangeRequestPage {
    ProviderChangeRequestPage {
        items,
        truncated,
        cursor_advance: None,
        continues,
    }
}

pub fn diff_slice(patch: &str, next_cursor: Option<&str>) -> ProviderDiffSlice {
    ProviderDiffSlice {
        patch: patch.into(),
        truncated: false,
        next_cursor: next_cursor.map(Into::into),
        omitted_file_stats: None,
    }
}

/// `unusable(provider, reason)`.
pub fn unusable(provider: Kind, reason: ProviderFailureReason) -> PullRequestProviderError {
    PullRequestProviderError::new(provider, "getViewer", reason, format!("{} is not usable.", provider.as_str()))
}

/// `requestFailed`.
pub fn request_failed() -> PullRequestProviderError {
    PullRequestProviderError::failed(Kind::Github, "listChangeRequests", "HTTP 404")
}

pub fn failed(provider: Kind, operation: &str, detail: &str) -> PullRequestProviderError {
    PullRequestProviderError::failed(provider, operation, detail)
}

pub fn rate_limited(provider: Kind, operation: &str, detail: &str, retry_at: Option<i64>) -> PullRequestProviderError {
    PullRequestProviderError::new(provider, operation, ProviderFailureReason::RateLimited, detail).with_retry_at(retry_at)
}

/// `FULL_REVIEW`.
pub fn full_review() -> PullRequestReviewCapabilities {
    PullRequestReviewCapabilities {
        inline_comment: true,
        reply: true,
        resolve: true,
        verdicts: vec![
            PullRequestReviewVerdict::Comment,
            PullRequestReviewVerdict::Approve,
            PullRequestReviewVerdict::RequestChanges,
        ],
    }
}

/// `FULL_REVIEWERS`.
pub fn full_reviewers() -> PullRequestReviewerCapabilities {
    PullRequestReviewerCapabilities {
        request: true,
        list_candidates: true,
    }
}

/// The capabilities `fakeProvider` claims: everything a host could offer.
pub fn full_capabilities() -> PullRequestCapabilities {
    PullRequestCapabilities {
        diff: true,
        comment: true,
        actions: vec![
            PullRequestAction::Merge,
            PullRequestAction::Ready,
            PullRequestAction::Draft,
            PullRequestAction::Close,
            PullRequestAction::Reopen,
        ],
        merge_methods: vec![PullRequestMergeMethod::Merge],
        update_methods: None,
        search: true,
        reactions: Some(true),
        viewed_files: None,
        review: full_review(),
        reviewers: full_reviewers(),
        edit: Some(PullRequestEditCapabilities {
            change_request: true,
            comment: true,
        }),
        stacks: None,
        stack_actions: None,
        labels: None,
    }
}

/// Capabilities a test spells out itself (no edit, as the TS literals that list their own).
pub fn capabilities(actions: &[PullRequestAction], merge_methods: &[PullRequestMergeMethod]) -> PullRequestCapabilities {
    PullRequestCapabilities {
        actions: actions.to_vec(),
        merge_methods: merge_methods.to_vec(),
        edit: None,
        ..full_capabilities()
    }
}

// -------------------------------------------------------------------------------------------
// Fake provider
// -------------------------------------------------------------------------------------------

/// One provider call, as the test supplies it.
pub type Handler<I, O> = Arc<dyn Fn(I) -> BoxFuture<'static, ProviderResult<O>> + Send + Sync>;

/// A handler from an async closure.
pub fn h<I, O, F, Fut>(f: F) -> Handler<I, O>
where
    F: Fn(I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ProviderResult<O>> + Send + 'static,
{
    Arc::new(move |input| f(input).boxed())
}

/// A handler answering `value` every time.
pub fn ok<I: 'static, O: Clone + Send + Sync + 'static>(value: O) -> Handler<I, O> {
    Arc::new(move |_| {
        let value = value.clone();
        async move { Ok(value) }.boxed()
    })
}

/// A handler failing with `error` every time.
pub fn fail<I: 'static, O: 'static>(error: PullRequestProviderError) -> Handler<I, O> {
    Arc::new(move |_| {
        let error = error.clone();
        async move { Err(error) }.boxed()
    })
}

/// `Effect.die(message)`: a call the test says must not happen.
pub fn die<I: 'static, O: 'static>(message: &'static str) -> Handler<I, O> {
    Arc::new(move |_| async move { panic!("{message}") }.boxed())
}

/// A credential scope that runs the operation as it is.
pub struct PassthroughScope;

impl CredentialScope for PassthroughScope {
    fn run<'a>(&'a self, future: BoxFuture<'a, ()>) -> BoxFuture<'a, ()> {
        future
    }
}

/// `fakeProvider(kind, overrides)`: a provider whose every call is supplied by the test; anything
/// unset succeeds emptily, except the reads a test has to supply (they panic).
#[derive(Clone)]
pub struct FakeProvider {
    pub kind: Kind,
    /// The capability sets the provider can claim, and which one it claims right now.
    pub capability_sets: Arc<Vec<PullRequestCapabilities>>,
    pub capability_set: Arc<AtomicUsize>,
    pub get_viewer: Handler<ProviderHostRef, String>,
    pub get_routing_identity: Option<Handler<(String, String), RoutingIdentity>>,
    pub verified_credential: Option<Handler<(String, String), VerifiedCredential>>,
    pub list_change_requests: Handler<ListChangeRequestsInput, ProviderChangeRequestPage>,
    pub list_change_requests_across: Option<Handler<ListChangeRequestsAcrossInput, ProviderBatchedChangeRequestPage>>,
    pub list_change_request_stats: Option<Handler<ListChangeRequestStatsInput, Vec<ProviderChangeRequestStat>>>,
    pub get_change_request: Handler<ChangeRequestRef, ProviderChangeRequestDetail>,
    pub get_change_request_preview: Option<Handler<ChangeRequestRef, ProviderChangeRequestPreview>>,
    pub get_change_request_summary: Option<Handler<ChangeRequestRef, ProviderChangeRequestSummary>>,
    pub get_change_request_stack: Option<Handler<GetChangeRequestStackInput, Option<ProviderChangeRequestStack>>>,
    pub get_change_request_activity: Handler<ChangeRequestRef, ProviderChangeRequestActivity>,
    pub get_review_thread_comments: Option<Handler<ReviewThreadCommentsInput, PullRequestThreadCommentsResult>>,
    pub get_viewer_permissions: Handler<ViewerPermissionsInput, PullRequestViewerPermissions>,
    pub get_diff: Handler<GetDiffInput, ProviderDiffSlice>,
    pub get_diff_file_contents: Option<Handler<DiffFileContentsInput, ProviderDiffFileContents>>,
    pub get_files_viewed: Option<Handler<ChangeRequestRef, ProviderFilesViewed>>,
    pub set_files_viewed: Option<Handler<SetFilesViewedInput, ()>>,
    pub get_file_revisions: Option<Handler<FileRevisionsInput, ProviderFileRevisions>>,
    pub run_action: Handler<RunActionInput, ()>,
    pub update_change_request: Option<Handler<UpdateChangeRequestInput, ()>>,
    pub comment: Handler<CommentInput, ()>,
    pub update_comment: Option<Handler<UpdateCommentInput, ()>>,
    pub submit_review: Handler<SubmitReviewInput, ()>,
    pub list_reviewer_candidates: Handler<ChangeRequestRef, PullRequestReviewerCandidateList>,
    pub set_reviewer_request: Handler<SetReviewerRequestInput, ()>,
    pub list_label_candidates: Option<Handler<ChangeRequestRef, PullRequestLabelCandidateList>>,
    pub set_labels: Option<Handler<SetLabelsInput, ()>>,
    pub reply_to_thread: Handler<ReplyToThreadInput, ()>,
    pub set_reaction: Handler<SetReactionInput, ()>,
    pub set_thread_resolution: Handler<SetThreadResolutionInput, ()>,
}

impl FakeProvider {
    pub fn new(kind: Kind) -> Self {
        Self {
            kind,
            capability_sets: Arc::new(vec![full_capabilities()]),
            capability_set: Arc::new(AtomicUsize::new(0)),
            get_viewer: ok("bilal".to_owned()),
            get_routing_identity: None,
            verified_credential: None,
            list_change_requests: ok(page(Vec::new(), false, true)),
            list_change_requests_across: None,
            list_change_request_stats: None,
            get_change_request: die("unused"),
            get_change_request_preview: None,
            get_change_request_summary: None,
            get_change_request_stack: None,
            get_change_request_activity: die("unused"),
            get_review_thread_comments: None,
            // A viewer who may do everything the host can, so a test only narrows what it is about.
            get_viewer_permissions: ok(all_permissions()),
            get_diff: die("unused"),
            get_diff_file_contents: None,
            get_files_viewed: None,
            set_files_viewed: None,
            get_file_revisions: None,
            run_action: ok(()),
            update_change_request: Some(ok(())),
            comment: ok(()),
            update_comment: Some(ok(())),
            submit_review: ok(()),
            list_reviewer_candidates: ok(PullRequestReviewerCandidateList {
                candidates: Vec::new(),
                truncated: false,
            }),
            set_reviewer_request: ok(()),
            list_label_candidates: None,
            set_labels: None,
            reply_to_thread: ok(()),
            set_reaction: ok(()),
            set_thread_resolution: ok(()),
        }
    }

    /// The provider claiming `capabilities` instead.
    pub fn with_capabilities(mut self, capabilities: PullRequestCapabilities) -> Self {
        self.capability_sets = Arc::new(vec![capabilities]);
        self
    }

    pub fn shared(self) -> SharedProvider {
        Arc::new(self)
    }
}

#[async_trait]
impl PullRequestProviderApi for FakeProvider {
    fn kind(&self) -> Kind {
        self.kind
    }

    fn capabilities(&self) -> &PullRequestCapabilities {
        &self.capability_sets[self.capability_set.load(Ordering::SeqCst)]
    }

    fn optional_methods(&self) -> OptionalMethods {
        OptionalMethods {
            with_verified_credential: self.verified_credential.is_some(),
            get_routing_identity: self.get_routing_identity.is_some(),
            list_change_requests_across: self.list_change_requests_across.is_some(),
            list_change_request_stats: self.list_change_request_stats.is_some(),
            get_change_request_preview: self.get_change_request_preview.is_some(),
            get_change_request_summary: self.get_change_request_summary.is_some(),
            get_change_request_stack: self.get_change_request_stack.is_some(),
            get_review_thread_comments: self.get_review_thread_comments.is_some(),
            get_diff_file_contents: self.get_diff_file_contents.is_some(),
            get_files_viewed: self.get_files_viewed.is_some(),
            set_files_viewed: self.set_files_viewed.is_some(),
            get_file_revisions: self.get_file_revisions.is_some(),
            update_change_request: self.update_change_request.is_some(),
            update_comment: self.update_comment.is_some(),
            list_label_candidates: self.list_label_candidates.is_some(),
            set_labels: self.set_labels.is_some(),
        }
    }

    async fn verified_credential(&self, cwd: &str, host: &str) -> ProviderResult<VerifiedCredential> {
        (self.verified_credential.as_ref().expect("declared"))((cwd.to_owned(), host.to_owned())).await
    }

    async fn get_routing_identity(&self, cwd: &str, host: &str) -> ProviderResult<RoutingIdentity> {
        (self.get_routing_identity.as_ref().expect("declared"))((cwd.to_owned(), host.to_owned())).await
    }

    async fn get_viewer(&self, input: ProviderHostRef) -> ProviderResult<String> {
        (self.get_viewer)(input).await
    }

    async fn list_change_requests(&self, input: ListChangeRequestsInput) -> ProviderResult<ProviderChangeRequestPage> {
        (self.list_change_requests)(input).await
    }

    async fn list_change_requests_across(&self, input: ListChangeRequestsAcrossInput) -> ProviderResult<ProviderBatchedChangeRequestPage> {
        (self.list_change_requests_across.as_ref().expect("declared"))(input).await
    }

    async fn list_change_request_stats(&self, input: ListChangeRequestStatsInput) -> ProviderResult<Vec<ProviderChangeRequestStat>> {
        (self.list_change_request_stats.as_ref().expect("declared"))(input).await
    }

    async fn get_change_request(&self, input: ChangeRequestRef) -> ProviderResult<ProviderChangeRequestDetail> {
        (self.get_change_request)(input).await
    }

    async fn get_change_request_preview(&self, input: ChangeRequestRef) -> ProviderResult<ProviderChangeRequestPreview> {
        (self.get_change_request_preview.as_ref().expect("declared"))(input).await
    }

    async fn get_change_request_summary(&self, input: ChangeRequestRef) -> ProviderResult<ProviderChangeRequestSummary> {
        (self.get_change_request_summary.as_ref().expect("declared"))(input).await
    }

    async fn get_change_request_stack(&self, input: GetChangeRequestStackInput) -> ProviderResult<Option<ProviderChangeRequestStack>> {
        (self.get_change_request_stack.as_ref().expect("declared"))(input).await
    }

    async fn get_change_request_activity(&self, input: ChangeRequestRef) -> ProviderResult<ProviderChangeRequestActivity> {
        (self.get_change_request_activity)(input).await
    }

    async fn get_review_thread_comments(&self, input: ReviewThreadCommentsInput) -> ProviderResult<PullRequestThreadCommentsResult> {
        (self.get_review_thread_comments.as_ref().expect("declared"))(input).await
    }

    async fn get_viewer_permissions(&self, input: ViewerPermissionsInput) -> ProviderResult<PullRequestViewerPermissions> {
        (self.get_viewer_permissions)(input).await
    }

    async fn get_diff(&self, input: GetDiffInput) -> ProviderResult<ProviderDiffSlice> {
        (self.get_diff)(input).await
    }

    async fn get_diff_file_contents(&self, input: DiffFileContentsInput) -> ProviderResult<ProviderDiffFileContents> {
        (self.get_diff_file_contents.as_ref().expect("declared"))(input).await
    }

    async fn get_files_viewed(&self, input: ChangeRequestRef) -> ProviderResult<ProviderFilesViewed> {
        (self.get_files_viewed.as_ref().expect("declared"))(input).await
    }

    async fn set_files_viewed(&self, input: SetFilesViewedInput) -> ProviderResult<()> {
        (self.set_files_viewed.as_ref().expect("declared"))(input).await
    }

    async fn get_file_revisions(&self, input: FileRevisionsInput) -> ProviderResult<ProviderFileRevisions> {
        (self.get_file_revisions.as_ref().expect("declared"))(input).await
    }

    async fn run_action(&self, input: RunActionInput) -> ProviderResult<()> {
        (self.run_action)(input).await
    }

    async fn update_change_request(&self, input: UpdateChangeRequestInput) -> ProviderResult<()> {
        (self.update_change_request.as_ref().expect("declared"))(input).await
    }

    async fn comment(&self, input: CommentInput) -> ProviderResult<()> {
        (self.comment)(input).await
    }

    async fn update_comment(&self, input: UpdateCommentInput) -> ProviderResult<()> {
        (self.update_comment.as_ref().expect("declared"))(input).await
    }

    async fn submit_review(&self, input: SubmitReviewInput) -> ProviderResult<()> {
        (self.submit_review)(input).await
    }

    async fn list_reviewer_candidates(&self, input: ChangeRequestRef) -> ProviderResult<PullRequestReviewerCandidateList> {
        (self.list_reviewer_candidates)(input).await
    }

    async fn set_reviewer_request(&self, input: SetReviewerRequestInput) -> ProviderResult<()> {
        (self.set_reviewer_request)(input).await
    }

    async fn list_label_candidates(&self, input: ChangeRequestRef) -> ProviderResult<PullRequestLabelCandidateList> {
        (self.list_label_candidates.as_ref().expect("declared"))(input).await
    }

    async fn set_labels(&self, input: SetLabelsInput) -> ProviderResult<()> {
        (self.set_labels.as_ref().expect("declared"))(input).await
    }

    async fn reply_to_thread(&self, input: ReplyToThreadInput) -> ProviderResult<()> {
        (self.reply_to_thread)(input).await
    }

    async fn set_reaction(&self, input: SetReactionInput) -> ProviderResult<()> {
        (self.set_reaction)(input).await
    }

    async fn set_thread_resolution(&self, input: SetThreadResolutionInput) -> ProviderResult<()> {
        (self.set_thread_resolution)(input).await
    }
}

// -------------------------------------------------------------------------------------------
// Fake projections and provider refinement
// -------------------------------------------------------------------------------------------

/// The projection reader `makeService` mocks: the project shells and nothing else.
#[derive(Clone, Default)]
pub struct FakeProjections {
    pub projects: Arc<Mutex<Vec<OrchestrationProjectShell>>>,
}

fn unused<T>() -> Result<T, TaggedError> {
    unimplemented!("not read by the pull request service")
}

#[async_trait]
impl ProjectionReads for FakeProjections {
    async fn get_user_input_activity(&self, _: &ThreadId, _: &ApprovalRequestId) -> Result<Option<OrchestrationThreadActivity>, TaggedError> {
        unused()
    }
    async fn list_activities_by_kind(&self, _: &str) -> Result<Vec<OrchestrationThreadActivity>, TaggedError> {
        unused()
    }
    async fn get_command_read_model(&self) -> Result<OrchestrationReadModel, TaggedError> {
        unused()
    }
    async fn get_snapshot(&self) -> Result<OrchestrationReadModel, TaggedError> {
        unused()
    }
    async fn get_shell_snapshot(&self, _: bool) -> Result<OrchestrationShellSnapshot, TaggedError> {
        unused()
    }
    async fn get_archived_shell_snapshot(&self) -> Result<OrchestrationShellSnapshot, TaggedError> {
        unused()
    }
    async fn list_threads_with_pull_requests(&self) -> Result<Vec<ThreadPullRequests>, TaggedError> {
        unused()
    }
    async fn get_deleted_worktree_threads(&self) -> Result<Vec<DeletedWorktreeThread>, TaggedError> {
        unused()
    }
    async fn search_threads(&self, _: OrchestrationSearchThreadsInput) -> Result<OrchestrationSearchThreadsResult, TaggedError> {
        unused()
    }
    async fn get_snapshot_sequence(&self) -> Result<i64, TaggedError> {
        unused()
    }
    async fn get_counts(&self) -> Result<SnapshotCounts, TaggedError> {
        unused()
    }
    async fn get_event_replay_stats(&self, _: i64, _: i64) -> Result<ReplayStats, TaggedError> {
        unused()
    }
    async fn get_active_project_by_workspace_root(&self, _: &str) -> Result<Option<OrchestrationProject>, TaggedError> {
        unused()
    }
    async fn get_project_shell_by_id(&self, project_id: &ProjectId) -> Result<Option<OrchestrationProjectShell>, TaggedError> {
        Ok(self.projects.lock().unwrap().iter().find(|project| project.id == *project_id).cloned())
    }
    async fn get_project_shells(&self, project_ids: Option<Vec<ProjectId>>) -> Result<Vec<OrchestrationProjectShell>, TaggedError> {
        Ok(self
            .projects
            .lock()
            .unwrap()
            .iter()
            .filter(|project| project_ids.as_ref().is_none_or(|ids| ids.contains(&project.id)))
            .cloned()
            .collect())
    }
    async fn get_first_active_thread_id_by_project_id(&self, _: &ProjectId) -> Result<Option<ThreadId>, TaggedError> {
        unused()
    }
    async fn get_imported_agent_session_sources(&self, _: &ProjectId) -> Result<Vec<ImportedAgentSessionSource>, TaggedError> {
        unused()
    }
    async fn get_thread_checkpoint_context(&self, _: &ThreadId) -> Result<Option<ThreadCheckpointContext>, TaggedError> {
        unused()
    }
    async fn get_full_thread_diff_context(&self, _: &ThreadId, _: i64) -> Result<Option<FullThreadDiffContext>, TaggedError> {
        unused()
    }
    async fn get_thread_shell_by_id(&self, _: &ThreadId) -> Result<Option<OrchestrationThreadShell>, TaggedError> {
        unused()
    }
    async fn get_thread_runtime_context(&self, _: &ThreadId) -> Result<Option<ThreadRuntimeContext>, TaggedError> {
        unused()
    }
    async fn get_turn_start_message(&self, _: &ThreadId, _: &MessageId) -> Result<Option<TurnStartMessage>, TaggedError> {
        unused()
    }
    async fn get_thread_detail_by_id(&self, _: &ThreadId, _: ThreadDetailQuery) -> Result<Option<OrchestrationThread>, TaggedError> {
        unused()
    }
    async fn get_thread_detail_snapshot(
        &self,
        _: &ThreadId,
        _: Option<OrchestrationThreadDetailWindow>,
    ) -> Result<Option<OrchestrationThreadDetailSnapshot>, TaggedError> {
        unused()
    }
}

/// `resolveHandle` as a test supplies it: the provider the remote refines to, if any. Called
/// with the checkout and the context the service hands the source control registry.
pub type Refine = Arc<dyn Fn(&str, &SourceControlProviderContext) -> Option<SourceControlProviderInfo> + Send + Sync>;

/// The managed discovery spec the source control registry refines unknown remotes through.
struct TestRefiner(Option<Refine>);

#[async_trait]
impl ManagedCliDiscovery for TestRefiner {
    fn kind(&self) -> Kind {
        Kind::Unknown
    }
    fn label(&self) -> &str {
        "test refiner"
    }
    fn install_hint(&self) -> &str {
        ""
    }
    async fn probe(&self, _: &str) -> SourceControlProviderDiscoveryItem {
        unimplemented!("not probed")
    }
    async fn refine_unknown_remote(&self, cwd: &str, context: &SourceControlProviderContext) -> Option<SourceControlProviderInfo> {
        match &self.0 {
            Some(refine) => refine(cwd, context),
            None => panic!("Unexpected provider refinement"),
        }
    }
}

/// A refinement answering `kind` (with the context's own base URL).
pub fn refine_to(kind: Kind) -> Refine {
    Arc::new(move |_, context| {
        Some(SourceControlProviderInfo {
            kind,
            ..context.provider.clone()
        })
    })
}

pub fn info(kind: Kind, name: &str, base_url: &str) -> SourceControlProviderInfo {
    SourceControlProviderInfo {
        kind,
        name: name.into(),
        base_url: base_url.into(),
    }
}

// -------------------------------------------------------------------------------------------
// The service
// -------------------------------------------------------------------------------------------

/// A service and the handles a test drives it with.
pub struct Harness {
    pub service: PullRequestService,
    pub clock: Arc<ManualClock>,
    pub projections: FakeProjections,
}

impl std::ops::Deref for Harness {
    type Target = PullRequestService;
    fn deref(&self) -> &PullRequestService {
        &self.service
    }
}

impl Harness {
    /// `TestClock.adjust`.
    pub async fn adjust(&self, millis: i64) {
        self.clock.advance(millis);
        settle().await;
    }

    /// `TestClock.setTime`.
    pub async fn set_time(&self, millis: i64) {
        self.clock.set(millis);
        settle().await;
    }

    pub fn push_project(&self, project: impl Into<OrchestrationProjectShell>) {
        self.projections.projects.lock().unwrap().push(project.into());
    }

    /// The current refresh revision (`Stream.runHead(subscribeRefreshes)`).
    pub async fn refresh_revision(&self) -> u64 {
        use futures::StreamExt;
        self.service.subscribe_refreshes().next().await.expect("a refresh revision")
    }
}

/// `makeService({projects, providers, resolveHandle?})`.
pub fn make_service(projects: Vec<OrchestrationProjectShell>, providers: Vec<FakeProvider>) -> Harness {
    make_service_with(projects, providers, None)
}

pub fn make_service_with(projects: Vec<OrchestrationProjectShell>, providers: Vec<FakeProvider>, refine: Option<Refine>) -> Harness {
    make_service_over(projects, providers.into_iter().map(FakeProvider::shared).collect(), refine)
}

/// `makeService` over any providers (a real forge's included).
pub fn make_service_over(projects: Vec<OrchestrationProjectShell>, providers: Vec<SharedProvider>, refine: Option<Refine>) -> Harness {
    let clock = ManualClock::new(0);
    let shared: SharedClock = clock.clone();
    let projections = FakeProjections {
        projects: Arc::new(Mutex::new(projects)),
    };
    let process = VcsProcess::default();
    let vcs = zc_vcs::VcsDriverRegistry::new(zc_vcs::VcsProjectConfig::new(), Arc::new(zc_vcs::GitVcsProcessDriver::new(process.clone())));
    let source_control = SourceControlProviderRegistry::new(
        vec![SourceControlProviderRegistration {
            kind: Kind::Unknown,
            provider: Arc::new(UnsupportedProvider(Kind::Unknown)),
            discovery: DiscoverySpec::ManagedCli(Arc::new(TestRefiner(refine))),
        }],
        process,
        vcs,
        "/",
        shared.clone(),
    );
    let service = PullRequestService::new(PullRequestServiceDeps {
        registry: zc_pullrequest::PullRequestProviderRegistry::from_providers(providers),
        projections: Arc::new(projections.clone()),
        source_control,
        rate_limits: zc_sourcecontrol::rate_limit::SourceControlRateLimit::new(shared.clone()),
        db: zc_db::Db::open_in_memory().expect("an in-memory database"),
        read_cache: PullRequestReadCache::memory_with_clock(shared.clone()),
        clock: shared,
    });
    Harness { service, clock, projections }
}

/// Lets spawned work run (`Effect.yieldNow` for forked fibers).
pub async fn settle() {
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

// -------------------------------------------------------------------------------------------
// References and inputs
// -------------------------------------------------------------------------------------------

pub fn reference(project_id: &str, repository: &str, number: i64) -> PullRequestRef {
    PullRequestRef {
        project_id: ProjectId::new(project_id),
        host: None,
        expected_account_id: None,
        allow_stale: None,
        repository: repository.into(),
        number,
    }
}

pub fn hosted_ref(project_id: &str, host: &str, repository: &str, number: i64) -> PullRequestRef {
    PullRequestRef {
        host: Some(host.into()),
        ..reference(project_id, repository, number)
    }
}

pub fn strict(reference: &PullRequestRef) -> PullRequestRef {
    PullRequestRef {
        allow_stale: Some(false),
        ..reference.clone()
    }
}

pub fn action(reference: &PullRequestRef, action: PullRequestAction) -> PullRequestActionInput {
    PullRequestActionInput {
        stack_number: None,
        expected_stack_heads: None,
        project_id: reference.project_id.clone(),
        host: reference.host.clone(),
        expected_account_id: reference.expected_account_id.clone(),
        allow_stale: reference.allow_stale,
        repository: reference.repository.clone(),
        number: reference.number,
        action,
        merge_method: None,
        update_method: None,
    }
}

pub fn diff_input(reference: &PullRequestRef) -> PullRequestDiffInput {
    PullRequestDiffInput {
        project_id: reference.project_id.clone(),
        host: reference.host.clone(),
        expected_account_id: reference.expected_account_id.clone(),
        allow_stale: reference.allow_stale,
        repository: reference.repository.clone(),
        number: reference.number,
        cursor: None,
        commit: None,
    }
}

pub fn comment_input(reference: &PullRequestRef, body: &str) -> PullRequestCommentInput {
    PullRequestCommentInput {
        project_id: reference.project_id.clone(),
        host: reference.host.clone(),
        expected_account_id: reference.expected_account_id.clone(),
        allow_stale: reference.allow_stale,
        repository: reference.repository.clone(),
        number: reference.number,
        body: body.into(),
    }
}

pub fn set_files(reference: &PullRequestRef, files: &[(&str, bool)]) -> PullRequestSetFilesViewedInput {
    PullRequestSetFilesViewedInput {
        project_id: reference.project_id.clone(),
        host: reference.host.clone(),
        expected_account_id: reference.expected_account_id.clone(),
        allow_stale: reference.allow_stale,
        repository: reference.repository.clone(),
        number: reference.number,
        files: files
            .iter()
            .map(|(path, viewed)| PullRequestSetFilesViewedInputFilesItem {
                path: (*path).into(),
                viewed: *viewed,
            })
            .collect(),
    }
}

pub fn list_input(state: PullRequestListState) -> PullRequestListInput {
    PullRequestListInput {
        state,
        involvement: None,
        filters: None,
        project_id: None,
        project_ids: None,
        host: None,
        limit: None,
        cursors: None,
        query: None,
    }
}

pub fn open_list() -> PullRequestListInput {
    list_input(PullRequestListState::Open)
}

pub fn no_filters() -> PullRequestListFilters {
    PullRequestListFilters {
        draft: None,
        review: None,
        checks: None,
        labels: None,
        excluded_labels: None,
        author: None,
    }
}

pub fn cursors(entries: &[(&str, &str)]) -> Option<std::collections::BTreeMap<String, String>> {
    Some(entries.iter().map(|(key, value)| ((*key).to_owned(), (*value).to_owned())).collect())
}

/// The `_tag` and, for an operation error, its operation.
pub fn tag(error: &PullRequestError) -> &'static str {
    error.tag()
}

pub fn operation_of(error: &PullRequestError) -> Option<&str> {
    match error {
        PullRequestError::Operation { operation, .. } => Some(operation),
        PullRequestError::Unavailable { .. } => None,
    }
}

pub fn reason_of(error: &PullRequestError) -> Option<PullRequestUnavailableReason> {
    match error {
        PullRequestError::Unavailable { reason, .. } => Some(*reason),
        PullRequestError::Operation { .. } => None,
    }
}

pub fn detail_text(error: &PullRequestError) -> String {
    match error {
        PullRequestError::Operation { detail, .. } => detail.clone(),
        PullRequestError::Unavailable { .. } => String::new(),
    }
}

/// A counter shared with a handler.
#[derive(Clone, Default)]
pub struct Counter(pub Arc<AtomicUsize>);

impl Counter {
    pub fn bump(&self) -> usize {
        self.0.fetch_add(1, Ordering::SeqCst) + 1
    }
    pub fn get(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

/// A list shared with a handler.
#[derive(Clone)]
pub struct Log<T>(pub Arc<Mutex<Vec<T>>>);

impl<T> Default for Log<T> {
    fn default() -> Self {
        Self(Arc::default())
    }
}

impl<T: Clone> Log<T> {
    pub fn push(&self, value: T) {
        self.0.lock().unwrap().push(value);
    }
    pub fn all(&self) -> Vec<T> {
        self.0.lock().unwrap().clone()
    }
    pub fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }
    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

/// A shared value a handler reads and a test changes.
#[derive(Clone)]
pub struct Cell<T>(pub Arc<Mutex<T>>);

impl<T: Clone> Cell<T> {
    pub fn new(value: T) -> Self {
        Self(Arc::new(Mutex::new(value)))
    }
    pub fn get(&self) -> T {
        self.0.lock().unwrap().clone()
    }
    pub fn set(&self, value: T) {
        *self.0.lock().unwrap() = value;
    }
}

/// A `Deferred<void>`: a gate a handler waits on and a test opens.
#[derive(Clone)]
pub struct Gate(pub Arc<tokio::sync::Notify>, pub Arc<std::sync::atomic::AtomicBool>);

impl Default for Gate {
    fn default() -> Self {
        Self(Arc::new(tokio::sync::Notify::new()), Arc::new(std::sync::atomic::AtomicBool::new(false)))
    }
}

impl Gate {
    pub fn open(&self) {
        self.1.store(true, Ordering::SeqCst);
        self.0.notify_waiters();
    }
    pub async fn wait(&self) {
        loop {
            let notified = self.0.notified();
            if self.1.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
    pub fn is_open(&self) -> bool {
        self.1.load(Ordering::SeqCst)
    }
}
