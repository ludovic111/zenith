//! WP-20 (zc-sourcecontrol) in the server: `server.discoverSourceControl` (Settings → Source
//! control) and `sourceControl.lookupRepository|cloneRepository|publishRepository`. The
//! provider registry also resolves unknown remotes for VCS status (the `pull_request_status`
//! slot, see [`super::plugins::slots`]).

use std::sync::Arc;

use async_trait::async_trait;
use zc_rpc::RpcRouterBuilder;
use zc_sourcecontrol::rpc::{AfterPublish, SourceControlRpcServices};

use super::{AppState, Plugin};

/// WP-20's plugin.
pub struct SourceControlPlugin {
    services: SourceControlRpcServices,
}

impl SourceControlPlugin {
    pub fn new(state: &Arc<AppState>) -> Self {
        // `ws.ts` after a publish: refresh the repository identity of `cwd` (WP-25: the new
        // remote is its identity from now on) and its git status.
        let broadcaster = state.vcs.broadcaster.clone();
        let identities = state.repository_identities.clone();
        let after_publish: AfterPublish = Arc::new(move |cwd: String| {
            let broadcaster = broadcaster.clone();
            let identities = identities.clone();
            Box::pin(async move {
                identities.refresh(&cwd).await;
                if let Err(error) = broadcaster.refresh_status(&cwd).await {
                    tracing::warn!(cwd, ?error, "failed to refresh git status after publishing the repository");
                }
            })
        });
        Self {
            services: state.shared.source_control.rpc_services(Some(after_publish)),
        }
    }
}

#[async_trait]
impl Plugin for SourceControlPlugin {
    fn name(&self) -> &'static str {
        "source-control"
    }

    fn register_rpc(&self, builder: RpcRouterBuilder) -> RpcRouterBuilder {
        zc_sourcecontrol::rpc::register(builder, self.services.clone())
    }
}
