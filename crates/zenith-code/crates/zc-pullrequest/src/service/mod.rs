//! `PullRequestService.ts`: the pull request inbox and review surface over every forge.
//!
//! Every read leaves the process (a CLI per repository, against hosts whose limits are low), so
//! answers are shared for a short while and concurrent identical reads share one request. The
//! windows sit near the clients' own stale times. Reads that must not share (the refresh button,
//! a client reloading after its own action) go through [`PullRequestService::invalidate`] rather
//! than a flag on the read.
//!
//! Invalidation works through epochs: a cache key carries its scope's epoch, so bumping the epoch
//! strands every entry made under the old one. The counter is shared and monotonic.
//!
//! | Module | Part of the TS service |
//! |---|---|
//! | [`projects`] | `listWorkspaceProjects`, `refineUnknownProjectKinds`, `requireProject`, `canonicalRef` |
//! | [`listing`] | `list` (cursors, viewers, batched reads), `listStats` |
//! | [`routing`] | `routing`, `routingIdentity`, `withRoutingCredential` |
//! | [`reads`] | `summary`, `stack`, `detail`, `preview`, `activity`, `threadComments`, `diff`, `diffFileContents`, `filesViewed` |
//! | [`writes`] | `runAction` and the other writes, with their capability and permission refusals |
//! | [`epochs`] | `invalidate`, `refreshAfterTurn`, the epochs and the refresh / merge streams |
//! | [`ports`] | `impl zc_ports::PullRequests` |

mod epochs;
mod listing;
mod ports;
mod projects;
mod rate_limited;
mod reads;
pub mod refs;
mod routing;
mod writes;

use std::convert::Infallible;
use std::future::Future;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use zc_contracts::{
    ProjectId, PullRequestActionInput, PullRequestActivity, PullRequestCommentInput, PullRequestCommentUpdateInput, PullRequestDetail,
    PullRequestDiffFileContentsInput, PullRequestDiffFileContentsResult, PullRequestDiffInput, PullRequestDiffResult, PullRequestFilesViewedResult,
    PullRequestInvalidateInput, PullRequestLabelCandidateList, PullRequestLabelChangeInput, PullRequestListInput, PullRequestListResult,
    PullRequestListStatsInput, PullRequestListStatsResult, PullRequestPreview, PullRequestReactionInput, PullRequestRef, PullRequestReviewerCandidateList,
    PullRequestReviewerRequestInput, PullRequestRoutingIdentityInput, PullRequestRoutingIdentityResult, PullRequestRoutingResult,
    PullRequestSetFilesViewedInput, PullRequestStack, PullRequestSubmitReviewInput, PullRequestSummary, PullRequestThreadCommentsInput,
    PullRequestThreadCommentsResult, PullRequestThreadReplyInput, PullRequestThreadResolutionInput, PullRequestUpdateInput,
};
use zc_core::pubsub::SnapshotHub;
use zc_sourcecontrol::rate_limit::SourceControlRateLimit;
use zc_sourcecontrol::util::SharedClock;
use zc_sourcecontrol::SourceControlProviderRegistry;

use crate::error::PullRequestError;
use crate::read_cache::PullRequestReadCache;
use crate::registry::PullRequestProviderRegistry;
use crate::ttl_cache::TtlCache;
use crate::viewed_files::ViewedFiles;

pub use projects::SupportedProject;
pub use refs::RefInput;
pub use routing::RoutingCredentialInfo;

pub(crate) use epochs::Epochs;
pub(crate) use listing::{ListKey, ResolvedViewer, StatsBatchKey, ViewerKey};
pub(crate) use reads::change_request_of as reads_change_request;
pub(crate) use reads::{DiffKey, LastGood, StaleDiff};
pub(crate) use refs::RefKey;

/// Rows per repository when the client does not ask for a page size, and rows per slice when a
/// listing is carried on from a cursor. 99 and not 100: every provider asks its host for one row
/// over this to probe for a next page, and 100 is what a page of GitHub's API (and GitLab's
/// `per_page`) serves.
pub const DEFAULT_REPOSITORY_LIST_LIMIT: i64 = 99;
/// Repositories read at once (each is a CLI process mostly waiting on its host).
pub const REPOSITORY_CONCURRENCY: usize = 12;
/// Repositories named in one read across a host.
pub const REPOSITORY_SEARCH_CHUNK: usize = 100;

