//! What GitManager asks the source control layer (`SourceControlProviderRegistry.resolve` and
//! `resolveHandle`), as a trait so the tests can pin a provider like the TS tests do, and the
//! conversions of its errors into `GitManagerServiceError`.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value};
use zc_ports::TaggedError;
use zc_sourcecontrol::registry::{from_vcs_info, to_vcs_info};
use zc_sourcecontrol::{SourceControlProvider, SourceControlProviderError, SourceControlProviderRegistry};
use zc_vcs::contracts::SourceControlProviderInfo;
use zc_vcs::GitManagerServiceError;

/// `sourceControlProviders.resolve` / `resolveHandle` as GitManager uses them.
#[async_trait]
pub trait SourceControlProviders: Send + Sync {
    /// `resolve({cwd})`: the provider of the checkout's remotes.
    async fn resolve(&self, cwd: &str) -> Result<Arc<dyn SourceControlProvider>, SourceControlProviderError>;

    /// `resolveHandle({cwd, context: {provider, remoteName, remoteUrl}})` → its provider info,
    /// for a remote whose URL named no forge. `None` keeps the URL-derived info.
    async fn resolve_unknown_provider(
        &self,
        cwd: &str,
        remote_name: &str,
        remote_url: &str,
        provider: &SourceControlProviderInfo,
    ) -> Option<SourceControlProviderInfo>;
}

#[async_trait]
impl SourceControlProviders for SourceControlProviderRegistry {
    async fn resolve(&self, cwd: &str) -> Result<Arc<dyn SourceControlProvider>, SourceControlProviderError> {
        SourceControlProviderRegistry::resolve(self, cwd).await
    }

    async fn resolve_unknown_provider(
        &self,
        cwd: &str,
        remote_name: &str,
        remote_url: &str,
        provider: &SourceControlProviderInfo,
    ) -> Option<SourceControlProviderInfo> {
        self.resolve_unknown_remote_provider(cwd, remote_name, remote_url, from_vcs_info(provider.clone()))
            .await
            .map(|info| to_vcs_info(&info))
    }
}

/// One provider for every checkout (the TS tests' registry mock: `resolve` and `resolveHandle`
/// both answer it, with no context).
pub struct FixedProvider(pub Arc<dyn SourceControlProvider>);

#[async_trait]
impl SourceControlProviders for FixedProvider {
    async fn resolve(&self, _cwd: &str) -> Result<Arc<dyn SourceControlProvider>, SourceControlProviderError> {
        Ok(self.0.clone())
    }

    async fn resolve_unknown_provider(&self, _: &str, _: &str, _: &str, _: &SourceControlProviderInfo) -> Option<SourceControlProviderInfo> {
        None
    }
}

/// A `TaggedError` carrying a wire-encoded error object and its message.
pub fn tagged_from_value(value: Value, message: String) -> TaggedError {
    let mut fields = match value {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    let tag = fields.remove("_tag").and_then(|tag| tag.as_str().map(str::to_owned)).unwrap_or_default();
    TaggedError { tag, fields, message }
}

/// A `SourceControlProviderError` as a `GitManagerServiceError` member.
pub fn provider_error(error: SourceControlProviderError) -> GitManagerServiceError {
    let message = error.message();
    GitManagerServiceError::Other(tagged_from_value(serde_json::to_value(&error).unwrap_or(Value::Null), message))
}

/// The provider kind as its wire string.
pub fn kind_str(provider: &dyn SourceControlProvider) -> &'static str {
    provider.kind().as_str()
}
