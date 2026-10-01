//! The hook zc-vcs's status leaves for this crate: [`zc_vcs::PullRequestStatusSource::resolve_unknown_provider`]
//! (GitManager's `resolveHostingProvider` asking `sourceControlProviders.resolveHandle` about a
//! remote whose URL named no forge).
//!
//! [`WithSourceControlProviders`] wraps any status source (for instance
//! [`zc_vcs::status::NoPullRequests`]) and answers that question from the registry; every
//! other method is delegated. The server's GitManager (zc-git) answers it itself.

use std::sync::Arc;

use async_trait::async_trait;
use zc_vcs::contracts::{SourceControlProviderInfo as VcsProviderInfo, VcsStatusChangeRequest};
use zc_vcs::driver_core::GitRemoteStatusDetails;
use zc_vcs::errors::GitManagerServiceError;
use zc_vcs::PullRequestStatusSource;

use crate::registry::{from_vcs_info, to_vcs_info, SourceControlProviderRegistry};

/// A [`PullRequestStatusSource`] whose unknown remotes are resolved by the registry.
pub struct WithSourceControlProviders<S> {
    inner: S,
    registry: SourceControlProviderRegistry,
}

impl<S> WithSourceControlProviders<S> {
    pub fn new(inner: S, registry: SourceControlProviderRegistry) -> Self {
        Self { inner, registry }
    }
}

/// Wraps a shared status source.
pub fn with_source_control_providers(inner: Arc<dyn PullRequestStatusSource>, registry: SourceControlProviderRegistry) -> Arc<dyn PullRequestStatusSource> {
    Arc::new(WithSourceControlProviders::new(inner, registry))
}

#[async_trait]
impl<S> PullRequestStatusSource for WithSourceControlProviders<S>
where
    S: std::ops::Deref + Send + Sync,
    S::Target: PullRequestStatusSource,
{
    async fn lookup_status_pr(
        &self,
        cwd: &str,
        details: &GitRemoteStatusDetails,
        refresh_missing_pull_request: bool,
    ) -> Result<Option<VcsStatusChangeRequest>, GitManagerServiceError> {
        self.inner.lookup_status_pr(cwd, details, refresh_missing_pull_request).await
    }

    async fn invalidate(&self, cwd: &str) {
        self.inner.invalidate(cwd).await;
    }

    async fn resolve_unknown_provider(&self, cwd: &str, remote_name: &str, remote_url: &str, provider: &VcsProviderInfo) -> Option<VcsProviderInfo> {
        self.registry
            .resolve_unknown_remote_provider(cwd, remote_name, remote_url, from_vcs_info(provider.clone()))
            .await
            .map(|info| to_vcs_info(&info))
    }
}
