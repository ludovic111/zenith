//! The per-change-request reads and their caches: `summary` and `stack` (persisted, revisioned),
//! `detail` (served from the last good answer while it refreshes), `preview`, `activity`,
//! `threadComments`, `diff` (stale-while-revalidate), `diffFileContents` and `filesViewed`.

use std::sync::Mutex;

use futures::future::{try_join, BoxFuture, FutureExt};
use serde::de::DeserializeOwned;
use serde::Serialize;
use zc_contracts::{
    JsNumber, PullRequestActivity, PullRequestDetail, PullRequestDiffFileContentsInput, PullRequestDiffFileContentsResult, PullRequestDiffResult,
    PullRequestDiffStat, PullRequestFilesViewedResult, PullRequestPreview, PullRequestRef, PullRequestSetFilesViewedInput, PullRequestStack,
    PullRequestStackLayersItem, PullRequestState, PullRequestSummary, PullRequestThreadCommentsInput, PullRequestThreadCommentsResult,
};
use zc_sourcecontrol::util::{encode_uri_component, js_trim};

use super::projects::SupportedProject;
use super::refs::{ref_scope, CredRef, RefInput, RefKey};
use super::routing::{inherit, spawn_inheriting};
use super::{can_cache_diff, PullRequestService, DIFF_CACHE_CAPACITY, DIFF_STALE_WINDOW_MS, STALE_DETAIL_WINDOW_MS};
use crate::error::{Cause, ProviderFailureReason, PullRequestError, PullRequestProviderError};
use crate::provider::{
    ChangeRequestRef, DiffFileContentsInput, GetChangeRequestStackInput, GetDiffInput, ProviderChangeRequestDetail, ProviderChangeRequestSummary,
    ReviewThreadCommentsInput,
};
use crate::util::{lower, OrderedMap};

/// A held answer per reference, for answering through a failed or slow refresh
/// (`makeLastGoodRead`).
pub(crate) struct LastGood<A> {
    held: Mutex<OrderedMap<RefKey, (i64, A)>>,
    capacity: usize,
}

impl<A: Clone> LastGood<A> {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            held: Mutex::new(OrderedMap::new()),
            capacity,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, OrderedMap<RefKey, (i64, A)>> {
        self.held.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn peek(&self, key: &RefKey) -> Option<A> {
        self.lock().get(key).map(|(_, value)| value.clone())
    }

    fn peek_entry(&self, key: &RefKey) -> Option<(i64, A)> {
        self.lock().get(key).cloned()
    }

    pub(crate) fn record(&self, key: RefKey, value: A, at: i64) {
        let mut held = self.lock();
        held.remove(&key);
        if held.len() >= self.capacity {
            held.pop_first();
        }
        held.insert_last(key, (at, value));
    }
}

/// A diff slice's cache key: the reference, the slice, the commit, and (for the head's own diff)
/// the revision the held summary last saw, so a moved head is a different key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct DiffKey {
    pub reference: RefKey,
    pub cursor: Option<String>,
    pub commit: Option<String>,
    pub revision: Option<String>,
}

/// `staleDiff`: diffs answered from what was held while the next value is fetched.
#[derive(Default)]
pub(crate) struct StaleDiff {
    held: Mutex<OrderedMap<DiffKey, (i64, PullRequestDiffResult)>>,
}

impl StaleDiff {
    fn lock(&self) -> std::sync::MutexGuard<'_, OrderedMap<DiffKey, (i64, PullRequestDiffResult)>> {
        self.held.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn record(&self, key: DiffKey, value: &PullRequestDiffResult, at: i64) {
        let mut held = self.lock();
        held.remove(&key);
        if !can_cache_diff(value) {
            return;
        }
        if held.len() >= DIFF_CACHE_CAPACITY {
            held.pop_first();
        }
        held.insert_last(key, (at, value.clone()));
    }
}

