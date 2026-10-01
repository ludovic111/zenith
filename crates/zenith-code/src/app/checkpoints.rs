//! WP-11 (zc-checkpoints) in the server: turn diffs, the worktree bootstrap of
//! `thread.turn.start`, the worktree setup tracker, setup scripts, the checkpoint reactor,
//! storage cleanup and the background policy RPCs.
//!
//! The background policy itself is a slot ([`super::plugins::SharedServices::background`]):
//! every poller asks it. The checkpoint reactor and storage cleanup start in their
//! `OrchestrationReactor` slots (see [`CheckpointsPlugin::external_reactors`]).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use zc_checkpoints::rpc::{BackgroundRpc, CheckpointRpcServices};
use zc_checkpoints::storage_cleanup::{StorageCleanupDeps, StorageCleanupPaths};
use zc_checkpoints::{
    BootstrapDeps, BootstrapDispatcher, CheckpointDiffQuery, CheckpointReactor, CheckpointReactorDeps, ProjectSetupScriptRunner, ReactorTasks,
    RuntimeReceiptBus, StorageCleanup, ThreadDeletionDrain, VcsCheckpointStore, WorkspaceEntriesRefresher, WorktreeSetupTracker,
};
use zc_contracts::{OrchestrationClientOrigin, OrchestrationCommand};
use zc_ports::{GitWorkflow, OrchestrationDispatch, ProviderService, SettingsService, TerminalManager, VcsStatusRefresher};
use zc_reactors::reactor::ExternalReactor;
use zc_reactors::ReactorSlot;
use zc_rpc::RpcRouterBuilder;
use zc_workspace::WorkspaceEntries;

use super::environment::Capabilities;
use super::orchestration::{DispatchHook, HookResult};
use super::{AppState, Plugin};

/// `WorkspaceEntries.refresh(cwd)` after a checkpoint (the @-mention index).
pub struct EntriesRefresher(pub WorkspaceEntries);

#[async_trait]
impl WorkspaceEntriesRefresher for EntriesRefresher {
    async fn refresh(&self, cwd: &str) -> Result<(), String> {
        self.0.refresh(cwd).await;
        Ok(())
    }
}

/// `dispatchBootstrapTurnStart`: a `thread.turn.start` carrying a `bootstrap` (create the
/// thread, prepare its worktree, run the setup script, then start the turn).
pub struct BootstrapHook(pub BootstrapDeps);

#[async_trait]
impl DispatchHook for BootstrapHook {
    async fn dispatch(&self, command: &OrchestrationCommand, origin: Option<&OrchestrationClientOrigin>) -> Option<HookResult> {
        let OrchestrationCommand::ThreadTurnStartCommand(turn) = command else {
            return None;
        };
        turn.bootstrap.as_ref()?;
        let dispatcher = BootstrapDispatcher::new(self.0.clone(), origin.cloned());
        Some(dispatcher.dispatch_bootstrap_turn_start(turn.clone()).await)
    }
}

/// The checkpoint reactor in its slot (its subscriptions live as long as the plugin).
struct CheckpointReactorSlot {
    reactor: CheckpointReactor,
    tasks: Mutex<Option<ReactorTasks>>,
}

#[async_trait]
impl ExternalReactor for CheckpointReactorSlot {
    async fn start(&self) {
        let tasks = self.reactor.start();
        *self.tasks.lock().unwrap_or_else(|p| p.into_inner()) = Some(tasks);
    }
}

/// Storage cleanup in its slot.
struct StorageCleanupSlot {
    cleanup: StorageCleanup,
    tasks: Mutex<Option<ReactorTasks>>,
}

#[async_trait]
impl ExternalReactor for StorageCleanupSlot {
    async fn start(&self) {
        if std::env::var("ZENITH_CODE_SKIP_STORAGE_CLEANUP").as_deref() == Ok("1") {
            tracing::info!("storage cleanup skipped (ZENITH_CODE_SKIP_STORAGE_CLEANUP=1)");
            return;
        }
        let tasks = self.cleanup.start().await;
        *self.tasks.lock().unwrap_or_else(|p| p.into_inner()) = Some(tasks);
    }
}

/// WP-11's plugin.
pub struct CheckpointsPlugin {
    state: Arc<AppState>,
    rpc: CheckpointRpcServices,
    bootstrap: BootstrapDeps,
    reactor: Arc<CheckpointReactorSlot>,
    cleanup: Arc<StorageCleanupSlot>,
}

