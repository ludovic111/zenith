//! `PullRequestProviderRegistry.make`: the hosts this build can read change requests from, built
//! over the forge clients zc-sourcecontrol already built (so the GitHub GraphQL budget, the
//! rate-limit pauses and the CLI concurrency limits are shared with the rest of the server).

use std::sync::Arc;

use zc_sourcecontrol::util::SharedClock;
use zc_sourcecontrol::SourceControl;

use crate::azure::AzureDevOpsPullRequestProvider;
use crate::bitbucket::BitbucketPullRequestProvider;
use crate::forgejo::ForgejoPullRequestProvider;
use crate::github::cli::GitHubPullRequestCli;
use crate::github::provider::GitHubPullRequestProvider;
use crate::gitlab::GitLabPullRequestProvider;
use crate::registry::PullRequestProviderRegistry;

/// Every provider, in the TS order (GitHub, GitLab, Forgejo, Bitbucket, Azure DevOps).
pub fn provider_registry(source_control: &SourceControl, clock: SharedClock) -> PullRequestProviderRegistry {
    PullRequestProviderRegistry::from_providers(vec![
        Arc::new(GitHubPullRequestProvider::new(GitHubPullRequestCli::new(
            source_control.github.clone(),
            clock.clone(),
        ))),
        Arc::new(GitLabPullRequestProvider::new(source_control.gitlab.clone())),
        Arc::new(ForgejoPullRequestProvider::new(source_control.forgejo.clone())),
        Arc::new(BitbucketPullRequestProvider::new(source_control.bitbucket.clone(), clock)),
        Arc::new(AzureDevOpsPullRequestProvider::new(source_control.azure.clone())),
    ])
}
