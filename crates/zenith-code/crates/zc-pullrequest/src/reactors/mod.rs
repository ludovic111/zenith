//! The two pull request reactors of `orchestration/`:
//!
//! - [`ThreadPullRequestReactor`] (`ThreadPullRequestReactor.ts`): discovers each thread's
//!   branch pull request (`branchPullRequest`) and replaces a terminal manual link with the
//!   branch's open one, through `thread.pull-request.sync`. Every minute, after turns and after
//!   thread or project edits.
//! - [`PullRequestSyncReactor`] (`PullRequestSyncReactor.ts`): keeps every thread ↔ pull request
//!   link's host snapshot current (`thread.pull-request-link.sync`) and auto-links the layers of
//!   native stacks (`thread.pull-request.link` with `source: "stack"`). Every minute, open PRs of
//!   active threads each sweep, closed ones every 15 minutes, merged ones only on request.
//!
//! # Wiring (`server.ts`, `OrchestrationReactor.ts`, `ws.ts`)
//!
//! Both are built once per server from their deps structs (the ports of `zc_ports::Ports`
//! plus the small traits below) and registered in `zc_reactors::OrchestrationReactor::external`:
//!
//! - [`ThreadPullRequestReactor`] in `ReactorSlot::ThreadPullRequestReactor`, which starts after
//!   the thread deletion reactor and **before** the settlement reactor;
//! - [`PullRequestSyncReactor`] in `ReactorSlot::PullRequestSyncReactor`, right **after** the
//!   settlement reactor (so settlement already listens when the first synced snapshot lands).
//!
//! Each `start` subscribes to the domain events before it returns. Under a server activation
//! (`ServerActivation` / `forkParked`), call `start_with_activation`: subscribed now, events and
//! sweeps processed only once the activation resolves.
//!
//! The RPC layer keeps an `Arc<PullRequestSyncReactor>`: `pullRequests.runAction` (after the
//! action) and `pullRequests.invalidate` (after invalidating, unless `filesViewedOnly` or no
//! `reference`) resolve the reference with [`crate::sync_key::resolve_pull_request_sync_key`]
//! and call [`PullRequestSyncReactor::request_sync`]; `pullRequests.linkedThreads` resolves the
//! same key and reads [`crate::linked_threads::list_linked_pull_request_threads`] (`{threads:
//! []}` when there is no key).

pub mod sync;
pub mod thread_pull_request;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use zc_contracts::RepositoryIdentity;

pub use sync::{sibling_pull_request_url, PullRequestSyncDeps, PullRequestSyncReactor, SLOW_SYNC_INTERVAL_MS};
pub use thread_pull_request::{
    pull_request_matches_project, read_sweep_snapshot, SweepSnapshot, ThreadPullRequestDeps, ThreadPullRequestReactor, BACKFILL_ATTEMPTS,
};
pub use zc_reactors::common::{system_uuids, UuidSource};
pub use zc_reactors::settlement::{fs_path_exists, PathExists};

/// `Schedule.spaced("1 minute")`: the periodic pass of both reactors.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// `RepositoryIdentityResolver.resolve(cwd, {refresh})` (`project/RepositoryIdentityResolver.ts`):
/// the git identity of a workspace. `refresh` skips the resolver's cache (a turn may have added
/// the remote the pull request lives on).
#[async_trait]
pub trait RepositoryIdentities: Send + Sync {
    async fn resolve(&self, cwd: &str, refresh: bool) -> Option<RepositoryIdentity>;
}

/// [`RepositoryIdentities`] over zc-projections' resolver, which has no refresh option (its own
/// cache decides). For the server wiring until the project crate's cached resolver lands.
pub struct ProjectionRepositoryIdentities(pub Arc<dyn zc_projections::RepositoryIdentityResolver>);

#[async_trait]
impl RepositoryIdentities for ProjectionRepositoryIdentities {
    async fn resolve(&self, cwd: &str, _refresh: bool) -> Option<RepositoryIdentity> {
        self.0.resolve(cwd).await
    }
}

/// What a reactor waits for before it processes anything (`ServerActivation`).
pub type Activation = futures::future::BoxFuture<'static, ()>;
