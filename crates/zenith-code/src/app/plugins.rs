//! Where the packages plug into the server: **one line each**.
//!
//! Two kinds of plug-in points:
//!
//! 1. [`slots`]: services the core is built *with* (the engine's liveness guard, the
//!    projections' live state and repository identities, the background policy every poller
//!    asks, the editors in `ServerConfig`), and
//!    [`drivers`]: the provider drivers the instance registry is built with. The concrete
//!    services behind the slots that plugins use too live in [`SharedServices`].
//! 2. [`plugins`]: packages built *from* the running core ([`AppState`]); each is a
//!    [`Plugin`] that adds RPC methods, HTTP routes, capabilities, `ServerConfig` fields and
//!    events, dispatch hooks, and work started in the `reactors.start` phase.
//!
//! | Package | Plugs in as |
//! |---|---|
//! | WP-10 reactors | `slots.background_liveness`, `slots.thread_live_state` (its registries); [`super::reactors::ReactorsPlugin`]: `start` (ingestion, provider command reactor, checkpoint reactor, deletion, settlement, storage cleanup, in `OrchestrationReactor` order), a dispatch hook (`drainThrough` after `thread.create`), capabilities `threadAutoSettlement`, `inlineMessageContext` |
//! | WP-11 checkpoints, worktree bootstrap, background policy | `slots.background_policy`; [`super::checkpoints::CheckpointsPlugin`]: `getTurnDiff`/`getFullThreadDiff`, `subscribeWorktreeSetup`/`worktreeSetup.cancel`, background RPCs, a dispatch hook taking over `thread.turn.start` with `bootstrap`, capabilities `requiredWorktreeBootstrap`, `storageCleanup`, `projectWorktreeCleanup` |
//! | WP-13/14 Claude, Codex (WP-15/16 ACP, OpenCode) | [`drivers`] |
//! | WP-17 text generation | [`drivers`] (wrapped by `zc_textgen::with_text_generation`), [`text_generation`] (`AppState::text_generation`, for the reactors and the stacked actions; thread title links resolved by the source control registry) |
//! | WP-19 git (GitManager) | [`git_manager`] (the VCS workflow's backend: status with the branch's PR, stacked actions, PR resolve/prepare, the settlement reactor's branch lookups); [`super::git::GitPlugin`]: `git.runStackedAction`, `git.resolvePullRequest`, `git.preparePullRequestThread` |
//! | WP-20 source control | [`SharedServices::source_control`] (GitManager resolves forges and unknown remotes through its registry); [`super::source_control::SourceControlPlugin`]: `sourceControl.*`, `server.discoverSourceControl` |
//! | WP-21..23 pull requests | `plugins`: `pullRequests.*`, `POST /api/pull-requests/diff`, capabilities `pullRequests`, `threadPullRequests`, `pullRequestStackActions` |
//! | WP-24 workspace search and editors | `slots.editors`; [`super::workspace::WorkspacePlugin`]: `projects.*`, `filesystem.browse`, `shell.openInEditor` |
//! | WP-25 project (clone, favicon, agent sessions) | `slots.repository_identities` (Forgejo refinement through `SharedServices::source_control`; refreshed after a clone and after `sourceControl.publishRepository`), then capability `repositoryIdentity`; `plugins`: `projectClone.*` (cloning through the shared repository service), a dispatch hook (clone guard), `agentSessions.*`, capability `projectCloneTracking` |
//! | WP-27 MCP | `plugins`: `/mcp` route, `previewAutomation.*` (the registry is core: `AppState::mcp_sessions`, the provider service's hook and `DriverEnv::mcp_sessions`) |
//! | WP-28 preview | `plugins`: `preview.*`, `subscribePreviewEvents`, `subscribeDiscoveredLocalServers` (port discovery is core: `AppState::port_discovery`, the terminals' process registry) |
//! | WP-29 assets | `plugins`: `/api/assets/*`, `/api/attachments/upload/*`, `assets.*`, `attachments.*`, capabilities `attachmentUploads`, `fileAttachments`, `questionAttachments` |
//! | WP-30 usage | `plugins`: `server.getUsageSummary`, `server.refreshUsageRates`, a config source (`usageLimitSourcesUpdated`), capabilities `usageLimitSources`, `usagePriceOverrides` |
//! | WP-31 telemetry | [`telemetry::TelemetryPlugin`]: `server.getTraceDiagnostics|getProcessDiagnostics|getHostResources|getProcessResourceHistory|getResourceTelemetryHistory|retryResourceTelemetry|signalProcess`, `subscribeResourceTelemetry`, `POST /api/observability/v1/traces`, `ServerConfig.observability`; the trace file writer |
//!
//! A plugin's RPC methods take precedence over the "not implemented" placeholders, and its
//! routes over the placeholder typed endpoints, so nothing else needs to change.

