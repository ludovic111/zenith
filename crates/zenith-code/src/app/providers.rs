//! The provider core as `server.ts` composes it (`ProviderLayerLive`): event loggers, model
//! manifest, instance registry hydrated from the settings, status registry, adapter registry,
//! session directory, provider service and session reaper.
//!
//! The MCP session registry is the provider service's credential hook and the drivers'
//! session lookup (`DriverEnv::mcp_sessions`), as `McpSessionRegistry` is in TS.
//!
//! Drivers come from `app::plugins::drivers` (one line per driver crate, WP-13 Claude, WP-14
//! Codex, …). Without drivers every configured instance shows as an unavailable snapshot, which
//! is also what TS shows for a driver it does not know.

use std::sync::Arc;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zc_core::ServerConfig;
use zc_db::Db;
use zc_mcp::McpSessionRegistry;
use zc_ports::{BackgroundPolicy, ProjectionReads, SettingsService};
use zc_providers::hooks::ProjectionThreadShells;
use zc_providers::logger::EventNdjsonLogStoreOptions;
use zc_providers::manifest::ModelManifestOptions;
use zc_providers::reaper::ReaperOptions;
use zc_providers::{
    Driver, DriverEnv, InstanceAdapterRegistry, ModelManifest, ProviderEventLoggers, ProviderInstanceRegistry, ProviderRegistry, ProviderServiceImpl,
    ProviderServiceOptions, ProviderSessionDirectory, ProviderSessionReaper,
};

/// The server's environment as the provider CLIs inherit it, minus `RUST_LOG`: it configures
/// this server's own logging, and Codex (a Rust binary) would otherwise honour it too and
/// flood its stderr with tracing lines, each surfacing as a `runtime.warning` (the TS server,
/// a Node process, never had the variable to pass on).
pub fn provider_base_environment() -> std::collections::HashMap<String, String> {
    let mut env = zc_providers::driver::process_environment();
    env.remove("RUST_LOG");
    env
}

/// The running provider core.
pub struct ProviderStack {
    pub loggers: ProviderEventLoggers,
    pub manifest: ModelManifest,
    pub instances: ProviderInstanceRegistry,
    pub registry: ProviderRegistry,
    pub directory: ProviderSessionDirectory,
    pub service: ProviderServiceImpl,
    reads: Arc<dyn ProjectionReads>,
    hydration: JoinHandle<()>,
    reaper_stop: CancellationToken,
}

impl ProviderStack {
    /// Builds the core. `drivers` gets the shared driver environment and returns the drivers.
    pub async fn start(
        config: &ServerConfig,
        db: &Db,
        settings: Arc<dyn SettingsService>,
        background: Arc<dyn BackgroundPolicy>,
        reads: Arc<dyn ProjectionReads>,
        mcp: &McpSessionRegistry,
        drivers: impl FnOnce(&DriverEnv) -> Vec<Arc<dyn Driver>>,
    ) -> Self {
        let paths = &config.paths;
        let loggers = ProviderEventLoggers::open(&paths.provider_event_log_path, EventNdjsonLogStoreOptions::default());
        let manifest = ModelManifest::new(ModelManifestOptions::new(Some(paths.state_dir.join("model-manifest.json"))).with_settings(settings.clone()));
        let env = DriverEnv {
            event_loggers: loggers.clone(),
            model_manifest: manifest.clone(),
            settings: settings.clone(),
            background_policy: Some(background),
            server_cwd: config.cwd.clone(),
            state_dir: paths.state_dir.clone(),
            attachments_dir: paths.attachments_dir.clone(),
            base_env: provider_base_environment(),
            // Drivers hand the thread's `t3-code` MCP session to their agent (see `app::mcp`).
            mcp_sessions: Some(Arc::new(mcp.clone())),
        };
        let drivers = drivers(&env);
        let (instances, hydration) = zc_providers::instance_registry::hydrate(drivers, settings.clone()).await;
        let registry = ProviderRegistry::start(instances.clone(), manifest.clone(), paths.provider_status_cache_dir.clone()).await;
        let directory = ProviderSessionDirectory::new(db.clone());
        let mut options = ProviderServiceOptions::new(&paths.attachments_dir);
        options.canonical_event_logger = loggers.canonical.clone();
        options.settings = Some(settings);
        options.thread_shells = Some(Arc::new(ProjectionThreadShells(reads.clone())));
        // `issueMcpCredential` / `revokeActiveMcpThread` / `touchActiveMcpThread`.
        options.mcp = Some(Arc::new(mcp.clone()));
        let service = ProviderServiceImpl::start(Arc::new(InstanceAdapterRegistry::new(instances.clone())), directory.clone(), options).await;
        Self {
            loggers,
            manifest,
            instances,
            registry,
            directory,
            service,
            reads,
            hydration,
            reaper_stop: CancellationToken::new(),
        }
    }

