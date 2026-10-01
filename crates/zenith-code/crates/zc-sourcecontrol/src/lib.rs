//! zc-sourcecontrol: zenith code's source control providers in Rust (WP-20 of
//! `docs/zenith-code-rust-plan.md`, §6.3).
//!
//! | Module | Ported from (`apps/server/src/sourceControl/`) |
//! |---|---|
//! | [`provider`] | `SourceControlProvider.ts`: the provider trait, inputs, owner refs, transport-safe values |
//! | [`errors`] | `SourceControlProviderError` / `SourceControlRepositoryError` (contracts) and the `cause` defects |
//! | [`rate_limit`] | `SourceControlRateLimit.ts` (per provider/host/credential pauses, `Retry-After`) |
//! | [`graphql_budget`] | `githubGraphQlBudget.ts` (GraphQL points with a 10% reserve) |
//! | [`github`] | `GitHubCli.ts`, `gitHubPullRequests.ts`, `gitHubAuthStatus.ts`, `GitHubSourceControlProvider.ts` |
//! | [`gitlab`] | `GitLabCli.ts`, `gitLabMergeRequests.ts`, `gitLabAuthStatus.ts`, `GitLabSourceControlProvider.ts` |
//! | [`azure`] | `AzureDevOpsCli.ts`, `azureDevOpsPullRequests.ts`, `AzureDevOpsSourceControlProvider.ts` |
//! | [`forgejo`] | `ForgejoCli.ts`, `forgejoPullRequests.ts`, `ForgejoSourceControlProvider.ts` |
//! | [`bitbucket`] | `BitbucketApi.ts`, `bitbucketPullRequests.ts`, `BitbucketSourceControlProvider.ts` |
//! | [`discovery`] | `SourceControlProviderDiscovery.ts` |
//! | [`source_control_discovery`] | `SourceControlDiscovery.ts` (`server.discoverSourceControl`) |
//! | [`registry`] | `SourceControlProviderRegistry.ts` |
//! | [`repository`], [`clone_progress`] | `SourceControlRepositoryService.ts`, `project/gitCloneProgress.ts` |
//! | [`pr_template`] | `PrTemplateDetection.ts` |
//! | [`status_hook`] | the `resolve_unknown_provider` hook of zc-vcs's status (GitManager `resolveHostingProvider`) |
//! | [`rpc`] | the `ws.ts` handlers of `server.discoverSourceControl` and `sourceControl.*` |
//! | [`wiring`] | `SourceControlProviderRegistry.make` + layers: everything built once |
//!
//! Every forge CLI goes through zc-core's `VcsProcess` (concurrency 8, `gh` 4, stderr
//! classification); Bitbucket uses `reqwest` with redirects handled by hand. Effect `Context`
//! references (`PinnedGitHubCredential`, `AllowGitHubReserve`, `CredentialScope`) are tokio
//! task-locals; Effect's `Clock` is the injectable [`util::Clock`].

#![allow(clippy::result_large_err)]

pub mod azure;
pub mod bitbucket;
pub mod cache;
pub mod clone_progress;
pub mod discovery;
pub mod errors;
pub mod forgejo;
pub mod github;
pub mod gitlab;
pub mod graphql_budget;
pub mod http;
pub mod pr_template;
pub mod provider;
pub mod rate_limit;
pub mod records;
pub mod registry;
pub mod repository;
pub mod rpc;
pub mod source_control_discovery;
pub mod status_hook;
pub mod util;
pub mod wiring;

pub use errors::{Cause, SourceControlProviderError, SourceControlRepositoryError};
pub use provider::{SourceControlProvider, SourceControlProviderContext, SourceControlRefSelector};
pub use registry::{SourceControlProviderHandle, SourceControlProviderRegistry};
pub use repository::{SourceControlCloneOptions, SourceControlPreparedClone, SourceControlRepositoryService};
pub use source_control_discovery::SourceControlDiscovery;
pub use status_hook::WithSourceControlProviders;
pub use wiring::{SourceControl, SourceControlDeps};