/// `toPullRequestError(operation)`.
fn provider_error(operation: &'static str) -> impl Fn(PullRequestProviderError) -> PullRequestError {
    move |error| PullRequestError::from_provider(operation, error)
}

pub(crate) fn change_request_of(project: &SupportedProject, number: i64) -> ChangeRequestRef {
    ChangeRequestRef {
        cwd: project.project.workspace_root.clone(),
        repository: project.repository.clone(),
        host: project.host.clone(),
        number,
    }
}

/// A detail read as the summary fields it carries (hosts without a narrow summary read).
fn summary_of_detail(detail: ProviderChangeRequestDetail) -> ProviderChangeRequestSummary {
    let change_request = detail.change_request;
    ProviderChangeRequestSummary {
        number: change_request.number,
        title: change_request.title,
        url: change_request.url,
        head_branch: change_request.head_branch,
        base_branch: change_request.base_branch,
        state: change_request.state,
        is_draft: Some(change_request.is_draft),
        closed_at: Some(detail.closed_at),
        merged_at: Some(detail.merged_at),
        updated_at: change_request.updated_at,
        author: Some(change_request.author),
        additions: Some(change_request.additions),
        deletions: Some(change_request.deletions),
        changed_files: Some(detail.changed_files),
        review_decision: change_request.review_decision,
        checks_state: change_request.checks_state,
        mergeability: Some(change_request.mergeability),
    }
}

/// `summaryFromDetail`: the detail's fields over the last summary (a detail carries no review
/// or check summary, so those stay as last observed).
fn summary_from_detail(detail: &PullRequestDetail, previous: Option<PullRequestSummary>) -> PullRequestSummary {
    let base = previous.unwrap_or(PullRequestSummary {
        provider: detail.provider,
        project_id: detail.project_id.clone(),
        repository: detail.repository.clone(),
        number: detail.number,
        title: detail.title.clone(),
        url: detail.url.clone(),
        state: detail.state,
        is_draft: None,
        head_branch: detail.head_branch.clone(),
        base_branch: detail.base_branch.clone(),
        closed_at: None,
        merged_at: None,
        updated_at: detail.updated_at.clone(),
        observed_at: None,
        author: None,
        additions: None,
        deletions: None,
        changed_files: None,
        review_decision: None,
        checks_state: None,
        mergeability: None,
    });
    PullRequestSummary {
        provider: detail.provider,
        project_id: detail.project_id.clone(),
        repository: detail.repository.clone(),
        number: detail.number,
        title: detail.title.clone(),
        url: detail.url.clone(),
        state: detail.state,
        is_draft: Some(detail.is_draft),
        author: Some(detail.author.clone()),
        additions: Some(detail.additions),
        deletions: Some(detail.deletions),
        changed_files: Some(detail.changed_files),
        mergeability: Some(detail.mergeability),
        head_branch: detail.head_branch.clone(),
        base_branch: detail.base_branch.clone(),
        closed_at: Some(detail.closed_at.clone()),
        merged_at: Some(detail.merged_at.clone()),
        updated_at: detail.updated_at.clone(),
        observed_at: detail.observed_at,
        ..base
    }
}

/// `previewFields`.
fn preview_of_detail(detail: &PullRequestDetail) -> PullRequestPreview {
    PullRequestPreview {
        project_id: detail.project_id.clone(),
        repository: detail.repository.clone(),
        number: detail.number,
        title: detail.title.clone(),
        url: detail.url.clone(),
        author: detail.author.clone(),
        state: detail.state,
        is_draft: detail.is_draft,
        created_at: detail.created_at.clone(),
    }
}

