//! The server assembly: every crate's services built with real implementations, their RPC
//! handlers and HTTP routes registered, and the startup sequence of
//! `serverRuntimeStartup.ts` (plan §6.18). `serve` is [`App::build`] + [`App::startup`] around
//! the HTTP listener; tests and tools build an [`App`] the same way.
//!
//! Construction, in dependency order ([`App::build`]):
//!
//! 1. [`Foundation`]: config, database (migrated), secret store, settings service;
//! 2. [`Slots`] from [`plugins::slots`] (services the core is built *with*);
//! 3. [`AppState`]: auth, environment id, keybindings, themes, projections (pipeline +
//!    snapshot queries), the orchestration engine (projecting every event), the client
//!    dispatcher, the provider core (drivers from [`plugins::drivers`]), terminals, VCS,
//!    lifecycle events, the readiness gate;
//! 4. the [`Plugin`]s from [`plugins::plugins`] (packages built *from* the core);
//! 5. capabilities → environment descriptor; `ServerConfig` assembly (zc-settings plus the
//!    contributors and sources of [`config`] and the plugins);
//! 6. the RPC router (core handlers, plugin handlers, then a "not implemented by the Rust
//!    server yet" placeholder for every remaining method of the table) and the HTTP router
//!    (core routes, plugin routes, then placeholder typed endpoints as a fallback before the SPA).

pub mod checkpoints;
pub mod config;
pub mod environment;
pub mod git;
pub mod http_auth;
pub mod lifecycle;
pub mod mcp;
pub mod orchestration;
pub mod plugins;
pub mod preview;
pub mod project;
pub mod providers;
pub mod pull_requests;
pub mod reactors;
pub mod server_rpc;
pub mod source_control;
pub mod startup;
pub mod terminal_env;
pub mod usage;
pub mod workspace;

use std::io::Write;
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use axum::response::IntoResponse;
use axum::routing::{get, on, MethodFilter};
use axum::{Json, Router};
use futures::StreamExt as _;
use serde_json::{json, Value};
use zc_auth::environment_auth::build_pairing_url;
use zc_auth::{register_auth_access, rpc_method_options, rpc_scope_table, AuthWsAuthenticator, EnvironmentAuth};
use zc_contracts::{RpcKind, ENDPOINTS, METHODS};
use zc_core::config::{ServerConfig, StartupPresentation};
use zc_core::runtime_state::{
    clear_persisted_server_runtime_state, format_host_for_url, is_wildcard_host, make_persisted_server_runtime_state, persist_server_runtime_state,
};
use zc_core::ServerSecretStore;
use zc_db::Db;
use zc_http::{EnvironmentError, ReadinessGate};
use zc_orchestration::engine::{EngineConfig, ProjectionEngineReads};
use zc_orchestration::{BackgroundLiveness, OrchestrationEngine, SystemEnv};
use zc_ports::contracts::{AuthSessionId, BackgroundPolicySnapshot, BackgroundScope, ClientActivityReportInput, HostPowerSnapshot, RpcClientId};
use zc_ports::{BackgroundPolicy, BackgroundPolicySubscription, ProjectionReads};
use zc_projections::{ProjectionSnapshotQuery, RepositoryIdentityResolver, ThreadLiveState};
use zc_rpc::{RpcError, RpcRouter, RpcRouterBuilder, RpcServer};
use zc_settings::config::{observability, ConfigEventSource, ServerConfigParts, ServerConfigService, SnapshotContributor};
use zc_settings::rpc::SettingsRpc;
use zc_settings::{EnvironmentThemeService, KeybindingsService, ServerSettingsService};
use zc_terminal::{PortablePtyAdapter, TerminalManager, TerminalManagerOptions};
use zc_vcs::registry::{VcsDriverRegistry, VcsProjectConfig};
use zc_vcs::rpc::VcsRpcServices;
use zc_vcs::vcs_driver::GitVcsProcessDriver;
use zc_vcs::GitVcsDriver;

use crate::server::{self, AppServices, HttpConfig};
use config::{EditorDiscovery, EnvironmentContributor, ProviderStatusSource, ProvidersContributor};
use environment::{Capabilities, ServerEnvironment};
use lifecycle::ServerLifecycleEvents;
use orchestration::{ClientDispatcher, DispatchHook, HttpDispatcher};
use providers::ProviderStack;

pub use environment::SERVER_VERSION;