    /// `providerSessionReaper.start()` (the `reactors.start` phase).
    pub fn start_reaper(&self) -> JoinHandle<()> {
        let reaper = ProviderSessionReaper::new(
            self.service.clone(),
            self.directory.clone(),
            Some(Arc::new(ProjectionThreadShells(self.reads.clone()))),
            ReaperOptions::default(),
        );
        reaper.start(self.reaper_stop.clone())
    }

    /// Stops every session and instance, then flushes the logs.
    pub async fn shutdown(&self) {
        self.reaper_stop.cancel();
        self.service.shutdown().await;
        self.registry.close();
        self.hydration.abort();
        self.instances.close().await;
        self.loggers.close();
    }
}

/// The `server.refreshProviders` payload.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RefreshProvidersInput {
    instance_id: Option<zc_contracts::ProviderInstanceId>,
    cwd: Option<String>,
    refresh_models: Option<bool>,
}

/// `server.refreshProviders` (`ws.ts`): re-probe one instance (or its workspace snapshot for
/// `cwd`), or every instance; `refreshModels` (an explicit user request) also refreshes the
/// model manifest, invalidates the drivers' discovery caches and rediscovers models. The usage
/// limit sources of an untargeted refresh are WP-30 and not refreshed yet.
pub fn register_rpc(
    builder: zc_rpc::RpcRouterBuilder,
    registry: ProviderRegistry,
    instances: ProviderInstanceRegistry,
    manifest: ModelManifest,
) -> zc_rpc::RpcRouterBuilder {
    const TAG: &str = "server.refreshProviders";
    builder.unary_with(TAG, zc_auth::rpc_method_options(TAG), move |_ctx, payload| {
        let (registry, instances, manifest) = (registry.clone(), instances.clone(), manifest.clone());
        async move {
            let input: RefreshProvidersInput = serde_json::from_value(payload).map_err(|error| zc_rpc::RpcError::die_text(error.to_string()))?;
            let refresh_models = input.refresh_models == Some(true);
            let targeted = |instance_id: &zc_contracts::ProviderInstanceId| input.instance_id.as_ref().is_none_or(|id| id == instance_id);
            if refresh_models {
                manifest.force_refresh().await;
                let selected: Vec<_> = instances
                    .list_instances()
                    .into_iter()
                    .filter(|instance| targeted(&instance.instance_id))
                    .collect();
                futures::future::join_all(selected.into_iter().map(|instance| async move {
                    if let Some(invalidate) = &instance.invalidate_caches {
                        if let Err(error) = invalidate().await {
                            tracing::warn!(instance_id = %instance.instance_id, detail = error.detail, "failed to invalidate provider caches");
                        }
                    }
                    instance.snapshot.resolve_maintenance(true).await;
                }))
                .await;
            }
            let mut providers = match (&input.instance_id, &input.cwd) {
                (Some(instance_id), Some(cwd)) => registry.refresh_workspace_snapshot(instance_id, cwd).await,
                (Some(instance_id), None) => registry.refresh_instance(instance_id).await,
                (None, _) => registry.refresh(None).await,
            };
            if refresh_models {
                for instance in instances.list_instances() {
                    let Some(refresh) = &instance.refresh_models else { continue };
                    let live = providers
                        .iter()
                        .any(|provider| provider.instance_id == instance.instance_id && provider.enabled && provider.installed);
                    if !targeted(&instance.instance_id) || !live {
                        continue;
                    }
                    if let Err(error) = refresh().await {
                        return Err(zc_rpc::RpcError::fail(zc_contracts::ProviderSetupError {
                            tag: zc_contracts::LitProviderSetupError,
                            instance_id: instance.instance_id.clone(),
                            operation: "refresh-models".into(),
                            detail: error.detail,
                            cause: None,
                        }));
                    }
                    providers = registry.refresh_instance(&instance.instance_id).await;
                }
            }
            Ok(serde_json::json!({ "providers": providers }))
        }
    })
}
