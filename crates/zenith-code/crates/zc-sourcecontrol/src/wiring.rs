//! `SourceControlProviderRegistry.make` and the layers around it: every service of this crate
//! built once, the way `server.ts` composes them.

use std::sync::Arc;

use zc_core::vcs_process::VcsProcess;
use zc_vcs::{GitVcsDriver, VcsDriverRegistry};

use crate::azure::{AzureDevOpsCli, AzureDevOpsSourceControlProvider};
use crate::bitbucket::{BitbucketApi, BitbucketApiConfig, BitbucketCredentialSource, BitbucketSourceControlProvider};
use crate::discovery::DiscoverySpec;
use crate::forgejo::{ForgejoCli, ForgejoDiscovery, ForgejoEnvironment, ForgejoSourceControlProvider};
use crate::github::{GitHubCli, GitHubSourceControlProvider};
use crate::gitlab::{GitLabCli, GitLabSourceControlProvider};
use crate::graphql_budget::GitHubGraphQlBudget;
use crate::rate_limit::SourceControlRateLimit;
use crate::registry::{SourceControlProviderRegistration, SourceControlProviderRegistry};
use crate::repository::SourceControlRepositoryService;
use crate::rpc::{AfterPublish, SourceControlRpcServices};
use crate::source_control_discovery::SourceControlDiscovery;
use crate::util::SharedClock;

/// What the source control layer is built from.
#[derive(Clone)]
pub struct SourceControlDeps {
    /// The server's working directory (`ServerConfig.cwd`).
    pub cwd: String,
    pub process: VcsProcess,
    pub git: GitVcsDriver,
    pub vcs: VcsDriverRegistry,
    pub bitbucket_credentials: Arc<dyn BitbucketCredentialSource>,
    pub bitbucket_config: BitbucketApiConfig,
    pub forgejo_environment: ForgejoEnvironment,
    pub clock: SharedClock,
}

/// Every source control service (the CLIs are exposed for the pull request providers, WP-21..23).
#[derive(Clone)]
pub struct SourceControl {
    pub github: GitHubCli,
    pub gitlab: GitLabCli,
    pub azure: AzureDevOpsCli,
    pub forgejo: ForgejoCli,
    pub bitbucket: BitbucketApi,
    pub budget: GitHubGraphQlBudget,
    pub limits: SourceControlRateLimit,
    pub registry: SourceControlProviderRegistry,
    pub discovery: SourceControlDiscovery,
    pub repositories: SourceControlRepositoryService,
}

impl SourceControl {
    pub fn new(deps: SourceControlDeps) -> Self {
        let budget = GitHubGraphQlBudget::new(deps.clock.clone());
        let limits = SourceControlRateLimit::new(deps.clock.clone());
        let github = GitHubCli::with_limits(deps.process.clone(), budget.clone(), limits.clone(), deps.clock.clone());
        let gitlab = GitLabCli::new(deps.process.clone());
        let azure = AzureDevOpsCli::new(deps.process.clone());
        let forgejo = ForgejoCli::new(deps.process.clone(), deps.forgejo_environment.clone(), deps.clock.clone());
        let bitbucket = BitbucketApi::new(
            deps.bitbucket_config.clone(),
            deps.bitbucket_credentials.clone(),
            deps.clock.clone(),
            deps.git.clone(),
            deps.vcs.clone(),
        );
        let registrations = vec![
            SourceControlProviderRegistration {
                kind: zc_contracts::SourceControlProviderKind::Github,
                provider: Arc::new(GitHubSourceControlProvider::new(github.clone())),
                discovery: DiscoverySpec::Cli(crate::github::provider::discovery()),
            },
            SourceControlProviderRegistration {
                kind: zc_contracts::SourceControlProviderKind::Gitlab,
                provider: Arc::new(GitLabSourceControlProvider::new(gitlab.clone())),
                discovery: DiscoverySpec::Cli(crate::gitlab::provider::discovery()),
            },
            SourceControlProviderRegistration {
                kind: zc_contracts::SourceControlProviderKind::AzureDevops,
                provider: Arc::new(AzureDevOpsSourceControlProvider::new(azure.clone())),
                discovery: DiscoverySpec::Cli(crate::azure::provider::discovery()),
            },
            SourceControlProviderRegistration {
                kind: zc_contracts::SourceControlProviderKind::Bitbucket,
                provider: Arc::new(BitbucketSourceControlProvider::new(bitbucket.clone())),
                discovery: DiscoverySpec::Api(crate::bitbucket::provider::discovery(bitbucket.clone())),
            },
            SourceControlProviderRegistration {
                kind: zc_contracts::SourceControlProviderKind::Forgejo,
                provider: Arc::new(ForgejoSourceControlProvider::new(forgejo.clone(), deps.process.clone())),
                discovery: DiscoverySpec::ManagedCli(Arc::new(ForgejoDiscovery::new(forgejo.clone(), deps.process.clone()))),
            },
        ];
        let registry = SourceControlProviderRegistry::new(registrations, deps.process.clone(), deps.vcs.clone(), deps.cwd.clone(), deps.clock.clone());
        let discovery = SourceControlDiscovery::new(deps.process.clone(), registry.clone(), deps.cwd.clone());
        let repositories = SourceControlRepositoryService::new(deps.cwd.clone(), deps.git.clone(), registry.clone());
        Self {
            github,
            gitlab,
            azure,
            forgejo,
            bitbucket,
            budget,
            limits,
            registry,
            discovery,
            repositories,
        }
    }

    /// The services of [`crate::rpc::register`].
    pub fn rpc_services(&self, after_publish: Option<AfterPublish>) -> SourceControlRpcServices {
        SourceControlRpcServices {
            discovery: self.discovery.clone(),
            repositories: self.repositories.clone(),
            after_publish,
        }
    }
}