/// What everything else is built on.
pub struct Foundation {
    pub config: Arc<ServerConfig>,
    pub db: Db,
    pub secrets: ServerSecretStore,
    pub settings: ServerSettingsService,
    /// The process runner every git and forge CLI call goes through (shared limits).
    pub vcs_process: zc_core::VcsProcess,
    /// The git driver (`GitVcsDriver`) and the VCS driver registry (`VcsDriverRegistryLive`).
    pub git: GitVcsDriver,
    pub vcs_registry: VcsDriverRegistry,
}

/// The services the core is built with ([`plugins::slots`]).
pub struct Slots {
    /// WP-11: `BackgroundPolicy` (pollers ask it before working). Default: always run.
    pub background_policy: Arc<dyn BackgroundPolicy>,
    /// WP-11: `ThreadBackgroundLivenessService` (the engine refuses auto-settle of busy threads).
    pub background_liveness: Arc<dyn BackgroundLiveness>,
    /// WP-11: background liveness and plan progress on thread shells.
    pub thread_live_state: Arc<dyn ThreadLiveState>,
    /// WP-25/WP-20: repository identities on project shells (`repositoryIdentity` capability).
    pub repository_identities: Arc<dyn RepositoryIdentityResolver>,
    /// WP-24: editors and the file-manager reveal in `ServerConfig`.
    pub editors: Arc<dyn EditorDiscovery>,
    /// The concrete services behind the slots that the plugins also use (the liveness and
    /// plan-progress registries, the background policy, source control, the launcher, …).
    pub shared: Arc<plugins::SharedServices>,
}

/// A background policy where every poller may always run and nothing is tracked (tests).
#[derive(Debug, Default, Clone, Copy)]
pub struct AlwaysRunBackgroundPolicy;

#[async_trait]
impl BackgroundPolicy for AlwaysRunBackgroundPolicy {
    async fn report_client_activity(&self, _: &AuthSessionId, _: &RpcClientId, _: ClientActivityReportInput) {}
    async fn remove_rpc_client(&self, _: &AuthSessionId, _: &RpcClientId) {}
    async fn report_host_power_state(&self, _: HostPowerSnapshot) {}
    async fn snapshot(&self) -> BackgroundPolicySnapshot {
        BackgroundPolicySnapshot::default()
    }
    async fn subscribe(&self) -> BackgroundPolicySubscription {
        BackgroundPolicySubscription {
            latest: BackgroundPolicySnapshot::default(),
            changes: futures::stream::empty().boxed(),
        }
    }
    async fn has_demand(&self, _: &BackgroundScope) -> bool {
        true
    }
    async fn should_run_scope_work(&self, _: &BackgroundScope) -> bool {
        true
    }
    async fn should_run_opportunistic_work(&self) -> bool {
        true
    }
}

/// The running core every plugin is built from.
pub struct AppState {
    pub config: Arc<ServerConfig>,
    pub db: Db,
    pub secrets: ServerSecretStore,
    pub auth: Arc<EnvironmentAuth>,
    pub environment_id: String,
    pub settings: ServerSettingsService,
    pub keybindings: KeybindingsService,
    pub themes: EnvironmentThemeService,
    /// The projection queries (`ProjectionSnapshotQuery`), also as [`Self::reads`].
    pub projections: Arc<ProjectionSnapshotQuery>,
    pub reads: Arc<dyn ProjectionReads>,
    /// `slots.repository_identities` (refreshed after a clone or a publish).
    pub repository_identities: Arc<dyn RepositoryIdentityResolver>,
    pub engine: OrchestrationEngine,
    pub dispatcher: Arc<ClientDispatcher>,
    pub providers: ProviderStack,
    /// Commit messages, change requests, branch names and thread titles, routed by the model
    /// selection's instance ([`plugins::text_generation`]); what the reactors and the git
    /// stacked actions are built with.
    pub text_generation: Arc<dyn zc_ports::TextGeneration>,
    pub terminals: TerminalManager,
    pub git: GitVcsDriver,
    pub vcs_process: zc_core::VcsProcess,
    pub vcs_registry: VcsDriverRegistry,
    pub vcs: VcsRpcServices,
    pub background_policy: Arc<dyn BackgroundPolicy>,
    /// See [`Slots::shared`].
    pub shared: Arc<plugins::SharedServices>,
    pub editors: Arc<dyn EditorDiscovery>,
    pub lifecycle: ServerLifecycleEvents,
    pub readiness: ReadinessGate,
    /// The `t3-code` MCP credentials (`McpSessionRegistry`): the provider service issues one per
    /// session, the drivers hand it to their agent, `/mcp` checks it.
    pub mcp_sessions: zc_mcp::McpSessionRegistry,
    /// Local server discovery for the preview panel; the terminal manager registers the
    /// processes of each terminal with it.
    pub port_discovery: zc_preview::PortDiscovery,
}

