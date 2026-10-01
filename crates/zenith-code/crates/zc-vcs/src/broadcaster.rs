//! `vcs/VcsStatusBroadcaster.ts`: the cached, pushed VCS status behind `subscribeVcsStatus`
//! and `vcs.refreshStatus`.
//!
//! - Status is cached per canonical cwd as two parts (local, remote), each with a fingerprint
//!   (`JSON.stringify`); only a changed part is published.
//! - Remote reads that write the cache hold a per-cwd lock, so a poll that started before a
//!   turn-end refresh cannot overwrite the fresher PR.
//! - Each streamed cwd has one remote poller, refcounted by subscribers and remembering which
//!   requested cwds want it (background policy is asked per requested cwd). It runs every
//!   `automaticGitFetchInterval` (30 s when that is 0, which also turns upstream fetching off
//!   after the first load), backing off 30 s → 15 min on failures, and auto-pulls a clean,
//!   behind default branch when the project enables it.
//! - No file watcher: local refreshes come from RPCs and reactors.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::{Stream, StreamExt};
use serde::Serialize;
use zc_core::pubsub::PubSub;
use zc_ports::contracts::BackgroundScope;
use zc_ports::{BackgroundPolicy, ProjectionReads, SettingsService};

use crate::contracts::*;
use crate::errors::{GitCommandError, GitManagerServiceError};
use crate::status::{canonicalize_existing_path, RemoteStatusOptions};
use crate::workflow::GitWorkflowService;

pub const DEFAULT_VCS_STATUS_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const VCS_STATUS_REFRESH_FAILURE_BASE_DELAY: Duration = Duration::from_secs(30);
const VCS_STATUS_REFRESH_FAILURE_MAX_DELAY: Duration = Duration::from_secs(15 * 60);

/// `remoteRefreshFailureDelay`: 30 s doubling per failure (capped at 15 min), never shorter
/// than the configured interval.
pub fn remote_refresh_failure_delay(consecutive_failures: u32, configured: Duration) -> Duration {
    let exponent = consecutive_failures.saturating_sub(1).min(20);
    let backoff = VCS_STATUS_REFRESH_FAILURE_BASE_DELAY
        .saturating_mul(1 << exponent)
        .min(VCS_STATUS_REFRESH_FAILURE_MAX_DELAY);
    configured.max(backoff)
}

/// `remoteRefreshFailureDiagnostics` for one failure: its `_tag` and `operation` (bounded to 128
/// characters), which are safe to log.
pub fn failure_diagnostics(error: &GitManagerServiceError) -> (String, Option<String>) {
    let bounded = |value: &str| value.chars().take(128).collect::<String>();
    match error {
        GitManagerServiceError::Manager(e) => ("GitManagerError".into(), Some(bounded(&e.operation))),
        GitManagerServiceError::Command(e) => ("GitCommandError".into(), Some(bounded(&e.operation))),
        GitManagerServiceError::Other(e) => (bounded(&e.tag), e.fields.get("operation").and_then(|v| v.as_str()).map(bounded)),
    }
}

/// The workflow calls the broadcaster makes (`GitWorkflowService`'s status slice).
#[async_trait]
pub trait StatusWorkflow: Send + Sync {
    async fn local_status(&self, cwd: &str) -> Result<VcsStatusLocalResult, GitManagerServiceError>;
    async fn remote_status(&self, cwd: &str, options: RemoteStatusOptions) -> Result<Option<VcsStatusRemoteResult>, GitManagerServiceError>;
    async fn invalidate_local_status(&self, cwd: &str);
    async fn invalidate_remote_status(&self, cwd: &str);
    async fn invalidate_status(&self, cwd: &str);
    async fn pull_current_branch(&self, cwd: &str) -> Result<VcsPullResult, GitCommandError>;
}

