//! WP-28, the in-app browser preview (zc-preview) in the running server: the tab manager and
//! its events, local server discovery (polled while a client subscribes; the terminal manager
//! registers each terminal's processes with it, see [`AppState::port_discovery`]).

use std::sync::{Arc, Mutex};

use tokio::task::JoinHandle;
use zc_preview::PreviewManager;
use zc_rpc::RpcRouterBuilder;

use super::{AppState, Plugin};

/// `preview.*`, `subscribePreviewEvents`, `subscribeDiscoveredLocalServers`.
pub struct PreviewPlugin {
    manager: PreviewManager,
    discovery: zc_preview::PortDiscovery,
    poller: Mutex<Option<JoinHandle<()>>>,
}

impl PreviewPlugin {
    pub fn new(state: &Arc<AppState>) -> Self {
        Self {
            manager: PreviewManager::new(),
            discovery: state.port_discovery.clone(),
            poller: Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl Plugin for PreviewPlugin {
    fn name(&self) -> &'static str {
        "preview"
    }

    fn register_rpc(&self, builder: RpcRouterBuilder) -> RpcRouterBuilder {
        zc_preview::rpc::register(builder, self.manager.clone(), self.discovery.clone())
    }

    async fn start(&self) -> anyhow::Result<()> {
        *self.poller.lock().unwrap() = Some(self.discovery.start());
        Ok(())
    }

    async fn shutdown(&self) {
        if let Some(poller) = self.poller.lock().unwrap().take() {
            poller.abort();
        }
    }
}