/// A package plugged into the running server. Everything has a no-op default.
#[async_trait]
pub trait Plugin: Send + Sync {
    fn name(&self) -> &'static str;
    /// Descriptor capabilities this package implements.
    fn capabilities(&self, _capabilities: &mut Capabilities) {}
    /// RPC methods (they replace the "not implemented" placeholders).
    fn register_rpc(&self, builder: RpcRouterBuilder) -> RpcRouterBuilder {
        builder
    }
    /// HTTP routes (they take precedence over the placeholder typed endpoints).
    fn routes(&self) -> Router {
        Router::new()
    }
    /// Fields of the `ServerConfig` snapshot.
    fn config_contributors(&self) -> Vec<Arc<dyn SnapshotContributor>> {
        Vec::new()
    }
    /// Live `subscribeServerConfig` events.
    fn config_sources(&self) -> Vec<Arc<dyn ConfigEventSource>> {
        Vec::new()
    }
    /// Hooks into client command dispatch.
    fn dispatch_hooks(&self) -> Vec<Arc<dyn DispatchHook>> {
        Vec::new()
    }
    /// Started in the `reactors.start` phase, before the reconcilers and the command gate.
    async fn start(&self) -> anyhow::Result<()> {
        Ok(())
    }
    /// A WebSocket RPC connection closed (after every request of it ended).
    fn connection_closed(&self, _connection_id: u64) {}
    /// Server shutdown, in reverse plugin order.
    async fn shutdown(&self) {}
}

/// The capabilities of the core. (`repositoryIdentity` comes with the plugin that fills the
/// repository identity slot.)
pub fn core_capabilities() -> Capabilities {
    let mut capabilities = Capabilities::default();
    capabilities.enable(&[
        // server.probe
        "connectionProbe",
        // zc-settings: per-project overrides, published themes, the environment icon setting
        "projectSettingsOverrides",
        "environmentThemes",
        "environmentIcon",
        // startup reconciliation honours continueThreadsAfterServerUpdate
        "threadRestartContinuation",
        // zc-orchestration's decider and zc-projections handle these commands
        "threadSettlement",
        "threadSnooze",
        "threadPinning",
        "threadPinReorder",
        "threadActiveReorder",
        "threadAutoSettleOptOut",
        "threadPullRequestLinking",
    ]);
    capabilities
}

/// The assembled server.
pub struct App {
    pub state: Arc<AppState>,
    pub environment: ServerEnvironment,
    pub plugins: Vec<Arc<dyn Plugin>>,
    pub config_service: ServerConfigService,
    pub rpc: Arc<RpcServer>,
    pub router: Router,
    /// The port the listener got (set by [`App::startup`]).
    listening_port: std::sync::OnceLock<u16>,
}

/// Opens the database (migrating it) off the async runtime.
pub async fn open_database(config: &ServerConfig) -> anyhow::Result<Db> {
    let db_path = config.paths.db_path.clone();
    tokio::task::spawn_blocking(move || Db::open(&db_path))
        .await?
        .context("could not open the database")
}