#[async_trait]
impl StatusWorkflow for GitWorkflowService {
    async fn local_status(&self, cwd: &str) -> Result<VcsStatusLocalResult, GitManagerServiceError> {
        GitWorkflowService::local_status(self, cwd).await
    }
    async fn remote_status(&self, cwd: &str, options: RemoteStatusOptions) -> Result<Option<VcsStatusRemoteResult>, GitManagerServiceError> {
        GitWorkflowService::remote_status(self, cwd, options).await
    }
    async fn invalidate_local_status(&self, cwd: &str) {
        GitWorkflowService::invalidate_local_status(self, cwd).await
    }
    async fn invalidate_remote_status(&self, cwd: &str) {
        GitWorkflowService::invalidate_remote_status(self, cwd).await
    }
    async fn invalidate_status(&self, cwd: &str) {
        GitWorkflowService::invalidate_status(self, cwd).await
    }
    async fn pull_current_branch(&self, cwd: &str) -> Result<VcsPullResult, GitCommandError> {
        GitWorkflowService::pull_current_branch(self, cwd).await
    }
}

/// `VcsAutoPullPolicy`: whether the project at a cwd pulls automatically.
#[async_trait]
pub trait AutoPullPolicy: Send + Sync {
    async fn is_enabled(&self, cwd: &str) -> bool;
}

/// The default policy: never.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoAutoPull;

#[async_trait]
impl AutoPullPolicy for NoAutoPull {
    async fn is_enabled(&self, _cwd: &str) -> bool {
        false
    }
}

/// `autoPullPolicyLayer`: the active project at that workspace root, with its
/// `defaultAutoPull` (project override, else the environment setting). Failures mean false.
pub struct ProjectAutoPullPolicy {
    pub projections: Arc<dyn ProjectionReads>,
    pub settings: Arc<dyn SettingsService>,
}

/// `resolveProjectSettings(settings, projectId).settings.defaultAutoPull`.
pub fn resolve_default_auto_pull(settings: &serde_json::Value, project_id: &str) -> bool {
    let override_value = settings
        .get("projectSettingsOverrides")
        .and_then(|o| o.get(project_id))
        .and_then(|o| o.get("defaultAutoPull"))
        .and_then(serde_json::Value::as_bool);
    override_value.unwrap_or_else(|| settings.get("defaultAutoPull").and_then(serde_json::Value::as_bool).unwrap_or(false))
}

#[async_trait]
impl AutoPullPolicy for ProjectAutoPullPolicy {
    async fn is_enabled(&self, cwd: &str) -> bool {
        let Ok(Some(project)) = self.projections.get_active_project_by_workspace_root(cwd).await else {
            return false;
        };
        let project_id = project.id.to_string();
        let Ok(settings) = self.settings.get_settings().await else {
            return false;
        };
        resolve_default_auto_pull(&serde_json::to_value(&settings).unwrap_or_default(), &project_id)
    }
}

/// `automaticRemoteRefreshInterval` (the `automaticGitFetchInterval` setting).
pub type RefreshInterval = Arc<dyn Fn() -> BoxFuture<'static, Duration> + Send + Sync>;

/// A fixed interval.
pub fn fixed_interval(interval: Duration) -> RefreshInterval {
    Arc::new(move || Box::pin(async move { interval }))
}

#[derive(Clone)]
struct VcsStatusChange {
    cwd: String,
    event: VcsStatusStreamEvent,
}

#[derive(Clone)]
struct Cached<T> {
    fingerprint: String,
    value: T,
}

#[derive(Clone, Default)]
struct CachedVcsStatus {
    local: Option<Cached<VcsStatusLocalResult>>,
    remote: Option<Cached<Option<VcsStatusRemoteResult>>>,
}

struct ActivePoller {
    task: tokio::task::JoinHandle<()>,
    subscriber_count: usize,
    demand_cwds: Arc<Mutex<Vec<(String, usize)>>>,
}

fn fingerprint<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn demand_keys(demand: &Mutex<Vec<(String, usize)>>) -> Vec<String> {
    demand.lock().unwrap_or_else(|p| p.into_inner()).iter().map(|(cwd, _)| cwd.clone()).collect()
}

struct Inner {
    workflow: Arc<dyn StatusWorkflow>,
    background: Arc<dyn BackgroundPolicy>,
    auto_pull: Arc<dyn AutoPullPolicy>,
    changes: PubSub<VcsStatusChange>,
    cache: Mutex<HashMap<String, CachedVcsStatus>>,
    remote_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    pollers: Mutex<HashMap<String, ActivePoller>>,
}