pub(crate) const LIST_CACHE_TTL_MS: i64 = 30_000;
pub(crate) const DETAIL_CACHE_TTL_MS: i64 = 15_000;
pub(crate) const DIFF_CACHE_TTL_MS: i64 = 60_000;
/// A commit is content-addressed, so its own diff cannot change under its key.
pub(crate) const COMMIT_DIFF_CACHE_TTL_MS: i64 = 10 * 60_000;
/// Sized like the client's own stale time; a row's counts move only when somebody pushes.
pub(crate) const LIST_STATS_CACHE_TTL_MS: i64 = 60_000;
/// Short and with no stale window: the reader's own bookkeeping, held only so opening a change
/// request on two devices costs one read.
pub(crate) const FILES_VIEWED_CACHE_TTL_MS: i64 = 15_000;
/// A diff stays interactive while its next value is fetched off the critical path.
pub(crate) const DIFF_STALE_WINDOW_MS: i64 = 10 * 60_000;
/// How long one host's signed-in login is believed without asking its CLI again.
pub(crate) const VIEWER_CACHE_TTL_MS: i64 = 10 * 60_000;
pub(crate) const SEARCH_VISIBILITY_TTL_MS: i64 = 10 * 60_000;
pub(crate) const STALE_DETAIL_WINDOW_MS: i64 = 10 * 60_000;
pub(crate) const LIST_CACHE_CAPACITY: usize = 64;
pub(crate) const LIST_STATS_CACHE_CAPACITY: usize = 32;
pub(crate) const DETAIL_CACHE_CAPACITY: usize = 128;
pub(crate) const DIFF_CACHE_CAPACITY: usize = 128;
/// Each diff cache retains at most 64 MiB of patch text, counting UTF-16 storage.
pub(crate) const MAX_CACHED_DIFF_PATCH_BYTES: usize = 512 * 1024;
pub(crate) const FILES_VIEWED_CACHE_CAPACITY: usize = 128;
pub(crate) const VIEWER_CACHE_CAPACITY: usize = 32;
pub(crate) const REF_EPOCH_CAPACITY: usize = 2_048;
/// The merge feed keeps the newest 64 events for a slow subscriber (`PubSub.sliding(64)`).
pub(crate) const MERGE_EVENTS_CAPACITY: usize = 64;

/// What the service is built from (`PullRequestService.layer`'s requirements).
pub struct PullRequestServiceDeps {
    pub registry: PullRequestProviderRegistry,
    pub projections: Arc<dyn zc_ports::ProjectionReads>,
    pub source_control: SourceControlProviderRegistry,
    pub rate_limits: SourceControlRateLimit,
    pub db: zc_db::Db,
    pub read_cache: PullRequestReadCache,
    pub clock: SharedClock,
}

pub(crate) struct Inner {
    pub registry: PullRequestProviderRegistry,
    pub projections: Arc<dyn zc_ports::ProjectionReads>,
    pub source_control: SourceControlProviderRegistry,
    pub rate_limits: SourceControlRateLimit,
    pub db: zc_db::Db,
    pub read_cache: PullRequestReadCache,
    pub clock: SharedClock,
    pub merges: tokio::sync::broadcast::Sender<zc_ports::pull_requests::PullRequestMergeEvent>,
    pub refreshes: SnapshotHub<u64>,
    pub epochs: Mutex<Epochs>,
    pub viewers_by_host: Mutex<std::collections::HashMap<String, (i64, ResolvedViewer)>>,
    pub viewer_flights: TtlCache<ViewerKey, ResolvedViewer, Infallible>,
    pub search_visible_at: Mutex<crate::util::OrderedMap<String, i64>>,
    pub list_cache: TtlCache<ListKey, PullRequestListResult, PullRequestError>,
    pub list_stats_cache: TtlCache<StatsBatchKey, (PullRequestListStatsResult, i64), PullRequestError>,
    pub detail_cache: TtlCache<RefKey, PullRequestDetail, PullRequestError>,
    pub activity_cache: TtlCache<RefKey, PullRequestActivity, PullRequestError>,
    pub preview_cache: TtlCache<RefKey, PullRequestPreview, PullRequestError>,
    pub diff_cache: TtlCache<DiffKey, PullRequestDiffResult, PullRequestError>,
    pub files_viewed_cache: TtlCache<(RefKey, u64), PullRequestFilesViewedResult, PullRequestError>,
    pub last_good_summary: LastGood<PullRequestSummary>,
    pub last_good_detail: LastGood<PullRequestDetail>,
    pub stale_diff: StaleDiff,
    pub viewed_files: ViewedFiles,
}

