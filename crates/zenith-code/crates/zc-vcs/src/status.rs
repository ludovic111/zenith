//! The status slice of `git/GitManager.ts`: `localStatus`, `remoteStatus`, `status` and the
//! `invalidate*` methods, with their 1 s result caches keyed by the canonical cwd.
//!
//! GitManager itself (stacked actions, PR resolution and preparation, the PR-lookup caches)
//! is WP-19. The two things it adds to status come through [`PullRequestStatusSource`]:
//! the `pr` of the remote status (`lookupStatusPr`) and the forge of a remote whose URL alone
//! does not say (`sourceControlProviders.resolveHandle`). [`NoPullRequests`] reports none.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::cache::OutcomeCache;
use crate::contracts::*;
use crate::driver_core::{GitRemoteStatusDetails, GitStatusDetails, GitVcsDriver};
use crate::errors::{GitCommandError, GitManagerServiceError};
use crate::shared_git::detect_source_control_provider_from_remote_url;

const STATUS_RESULT_CACHE_TTL: Duration = Duration::from_secs(1);
const STATUS_RESULT_CACHE_CAPACITY: usize = 2_048;

/// `GitRemoteStatusOptions` with the TS defaults (`refreshUpstream` unless set to false).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteStatusOptions {
    /// Fetch the upstream (rate-limited) before counting. TS: `refreshUpstream !== false`.
    pub refresh_upstream: bool,
    /// Retry a cached missing PR without clearing known PRs or failed-lookup backoff.
    pub refresh_missing_pull_request: bool,
}

impl Default for RemoteStatusOptions {
    fn default() -> Self {
        Self {
            refresh_upstream: true,
            refresh_missing_pull_request: false,
        }
    }
}

impl RemoteStatusOptions {
    /// Whether this read bypasses the 1 s result cache (`refreshUpstream === false ||
    /// refreshMissingPullRequest`).
    fn uncached(self) -> bool {
        !self.refresh_upstream || self.refresh_missing_pull_request
    }
}

/// What GitManager (WP-19) plugs into status.
#[async_trait]
pub trait PullRequestStatusSource: Send + Sync {
    /// `lookupStatusPr(cwd, {branch, upstreamRef, defaultBranch, isDefaultBranch},
    /// refreshMissingPullRequest)`: the change request of the checked-out branch.
    async fn lookup_status_pr(
        &self,
        cwd: &str,
        details: &GitRemoteStatusDetails,
        refresh_missing_pull_request: bool,
    ) -> Result<Option<VcsStatusChangeRequest>, GitManagerServiceError>;

    /// `bumpPrLookupEpoch(cwd)`: an explicit refresh bypasses the PR-lookup cache.
    async fn invalidate(&self, _cwd: &str) {}

    /// The forge behind a remote whose URL is not recognized (`kind: "unknown"`), from the
    /// source-control providers (WP-20). `None` keeps the URL-derived info.
    async fn resolve_unknown_provider(
        &self,
        _cwd: &str,
        _remote_name: &str,
        _remote_url: &str,
        _provider: &SourceControlProviderInfo,
    ) -> Option<SourceControlProviderInfo> {
        None
    }
}

/// The placeholder [`PullRequestStatusSource`]: no PRs, URL-only forge detection.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPullRequests;

#[async_trait]
impl PullRequestStatusSource for NoPullRequests {
    async fn lookup_status_pr(
        &self,
        _cwd: &str,
        _details: &GitRemoteStatusDetails,
        _refresh_missing_pull_request: bool,
    ) -> Result<Option<VcsStatusChangeRequest>, GitManagerServiceError> {
        Ok(None)
    }
}

/// `isNotGitRepositoryError`: TS matches the error *message*.
fn is_not_git_repository_error(error: &GitCommandError) -> bool {
    error.message().to_lowercase().contains("not a git repository")
}

/// `canonicalizeExistingPath`: the real path, or the input when it does not exist.
pub async fn canonicalize_existing_path(path: &str) -> String {
    tokio::fs::canonicalize(path)
        .await
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_owned())
}

/// The status half of GitManager.
#[derive(Clone)]
pub struct GitStatusService {
    git: GitVcsDriver,
    pull_requests: Arc<dyn PullRequestStatusSource>,
    local_cache: OutcomeCache<String, VcsStatusLocalResult, GitManagerServiceError>,
    remote_cache: OutcomeCache<String, Option<VcsStatusRemoteResult>, GitManagerServiceError>,
}

impl GitStatusService {
    pub fn new(git: GitVcsDriver, pull_requests: Arc<dyn PullRequestStatusSource>) -> Self {
        fn ttl<V, E>(result: &Result<V, E>) -> Duration {
            if result.is_ok() {
                STATUS_RESULT_CACHE_TTL
            } else {
                Duration::ZERO
            }
        }
        Self {
            git,
            pull_requests,
            local_cache: OutcomeCache::new(STATUS_RESULT_CACHE_CAPACITY, |r, _| ttl(r)),
            remote_cache: OutcomeCache::new(STATUS_RESULT_CACHE_CAPACITY, |r, _| ttl(r)),
        }
    }