/// `VcsStatusBroadcaster`. Cheap to clone.
#[derive(Clone)]
pub struct VcsStatusBroadcaster {
    inner: Arc<Inner>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // The broadcaster scope closes: stop every poller.
        let pollers = self.pollers.get_mut().unwrap_or_else(|p| p.into_inner());
        for (_, poller) in pollers.drain() {
            poller.task.abort();
        }
        self.changes.shutdown();
    }
}

impl VcsStatusBroadcaster {
    pub fn new(workflow: Arc<dyn StatusWorkflow>, background: Arc<dyn BackgroundPolicy>, auto_pull: Arc<dyn AutoPullPolicy>) -> Self {
        Self {
            inner: Arc::new(Inner {
                workflow,
                background,
                auto_pull,
                changes: PubSub::new(),
                cache: Mutex::new(HashMap::new()),
                remote_locks: Mutex::new(HashMap::new()),
                pollers: Mutex::new(HashMap::new()),
            }),
        }
    }

    fn remote_lock(&self, cwd: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.inner
            .remote_locks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(cwd.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn cached(&self, cwd: &str) -> Option<CachedVcsStatus> {
        self.inner.cache.lock().unwrap_or_else(|p| p.into_inner()).get(cwd).cloned()
    }

    fn update_cached_local(&self, cwd: &str, local: VcsStatusLocalResult, publish: bool) -> VcsStatusLocalResult {
        let next = Cached {
            fingerprint: fingerprint(&local),
            value: local.clone(),
        };
        let changed = {
            let mut cache = self.inner.cache.lock().unwrap_or_else(|p| p.into_inner());
            let entry = cache.entry(cwd.to_owned()).or_default();
            let changed = entry.local.as_ref().map(|c| &c.fingerprint) != Some(&next.fingerprint);
            entry.local = Some(next);
            changed
        };
        if publish && changed {
            self.inner.changes.publish(VcsStatusChange {
                cwd: cwd.to_owned(),
                event: VcsStatusStreamEvent::LocalUpdated { local: local.clone() },
            });
        }
        local
    }

    fn update_cached_remote(&self, cwd: &str, remote: Option<VcsStatusRemoteResult>, publish: bool) -> Option<VcsStatusRemoteResult> {
        let next = Cached {
            fingerprint: fingerprint(&remote),
            value: remote.clone(),
        };
        let changed = {
            let mut cache = self.inner.cache.lock().unwrap_or_else(|p| p.into_inner());
            let entry = cache.entry(cwd.to_owned()).or_default();
            let changed = entry.remote.as_ref().map(|c| &c.fingerprint) != Some(&next.fingerprint);
            entry.remote = Some(next);
            changed
        };
        if publish && changed {
            self.inner.changes.publish(VcsStatusChange {
                cwd: cwd.to_owned(),
                event: VcsStatusStreamEvent::RemoteUpdated { remote: remote.clone() },
            });
        }
        remote
    }

    fn update_cached_status(&self, cwd: &str, local: VcsStatusLocalResult, remote: Option<VcsStatusRemoteResult>, publish: bool) -> VcsStatusResult {
        let next_local = Cached {
            fingerprint: fingerprint(&local),
            value: local.clone(),
        };
        let next_remote = Cached {
            fingerprint: fingerprint(&remote),
            value: remote.clone(),
        };
        let changed = {
            let mut cache = self.inner.cache.lock().unwrap_or_else(|p| p.into_inner());
            let entry = cache.entry(cwd.to_owned()).or_default();
            let changed = entry.local.as_ref().map(|c| &c.fingerprint) != Some(&next_local.fingerprint)
                || entry.remote.as_ref().map(|c| &c.fingerprint) != Some(&next_remote.fingerprint);
            entry.local = Some(next_local);
            entry.remote = Some(next_remote);
            changed
        };
        if publish && changed {
            self.inner.changes.publish(VcsStatusChange {
                cwd: cwd.to_owned(),
                event: VcsStatusStreamEvent::Snapshot {
                    local: local.clone(),
                    remote: remote.clone(),
                },
            });
        }
        VcsStatusResult::merge(local, remote)
    }

    async fn get_or_load_local_status(&self, cwd: &str) -> Result<VcsStatusLocalResult, GitManagerServiceError> {
        if let Some(local) = self.cached(cwd).and_then(|c| c.local) {
            return Ok(local.value);
        }
        let local = self.inner.workflow.local_status(cwd).await?;
        Ok(self.update_cached_local(cwd, local, false))
    }

    /// `getStatus(input)`: cached parts, loading what is missing (under the remote lock).
    pub async fn get_status(&self, cwd: &str) -> Result<VcsStatusResult, GitManagerServiceError> {
        let cwd = canonicalize_existing_path(cwd).await;
        if let Some(CachedVcsStatus {
            local: Some(local),
            remote: Some(remote),
        }) = self.cached(&cwd)
        {
            return Ok(VcsStatusResult::merge(local.value, remote.value));
        }
        let lock = self.remote_lock(&cwd);
        let _held = lock.lock().await;
        let latest = self.cached(&cwd).unwrap_or_default();
        let (local, remote) = tokio::try_join!(
            async {
                match latest.local {
                    Some(local) => Ok(local.value),
                    None => self.inner.workflow.local_status(&cwd).await,
                }
            },
            async {
                match latest.remote {
                    Some(remote) => Ok(remote.value),
                    None => self.inner.workflow.remote_status(&cwd, RemoteStatusOptions::default()).await,
                }
            }
        )?;
        Ok(self.update_cached_status(&cwd, local, remote, false))
    }

    /// `refreshLocalStatus(cwd)`: invalidate, reload and publish the local part.
    pub async fn refresh_local_status(&self, raw_cwd: &str) -> Result<VcsStatusLocalResult, GitManagerServiceError> {
        let cwd = canonicalize_existing_path(raw_cwd).await;
        self.inner.workflow.invalidate_local_status(&cwd).await;
        let local = self.inner.workflow.local_status(&cwd).await?;
        Ok(self.update_cached_local(&cwd, local, true))
    }

    async fn maybe_auto_pull_inner(
        &self,
        cwd: &str,
        remote: &Option<VcsStatusRemoteResult>,
        policy_cwds: &[String],
    ) -> Result<Option<(VcsStatusLocalResult, Option<VcsStatusRemoteResult>)>, String> {
        let mut enabled = false;
        for policy_cwd in policy_cwds {
            if self.inner.auto_pull.is_enabled(policy_cwd).await {
                enabled = true;
            }
        }
        let Some(remote) = remote else {
            return Ok(None);
        };
        if !remote.has_upstream || remote.ahead_count > 0 || remote.behind_count == 0 || !enabled {
            return Ok(None);
        }
        let workflow = &self.inner.workflow;
        workflow.invalidate_local_status(cwd).await;
        let local = workflow.local_status(cwd).await.map_err(|e| e.message())?;
        if !local.is_repo || !local.is_default_ref || local.has_working_tree_changes {
            return Ok(None);
        }
        workflow.pull_current_branch(cwd).await.map_err(|e| e.message())?;
        workflow.invalidate_status(cwd).await;
        let (refreshed_local, refreshed_remote) = tokio::try_join!(
            workflow.local_status(cwd),
            workflow.remote_status(
                cwd,
                RemoteStatusOptions {
                    refresh_upstream: false,
                    refresh_missing_pull_request: false,
                }
            )
        )
        .map_err(|e| e.message())?;
        self.update_cached_status(cwd, refreshed_local.clone(), refreshed_remote.clone(), true);
        Ok(Some((refreshed_local, refreshed_remote)))
    }

    /// `maybeAutoPull`: failures log and mean "no pull".
    async fn maybe_auto_pull(
        &self,
        cwd: &str,
        remote: &Option<VcsStatusRemoteResult>,
        policy_cwds: &[String],
    ) -> Option<(VcsStatusLocalResult, Option<VcsStatusRemoteResult>)> {
        match self.maybe_auto_pull_inner(cwd, remote, policy_cwds).await {
            Ok(pulled) => pulled,
            Err(detail) => {
                tracing::warn!(cwd, %detail, "Automatic project pull failed");
                None
            }
        }
    }

    async fn refresh_remote_status(
        &self,
        cwd: &str,
        refresh_upstream: bool,
        policy_cwds: Vec<String>,
    ) -> Result<Option<VcsStatusRemoteResult>, GitManagerServiceError> {
        let lock = self.remote_lock(cwd);
        let _held = lock.lock().await;
        if refresh_upstream {
            self.inner.workflow.invalidate_remote_status(cwd).await;
        }
        let remote = self
            .inner
            .workflow
            .remote_status(
                cwd,
                RemoteStatusOptions {
                    refresh_upstream,
                    refresh_missing_pull_request: false,
                },
            )
            .await?;
        let policy_cwds = if policy_cwds.is_empty() { vec![cwd.to_owned()] } else { policy_cwds };
        if let Some((_, remote)) = self.maybe_auto_pull(cwd, &remote, &policy_cwds).await {
            return Ok(remote);
        }
        Ok(self.update_cached_remote(cwd, remote, true))
    }

    /// `refreshStatus(cwd)`: invalidate everything (including the PR-lookup cache), reload,
    /// maybe auto-pull, publish.
    pub async fn refresh_status(&self, raw_cwd: &str) -> Result<VcsStatusResult, GitManagerServiceError> {
        let cwd = canonicalize_existing_path(raw_cwd).await;
        let lock = self.remote_lock(&cwd);
        let _held = lock.lock().await;
        let workflow = &self.inner.workflow;
        workflow.invalidate_status(&cwd).await;
        let (local, remote) = tokio::try_join!(workflow.local_status(&cwd), workflow.remote_status(&cwd, RemoteStatusOptions::default()))?;
        if let Some((local, remote)) = self.maybe_auto_pull(&cwd, &remote, &[raw_cwd.to_owned()]).await {
            return Ok(VcsStatusResult::merge(local, remote));
        }
        Ok(self.update_cached_status(&cwd, local, remote, true))
    }

    async fn any_scope_work(&self, cwds: &[String]) -> bool {
        let checks = cwds.iter().map(|cwd| async move {
            let scope = BackgroundScope::VcsStatus { cwd: cwd.clone() };
            self.inner.background.should_run_scope_work(&scope).await
        });
        futures::future::join_all(checks).await.into_iter().any(|b| b)
    }

    /// `refreshPullRequestStatus(cwd)`: after a turn, re-read the remote part of a loaded cwd
    /// (no fetch, retrying a missing PR) when background policy allows it.
    pub async fn refresh_pull_request_status(&self, raw_cwd: &str) -> Result<Option<VcsStatusRemoteResult>, GitManagerServiceError> {
        let cwd = canonicalize_existing_path(raw_cwd).await;
        let lock = self.remote_lock(&cwd);
        let _held = lock.lock().await;
        let loaded = self.cached(&cwd).and_then(|c| c.remote).is_some_and(|r| r.value.is_some());
        if !loaded {
            return Ok(None);
        }
        let demand = {
            let pollers = self.inner.pollers.lock().unwrap_or_else(|p| p.into_inner());
            pollers.get(&cwd).map(|p| demand_keys(&p.demand_cwds))
        }
        .unwrap_or_else(|| vec![raw_cwd.to_owned()]);
        if !self.any_scope_work(&demand).await {
            return Ok(None);
        }
        // Resolve the checked-out branch again: a cached PR can belong to the previous branch.
        let remote = self
            .inner
            .workflow
            .remote_status(
                &cwd,
                RemoteStatusOptions {
                    refresh_upstream: false,
                    refresh_missing_pull_request: true,
                },
            )
            .await?;
        Ok(self.update_cached_remote(&cwd, remote, true))
    }

    /// The poller body (`makeRemoteRefreshLoop`).
    async fn remote_refresh_loop(
        weak: std::sync::Weak<Inner>,
        cwd: String,
        demand: Arc<Mutex<Vec<(String, usize)>>>,
        interval: RefreshInterval,
        refresh_immediately: bool,
    ) {
        let mut consecutive_failures: u32 = 0;
        let mut needs_initial = refresh_immediately;
        if !refresh_immediately {
            let configured = interval().await;
            tokio::time::sleep(if configured.is_zero() {
                DEFAULT_VCS_STATUS_REFRESH_INTERVAL
            } else {
                configured
            })
            .await;
        }
        loop {
            let configured = interval().await;
            let active = if configured.is_zero() {
                DEFAULT_VCS_STATUS_REFRESH_INTERVAL
            } else {
                configured
            };
            let delay = 'iteration: {
                if configured.is_zero() && !needs_initial {
                    break 'iteration active;
                }
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let this = VcsStatusBroadcaster { inner };
                let demand_cwds = demand_keys(&demand);
                let should_run = needs_initial || this.any_scope_work(&demand_cwds).await;
                if !should_run {
                    break 'iteration active;
                }
                match this.refresh_remote_status(&cwd, !configured.is_zero(), demand_cwds).await {
                    Ok(_) => {
                        needs_initial = false;
                        consecutive_failures = 0;
                        active
                    }
                    Err(error) => {
                        consecutive_failures += 1;
                        let next = remote_refresh_failure_delay(consecutive_failures, active);
                        // `remoteRefreshFailureDiagnostics`: the failure's tag and operation
                        // only, never its detail, cwd or nested cause.
                        let (failure_tag, failure_operation) = failure_diagnostics(&error);
                        tracing::warn!(
                            cwd_length = cwd.encode_utf16().count(),
                            failure_tag,
                            failure_operation,
                            consecutive_failures,
                            next_delay_ms = next.as_millis() as u64,
                            "VCS remote status refresh failed"
                        );
                        next
                    }
                }
            };
            tokio::time::sleep(delay).await;
        }
    }

    fn retain_remote_poller(&self, cwd: &str, demand_cwd: &str, interval: RefreshInterval, refresh_immediately: bool) {
        let mut pollers = self.inner.pollers.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(existing) = pollers.get_mut(cwd) {
            let mut demand = existing.demand_cwds.lock().unwrap_or_else(|p| p.into_inner());
            match demand.iter_mut().find(|(c, _)| c == demand_cwd) {
                Some(entry) => entry.1 += 1,
                None => demand.push((demand_cwd.to_owned(), 1)),
            }
            drop(demand);
            existing.subscriber_count += 1;
            return;
        }
        let demand = Arc::new(Mutex::new(vec![(demand_cwd.to_owned(), 1)]));
        let task = tokio::spawn(Self::remote_refresh_loop(
            Arc::downgrade(&self.inner),
            cwd.to_owned(),
            demand.clone(),
            interval,
            refresh_immediately,
        ));
        pollers.insert(
            cwd.to_owned(),
            ActivePoller {
                task,
                subscriber_count: 1,
                demand_cwds: demand,
            },
        );
    }

    fn release_remote_poller(inner: &Inner, cwd: &str, demand_cwd: &str) {
        let mut pollers = inner.pollers.lock().unwrap_or_else(|p| p.into_inner());
        let Some(existing) = pollers.get_mut(cwd) else {
            return;
        };
        if existing.subscriber_count > 1 {
            let mut demand = existing.demand_cwds.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(index) = demand.iter().position(|(c, _)| c == demand_cwd) {
                if demand[index].1 <= 1 {
                    demand.remove(index);
                } else {
                    demand[index].1 -= 1;
                }
            }
            drop(demand);
            existing.subscriber_count -= 1;
            return;
        }
        if let Some(poller) = pollers.remove(cwd) {
            poller.task.abort();
        }
    }

    /// Number of live pollers (tests and diagnostics).
    pub fn active_poller_count(&self) -> usize {
        self.inner.pollers.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// `streamStatus(input, {automaticRemoteRefreshInterval})`: a `snapshot` first (local
    /// loaded if needed, remote from cache or `null`), then this cwd's `localUpdated` /
    /// `remoteUpdated` / `snapshot` events. Fails only while setting up. Dropping the stream
    /// releases the poller.
    pub async fn stream_status(&self, input_cwd: &str, interval: Option<RefreshInterval>) -> Result<VcsStatusStream, GitManagerServiceError> {
        let cwd = canonicalize_existing_path(input_cwd).await;
        let subscription = self.inner.changes.subscribe();
        let initial_local = self.get_or_load_local_status(&cwd).await?;
        let cached = self.cached(&cwd).unwrap_or_default();
        let initial_remote = cached.remote.as_ref().and_then(|r| r.value.clone());
        self.retain_remote_poller(
            &cwd,
            input_cwd,
            interval.unwrap_or_else(|| fixed_interval(DEFAULT_VCS_STATUS_REFRESH_INTERVAL)),
            cached.remote.is_none(),
        );
        let filter_cwd = cwd.clone();
        let events = futures::stream::once(async move {
            VcsStatusStreamEvent::Snapshot {
                local: initial_local,
                remote: initial_remote,
            }
        })
        .chain(subscription.filter_map(move |change| {
            let keep = change.cwd == filter_cwd;
            async move { keep.then_some(change.event) }
        }));
        Ok(VcsStatusStream {
            events: Box::pin(events),
            release: Some(ReleaseGuard {
                inner: Arc::downgrade(&self.inner),
                cwd,
                demand_cwd: input_cwd.to_owned(),
            }),
        })
    }
}

struct ReleaseGuard {
    inner: std::sync::Weak<Inner>,
    cwd: String,
    demand_cwd: String,
}

impl Drop for ReleaseGuard {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            VcsStatusBroadcaster::release_remote_poller(&inner, &self.cwd, &self.demand_cwd);
        }
    }
}