/// `PullRequestService`.
#[derive(Clone)]
pub struct PullRequestService {
    pub(crate) inner: Arc<Inner>,
}

impl std::fmt::Debug for PullRequestService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PullRequestService")
            .field("registry", &self.inner.registry)
            .finish_non_exhaustive()
    }
}

/// A time to live for successes only (failures are never kept).
fn ok_ttl<K, V>(ttl: i64) -> impl Fn(&K, &Result<V, PullRequestError>) -> i64 + Send + Sync + 'static {
    move |_, result| if result.is_ok() { ttl } else { 0 }
}

/// `canCacheDiff`: a slice whose patch is small enough to keep.
pub(crate) fn can_cache_diff(value: &PullRequestDiffResult) -> bool {
    zc_sourcecontrol::util::js_length(&value.patch) * 2 <= MAX_CACHED_DIFF_PATCH_BYTES
}

impl PullRequestService {
    /// `make`.
    pub fn new(deps: PullRequestServiceDeps) -> Self {
        let clock = deps.clock.clone();
        let (merges, _) = tokio::sync::broadcast::channel(MERGE_EVENTS_CAPACITY);
        Self {
            inner: Arc::new(Inner {
                registry: deps.registry,
                projections: deps.projections,
                source_control: deps.source_control,
                rate_limits: deps.rate_limits,
                db: deps.db,
                read_cache: deps.read_cache,
                merges,
                refreshes: SnapshotHub::new(0),
                epochs: Mutex::new(Epochs::default()),
                viewers_by_host: Mutex::default(),
                // The host-wide success map holds the real ten-minute answer. This short entry
                // keeps simultaneous cold page reads on one in-flight lookup; failures stay
                // retryable.
                viewer_flights: TtlCache::new(
                    VIEWER_CACHE_CAPACITY,
                    clock.clone(),
                    |_, result: &Result<ResolvedViewer, Infallible>| match result {
                        Ok(resolved) if resolved.error.is_none() => 1_000,
                        _ => 0,
                    },
                ),
                search_visible_at: Mutex::default(),
                list_cache: TtlCache::new(LIST_CACHE_CAPACITY, clock.clone(), ok_ttl(LIST_CACHE_TTL_MS)),
                list_stats_cache: TtlCache::new(LIST_STATS_CACHE_CAPACITY, clock.clone(), ok_ttl(LIST_STATS_CACHE_TTL_MS)),
                detail_cache: TtlCache::new(DETAIL_CACHE_CAPACITY, clock.clone(), ok_ttl(DETAIL_CACHE_TTL_MS)),
                activity_cache: TtlCache::new(DETAIL_CACHE_CAPACITY, clock.clone(), ok_ttl(DETAIL_CACHE_TTL_MS)),
                preview_cache: TtlCache::new(DETAIL_CACHE_CAPACITY, clock.clone(), ok_ttl(DETAIL_CACHE_TTL_MS)),
                // A slice too large to keep is never retained (the TS cache drops it right after
                // answering).
                diff_cache: TtlCache::new(
                    DIFF_CACHE_CAPACITY,
                    clock.clone(),
                    |key: &DiffKey, result: &Result<PullRequestDiffResult, PullRequestError>| match result {
                        Ok(value) if !can_cache_diff(value) => 0,
                        Ok(_) if key.commit.is_none() => DIFF_CACHE_TTL_MS,
                        Ok(_) => COMMIT_DIFF_CACHE_TTL_MS,
                        Err(_) => 0,
                    },
                ),
                files_viewed_cache: TtlCache::new(FILES_VIEWED_CACHE_CAPACITY, clock.clone(), ok_ttl(FILES_VIEWED_CACHE_TTL_MS)),
                last_good_summary: LastGood::new(DETAIL_CACHE_CAPACITY),
                last_good_detail: LastGood::new(DETAIL_CACHE_CAPACITY),
                stale_diff: StaleDiff::default(),
                viewed_files: ViewedFiles::default(),
                clock: deps.clock,
            }),
        }
    }

    pub(crate) fn now(&self) -> i64 {
        self.inner.clock.now_millis()
    }