impl PullRequestService {
    /// `summaryUncached`: the narrow summary read where the host has one, the detail otherwise.
    pub(crate) async fn summary_uncached(&self, input: &PullRequestRef) -> Result<PullRequestSummary, PullRequestError> {
        let project = self.require_project(input).await?;
        let change_request = change_request_of(&project, input.number);
        let observed_at = self.now();
        let read = if project.api.optional_methods().get_change_request_summary {
            project.api.get_change_request_summary(change_request).await
        } else {
            project.api.get_change_request(change_request).await.map(summary_of_detail)
        };
        let summary = read.map_err(provider_error("summary"))?;
        Ok(PullRequestSummary {
            provider: project.api.kind(),
            project_id: project.project.id.clone(),
            repository: project.repository.clone(),
            number: summary.number,
            title: summary.title,
            url: summary.url,
            state: summary.state,
            head_branch: summary.head_branch,
            base_branch: summary.base_branch,
            closed_at: Some(summary.closed_at.flatten()),
            merged_at: Some(summary.merged_at.flatten()),
            updated_at: summary.updated_at,
            observed_at: Some(JsNumber::from(observed_at)),
            is_draft: summary.is_draft,
            author: summary.author,
            additions: summary.additions,
            deletions: summary.deletions,
            changed_files: summary.changed_files,
            review_decision: summary.review_decision,
            checks_state: summary.checks_state,
            mergeability: summary.mergeability,
        })
    }

    async fn stack_uncached(&self, input: &PullRequestRef, include_details: bool) -> Result<Option<PullRequestStack>, PullRequestError> {
        let project = self.require_project(input).await?;
        if !project.api.optional_methods().get_change_request_stack {
            return Ok(None);
        }
        let stack = project
            .api
            .get_change_request_stack(GetChangeRequestStackInput {
                change_request: change_request_of(&project, input.number),
                include_details: Some(include_details),
            })
            .await
            .map_err(provider_error("stack"))?;
        Ok(stack.map(|stack| PullRequestStack {
            id: stack.id,
            number: stack.number,
            url: stack.url,
            base: stack.base,
            layers: stack
                .layers
                .into_iter()
                .map(|layer| PullRequestStackLayersItem {
                    number: layer.number,
                    title: layer.title,
                    is_draft: layer.is_draft,
                    head_sha: layer.head_sha,
                    head_branch: layer.head_branch,
                    state: layer.state,
                })
                .collect(),
        }))
    }