/// The stream of [`VcsStatusBroadcaster::stream_status`].
pub struct VcsStatusStream {
    events: Pin<Box<dyn Stream<Item = VcsStatusStreamEvent> + Send>>,
    release: Option<ReleaseGuard>,
}

impl Stream for VcsStatusStream {
    type Item = VcsStatusStreamEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        let polled = self.events.as_mut().poll_next(cx);
        if let std::task::Poll::Ready(None) = polled {
            self.release.take();
        }
        polled
    }
}

#[async_trait]
impl zc_ports::VcsStatusRefresher for VcsStatusBroadcaster {
    async fn refresh_local_status(&self, cwd: &str) -> Result<zc_ports::contracts::VcsStatusLocalResult, zc_ports::TaggedError> {
        use crate::errors::IntoTagged;
        VcsStatusBroadcaster::refresh_local_status(self, cwd)
            .await
            .map(|r| zc_ports::contracts::VcsStatusLocalResult(serde_json::to_value(r).unwrap_or_default()))
            .map_err(IntoTagged::into_tagged)
    }

    async fn refresh_status(&self, cwd: &str) -> Result<zc_ports::contracts::VcsStatusResult, zc_ports::TaggedError> {
        use crate::errors::IntoTagged;
        VcsStatusBroadcaster::refresh_status(self, cwd)
            .await
            .map(|r| zc_ports::contracts::VcsStatusResult(serde_json::to_value(r).unwrap_or_default()))
            .map_err(IntoTagged::into_tagged)
    }