    /// `list(input)`.
    pub async fn list(&self, input: PullRequestListInput) -> Result<PullRequestListResult, PullRequestError> {
        self.list_cached(input).await
    }

    /// `listStats(input)`: references that name nothing this workspace has are dropped.
    pub async fn list_stats(&self, input: PullRequestListStatsInput) -> Result<PullRequestListStatsResult, PullRequestError> {
        let mut refs = Vec::with_capacity(input.refs.len());
        for reference in &input.refs {
            if let Ok(canonical) = self.canonical_ref(reference).await {
                refs.push(canonical);
            }
        }
        self.list_stats_cached(refs).await
    }

    /// `routing(input)`.
    pub async fn routing(&self, input: PullRequestRef) -> Result<PullRequestRoutingResult, PullRequestError> {
        self.routing_impl(&input).await
    }

    /// `routingIdentity(input)`.
    pub async fn routing_identity(&self, input: PullRequestRoutingIdentityInput) -> Result<PullRequestRoutingIdentityResult, PullRequestError> {
        self.routing_identity_impl(&input).await
    }

    /// `withRoutingCredential(input, operation)` (ws.ts `withPullRequestViewer`): runs `operation`
    /// directly when `input.expected_account_id` is None, else verifies the account and runs it
    /// with the pinned credential and the routing-credential scope (a tokio task-local).
    pub async fn with_routing_credential<T: Send, F: Future<Output = Result<T, PullRequestError>> + Send>(
        &self,
        input: &PullRequestRef,
        operation: F,
    ) -> Result<T, PullRequestError> {
        self.with_routing_credential_impl(input, operation).await
    }

    /// `summary(input, {recoverTransientFailure})`.
    pub async fn summary(&self, input: PullRequestRef, recover_transient_failure: bool) -> Result<PullRequestSummary, PullRequestError> {
        let reference = self.credential_ref(&input).await?;
        self.summary_cached(reference, recover_transient_failure).await
    }

    /// `stack(input, {includeDetails})`: the host-native stack the pull request belongs to, or
    /// `None` when it is in none or the host keeps no such object.
    pub async fn stack(&self, input: PullRequestRef, include_details: bool) -> Result<Option<PullRequestStack>, PullRequestError> {
        let reference = self.credential_ref(&input).await?;
        self.stack_cached(reference, include_details).await
    }

    /// `subscribeMerges`: merges this server confirmed from now on.
    pub fn subscribe_merges(&self) -> zc_ports::EventStream<zc_ports::pull_requests::PullRequestMergeEvent> {
        let receiver = self.inner.merges.subscribe();
        tokio_stream::wrappers::BroadcastStream::new(receiver)
            .filter_map(|event| async move { event.ok() })
            .boxed()
    }

    /// `subscribeRefreshes` (SubscriptionRef changes: the current value first, then each bump;
    /// the initial 0 is skipped).
    pub fn subscribe_refreshes(&self) -> zc_ports::EventStream<u64> {
        let (current, changes) = self.inner.refreshes.subscribe();
        futures::stream::once(async move { current })
            .chain(changes)
            .filter(|revision| futures::future::ready(*revision > 0))
            .boxed()
    }

    /// `refreshAfterTurn(projectId)`.
    pub async fn refresh_after_turn(&self, project_id: &ProjectId) {
        self.refresh_after_turn_impl(project_id).await
    }

    /// `detail(input)`.
    pub async fn detail(&self, input: PullRequestRef) -> Result<PullRequestDetail, PullRequestError> {
        let reference = self.credential_ref(&input).await?;
        self.detail_cached(reference).await
    }

    /// `preview(input)`.
    pub async fn preview(&self, input: PullRequestRef) -> Result<PullRequestPreview, PullRequestError> {
        let reference = self.credential_ref(&input).await?;
        self.preview_cached(reference).await
    }

    /// `activity(input)`.
    pub async fn activity(&self, input: PullRequestRef) -> Result<PullRequestActivity, PullRequestError> {
        let reference = self.credential_ref(&input).await?;
        self.activity_cached(reference).await
    }

    /// `threadComments(input)`.
    pub async fn thread_comments(&self, input: PullRequestThreadCommentsInput) -> Result<PullRequestThreadCommentsResult, PullRequestError> {
        self.thread_comments_impl(&input).await
    }