impl App {
    /// Builds every service (nothing started yet: see [`App::startup`]).
    pub async fn build(config: ServerConfig) -> anyhow::Result<Self> {
        let config = Arc::new(config);
        let paths = &config.paths;
        zc_core::config::ensure_server_directories(paths)
            .await
            .context("could not create the server directories")?;

        // 1. Foundation.
        let db = open_database(&config).await?;
        let secrets = ServerSecretStore::open(&paths.secrets_dir).await.context("could not open the secret store")?;
        let settings = ServerSettingsService::new(&paths.settings_path, Arc::new(secrets.clone()), Arc::new(db.clone()));
        let vcs_process = zc_core::VcsProcess::default();
        let git = GitVcsDriver::new(&paths.worktrees_dir);
        let vcs_registry = VcsDriverRegistry::new(VcsProjectConfig::new(), Arc::new(GitVcsProcessDriver::new(vcs_process.clone())));
        let foundation = Foundation {
            config: config.clone(),
            db: db.clone(),
            secrets: secrets.clone(),
            settings: settings.clone(),
            vcs_process: vcs_process.clone(),
            git: git.clone(),
            vcs_registry: vcs_registry.clone(),
        };

        // 2. Slots.
        let slots = plugins::slots(&foundation);

        // 3. The core.
        let environment_id = zc_core::environment_id::read_or_create_environment_id(&paths.state_dir, &paths.environment_id_path)
            .await
            .context("could not read the environment id")?;
        let auth = EnvironmentAuth::open(
            db.clone(),
            secrets.clone(),
            crate::cli::cookie_input(&config, environment_id.clone()),
            zc_auth::system_clock(),
        )
        .await
        .context("could not read the server signing key")?;
        let auth = Arc::new(auth);
        let keybindings = KeybindingsService::new(&paths.keybindings_config_path);
        let themes = EnvironmentThemeService::start(&paths.environment_themes_dir).await;

        let projections = Arc::new(ProjectionSnapshotQuery::new(
            db.clone(),
            slots.repository_identities.clone(),
            slots.thread_live_state.clone(),
        ));
        let reads: Arc<dyn ProjectionReads> = projections.clone();
        let engine = OrchestrationEngine::start(EngineConfig {
            db: db.clone(),
            reads: Arc::new(ProjectionEngineReads(reads.clone())),
            pipeline: Arc::new(zc_projections::engine::EnginePipeline {
                pipeline: zc_projections::ProjectionPipeline::new(&paths.attachments_dir),
                db: db.clone(),
            }),
            liveness: slots.background_liveness.clone(),
            env: Arc::new(SystemEnv),
        })
        .await
        .map_err(|error| anyhow::anyhow!("could not start the orchestration engine: {error}"))?;

        let settings_port: Arc<dyn zc_ports::SettingsService> = Arc::new(settings.clone());
        // The endpoint is set once the listener is bound (`App::startup`).
        let mcp_sessions = zc_mcp::McpSessionRegistry::new(environment_id.clone(), zc_mcp::McpSessionRegistryOptions::default());
        let providers = ProviderStack::start(
            &config,
            &db,
            settings_port.clone(),
            slots.background_policy.clone(),
            reads.clone(),
            &mcp_sessions,
            plugins::drivers,
        )
        .await;

        let mut terminal_options = TerminalManagerOptions::new(&paths.terminal_logs_dir, Arc::new(PortablePtyAdapter));
        terminal_options.provider_environment = Some(Arc::new(terminal_env::ProviderTerminalEnvironment::new(settings_port.clone())));
        let port_discovery = zc_preview::PortDiscovery::new();
        terminal_options.process_registry = Some(Arc::new(port_discovery.clone()));
        let terminals = TerminalManager::new(terminal_options).await.context("could not start the terminal manager")?;
        let terminals_port: Arc<dyn zc_ports::TerminalManager> = Arc::new(terminals.clone());

        let text_generation = plugins::text_generation(&providers, &slots);
        let vcs = build_vcs(
            &config,
            &settings,
            &reads,
            &slots,
            &git,
            &vcs_registry,
            plugins::git_manager(&git, &slots, &providers, &text_generation, &reads, &terminals_port, &settings_port),
        );

        let dispatcher = Arc::new(ClientDispatcher::new(
            engine.clone(),
            reads.clone(),
            terminals_port.clone(),
            paths.attachments_dir.clone(),
        ));

        let state = Arc::new(AppState {
            config: config.clone(),
            db,
            secrets,
            auth: auth.clone(),
            environment_id: environment_id.clone(),
            settings: settings.clone(),
            keybindings: keybindings.clone(),
            themes: themes.clone(),
            projections,
            reads: reads.clone(),
            repository_identities: slots.repository_identities.clone(),
            engine: engine.clone(),
            dispatcher: dispatcher.clone(),
            text_generation,
            providers,
            terminals,
            git,
            vcs_process,
            vcs_registry,
            vcs: vcs.clone(),
            background_policy: slots.background_policy.clone(),
            shared: slots.shared.clone(),
            editors: slots.editors.clone(),
            lifecycle: ServerLifecycleEvents::new(),
            readiness: ReadinessGate::new(),
            mcp_sessions,
            port_discovery,
        });

        // 4. Plugins.
        let plugins = plugins::plugins(&state);
        for plugin in &plugins {
            for hook in plugin.dispatch_hooks() {
                dispatcher.add_hook(hook);
            }
        }

        // 5. Capabilities, descriptor, ServerConfig.
        let mut capabilities = core_capabilities();
        for plugin in &plugins {
            plugin.capabilities(&mut capabilities);
        }
        let environment = ServerEnvironment::detect(&environment_id, &config.cwd, &capabilities).await?;
        let auth_descriptor = serde_json::to_value(auth.descriptor())?;
        let mut config_service = ServerConfigService::new(
            settings.clone(),
            keybindings.clone(),
            ServerConfigParts {
                cwd: config.cwd.to_string_lossy().into_owned(),
                observability: observability(
                    &paths.logs_dir.to_string_lossy(),
                    config.otlp_traces_url.as_deref(),
                    config.otlp_metrics_url.as_deref(),
                    config.otlp_logs_url.as_deref(),
                ),
            },
        )
        .with_themes(themes)
        .with_contributor(Arc::new(EnvironmentContributor {
            environment: environment.descriptor_json().clone(),
            auth: auth_descriptor,
            editors: state.editors.clone(),
            remote_open_targets: true,
        }))
        .with_contributor(Arc::new(ProvidersContributor(state.providers.registry.clone())))
        .with_event_source(Arc::new(ProviderStatusSource(state.providers.registry.clone())));
        for plugin in &plugins {
            for contributor in plugin.config_contributors() {
                config_service = config_service.with_contributor(contributor);
            }
            for source in plugin.config_sources() {
                config_service = config_service.with_event_source(source);
            }
        }

        // 6. RPC and HTTP.
        let builder = RpcRouter::builder().scopes(rpc_scope_table());
        let builder = register_auth_access(builder, auth.clone());
        let builder = SettingsRpc::new(config_service.clone()).register(builder);
        let builder = server_rpc::register(builder, state.lifecycle.clone());
        let builder = zc_vcs::rpc::register(builder, vcs);
        let builder = zc_terminal::rpc::register(builder, terminals_port);
        let builder = orchestration::register(builder, dispatcher.clone());
        let builder = providers::register_rpc(
            builder,
            state.providers.registry.clone(),
            state.providers.instances.clone(),
            state.providers.manifest.clone(),
        );
        let builder = Arc::new(zc_projections::subscriptions::OrchestrationSubscriptions::new(
            Arc::new(engine.clone()),
            reads.clone(),
        ))
        .register(builder);
        let mut builder = builder;
        for plugin in &plugins {
            builder = plugin.register_rpc(builder);
        }
        let router = register_placeholders(builder).build()?;
        let rpc = RpcServer::new(router);
        let closing = plugins.clone();
        rpc.on_connection_closed(Arc::new(move |connection_id| {
            for plugin in &closing {
                plugin.connection_closed(connection_id);
            }
        }));

        let http_auth = http_auth::HttpAuth(auth.clone());
        let descriptor = environment.descriptor_json().clone();
        let mut api = Router::new()
            .route(
                "/.well-known/t3/environment",
                get(move || {
                    let descriptor = descriptor.clone();
                    async move { Json(descriptor) }
                }),
            )
            .merge(zc_auth::http::routes(auth.clone()))
            .merge(server_rpc::routes(auth.clone()))
            .merge(zc_projections::http::router(
                reads.clone(),
                Arc::new(HttpDispatcher(dispatcher)),
                Arc::new(http_auth.clone()),
            ))
            .merge(zc_sessions::router(zc_sessions::SessionsApi {
                reader: Arc::new(zc_sessions::SessionReader::new(zc_sessions::ReaderConfig::from_env())),
                projects: Arc::new(ProjectionProjectRoots(reads.clone())),
                auth: Arc::new(http_auth),
            }));
        for plugin in &plugins {
            api = api.merge(plugin.routes());
        }
        let services = AppServices {
            rpc: rpc.clone(),
            auth: Arc::new(AuthWsAuthenticator::new(auth.clone())),
            readiness: state.readiness.clone(),
        };
        let static_dir = config.static_dir.clone().or_else(server::default_static_dir);
        let router = server::router_with_fallback(&HttpConfig::from_env(static_dir), &services, api, placeholder_endpoints(auth));

        Ok(Self {
            state,
            environment,
            plugins,
            config_service,
            rpc,
            router,
            listening_port: std::sync::OnceLock::new(),
        })
    }