mod assets;
mod telemetry;

use std::sync::{Arc, Mutex};

use zc_checkpoints::{BackgroundPolicyService, HostPowerMonitor, ReactorTasks};
use zc_providers::{Driver, DriverEnv};
use zc_reactors::{ThreadBackgroundLivenessRegistry, ThreadPlanProgressRegistry};
use zc_sourcecontrol::{SourceControl, SourceControlDeps};
use zc_workspace::{ExternalLauncher, FffFactory, SearchIndexMap, WorkspaceEntries, WorkspacePaths};

use super::reactors::ReactorLiveState;
use super::workspace::LauncherEditors;
use super::{AppState, Foundation, Plugin, Slots};

/// The concrete services behind the slots, which the plugins also use.
pub struct SharedServices {
    /// WP-10: `ThreadBackgroundLivenessService` (the ingestion writes it, the engine and the
    /// thread shells read it).
    pub liveness: Arc<ThreadBackgroundLivenessRegistry>,
    /// WP-10: `ThreadPlanProgressService`.
    pub plan_progress: Arc<ThreadPlanProgressRegistry>,
    /// WP-11: `HostPowerMonitor` (no desktop telemetry: reports come over RPC).
    pub host_power: Arc<HostPowerMonitor>,
    /// WP-11: `BackgroundPolicy`.
    pub background: BackgroundPolicyService,
    /// WP-20: every source control service.
    pub source_control: SourceControl,
    /// WP-24: the workspace file index (also refreshed by the checkpoint reactor).
    pub workspace_entries: WorkspaceEntries,
    /// WP-24: editors and the file manager.
    pub launcher: Arc<ExternalLauncher>,
    /// The background policy's watchers (dropping them stops them).
    background_tasks: Mutex<Option<ReactorTasks>>,
}

impl SharedServices {
    /// Stops the background policy's watchers.
    pub fn stop(&self) {
        self.background_tasks.lock().unwrap_or_else(|p| p.into_inner()).take();
    }
}

/// The services the core is built with.
pub fn slots(foundation: &Foundation) -> Slots {
    let settings: Arc<dyn zc_ports::SettingsService> = Arc::new(foundation.settings.clone());

    let liveness = Arc::new(ThreadBackgroundLivenessRegistry::new());
    let plan_progress = Arc::new(ThreadPlanProgressRegistry::new());

    let host_power = Arc::new(HostPowerMonitor::new(None));
    let background = BackgroundPolicyService::new(host_power.clone(), settings.clone());
    let background_tasks = background.start();

    let source_control = SourceControl::new(SourceControlDeps {
        cwd: foundation.config.cwd.to_string_lossy().into_owned(),
        process: foundation.vcs_process.clone(),
        git: foundation.git.clone(),
        vcs: foundation.vcs_registry.clone(),
        bitbucket_credentials: Arc::new(zc_sourcecontrol::bitbucket::SettingsPortCredentials(settings.clone())),
        bitbucket_config: zc_sourcecontrol::bitbucket::BitbucketApiConfig::from_env(),
        forgejo_environment: zc_sourcecontrol::forgejo::ForgejoEnvironment::from_process(),
        clock: zc_sourcecontrol::util::system_clock(),
    });

    let workspace_entries = WorkspaceEntries::new(
        WorkspacePaths::new(),
        SearchIndexMap::new(Arc::new(FffFactory)),
        Arc::new(foundation.vcs_process.clone()),
    );
    let launcher = Arc::new(ExternalLauncher::default());

    let shared = Arc::new(SharedServices {
        liveness: liveness.clone(),
        plan_progress: plan_progress.clone(),
        host_power,
        background: background.clone(),
        source_control: source_control.clone(),
        workspace_entries,
        launcher: launcher.clone(),
        background_tasks: Mutex::new(Some(background_tasks)),
    });

    Slots {
        background_policy: Arc::new(background),
        background_liveness: liveness.clone(),
        thread_live_state: Arc::new(ReactorLiveState { liveness, plan_progress }),
        repository_identities: super::project::repository_identities(&source_control),
        editors: Arc::new(LauncherEditors(launcher)),
        shared,
    }
}