    /// `persistedRead`: a read kept in the persisted read cache under the serving checkout,
    /// scoped to its project and its reference. A payload that no longer decodes is read again.
    async fn persisted_read<T>(&self, input: &CredRef, operation: &str, read: BoxFuture<'static, Result<T, PullRequestError>>) -> Result<T, PullRequestError>
    where
        T: Serialize + DeserializeOwned + Clone + Send + Sync + 'static,
    {
        let project = self.require_project(&input.reference).await?;
        let key = [
            operation.to_owned(),
            project.api.kind().as_str().to_owned(),
            lower(&project.host),
            lower(&project.repository),
            project.project.id.as_str().to_owned(),
            project.project.workspace_root.clone(),
            input.reference.number.to_string(),
            input.reference.expected_account_id.clone().unwrap_or_default(),
            input.credential.clone().unwrap_or_default(),
        ]
        .iter()
        .map(|part| encode_uri_component(part))
        .collect::<Vec<_>>()
        .join(":");
        let lookup = inherit(read).shared();
        let encoded = {
            let lookup = lookup.clone();
            async move {
                let value = lookup.await?;
                serde_json::to_string(&value).map_err(|error| {
                    PullRequestError::operation("cache", "Could not encode PR cache data.")
                        .with_cause(Cause::new(zc_core::defect::Defect::error("Error", error.to_string())))
                })
            }
            .boxed()
        };
        let scopes = [format!("project:{}", input.reference.project_id.as_str()), ref_scope(&input.reference)];
        let payload = self.inner.read_cache.get(&key, encoded, &scopes).await?;
        match serde_json::from_str::<T>(&payload) {
            Ok(value) => Ok(value),
            Err(_) => lookup.await,
        }
    }

    /// `shouldReplaceHeldSummary`: a merged summary is never replaced by an unmerged one, and an
    /// older revision (or an older observation of the same one) never replaces a newer one.
    fn should_replace_held_summary(&self, key: &RefKey, next: &PullRequestSummary) -> bool {
        let Some(current) = self.inner.last_good_summary.peek(key) else {
            return true;
        };
        if current.state == PullRequestState::Merged && next.state != PullRequestState::Merged {
            return false;
        }
        if next.updated_at != current.updated_at {
            return next.updated_at > current.updated_at;
        }
        let observed = |summary: &PullRequestSummary| summary.observed_at.map_or(f64::NEG_INFINITY, |at| at.0);
        observed(next) >= observed(&current)
    }

    /// `summary`: a summary already held answers display reads without asking the host; a strict
    /// read (`allowStale: false`, or a settlement read that must not recover from a transient
    /// failure unless the held state is merged) goes to the persisted read.
    pub(crate) async fn summary_cached(&self, input: CredRef, recover_transient_failure: bool) -> Result<PullRequestSummary, PullRequestError> {
        let key = self.ref_key(&input);
        if let Some(held) = self.inner.last_good_summary.peek(&key) {
            if input.reference.allow_stale != Some(false) && (recover_transient_failure || held.state == PullRequestState::Merged) {
                return Ok(held);
            }
        }
        let this = self.clone();
        let reference = input.reference.clone();
        let read = async move { this.summary_uncached(&reference).await }.boxed();
        let value = self.persisted_read(&input, "summary", read).await?;
        if self.should_replace_held_summary(&key, &value) {
            self.inner.last_good_summary.record(key, value.clone(), self.now());
        }
        Ok(value)
    }

    pub(crate) async fn stack_cached(&self, input: CredRef, include_details: bool) -> Result<Option<PullRequestStack>, PullRequestError> {
        let this = self.clone();
        let reference = input.reference.clone();
        let read = async move { this.stack_uncached(&reference, include_details).await }.boxed();
        self.persisted_read(&input, &format!("stack:{include_details}"), read).await
    }

    async fn detail_uncached(&self, input: &PullRequestRef) -> Result<PullRequestDetail, PullRequestError> {
        let project = self.require_project(input).await?;
        let observed_at = self.now();
        let change_request = project.api.get_change_request(change_request_of(&project, input.number));
        let read = async { change_request.await.map_err(provider_error("detail")) };
        let viewer = async { Ok(self.viewer_of(&project).await) };
        let (detail, viewer) = try_join(read, viewer).await?;
        let core = detail.change_request;
        Ok(PullRequestDetail {
            provider: project.api.kind(),
            capabilities: project.api.capabilities().clone(),
            viewer_permissions: detail.viewer_permissions,
            project_id: project.project.id.clone(),
            project_title: project.project.title.clone(),
            workspace_root: project.project.workspace_root.clone(),
            repository: project.repository.clone(),
            number: core.number,
            title: core.title,
            body: detail.body,
            url: core.url,
            author: core.author,
            state: core.state,
            is_draft: core.is_draft,
            mergeability: core.mergeability,
            additions: core.additions,
            deletions: core.deletions,
            changed_files: detail.changed_files,
            head_branch: core.head_branch,
            head_repository_name_with_owner: core.head_repository_name_with_owner,
            base_branch: core.base_branch,
            created_at: core.created_at,
            updated_at: core.updated_at,
            observed_at: Some(JsNumber::from(observed_at)),
            merged_at: detail.merged_at,
            closed_at: detail.closed_at,
            reviewers: detail.reviewers,
            labels: core.labels,
            checks: detail.checks,
            merge_capabilities: detail.merge_capabilities,
            viewer: viewer.filter(|viewer| !js_trim(viewer).is_empty()),
            base_comparison: detail.base_comparison,
            behind_by: detail.behind_by,
            auto_merge_enabled: detail.auto_merge_enabled,
            auto_merge_method: detail.auto_merge_method,
            workflow_approvals_required: detail.workflow_approvals_required,
        })
    }

    /// The detail cache read, which also records the row's counts and (when it is newer) the
    /// summary it implies.
    async fn detail_read(&self, key: RefKey) -> Result<PullRequestDetail, PullRequestError> {
        let this = self.clone();
        let lookup_key = key.clone();
        let value = self
            .inner
            .detail_cache
            .get(key.clone(), move || {
                inherit(async move {
                    let stats_key = this.stats_cache_key(lookup_key.clone());
                    let value = this.detail_uncached(&lookup_key.to_ref().reference).await?;
                    this.record_stats(
                        stats_key,
                        PullRequestDiffStat {
                            project_id: value.project_id.clone(),
                            repository: value.repository.clone(),
                            number: value.number,
                            additions: value.additions,
                            deletions: value.deletions,
                        },
                        this.now(),
                    );
                    Ok(value)
                })
            })
            .await?;
        // Recorded from a host or cache read, never from the held value served while it
        // refreshes, and only when not older than a later strict summary.
        let summary = summary_from_detail(&value, self.inner.last_good_summary.peek(&key));
        if self.should_replace_held_summary(&key, &summary) {
            self.inner.last_good_summary.record(key, summary, self.now());
        }
        Ok(value)
    }

    /// `lastGoodDetail.read`: a transient host failure answers with a detail read within the
    /// last ten minutes.
    async fn last_good_detail_read(&self, key: RefKey) -> Result<PullRequestDetail, PullRequestError> {
        match self.detail_read(key.clone()).await {
            Ok(value) => {
                self.inner.last_good_detail.record(key, value.clone(), self.now());
                Ok(value)
            }
            Err(error) => {
                let PullRequestError::Operation { operation, cause, .. } = &error else {
                    return Err(error);
                };
                let Some(provider) = cause.as_ref().and_then(|cause| cause.downcast_ref::<PullRequestProviderError>()) else {
                    return Err(error);
                };
                if provider.reason != ProviderFailureReason::Failed && provider.reason != ProviderFailureReason::RateLimited {
                    return Err(error);
                }
                match self.inner.last_good_detail.peek_entry(&key) {
                    Some((at, value)) if self.now() - at <= STALE_DETAIL_WINDOW_MS => {
                        tracing::warn!(operation = %operation, reason = provider.reason.as_str(), "using recent pull request data after a failed refresh");
                        Ok(value)
                    }
                    _ => Err(error),
                }
            }
        }
    }

    /// `detail`: a change request already read answers at once and refreshes behind the answer
    /// (`serveHeld(…, "revalidate")`); `allowStale: false` waits for the host.
    pub(crate) async fn detail_cached(&self, input: CredRef) -> Result<PullRequestDetail, PullRequestError> {
        let key = self.ref_key(&input);
        if input.reference.allow_stale == Some(false) {
            let value = self.detail_read(key.clone()).await?;
            self.inner.last_good_detail.record(key, value.clone(), self.now());
            return Ok(value);
        }
        if let Some(held) = self.inner.last_good_detail.peek(&key) {
            let this = self.clone();
            spawn_inheriting(async move {
                let _ = this.last_good_detail_read(key).await;
            });
            return Ok(held);
        }
        self.last_good_detail_read(key).await
    }

    async fn preview_uncached(&self, input: &PullRequestRef) -> Result<PullRequestPreview, PullRequestError> {
        let project = self.require_project(input).await?;
        let change_request = change_request_of(&project, input.number);
        if project.api.optional_methods().get_change_request_preview {
            let preview = project
                .api
                .get_change_request_preview(change_request)
                .await
                .map_err(provider_error("preview"))?;
            return Ok(PullRequestPreview {
                project_id: project.project.id.clone(),
                repository: project.repository.clone(),
                number: preview.number,
                title: preview.title,
                url: preview.url,
                author: preview.author,
                state: preview.state,
                is_draft: preview.is_draft,
                created_at: preview.created_at,
            });
        }
        let detail = project.api.get_change_request(change_request).await.map_err(provider_error("preview"))?;
        let core = detail.change_request;
        Ok(PullRequestPreview {
            project_id: project.project.id.clone(),
            repository: project.repository.clone(),
            number: core.number,
            title: core.title,
            url: core.url,
            author: core.author,
            state: core.state,
            is_draft: core.is_draft,
            created_at: core.created_at,
        })
    }

    /// `preview`: an unexpired detail already read answers it (without waiting on one in flight).
    pub(crate) async fn preview_cached(&self, input: CredRef) -> Result<PullRequestPreview, PullRequestError> {
        let key = self.ref_key(&input);
        if let Some(detail) = self.inner.detail_cache.get_success(&key) {
            return Ok(preview_of_detail(&detail));
        }
        let this = self.clone();
        let lookup_key = key.clone();
        self.inner
            .preview_cache
            .get(key, move || inherit(async move { this.preview_uncached(&lookup_key.to_ref().reference).await }))
            .await
    }

    async fn activity_uncached(&self, input: &PullRequestRef) -> Result<PullRequestActivity, PullRequestError> {
        let project = self.require_project(input).await?;
        let activity = project
            .api
            .get_change_request_activity(change_request_of(&project, input.number))
            .await
            .map_err(provider_error("activity"))?;
        Ok(PullRequestActivity {
            author: activity.author,
            reviewers: activity.reviewers,
            comments: activity.comments,
            comment_count: activity.comment_count,
            comments_truncated: activity.comments_truncated,
            review_threads: activity.review_threads,
            commits: activity.commits,
            reactions: activity.reactions,
        })
    }

    pub(crate) async fn activity_cached(&self, input: CredRef) -> Result<PullRequestActivity, PullRequestError> {
        let key = self.ref_key(&input);
        let this = self.clone();
        let lookup_key = key.clone();
        self.inner
            .activity_cache
            .get(key, move || {
                inherit(async move { this.activity_uncached(&lookup_key.to_ref().reference).await })
            })
            .await
    }

    pub(crate) async fn thread_comments_impl(&self, input: &PullRequestThreadCommentsInput) -> Result<PullRequestThreadCommentsResult, PullRequestError> {
        let project = self.require_project(&input.reference()).await?;
        if !project.api.optional_methods().get_review_thread_comments {
            return Err(PullRequestError::operation("threadComments", "This host does not page review thread comments."));
        }
        project
            .api
            .get_review_thread_comments(ReviewThreadCommentsInput {
                change_request: change_request_of(&project, input.number),
                thread_id: input.thread_id.clone(),
                cursor: input.cursor.clone(),
            })
            .await
            .map_err(provider_error("threadComments"))
    }

    async fn diff_uncached(&self, input: &PullRequestRef, cursor: Option<String>, commit: Option<String>) -> Result<PullRequestDiffResult, PullRequestError> {
        let project = self.require_project(input).await?;
        if !project.api.capabilities().diff {
            return Err(PullRequestError::operation("diff", "This host cannot provide a diff for a change request."));
        }
        let slice = project
            .api
            .get_diff(GetDiffInput {
                change_request: change_request_of(&project, input.number),
                cursor,
                commit,
            })
            .await
            .map_err(provider_error("diff"))?;
        Ok(PullRequestDiffResult {
            patch: slice.patch,
            truncated: slice.truncated,
            next_cursor: slice.next_cursor,
            omitted_file_stats: slice.omitted_file_stats,
        })
    }

    async fn diff_read(&self, key: DiffKey) -> Result<PullRequestDiffResult, PullRequestError> {
        let this = self.clone();
        let lookup_key = key.clone();
        self.inner
            .diff_cache
            .get(key, move || {
                inherit(async move {
                    this.diff_uncached(&lookup_key.reference.to_ref().reference, lookup_key.cursor.clone(), lookup_key.commit.clone())
                        .await
                })
            })
            .await
    }

    /// The diff read, recorded for the stale window.
    async fn diff_recorded(&self, key: DiffKey) -> Result<PullRequestDiffResult, PullRequestError> {
        let value = self.diff_read(key.clone()).await?;
        self.inner.stale_diff.record(key, &value, self.now());
        Ok(value)
    }

    /// `diff`: not live-polled and expensive, so within its stale window it answers from what is
    /// held and refreshes behind the answer; refreshes and mutations still strand held values
    /// through the reference epoch.
    pub(crate) async fn diff_cached(&self, input: CredRef, cursor: Option<String>, commit: Option<String>) -> Result<PullRequestDiffResult, PullRequestError> {
        let reference = self.ref_key(&input);
        let revision = if commit.is_none() {
            self.inner.last_good_summary.peek(&reference).map(|summary| summary.updated_at)
        } else {
            None
        };
        let key = DiffKey {
            reference,
            cursor,
            commit,
            revision,
        };
        let now = self.now();
        let snapshot = self.inner.stale_diff.lock().get(&key).cloned();
        match snapshot {
            Some((at, value)) if now - at <= DIFF_STALE_WINDOW_MS => {
                // Its own task: the caller is answered and gone before the refresh lands. The read
                // still coalesces on the cache key, and a failed refresh costs nothing.
                let this = self.clone();
                spawn_inheriting(async move {
                    let _ = this.diff_recorded(key).await;
                });
                Ok(value)
            }
            _ => self.diff_recorded(key).await,
        }
    }

    pub(crate) async fn diff_file_contents_impl(
        &self,
        input: &PullRequestDiffFileContentsInput,
    ) -> Result<PullRequestDiffFileContentsResult, PullRequestError> {
        let project = self.require_project(&input.reference()).await?;
        if !project.api.capabilities().diff || !project.api.optional_methods().get_diff_file_contents {
            return Err(PullRequestError::operation(
                "diffFileContents",
                "This host cannot expand unchanged pull request lines.",
            ));
        }
        let contents = project
            .api
            .get_diff_file_contents(DiffFileContentsInput {
                change_request: change_request_of(&project, input.number),
                commit: input.commit.clone(),
                change_type: input.change_type,
                old_path: input.old_path.clone(),
                new_path: input.new_path.clone(),
            })
            .await
            .map_err(provider_error("diffFileContents"))?;
        Ok(PullRequestDiffFileContentsResult {
            old_contents: contents.old_contents,
            new_contents: contents.new_contents,
        })
    }

    /// `filesViewed`: canonicalised before it is keyed, since both its epochs are bumped against
    /// the remote's own spelling.
    pub(crate) async fn files_viewed_cached(&self, input: CredRef) -> Result<PullRequestFilesViewedResult, PullRequestError> {
        let reference = CredRef {
            reference: self.canonical_ref(&input.reference).await?,
            credential: input.credential,
        };
        let key = (
            self.ref_key(&reference),
            self.with_epochs(|epochs| epochs.files_viewed_epoch(&reference.reference)),
        );
        let this = self.clone();
        let lookup_key = key.0.clone();
        self.inner
            .files_viewed_cache
            .get(key, move || {
                inherit(async move {
                    let reference = lookup_key.to_ref().reference;
                    this.inner.viewed_files.files_viewed(&this, &reference).await
                })
            })
            .await
    }

    /// `setFilesViewed`: deliberately not a mutation (ticking a file off says nothing about the
    /// change request, and dropping a 300-file diff per checkbox is the feature's whole cost);
    /// only this reader's own bookkeeping is forgotten.
    pub(crate) async fn set_files_viewed_impl(&self, input: PullRequestSetFilesViewedInput) -> Result<(), PullRequestError> {
        let canonical = self.canonical_ref(&input.reference()).await?;
        self.inner.viewed_files.set_files_viewed(self, &input).await?;
        self.with_epochs(|epochs| epochs.bump_files_viewed(&canonical));
        Ok(())
    }
}