    /// The startup sequence of `serverRuntimeStartup.ts`, once the listener is bound to `port`
    /// and serving (requests wait on the readiness gate until the end). On failure the gate
    /// fails, so waiting requests get a 500.
    pub async fn startup(&self, port: u16) -> anyhow::Result<()> {
        let _ = self.listening_port.set(port);
        let result = self.startup_inner(port).await;
        if let Err(error) = &result {
            tracing::error!(error = %format!("{error:#}"), "server runtime startup failed");
            self.state.readiness.mark_failed(format!("{error:#}"));
        }
        result
    }

    async fn startup_inner(&self, port: u16) -> anyhow::Result<()> {
        let state = &self.state;
        let config = &state.config;

        // Credentials issued from here on announce the bound address (`getHttpMcpEndpointHost`).
        state.mcp_sessions.set_listen_address(config.host.as_deref(), port);

        tracing::debug!("startup phase: starting keybindings runtime");
        if let Err(error) = state.keybindings.start().await {
            tracing::warn!(?error, "failed to start keybindings runtime");
        }
        tracing::debug!("startup phase: starting server settings runtime");
        if let Err(error) = state.settings.start().await {
            tracing::warn!(?error, "failed to start server settings runtime");
        }

        tracing::debug!("startup phase: reactors.start");
        let _reaper = state.providers.start_reaper();
        for plugin in &self.plugins {
            plugin.start().await.with_context(|| format!("{} failed to start", plugin.name()))?;
        }

        tracing::debug!("startup phase: provider-sessions.reconcile");
        startup::reconcile_provider_sessions(
            &state.engine,
            &state.reads,
            &state.providers.directory,
            &state.providers.service,
            &state.settings,
        )
        .await;
        tracing::debug!("startup phase: worktree-setups.reconcile");
        startup::reconcile_worktree_setups(&state.engine, &state.reads).await;
        tracing::debug!("startup phase: projects.auto-pull");
        if std::env::var("ZENITH_CODE_SKIP_AUTO_PULL").as_deref() == Ok("1") {
            tracing::info!("automatic project pull skipped (ZENITH_CODE_SKIP_AUTO_PULL=1)");
        } else {
            startup::auto_pull_projects(&state.reads, &state.settings, &state.git).await;
        }

        let cwd = config.cwd.to_string_lossy().into_owned();
        let descriptor = self.environment.descriptor_json().clone();
        if config.auto_bootstrap_project_from_cwd {
            let state = state.clone();
            let descriptor = descriptor.clone();
            let cwd = cwd.clone();
            tokio::spawn(async move {
                let mut fields = startup::auto_bootstrap_from_cwd(&state.engine, &state.reads, &state.settings, &cwd).await;
                fields.insert("bootstrapStatus".into(), json!("complete"));
                let project_name = cwd.split(['/', '\\']).rfind(|s| !s.is_empty()).unwrap_or("project").to_owned();
                let mut payload = serde_json::Map::new();
                payload.insert("environment".into(), descriptor);
                payload.insert("cwd".into(), json!(cwd));
                payload.insert("projectName".into(), json!(project_name));
                payload.append(&mut fields);
                state.lifecycle.publish("welcome", Value::Object(payload));
            });
        }

        // The banner (`headless.output`).
        self.print_banner(port).await?;

        // http.wait: the listener is bound and serving before startup begins.
        tracing::debug!("startup phase: welcome.publish");
        state
            .lifecycle
            .publish_welcome(&descriptor, &cwd, if config.auto_bootstrap_project_from_cwd { "pending" } else { "complete" });

        // Activation: the runtime state file the dashboard and the CLI find the server by.
        let runtime_state = make_persisted_server_runtime_state(config.host.as_deref(), config.dev_url.as_deref(), port, false);
        if let Err(error) = persist_server_runtime_state(&config.paths.server_runtime_state_path, &runtime_state).await {
            tracing::warn!(%error, "Failed to persist server runtime state");
        }
        if config.tailscale_serve_enabled && !tailscale_serve(port, config.tailscale_serve_port, true).await {
            // A server started at boot can be up before tailscaled is: keep trying for a while.
            let serve_port = config.tailscale_serve_port;
            tokio::spawn(async move {
                for _ in 0..TAILSCALE_SERVE_RETRIES {
                    tokio::time::sleep(TAILSCALE_SERVE_RETRY_DELAY).await;
                    if tailscale_serve(port, serve_port, true).await {
                        break;
                    }
                }
            });
        }
        tokio::spawn(zc_auth::dpop::run_replay_marker_sweeper(config.paths.secrets_dir.clone()));

        tracing::debug!("Accepting commands");
        state.readiness.mark_ready();
        state.lifecycle.publish_ready(&descriptor);
        tracing::debug!("startup phase: complete");
        Ok(())
    }

