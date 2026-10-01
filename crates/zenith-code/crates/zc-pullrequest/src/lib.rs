//! zc-pullrequest: zenith code's pull request feature in Rust (WP-21..23 of
//! `docs/zenith-code-rust-plan.md`, §6.4). Ports `apps/server/src/pullRequest/**`, the PR
//! reactors of `orchestration/` and the `pullRequests.*` handlers of `ws.ts`.
//!
//! | Module | Ported from (`apps/server/src/`) |
//! |---|---|
//! | [`provider`] | `pullRequest/PullRequestProvider.ts`: neutral types + the provider trait (frozen) |
//! | [`error`] | `PullRequestProviderError`, contracts `PullRequestUnavailableError` / `PullRequestOperationError` |
//! | [`contract`] | helpers of `packages/contracts/src/pullRequest.ts` |
//! | [`registry`] | `pullRequest/PullRequestProviderRegistry.ts` |
//! | [`service`] | `pullRequest/PullRequestService.ts` |
//! | [`read_cache`] | `pullRequest/PullRequestReadCache.ts` |
//! | [`viewed_files`] | `pullRequest/pullRequestViewedFiles.ts` |
//! | [`linked_threads`], [`sync_key`], [`checks`] | `pullRequest/linkedThreads.ts`, `pullRequestSyncKey.ts`, `pullRequestChecks.ts` |
//! | [`http`] | `pullRequest/http.ts` (`POST /api/pull-requests/diff`) |
//! | [`rpc`] | the `pullRequests.*` handlers of `ws.ts` (with `withPullRequestViewer`) |
//! | [`reactors`] | `orchestration/ThreadPullRequestReactor.ts`, `orchestration/PullRequestSyncReactor.ts` |
//! | [`github`] | `gitHubPullRequestJson.ts`, `GitHubPullRequestCli.ts`, `GitHubPullRequestProvider.ts`, `githubStackActions.ts` |
//! | [`gitlab`] | `gitLabMergeRequestJson.ts`, `GitLabPullRequestCli.ts`, `GitLabPullRequestProvider.ts` |
//! | [`forgejo`] | `forgejoPullRequestJson.ts`, `ForgejoPullRequestProvider.ts` |
//! | [`azure`] | `azureDevOpsPullRequestJson.ts`, `AzureDevOpsPullRequestCli.ts`, `azureDevOpsDiff.ts`, `AzureDevOpsPullRequestProvider.ts` |
//! | [`bitbucket`] | `bitbucketPullRequestJson.ts`, `BitbucketPullRequestApi.ts`, `bitbucketDiffRevisions.ts`, `BitbucketPullRequestProvider.ts` |
//! | [`wiring`] | `PullRequestProviderRegistry.make` + `PullRequestService.layer`: everything built once |

#![allow(clippy::result_large_err)]

pub mod azure;
pub mod bindings;
pub mod bitbucket;
pub mod checks;
pub mod contract;
pub mod decode;
pub mod error;
pub mod forgejo;
pub mod github;
pub mod gitlab;
pub mod http;
pub mod linked_threads;
pub mod provider;
pub mod reactors;
pub mod read_cache;
pub mod registry;
pub mod rpc;
pub mod service;
pub mod sync_key;
mod ttl_cache;
mod util;
pub mod viewed_files;
pub mod wiring;

pub use error::{ProviderFailureReason, PullRequestError, PullRequestProviderError};
pub use provider::{PullRequestProviderApi, SharedProvider};
pub use read_cache::PullRequestReadCache;
pub use registry::PullRequestProviderRegistry;
pub use service::{PullRequestService, PullRequestServiceDeps};
