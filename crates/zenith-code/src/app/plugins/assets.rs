//! WP-29 (`zc-assets`): signed asset URLs, attachment uploads, GitHub media, app icons and
//! project favicons.
//!
//! - RPC: `assets.createUrl`, `attachments.createUploadUrl`, `attachments.delete`;
//! - routes: `GET|HEAD /api/assets/*`, `POST /api/attachments/upload/*`;
//! - capabilities: `attachmentUploads`, `questionAttachments`, `fileAttachments`;
//! - start: the sweep of stale pending uploads (`ensureServerDirectories` in TS).

use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use zc_ports::ProjectionReads;
use zc_rpc::RpcRouterBuilder;

use crate::app::environment::Capabilities;
use crate::app::{AppState, Plugin};

pub struct AssetsPlugin {
    assets: Arc<zc_assets::Assets>,
    reads: Arc<dyn ProjectionReads>,
}

impl AssetsPlugin {
    pub fn new(state: &Arc<AppState>) -> Self {
        let paths = &state.config.paths;
        Self {
            assets: Arc::new(zc_assets::Assets::new(
                &paths.attachments_dir,
                &paths.provider_status_cache_dir,
                state.secrets.clone(),
            )),
            reads: state.reads.clone(),
        }
    }
}

#[async_trait]
impl Plugin for AssetsPlugin {
    fn name(&self) -> &'static str {
        "assets"
    }

    fn capabilities(&self, capabilities: &mut Capabilities) {
        capabilities.enable(zc_assets::CAPABILITIES);
        capabilities.set("fileAttachments", zc_assets::file_attachments_capability());
    }

    fn register_rpc(&self, builder: RpcRouterBuilder) -> RpcRouterBuilder {
        self.assets.register_rpc(builder, self.reads.clone())
    }

    fn routes(&self) -> Router {
        self.assets.routes()
    }

    async fn start(&self) -> anyhow::Result<()> {
        let assets = self.assets.clone();
        tokio::task::spawn_blocking(move || assets.sweep_at_startup()).await?;
        Ok(())
    }
}