/// The provider drivers, in `BUILT_IN_DRIVER_KINDS` order (codex, claudeAgent, cursor, grok,
/// opencode, antigravity). One line per driver crate.
///
/// WP-17: `with_text_generation` gives every instance the text generator its driver kind has
/// in TS (Claude and Codex one-shot CLI calls; ACP and OpenCode answer "not supported yet").
pub fn drivers(env: &DriverEnv) -> Vec<Arc<dyn Driver>> {
    let drivers: Vec<Arc<dyn Driver>> = vec![
        // WP-27: each turn hands the agent its thread's `t3-code` MCP session.
        Arc::new(
            zc_provider_codex::CodexProviderDriver::new(env.clone()).with_mcp_sessions(env.mcp_sessions.clone().map(super::mcp::CodexMcpSessions::shared)),
        ),
        Arc::new(
            zc_provider_claude::ClaudeProviderDriver::new(env.clone()).with_mcp_sessions(env.mcp_sessions.clone().map(super::mcp::ClaudeMcpSessions::shared)),
        ),
    ];
    zc_textgen::with_text_generation(drivers, env)
}

/// WP-17: the server's `TextGeneration`, routed by `modelSelection.instanceId` through the
/// instance registry; WP-20's source control registry resolves the links in thread titles.
pub fn text_generation(providers: &super::providers::ProviderStack, slots: &Slots) -> Arc<dyn zc_ports::TextGeneration> {
    let links: Arc<dyn zc_textgen::links::ThreadTitleLinkResolver> = Arc::new(slots.shared.source_control.registry.clone());
    Arc::new(zc_textgen::TextGenerationService::from_registry(providers.instances.clone(), Some(links)))
}

/// WP-19: GitManager over the source control registry, text generation (the writer model of
/// the settings), the provider statuses, the projections (project overrides) and the setup
/// script runner (PR worktrees).
pub fn git_manager(
    git: &zc_vcs::GitVcsDriver,
    slots: &Slots,
    providers: &super::providers::ProviderStack,
    text_generation: &Arc<dyn zc_ports::TextGeneration>,
    reads: &Arc<dyn zc_ports::ProjectionReads>,
    terminals: &Arc<dyn zc_ports::TerminalManager>,
    settings: &Arc<dyn zc_ports::SettingsService>,
) -> zc_git::GitManager {
    zc_git::GitManager::new(zc_git::GitManagerDeps {
        git: git.clone(),
        providers: Arc::new(slots.shared.source_control.registry.clone()),
        text_generation: text_generation.clone(),
        settings: zc_git::SettingsSources {
            settings: settings.clone(),
            provider_status: Arc::new(providers.registry.clone()),
            projections: Some(reads.clone()),
        },
        setup_scripts: Some(Arc::new(super::git::SetupScripts(zc_checkpoints::setup_script::ProjectSetupScriptRunner::new(
            reads.clone(),
            terminals.clone(),
            settings.clone(),
        )))),
        temp_dir: zc_git::GitManagerDeps::default_temp_dir(),
        uuids: Arc::new(zc_core::uuid_v4),
    })
}

/// The packages built from the running core. One line per package.
pub fn plugins(state: &Arc<AppState>) -> Vec<Arc<dyn Plugin>> {
    let pull_requests = Arc::new(super::pull_requests::PullRequestsPlugin::new(state));
    let reactors = Arc::new(super::reactors::ReactorsPlugin::new(state, pull_requests.port()));
    let checkpoints = Arc::new(super::checkpoints::CheckpointsPlugin::new(
        state,
        reactors.deletion_drain(),
        pull_requests.port(),
    ));
    reactors.add_external(checkpoints.external_reactors());
    reactors.add_external(pull_requests.external_reactors());
    vec![
        reactors,
        checkpoints,
        pull_requests,
        Arc::new(super::workspace::WorkspacePlugin::new(state)),
        Arc::new(super::source_control::SourceControlPlugin::new(state)),
        Arc::new(super::git::GitPlugin::new(state)),
        Arc::new(assets::AssetsPlugin::new(state)),
        Arc::new(super::project::ProjectPlugin::new(state)),
        Arc::new(super::usage::UsagePlugin::new(state)),
        Arc::new(super::mcp::McpPlugin::new(state)),
        Arc::new(super::preview::PreviewPlugin::new(state)),
        Arc::new(telemetry::TelemetryPlugin::new(state)),
    ]
}
