//! `SourceControlProviderRegistry.ts`: picks the provider of a checkout from its remotes
//! (`origin` first, then the first recognized forge, then any remote), asks the providers about
//! remotes whose URL names no forge, and routes link lookups by URL.
//!
//! Detected contexts are cached 5 s per cwd (successes only).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use zc_contracts::{ChangeRequest, SourceControlProviderDiscoveryItem, SourceControlProviderInfo, SourceControlProviderKind, SourceControlRepositoryCloneUrls};
use zc_core::vcs_process::VcsProcess;
use zc_vcs::VcsDriverRegistry;

use crate::cache::ClockedCache;
use crate::discovery::{probe_source_control_provider, refine_unknown_remote_provider, DiscoverySpec};
use crate::errors::{Cause, SourceControlProviderError};
use crate::provider::*;
use crate::util::{parse_url, SharedClock};

const PROVIDER_DETECTION_CACHE_CAPACITY: usize = 2_048;
const PROVIDER_DETECTION_CACHE_TTL_MS: i64 = 5_000;

/// `SourceControlProviderRegistration`.
#[derive(Clone)]
pub struct SourceControlProviderRegistration {
    pub kind: SourceControlProviderKind,
    pub provider: Arc<dyn SourceControlProvider>,
    pub discovery: DiscoverySpec,
}

/// `SourceControlProviderHandle`: a provider bound to the detected context.
#[derive(Clone)]
pub struct SourceControlProviderHandle {
    pub provider: Arc<dyn SourceControlProvider>,
    pub context: Option<SourceControlProviderContext>,
}

/// zc-contracts' provider info from zc-vcs's (`detectSourceControlProviderFromRemoteUrl`).
pub fn detect_provider_from_remote_url(remote_url: &str) -> Option<SourceControlProviderInfo> {
    zc_vcs::shared_git::detect_source_control_provider_from_remote_url(remote_url).map(from_vcs_info)
}

/// Converts zc-vcs's `SourceControlProviderInfo` (its pre-zc-contracts copy).
pub fn from_vcs_info(info: zc_vcs::contracts::SourceControlProviderInfo) -> SourceControlProviderInfo {
    use zc_vcs::contracts::SourceControlProviderKind as V;
    SourceControlProviderInfo {
        kind: match info.kind {
            V::Github => SourceControlProviderKind::Github,
            V::Gitlab => SourceControlProviderKind::Gitlab,
            V::Forgejo => SourceControlProviderKind::Forgejo,
            V::AzureDevops => SourceControlProviderKind::AzureDevops,
            V::Bitbucket => SourceControlProviderKind::Bitbucket,
            V::Unknown => SourceControlProviderKind::Unknown,
        },
        name: info.name,
        base_url: info.base_url,
    }
}

/// And back.
pub fn to_vcs_info(info: &SourceControlProviderInfo) -> zc_vcs::contracts::SourceControlProviderInfo {
    use zc_vcs::contracts::SourceControlProviderKind as V;
    zc_vcs::contracts::SourceControlProviderInfo {
        kind: match info.kind {
            SourceControlProviderKind::Github => V::Github,
            SourceControlProviderKind::Gitlab => V::Gitlab,
            SourceControlProviderKind::Forgejo => V::Forgejo,
            SourceControlProviderKind::AzureDevops => V::AzureDevops,
            SourceControlProviderKind::Bitbucket => V::Bitbucket,
            SourceControlProviderKind::Unknown => V::Unknown,
        },
        name: info.name.clone(),
        base_url: info.base_url.clone(),
    }
}

/// `selectProviderContext`.
pub fn select_provider_context(remotes: &[(String, String)]) -> Option<SourceControlProviderContext> {
    let candidates: Vec<SourceControlProviderContext> = remotes
        .iter()
        .filter_map(|(name, url)| {
            detect_provider_from_remote_url(url).map(|provider| SourceControlProviderContext {
                provider,
                remote_name: name.clone(),
                remote_url: url.clone(),
                requested_host: None,
            })
        })
        .collect();
    candidates
        .iter()
        .find(|c| c.remote_name == "origin")
        .or_else(|| candidates.iter().find(|c| c.provider.kind != SourceControlProviderKind::Unknown))
        .or_else(|| candidates.first())
        .cloned()
}

/// `unsupportedProvider(kind)`: fails every operation, naming the request.
pub struct UnsupportedProvider(pub SourceControlProviderKind);

impl UnsupportedProvider {
    fn error(&self, operation: &str, cwd: &str) -> SourceControlProviderError {
        SourceControlProviderError::new(self.0, operation, cwd, format!("No {} source control provider is registered.", self.0.as_str()))
    }
}