    async fn refresh_pull_request_status(&self, cwd: &str) -> Result<Option<zc_ports::contracts::VcsStatusRemoteResult>, zc_ports::TaggedError> {
        use crate::errors::IntoTagged;
        VcsStatusBroadcaster::refresh_pull_request_status(self, cwd)
            .await
            .map(|r| r.map(|r| zc_ports::contracts::VcsStatusRemoteResult(serde_json::to_value(r).unwrap_or_default())))
            .map_err(IntoTagged::into_tagged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backs_off_exponentially_and_honors_larger_intervals() {
        let thirty = Duration::from_secs(30);
        assert_eq!(remote_refresh_failure_delay(1, thirty), Duration::from_secs(30));
        assert_eq!(remote_refresh_failure_delay(2, thirty), Duration::from_secs(60));
        assert_eq!(remote_refresh_failure_delay(3, thirty), Duration::from_secs(120));
        assert_eq!(remote_refresh_failure_delay(10, thirty), Duration::from_secs(900));
        assert_eq!(remote_refresh_failure_delay(1, Duration::from_secs(300)), Duration::from_secs(300));
    }

    #[test]
    fn auto_pull_prefers_the_project_override() {
        let settings = serde_json::json!({
            "defaultAutoPull": false,
            "projectSettingsOverrides": {"p1": {"defaultAutoPull": true}}
        });
        assert!(resolve_default_auto_pull(&settings, "p1"));
        assert!(!resolve_default_auto_pull(&settings, "p2"));
    }
}
