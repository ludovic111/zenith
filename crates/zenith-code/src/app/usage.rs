//! WP-30: the usage package (`zc-usage`) as a [`Plugin`]: `server.getUsageSummary`,
//! `server.refreshUsageRates`, the `usageLimitSourcesUpdated` config events, and the
//! `usageLimitSources` / `usagePriceOverrides` capabilities.

use std::sync::Arc;

use async_trait::async_trait;
use futures::FutureExt;
use zc_ports::contracts::BackgroundScope;
use zc_rpc::RpcRouterBuilder;
use zc_settings::config::ConfigEventSource;
use zc_usage::cliproxy::{CliproxyApi, ReqwestHubHttp};
use zc_usage::{UsageLimitSources, UsageLimitSourcesEvents, UsageService, UsageServiceOptions};

use super::environment::Capabilities;
use super::{AppState, Plugin};

pub struct UsagePlugin {
    pub service: UsageService,
    pub limit_sources: UsageLimitSources,
}

impl UsagePlugin {
    pub fn new(state: &Arc<AppState>) -> Self {
        let settings: Arc<dyn zc_usage::UsageSettings> = Arc::new(state.settings.clone());
        let hostname = super::environment::hostname().unwrap_or_else(|| "localhost".to_owned());
        let service = UsageService::new(UsageServiceOptions::system(state.config.paths.state_dir.clone(), settings.clone(), hostname));
        #[allow(clippy::cast_precision_loss)]
        let now_ms: Arc<dyn Fn() -> f64 + Send + Sync> = Arc::new(|| zc_core::time::now_millis() as f64);
        let policy = state.background_policy.clone();
        let limit_sources = UsageLimitSources::new(
            CliproxyApi::new(Arc::new(ReqwestHubHttp::default()), now_ms.clone()),
            settings,
            Arc::new(move || {
                let policy = policy.clone();
                async move { policy.should_run_scope_work(&BackgroundScope::ProviderStatus { instance_id: None }).await }.boxed()
            }),
            now_ms,
        );
        Self { service, limit_sources }
    }
}

#[async_trait]
impl Plugin for UsagePlugin {
    fn name(&self) -> &'static str {
        "usage"
    }

    fn capabilities(&self, capabilities: &mut Capabilities) {
        capabilities.enable(zc_usage::CAPABILITIES);
    }

    fn register_rpc(&self, builder: RpcRouterBuilder) -> RpcRouterBuilder {
        zc_usage::rpc::register(builder, self.service.clone())
    }

    fn config_sources(&self) -> Vec<Arc<dyn ConfigEventSource>> {
        vec![Arc::new(UsageLimitSourcesEvents(self.limit_sources.clone()))]
    }

    async fn start(&self) -> anyhow::Result<()> {
        self.limit_sources.start();
        Ok(())
    }

    async fn shutdown(&self) {
        self.limit_sources.shutdown();
    }
}