    /// `zenith server is ready.` and, unless `ZENITH_NO_STARTUP_TOKEN=1` (zenith.app's
    /// LaunchAgent, which pairs with `auth pairing create`), the connection string, a one-time
    /// administrative token and the pairing URL. No QR code.
    async fn print_banner(&self, port: u16) -> anyhow::Result<()> {
        let config = &self.state.config;
        if config.startup_presentation != StartupPresentation::Headless {
            return Ok(());
        }
        let text = if std::env::var("ZENITH_NO_STARTUP_TOKEN").as_deref() == Ok("1") {
            "zenith server is ready.\n".to_owned()
        } else {
            let connection = connection_string(config.host.as_deref(), port);
            let token = self
                .state
                .auth
                .issue_startup_pairing_credential()
                .await
                .context("could not issue the startup pairing credential")?
                .credential;
            let pairing_url = build_pairing_url(&connection, &token).context("invalid connection string")?;
            format!("zenith server is ready.\nConnection string: {connection}\nToken: {token}\nPairing URL: {pairing_url}\n\n")
        };
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(text.as_bytes())?;
        stdout.flush()?;
        Ok(())
    }

    /// Stops the plugins (in reverse), the providers and the terminals, and removes
    /// `server-runtime.json`.
    pub async fn shutdown(&self) {
        let state = &self.state;
        for plugin in self.plugins.iter().rev() {
            plugin.shutdown().await;
        }
        state.providers.shutdown().await;
        state.terminals.shutdown().await;
        if state.config.tailscale_serve_enabled {
            tailscale_serve(
                self.listening_port.get().copied().unwrap_or(state.config.port),
                state.config.tailscale_serve_port,
                false,
            )
            .await;
        }
        clear_persisted_server_runtime_state(&state.config.paths.server_runtime_state_path).await;
    }
}