    /// `diff(input)`.
    pub async fn diff(&self, input: PullRequestDiffInput) -> Result<PullRequestDiffResult, PullRequestError> {
        let reference = self.credential_ref(&input.reference()).await?;
        self.diff_cached(reference, input.cursor, input.commit).await
    }

    /// `diffFileContents(input)`.
    pub async fn diff_file_contents(&self, input: PullRequestDiffFileContentsInput) -> Result<PullRequestDiffFileContentsResult, PullRequestError> {
        self.diff_file_contents_impl(&input).await
    }

    /// `filesViewed(input)`.
    pub async fn files_viewed(&self, input: PullRequestRef) -> Result<PullRequestFilesViewedResult, PullRequestError> {
        let reference = self.credential_ref(&input).await?;
        self.files_viewed_cached(reference).await
    }

    /// `setFilesViewed(input)`.
    pub async fn set_files_viewed(&self, input: PullRequestSetFilesViewedInput) -> Result<(), PullRequestError> {
        self.set_files_viewed_impl(input).await
    }

    /// `runAction(input)`.
    pub async fn run_action(&self, input: PullRequestActionInput) -> Result<(), PullRequestError> {
        self.run_action_and_invalidate(input).await
    }

    /// `update(input)`.
    pub async fn update(&self, input: PullRequestUpdateInput) -> Result<(), PullRequestError> {
        let reference = input.reference();
        self.invalidated_by_mutation(&reference, self.update_impl(&input)).await
    }

    /// `comment(input)`.
    pub async fn comment(&self, input: PullRequestCommentInput) -> Result<(), PullRequestError> {
        let reference = input.reference();
        self.invalidated_by_mutation(&reference, self.comment_impl(&input)).await
    }

    /// `updateComment(input)`.
    pub async fn update_comment(&self, input: PullRequestCommentUpdateInput) -> Result<(), PullRequestError> {
        let reference = input.reference();
        self.invalidated_by_mutation(&reference, self.update_comment_impl(&input)).await
    }

    /// `submitReview(input)`.
    pub async fn submit_review(&self, input: PullRequestSubmitReviewInput) -> Result<(), PullRequestError> {
        let reference = input.reference();
        self.invalidated_by_mutation(&reference, self.submit_review_impl(&input)).await
    }

    /// `replyToThread(input)`.
    pub async fn reply_to_thread(&self, input: PullRequestThreadReplyInput) -> Result<(), PullRequestError> {
        let reference = input.reference();
        self.invalidated_by_mutation(&reference, self.reply_to_thread_impl(&input)).await
    }

    /// `setThreadResolution(input)`.
    pub async fn set_thread_resolution(&self, input: PullRequestThreadResolutionInput) -> Result<(), PullRequestError> {
        let reference = input.reference();
        self.invalidated_by_mutation(&reference, self.set_thread_resolution_impl(&input)).await
    }

    /// `setReaction(input)`.
    pub async fn set_reaction(&self, input: PullRequestReactionInput) -> Result<(), PullRequestError> {
        let reference = input.reference();
        self.invalidated_by_mutation(&reference, self.set_reaction_impl(&input)).await
    }

    /// `reviewerCandidates(input)`: read fresh per menu-open, so never cached.
    pub async fn reviewer_candidates(&self, input: PullRequestRef) -> Result<PullRequestReviewerCandidateList, PullRequestError> {
        self.reviewer_candidates_impl(&input).await
    }

    /// `requestReviewers(input)`.
    pub async fn request_reviewers(&self, input: PullRequestReviewerRequestInput) -> Result<(), PullRequestError> {
        let reference = input.reference();
        self.invalidated_by_mutation(&reference, self.request_reviewers_impl(&input)).await
    }

    /// `labelCandidates(input)`.
    pub async fn label_candidates(&self, input: PullRequestRef) -> Result<PullRequestLabelCandidateList, PullRequestError> {
        self.label_candidates_impl(&input).await
    }

    /// `setLabels(input)`.
    pub async fn set_labels(&self, input: PullRequestLabelChangeInput) -> Result<(), PullRequestError> {
        let reference = input.reference();
        self.invalidated_by_mutation(&reference, self.set_labels_impl(&input)).await
    }

    /// `invalidate(input, {notifyReaders})`. Never fails.
    pub async fn invalidate(&self, input: PullRequestInvalidateInput, notify_readers: bool) {
        self.invalidate_impl(input, notify_readers).await
    }
}
