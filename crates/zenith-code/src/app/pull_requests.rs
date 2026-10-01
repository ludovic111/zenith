//! WP-21..23 (zc-pullrequest) in the server: the pull request service over the five forge
//! providers, its `pullRequests.*` RPCs, `POST /api/pull-requests/diff`, and the two PR reactors.
//!
//! | Piece | Built from |
//! |---|---|
//! | providers | [`zc_pullrequest::wiring::provider_registry`] over the shared forge clients of [`super::plugins::SharedServices::source_control`] (one GitHub GraphQL budget, one set of rate-limit pauses) |
//! | `PullRequestService` | the providers, the projections, the source control registry (provider refinement), the rate limits, the database (viewed files), the read cache in `<providerStatusCacheDir>/pull-requests` |
//! | RPCs | `ws.ts` `pullRequests.*`, with `withPullRequestViewer`, linked threads and sync requests ([`ReactorLinks`]) |
//! | reactors | `ThreadPullRequestReactor` and `PullRequestSyncReactor`, handed to the reactors plugin for their slots ([`PullRequestsPlugin::external_reactors`]) |
//! | port | the service as `zc_ports::PullRequests` for the settlement and checkpoint reactors ([`PullRequestsPlugin::port`]) |
//!
//! Capabilities: `pullRequests`, `threadPullRequests`, `pullRequestStackActions`.

use std::sync::Arc;

use axum::Router;
use tokio_util::sync::CancellationToken;
use zc_ports::{GitWorkflow, OrchestrationDispatch, PullRequests};
use zc_pullrequest::reactors::{
    fs_path_exists, system_uuids, ProjectionRepositoryIdentities, PullRequestSyncDeps, PullRequestSyncReactor, ThreadPullRequestDeps, ThreadPullRequestReactor,
    SWEEP_INTERVAL,
};
use zc_pullrequest::rpc::{PullRequestRpcServices, ReactorLinks};
use zc_pullrequest::{PullRequestReadCache, PullRequestService, PullRequestServiceDeps};
use zc_reactors::reactor::ExternalReactor;
use zc_reactors::{ReactorSlot, SystemClock};
use zc_rpc::RpcRouterBuilder;

use super::environment::Capabilities;
use super::http_auth::HttpAuth;
use super::{AppState, Plugin};

/// The pull request package.
pub struct PullRequestsPlugin {
    service: PullRequestService,
    links: Arc<ReactorLinks>,
    thread_reactor: Arc<ThreadPullRequestReactor>,
    sync_reactor: Arc<PullRequestSyncReactor>,
    auth: HttpAuth,
    stop: CancellationToken,
}

impl PullRequestsPlugin {
    pub fn new(state: &Arc<AppState>) -> Self {
        let source_control = &state.shared.source_control;
        let clock = zc_sourcecontrol::util::system_clock();
        let service = PullRequestService::new(PullRequestServiceDeps {
            registry: zc_pullrequest::wiring::provider_registry(source_control, clock.clone()),
            projections: state.reads.clone(),
            source_control: source_control.registry.clone(),
            rate_limits: source_control.limits.clone(),
            db: state.db.clone(),
            read_cache: PullRequestReadCache::open_with_clock(&state.config.paths.provider_status_cache_dir.join("pull-requests"), clock.clone()),
            clock,
        });
        let port: Arc<dyn PullRequests> = Arc::new(service.clone());
        let engine: Arc<dyn OrchestrationDispatch> = Arc::new(state.engine.clone());
        let git: Arc<dyn GitWorkflow> = Arc::new(state.vcs.workflow.clone());
        let stop = CancellationToken::new();
        let thread_reactor = Arc::new(ThreadPullRequestReactor::new(
            ThreadPullRequestDeps {
                engine: engine.clone(),
                projections: state.reads.clone(),
                git,
                pull_requests: port.clone(),
                repository_identities: Arc::new(ProjectionRepositoryIdentities(state.repository_identities.clone())),
                uuids: system_uuids(),
                path_exists: fs_path_exists(),
                interval: SWEEP_INTERVAL,
            },
            stop.child_token(),
        ));
        let sync_reactor = Arc::new(PullRequestSyncReactor::new(
            PullRequestSyncDeps {
                engine,
                projections: state.reads.clone(),
                pull_requests: port,
                clock: Arc::new(SystemClock),
                uuids: system_uuids(),
                interval: SWEEP_INTERVAL,
            },
            stop.child_token(),
        ));
        let links = Arc::new(ReactorLinks {
            projections: state.reads.clone(),
            db: state.db.clone(),
            sync: Some(sync_reactor.clone()),
        });
        Self {
            service,
            links,
            thread_reactor,
            sync_reactor,
            auth: HttpAuth(state.auth.clone()),
            stop,
        }
    }

    /// The service as the `PullRequests` port (settlement, checkpoint reactor).
    pub fn port(&self) -> Arc<dyn PullRequests> {
        Arc::new(self.service.clone())
    }

    /// The two PR reactors, in their `OrchestrationReactor` slots: the thread reactor before the
    /// settlement reactor, the sync reactor after it.
    pub fn external_reactors(&self) -> Vec<(ReactorSlot, Arc<dyn ExternalReactor>)> {
        vec![
            (ReactorSlot::ThreadPullRequestReactor, self.thread_reactor.clone() as Arc<dyn ExternalReactor>),
            (ReactorSlot::PullRequestSyncReactor, self.sync_reactor.clone() as Arc<dyn ExternalReactor>),
        ]
    }
}

#[async_trait::async_trait]
impl Plugin for PullRequestsPlugin {
    fn name(&self) -> &'static str {
        "pull-requests"
    }

    fn capabilities(&self, capabilities: &mut Capabilities) {
        capabilities.enable(&["pullRequests", "threadPullRequests", "pullRequestStackActions"]);
    }

    fn register_rpc(&self, builder: RpcRouterBuilder) -> RpcRouterBuilder {
        zc_pullrequest::rpc::register(
            builder,
            PullRequestRpcServices {
                service: Arc::new(self.service.clone()),
                links: self.links.clone(),
            },
        )
    }

    fn routes(&self) -> Router {
        zc_pullrequest::http::router(Arc::new(self.service.clone()), Arc::new(self.auth.clone()))
    }

    async fn shutdown(&self) {
        self.stop.cancel();
    }
}