    pub fn driver(&self) -> &GitVcsDriver {
        &self.git
    }

    async fn read_config_nullable(&self, cwd: &str, key: &str) -> Option<String> {
        self.git.read_config_value(cwd, key).await.ok().flatten()
    }

    /// `resolveHostingProvider(cwd, branch)`.
    async fn resolve_hosting_provider(&self, cwd: &str, branch: Option<&str>) -> Option<SourceControlProviderInfo> {
        let preferred_remote = match branch {
            None => "origin".to_owned(),
            Some(branch) => self
                .read_config_nullable(cwd, &format!("branch.{branch}.remote"))
                .await
                .unwrap_or_else(|| "origin".into()),
        };
        let remote_url = match self.read_config_nullable(cwd, &format!("remote.{preferred_remote}.url")).await {
            Some(url) => Some(url),
            None => self.read_config_nullable(cwd, "remote.origin.url").await,
        };
        let remote_url = remote_url?;
        let provider = detect_source_control_provider_from_remote_url(&remote_url)?;
        if provider.kind != SourceControlProviderKind::Unknown {
            return Some(provider);
        }
        Some(
            self.pull_requests
                .resolve_unknown_provider(cwd, &preferred_remote, &remote_url, &provider)
                .await
                .unwrap_or(provider),
        )
    }

    async fn read_local_status(&self, cwd: &str) -> Result<VcsStatusLocalResult, GitManagerServiceError> {
        let details = match self.git.status_details_local(cwd).await {
            Ok(details) => details,
            Err(error) if is_not_git_repository_error(&error) => GitStatusDetails::non_repository(),
            Err(error) => return Err(error.into()),
        };
        let provider = if details.is_repo {
            self.resolve_hosting_provider(cwd, details.branch.as_deref()).await
        } else {
            None
        };
        Ok(VcsStatusLocalResult {
            is_repo: details.is_repo,
            source_control_provider: provider,
            has_primary_remote: details.has_origin_remote,
            is_default_ref: details.is_default_branch,
            ref_name: details.branch,
            has_working_tree_changes: details.has_working_tree_changes,
            working_tree: details.working_tree,
        })
    }

    async fn read_remote_status(&self, cwd: &str, options: RemoteStatusOptions) -> Result<Option<VcsStatusRemoteResult>, GitManagerServiceError> {
        let details = match self.git.status_details_remote(cwd, options.refresh_upstream).await {
            Ok(details) => details,
            Err(error) if is_not_git_repository_error(&error) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !details.is_repo {
            return Ok(None);
        }
        let pr = if details.branch.is_some() {
            self.pull_requests.lookup_status_pr(cwd, &details, options.refresh_missing_pull_request).await?
        } else {
            None
        };
        Ok(Some(VcsStatusRemoteResult {
            has_upstream: details.has_upstream,
            ahead_count: details.ahead_count,
            behind_count: details.behind_count,
            ahead_of_default_count: Some(details.ahead_of_default_count),
            pr,
        }))
    }

    /// `localStatus({cwd})`.
    pub async fn local_status(&self, cwd: &str) -> Result<VcsStatusLocalResult, GitManagerServiceError> {
        let key = canonicalize_existing_path(cwd).await;
        let this = self.clone();
        let read_key = key.clone();
        self.local_cache.get(key, move || async move { this.read_local_status(&read_key).await }).await
    }

    /// `remoteStatus({cwd}, options?)`.
    pub async fn remote_status(&self, cwd: &str, options: RemoteStatusOptions) -> Result<Option<VcsStatusRemoteResult>, GitManagerServiceError> {
        let key = canonicalize_existing_path(cwd).await;
        if options.uncached() {
            return self.read_remote_status(&key, options).await;
        }
        let this = self.clone();
        let read_key = key.clone();
        self.remote_cache
            .get(
                key,
                move || async move { this.read_remote_status(&read_key, RemoteStatusOptions::default()).await },
            )
            .await
    }

    /// `status({cwd})`.
    pub async fn status(&self, cwd: &str) -> Result<VcsStatusResult, GitManagerServiceError> {
        let (local, remote) = tokio::try_join!(self.local_status(cwd), self.remote_status(cwd, RemoteStatusOptions::default()))?;
        Ok(VcsStatusResult::merge(local, remote))
    }

    pub async fn invalidate_local_status(&self, cwd: &str) {
        let key = canonicalize_existing_path(cwd).await;
        self.local_cache.invalidate(&key);
    }

    pub async fn invalidate_remote_status(&self, cwd: &str) {
        let key = canonicalize_existing_path(cwd).await;
        self.remote_cache.invalidate(&key);
    }

    /// `invalidateStatus(cwd)`: both caches, and the PR-lookup cache (explicit freshness).
    pub async fn invalidate_status(&self, cwd: &str) {
        let key = canonicalize_existing_path(cwd).await;
        self.local_cache.invalidate(&key);
        self.remote_cache.invalidate(&key);
        self.pull_requests.invalidate(&key).await;
    }
}
