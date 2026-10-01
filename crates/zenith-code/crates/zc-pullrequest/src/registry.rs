//! `PullRequestProviderRegistry.ts`: the hosts this build can read change requests from. A host
//! with no entry still shows up in the provider list as unimplemented.

use zc_contracts::SourceControlProviderKind;

use crate::provider::SharedProvider;

/// `PullRequestProviderRegistry`.
#[derive(Clone, Default)]
pub struct PullRequestProviderRegistry {
    providers: Vec<SharedProvider>,
}

impl PullRequestProviderRegistry {
    /// `fromProviders(providers)`.
    pub fn from_providers(providers: Vec<SharedProvider>) -> Self {
        Self { providers }
    }

    /// `get(kind)`: `None` for a host with no implementation (reported as unsupported).
    pub fn get(&self, kind: SourceControlProviderKind) -> Option<SharedProvider> {
        self.providers.iter().find(|provider| provider.kind() == kind).cloned()
    }

    /// `kinds`.
    pub fn kinds(&self) -> Vec<SourceControlProviderKind> {
        self.providers.iter().map(|provider| provider.kind()).collect()
    }
}

impl std::fmt::Debug for PullRequestProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PullRequestProviderRegistry").field("kinds", &self.kinds()).finish()
    }
}