/// The VCS stack (`server.ts` `VcsLayerLive`): git driver, registry, the workflow over WP-19's
/// GitManager (status with PRs, stacked actions), broadcaster (auto-pull by project setting),
/// provisioning and review.
fn build_vcs(
    config: &ServerConfig,
    settings: &ServerSettingsService,
    reads: &Arc<dyn ProjectionReads>,
    slots: &Slots,
    git: &GitVcsDriver,
    registry: &VcsDriverRegistry,
    manager: zc_git::GitManager,
) -> VcsRpcServices {
    use zc_vcs::broadcaster::ProjectAutoPullPolicy;
    use zc_vcs::registry::VcsProvisioningService;
    use zc_vcs::workflow::GitWorkflowService;
    use zc_vcs::{ReviewService, VcsStatusBroadcaster};

    let git = git.clone();
    let registry = registry.clone();
    let workflow = GitWorkflowService::new(registry.clone(), git.clone(), Arc::new(manager));
    let settings_port: Arc<dyn zc_ports::SettingsService> = Arc::new(settings.clone());
    let broadcaster = VcsStatusBroadcaster::new(
        Arc::new(workflow.clone()),
        slots.background_policy.clone(),
        Arc::new(ProjectAutoPullPolicy {
            projections: reads.clone(),
            settings: settings_port,
        }),
    );
    let review = ReviewService::new(&config.cwd, &config.paths.worktrees_dir, registry.clone(), git.clone());
    let interval_settings = settings.clone();
    let automatic_git_fetch_interval: zc_vcs::broadcaster::RefreshInterval = Arc::new(move || {
        let settings = interval_settings.clone();
        Box::pin(async move {
            let ms = match settings.get_settings_value().await {
                Ok(value) => zc_settings::settings::background::resolve_server(&value).automatic_git_fetch_interval,
                Err(error) => {
                    tracing::warn!(?error, "Failed to read automatic Git fetch interval setting");
                    zc_settings::settings::background::DEFAULT_AUTOMATIC_GIT_FETCH_INTERVAL_MS
                }
            };
            std::time::Duration::from_millis(ms.max(0.0) as u64)
        })
    });
    VcsRpcServices {
        workflow,
        broadcaster,
        provisioning: VcsProvisioningService::new(registry),
        review,
        automatic_git_fetch_interval,
    }
}

/// Sessions are matched to projects by folder: each live project's root, plus every thread
/// worktree outside it.
struct ProjectionProjectRoots(Arc<dyn ProjectionReads>);

#[async_trait]
impl zc_sessions::ProjectRoots for ProjectionProjectRoots {
    async fn project_roots(&self) -> Vec<zc_sessions::ProjectRoot> {
        let snapshot = match self.0.get_shell_snapshot(false).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::warn!(%error, "could not read the projects for the Sessions page");
                return Vec::new();
            }
        };
        let mut roots: Vec<zc_sessions::ProjectRoot> = snapshot
            .projects
            .iter()
            .map(|project| zc_sessions::ProjectRoot {
                id: project.id.to_string(),
                title: project.title.to_string(),
                workspace_root: project.workspace_root.to_string(),
            })
            .collect();
        for thread in &snapshot.threads {
            let Some(worktree) = thread.worktree_path.as_ref().map(ToString::to_string) else {
                continue;
            };
            let Some(project) = snapshot.projects.iter().find(|project| project.id == thread.project_id) else {
                continue;
            };
            let root = project.workspace_root.to_string();
            let inside = std::path::Path::new(&worktree).starts_with(&root);
            if !inside && !roots.iter().any(|r| r.workspace_root == worktree) {
                roots.push(zc_sessions::ProjectRoot {
                    id: project.id.to_string(),
                    title: project.title.to_string(),
                    workspace_root: worktree,
                });
            }
        }
        roots
    }
}