impl CheckpointsPlugin {
    pub fn new(state: &Arc<AppState>, thread_deletion: Arc<dyn ThreadDeletionDrain>, pull_requests: Arc<dyn zc_ports::PullRequests>) -> Self {
        let engine: Arc<dyn OrchestrationDispatch> = Arc::new(state.engine.clone());
        let providers: Arc<dyn ProviderService> = Arc::new(state.providers.service.clone());
        let settings: Arc<dyn SettingsService> = Arc::new(state.settings.clone());
        let git: Arc<dyn GitWorkflow> = Arc::new(state.vcs.workflow.clone());
        let vcs_status: Arc<dyn VcsStatusRefresher> = Arc::new(state.vcs.broadcaster.clone());
        let terminals: Arc<dyn TerminalManager> = Arc::new(state.terminals.clone());
        let store: Arc<dyn zc_checkpoints::CheckpointStore> = Arc::new(VcsCheckpointStore::new(state.vcs_registry.clone()));
        let tracker = WorktreeSetupTracker::new();
        let paths = &state.config.paths;

        let rpc = CheckpointRpcServices {
            diff_query: CheckpointDiffQuery::new(state.reads.clone(), store.clone()),
            worktree_setup: tracker.clone(),
            background: BackgroundRpc::new(state.shared.background.clone()),
        };
        let reactor = CheckpointReactor::new(CheckpointReactorDeps {
            engine: engine.clone(),
            projections: state.reads.clone(),
            providers: providers.clone(),
            store,
            receipts: RuntimeReceiptBus::live(),
            workspace_entries: Arc::new(EntriesRefresher(state.shared.workspace_entries.clone())),
            vcs_status: vcs_status.clone(),
            pull_requests,
        });
        let cleanup = StorageCleanup::new(StorageCleanupDeps {
            paths: StorageCleanupPaths {
                worktrees_dir: paths.worktrees_dir.clone(),
                browser_artifacts_dir: paths.browser_artifacts_dir.clone(),
                logs_dir: paths.logs_dir.clone(),
            },
            settings: settings.clone(),
            projections: state.reads.clone(),
            engine: engine.clone(),
            thread_deletion: thread_deletion.clone(),
            providers,
            git: Arc::new(state.git.clone()),
            git_manager: git.clone(),
            terminals: terminals.clone(),
            clock: Arc::new(zc_core::time::now_millis),
            file_check_hook: None,
        });
        let bootstrap = BootstrapDeps {
            engine,
            projections: state.reads.clone(),
            git,
            vcs_status,
            settings: settings.clone(),
            terminals: terminals.clone(),
            setup_scripts: Arc::new(ProjectSetupScriptRunner::new(state.reads.clone(), terminals, settings)),
            tracker,
            thread_deletion,
        };
        Self {
            state: state.clone(),
            rpc,
            bootstrap,
            reactor: Arc::new(CheckpointReactorSlot {
                reactor,
                tasks: Mutex::new(None),
            }),
            cleanup: Arc::new(StorageCleanupSlot {
                cleanup,
                tasks: Mutex::new(None),
            }),
        }
    }

    /// The checkpoint reactor and storage cleanup, for the `OrchestrationReactor` start order.
    pub fn external_reactors(&self) -> Vec<(ReactorSlot, Arc<dyn ExternalReactor>)> {
        vec![
            (ReactorSlot::CheckpointReactor, self.reactor.clone() as Arc<dyn ExternalReactor>),
            (ReactorSlot::StorageCleanup, self.cleanup.clone() as Arc<dyn ExternalReactor>),
        ]
    }
}

#[async_trait]
impl Plugin for CheckpointsPlugin {
    fn name(&self) -> &'static str {
        "checkpoints"
    }

    fn capabilities(&self, capabilities: &mut Capabilities) {
        capabilities.enable(&["requiredWorktreeBootstrap", "storageCleanup", "projectWorktreeCleanup"]);
    }

    fn register_rpc(&self, builder: RpcRouterBuilder) -> RpcRouterBuilder {
        zc_checkpoints::rpc::register(builder, self.rpc.clone())
    }

    fn dispatch_hooks(&self) -> Vec<Arc<dyn DispatchHook>> {
        vec![Arc::new(BootstrapHook(self.bootstrap.clone()))]
    }

    fn connection_closed(&self, connection_id: u64) {
        let background = self.rpc.background.clone();
        tokio::spawn(async move { background.connection_closed(connection_id).await });
    }

    async fn shutdown(&self) {
        self.reactor.tasks.lock().unwrap_or_else(|p| p.into_inner()).take();
        self.cleanup.tasks.lock().unwrap_or_else(|p| p.into_inner()).take();
        self.state.shared.stop();
    }
}