#[async_trait]
impl SourceControlProvider for UnsupportedProvider {
    fn kind(&self) -> SourceControlProviderKind {
        self.0
    }
    async fn list_change_requests(&self, input: ListChangeRequestsInput) -> Result<Vec<ChangeRequest>, SourceControlProviderError> {
        Err(self.error("listChangeRequests", &input.cwd))
    }
    async fn get_change_request(&self, input: GetChangeRequestInput) -> Result<ChangeRequest, SourceControlProviderError> {
        Err(self
            .error("getChangeRequest", &input.cwd)
            .with_reference(transport_safe_source_control_error_value(&input.reference)))
    }
    async fn create_change_request(&self, input: CreateChangeRequestInput) -> Result<(), SourceControlProviderError> {
        Err(self
            .error("createChangeRequest", &input.cwd)
            .with_reference(transport_safe_source_control_error_value(&input.head_selector)))
    }
    async fn get_repository_clone_urls(&self, input: RepositoryCloneUrlsInput) -> Result<SourceControlRepositoryCloneUrls, SourceControlProviderError> {
        Err(self
            .error("getRepositoryCloneUrls", &input.cwd)
            .with_repository(transport_safe_source_control_error_value(&input.repository)))
    }
    async fn create_repository(&self, input: CreateRepositoryInput) -> Result<SourceControlRepositoryCloneUrls, SourceControlProviderError> {
        Err(self
            .error("createRepository", &input.cwd)
            .with_repository(transport_safe_source_control_error_value(&input.repository)))
    }
    async fn get_default_branch(&self, input: DefaultBranchInput) -> Result<Option<String>, SourceControlProviderError> {
        Err(self.error("getDefaultBranch", &input.cwd))
    }
    async fn checkout_change_request(&self, input: CheckoutChangeRequestInput) -> Result<(), SourceControlProviderError> {
        Err(self
            .error("checkoutChangeRequest", &input.cwd)
            .with_reference(transport_safe_source_control_error_value(&input.reference)))
    }
}

/// `bindProviderContext`: fills in the detected context where the caller gave none.
pub struct BoundProvider {
    inner: Arc<dyn SourceControlProvider>,
    context: SourceControlProviderContext,
}

impl BoundProvider {
    fn context(&self, given: Option<SourceControlProviderContext>) -> Option<SourceControlProviderContext> {
        given.or_else(|| Some(self.context.clone()))
    }
}

#[async_trait]
impl SourceControlProvider for BoundProvider {
    fn kind(&self) -> SourceControlProviderKind {
        self.inner.kind()
    }
    fn resolve_link(&self, cwd: &str, url: &url::Url) -> Option<LinkLookup> {
        self.inner.resolve_link(cwd, url)
    }
    async fn list_change_requests(&self, mut input: ListChangeRequestsInput) -> Result<Vec<ChangeRequest>, SourceControlProviderError> {
        input.context = self.context(input.context);
        self.inner.list_change_requests(input).await
    }
    async fn get_change_request(&self, mut input: GetChangeRequestInput) -> Result<ChangeRequest, SourceControlProviderError> {
        input.context = self.context(input.context);
        self.inner.get_change_request(input).await
    }
    async fn create_change_request(&self, mut input: CreateChangeRequestInput) -> Result<(), SourceControlProviderError> {
        input.context = self.context(input.context);
        self.inner.create_change_request(input).await
    }
    async fn get_repository_clone_urls(&self, mut input: RepositoryCloneUrlsInput) -> Result<SourceControlRepositoryCloneUrls, SourceControlProviderError> {
        input.context = self.context(input.context);
        self.inner.get_repository_clone_urls(input).await
    }
    async fn create_repository(&self, input: CreateRepositoryInput) -> Result<SourceControlRepositoryCloneUrls, SourceControlProviderError> {
        self.inner.create_repository(input).await
    }
    async fn get_default_branch(&self, mut input: DefaultBranchInput) -> Result<Option<String>, SourceControlProviderError> {
        input.context = self.context(input.context);
        self.inner.get_default_branch(input).await
    }
    async fn checkout_change_request(&self, mut input: CheckoutChangeRequestInput) -> Result<(), SourceControlProviderError> {
        input.context = self.context(input.context);
        self.inner.checkout_change_request(input).await
    }
}

struct Inner {
    cwd: String,
    process: VcsProcess,
    vcs: VcsDriverRegistry,
    providers: HashMap<SourceControlProviderKind, Arc<dyn SourceControlProvider>>,
    specs: Vec<DiscoverySpec>,
    contexts: ClockedCache<String, Option<SourceControlProviderContext>, SourceControlProviderError>,
}

/// The `SourceControlProviderRegistry` service.
#[derive(Clone)]
pub struct SourceControlProviderRegistry {
    inner: Arc<Inner>,
}