/// `resolveHeadlessConnectionString` for the hosts zenith uses (wildcards fall back to
/// `localhost`).
pub fn connection_string(host: Option<&str>, port: u16) -> String {
    let host = match host {
        None | Some("") => "localhost".to_owned(),
        Some(host) if is_wildcard_host(Some(host)) => "localhost".to_owned(),
        Some(host) => host.trim_start_matches('[').trim_end_matches(']').to_owned(),
    };
    format!("http://{}:{port}", format_host_for_url(&host))
}

/// How long the server keeps trying to turn Tailscale Serve on after a first failure (five
/// minutes: tailscaled starts, then connects).
const TAILSCALE_SERVE_RETRIES: u32 = 30;
const TAILSCALE_SERVE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(10);

/// `ensureTailscaleServe` / `disableTailscaleServe`, best effort; whether Tailscale took it.
async fn tailscale_serve(local_port: u16, serve_port: u16, enable: bool) -> bool {
    let args: Vec<String> = if enable {
        vec![
            "serve".into(),
            "--bg".into(),
            format!("--https={serve_port}"),
            format!("http://127.0.0.1:{local_port}"),
        ]
    } else {
        vec!["serve".into(), format!("--https={serve_port}"), "off".into()]
    };
    let mut input = zc_core::ProcessRunInput::new("tailscale", args);
    input.timeout = Some(std::time::Duration::from_secs(10));
    match zc_core::run_process(input).await {
        Ok(output) if output.code == Some(0) => {
            tracing::info!(serve_port, enable, "Tailscale Serve updated");
            return true;
        }
        Ok(output) => tracing::warn!(serve_port, enable, code = ?output.code, "Failed to update Tailscale Serve"),
        Err(error) => tracing::warn!(serve_port, enable, %error, "Failed to update Tailscale Serve"),
    }
    false
}

/// Registers a placeholder for every method of the table that has no handler yet, so the
/// per-method scope check applies to all of them and the answer is a per-request `Exit(Die)`.
pub fn register_placeholders(mut builder: RpcRouterBuilder) -> RpcRouterBuilder {
    for spec in METHODS.iter() {
        if builder.is_registered(spec.tag) {
            continue;
        }
        let tag = spec.tag;
        let message = format!("{tag} is not implemented by the Rust server yet");
        builder = match spec.kind {
            RpcKind::Unary => builder.unary_with(tag, rpc_method_options(tag), move |_ctx, _payload| {
                let message = message.clone();
                tracing::info!(method = tag, "rpc method not implemented by the Rust server yet");
                async move { Err::<Value, _>(RpcError::die(message)) }
            }),
            RpcKind::Stream => builder.stream_with(tag, rpc_method_options(tag), move |_ctx, _payload| {
                let message = message.clone();
                tracing::info!(method = tag, "rpc method not implemented by the Rust server yet");
                async move { Err::<futures::stream::Empty<Result<Value, RpcError>>, _>(RpcError::die(message)) }
            }),
        };
    }
    builder
}

/// The authenticated typed endpoints of the non-auth groups, as placeholders: they
/// authenticate like the real ones (401 without a session), then answer a typed 500
/// `internal_error`. Served as a fallback, so any real route of the same path wins.
pub fn placeholder_endpoints(auth: Arc<EnvironmentAuth>) -> Router {
    let mut router = Router::new();
    for spec in ENDPOINTS
        .iter()
        .filter(|spec| spec.authenticated && spec.group != "auth" && spec.errors.contains(&(500, "EnvironmentInternalError")))
    {
        let path = spec
            .path
            .split('/')
            .map(|segment| match segment.strip_prefix(':') {
                Some(name) => format!("{{{name}}}"),
                None => segment.to_owned(),
            })
            .collect::<Vec<_>>()
            .join("/");
        let filter = if spec.method == "GET" { MethodFilter::GET } else { MethodFilter::POST };
        let auth = auth.clone();
        router = router.route(
            &path,
            on(filter, move |request: axum::extract::Request| {
                let auth = auth.clone();
                async move {
                    let (parts, _) = request.into_parts();
                    match zc_auth::http::authenticate(&auth, &parts).await {
                        Ok(_) => EnvironmentError::internal("internal_error").into_response(),
                        Err(response) => response,
                    }
                }
            }),
        );
    }
    // `POST /api/observability/v1/traces` is the telemetry plugin's (WP-31).
    router
}