impl SourceControlProviderRegistry {
    /// `makeWithProviders(registrations)`. `cwd` is the server's working directory (discovery
    /// probes run there).
    pub fn new(
        registrations: Vec<SourceControlProviderRegistration>,
        process: VcsProcess,
        vcs: VcsDriverRegistry,
        cwd: impl Into<String>,
        clock: SharedClock,
    ) -> Self {
        let providers = registrations.iter().map(|r| (r.kind, r.provider.clone())).collect();
        let specs = registrations.into_iter().map(|r| r.discovery).collect();
        Self {
            inner: Arc::new(Inner {
                cwd: cwd.into(),
                process,
                vcs,
                providers,
                specs,
                contexts: ClockedCache::new(PROVIDER_DETECTION_CACHE_CAPACITY, clock, |result: &Result<_, _>| {
                    if result.is_ok() {
                        PROVIDER_DETECTION_CACHE_TTL_MS
                    } else {
                        0
                    }
                }),
            }),
        }
    }

    /// `get(kind)`: the registered provider, or one that fails every operation.
    pub fn get(&self, kind: SourceControlProviderKind) -> Arc<dyn SourceControlProvider> {
        self.inner.providers.get(&kind).cloned().unwrap_or_else(|| Arc::new(UnsupportedProvider(kind)))
    }

    async fn detect_provider_context(inner: &Inner, cwd: String) -> Result<Option<SourceControlProviderContext>, SourceControlProviderError> {
        let detection_error = |cause: Cause| {
            SourceControlProviderError::new(
                SourceControlProviderKind::Unknown,
                "detectProvider",
                &cwd,
                "Failed to detect source control provider.",
            )
            .with_cause(cause)
        };
        let handle = inner.vcs.resolve(&cwd, None).await.map_err(|e| detection_error(Cause::new(e)))?;
        let remotes = handle.driver.list_remotes(&cwd).await.map_err(|e| detection_error(Cause::new(e)))?;
        let pairs: Vec<(String, String)> = remotes.remotes.into_iter().map(|r| (r.name, r.url)).collect();
        let context = select_provider_context(&pairs);
        Ok(refine_unknown_remote_provider(&inner.specs, &inner.process, &cwd, context).await)
    }

    /// `resolveHandle({cwd, context?})`.
    pub async fn resolve_handle(
        &self,
        cwd: &str,
        context: Option<SourceControlProviderContext>,
    ) -> Result<SourceControlProviderHandle, SourceControlProviderError> {
        let context = match context {
            None => {
                let inner = self.inner.clone();
                let key = cwd.to_owned();
                self.inner
                    .contexts
                    .get(cwd.to_owned(), move || async move { Self::detect_provider_context(&inner, key).await })
                    .await?
            }
            Some(context) => refine_unknown_remote_provider(&self.inner.specs, &self.inner.process, cwd, Some(context)).await,
        };
        let kind = context.as_ref().map_or(SourceControlProviderKind::Unknown, |c| c.provider.kind);
        let provider = self.get(kind);
        let provider: Arc<dyn SourceControlProvider> = match &context {
            Some(context) => Arc::new(BoundProvider {
                inner: provider,
                context: context.clone(),
            }),
            None => provider,
        };
        Ok(SourceControlProviderHandle { provider, context })
    }

    /// `resolve({cwd})`.
    pub async fn resolve(&self, cwd: &str) -> Result<Arc<dyn SourceControlProvider>, SourceControlProviderError> {
        Ok(self.resolve_handle(cwd, None).await?.provider)
    }

    /// `resolveLink({cwd, url})`: `None` for anything but a credential-free https URL on a
    /// forge whose provider reads links.
    pub fn resolve_link(&self, cwd: &str, url: &url::Url) -> Option<LinkLookup> {
        if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
            return None;
        }
        let kind = detect_provider_from_remote_url(url.as_str())?.kind;
        self.inner.providers.get(&kind)?.resolve_link(cwd, url)
    }

    /// `resolveLink` from a URL string.
    pub fn resolve_link_str(&self, cwd: &str, url: &str) -> Option<LinkLookup> {
        self.resolve_link(cwd, &parse_url(url)?)
    }

    /// `discover`: every provider, probed concurrently, in registration order.
    pub async fn discover(&self) -> Vec<SourceControlProviderDiscoveryItem> {
        futures::future::join_all(
            self.inner
                .specs
                .iter()
                .map(|spec| probe_source_control_provider(spec, &self.inner.process, &self.inner.cwd)),
        )
        .await
    }

    /// The forge of a remote whose URL named none (`kind: "unknown"`), as GitManager's
    /// `resolveHostingProvider` asks it: `None` keeps the URL-derived info.
    pub async fn resolve_unknown_remote_provider(
        &self,
        cwd: &str,
        remote_name: &str,
        remote_url: &str,
        provider: SourceControlProviderInfo,
    ) -> Option<SourceControlProviderInfo> {
        let handle = self
            .resolve_handle(
                cwd,
                Some(SourceControlProviderContext {
                    provider,
                    remote_name: remote_name.to_owned(),
                    remote_url: remote_url.to_owned(),
                    requested_host: None,
                }),
            )
            .await
            .ok()?;
        handle.context.map(|context| context.provider)
    }
}
