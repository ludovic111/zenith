//! WP-10 (zc-reactors) in the server: the orchestration reactors of
//! `OrchestrationReactor.ts`, built over the real services, plus the ports they need that no
//! package implements yet (provider auth commands, pull requests).
//!
//! | Reactor | Built from |
//! |---|---|
//! | provider runtime ingestion | the engine, [`ProjectionReactorReads`], the provider service, settings, the checkpoint store as repository probe, the liveness / plan-progress registries ([`super::plugins::SharedServices`]) |
//! | provider command reactor | the same, plus the provider registry (status and workspace snapshots), the git workflow, VCS status, terminals, text generation (WP-17, titles and branch names); no provider auth commands yet (WP-12b) |
//! | checkpoint reactor, storage cleanup | WP-11 ([`super::checkpoints::CheckpointsPlugin::external_reactors`]), started in their slots |
//! | thread deletion | the engine, the provider service, terminals; `drainThrough` after `thread.create` ([`DeletionFence`]) |
//! | thread settlement | the engine, reads, settings, the git workflow, the pull request service (WP-21, [`super::pull_requests::PullRequestsPlugin::port`]) |

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use zc_checkpoints::{CheckpointStore, ThreadDeletionDrain, VcsCheckpointStore};
use zc_contracts::{OrchestrationCommand, ProjectId, ProviderInstanceId};
use zc_ports::contracts::{
    ProviderSetupError, PullRequestDiffInput, PullRequestDiffResult, PullRequestInvalidateInput, PullRequestRef, PullRequestStack, PullRequestSummary,
};
use zc_ports::pull_requests::PullRequestMergeEvent;
use zc_ports::{
    DispatchResult, EventStream, GitWorkflow, OrchestrationDispatch, ProviderAuthCommands, ProviderService, PullRequests, SettingsService, TaggedError,
    TerminalManager, VcsStatusRefresher,
};
use zc_projections::ThreadLiveState;
use zc_reactors::reactor::ExternalReactor;
use zc_reactors::settlement::{fs_path_exists, SWEEP_INTERVAL};
use zc_reactors::{
    CommandReactorDeps, IngestionDeps, OrchestrationReactor, ProjectionReactorReads, ProviderCommandReactor, ProviderRuntimeIngestion, ReactorClock,
    ReactorReads, ReactorSlot, RepositoryProbe, SettlementDeps, SystemClock, ThreadBackgroundLivenessRegistry, ThreadDeletionReactor,
    ThreadPlanProgressRegistry, ThreadSettlementReactor,
};

use super::environment::Capabilities;
use super::orchestration::DispatchHook;
use super::{AppState, Plugin};

/// `ThreadBackgroundLivenessService` + `ThreadPlanProgressService` as the projections read them
/// on thread shells.
pub struct ReactorLiveState {
    pub liveness: Arc<ThreadBackgroundLivenessRegistry>,
    pub plan_progress: Arc<ThreadPlanProgressRegistry>,
}

impl ThreadLiveState for ReactorLiveState {
    fn background_liveness(&self, thread_id: &str) -> Option<String> {
        let state = self.liveness.get_thread_background_liveness(thread_id)?;
        serde_json::to_value(state).ok()?.as_str().map(str::to_owned)
    }

    fn plan_progress(&self, thread_id: &str) -> Option<Value> {
        self.plan_progress
            .get_thread_plan_progress(thread_id)
            .and_then(|progress| serde_json::to_value(progress).ok())
    }
}

/// `ProviderAuthService.tryHandlePromptCommand` until WP-12b: no prompt is an auth command, so
/// `/login` and friends go to the provider like any text.
pub struct NoProviderAuthCommands;

#[async_trait]
impl ProviderAuthCommands for NoProviderAuthCommands {
    async fn try_handle_prompt_command(&self, _: &ProviderInstanceId, _: &str, _: bool) -> Result<bool, ProviderSetupError> {
        Ok(false)
    }
}

/// `PullRequestService` until WP-21: nothing is known about pull requests, no merge is ever
/// seen, refreshes are no-ops.
pub struct NoPullRequests;

fn pull_requests_unavailable() -> TaggedError {
    TaggedError::new("PullRequestUnavailableError", "Pull requests are not implemented by the Rust server yet.")
}

#[async_trait]
impl PullRequests for NoPullRequests {
    async fn summary(&self, _: PullRequestRef, _: bool) -> Result<PullRequestSummary, TaggedError> {
        Err(pull_requests_unavailable())
    }
    async fn stack(&self, _: PullRequestRef, _: bool) -> Result<Option<PullRequestStack>, TaggedError> {
        Ok(None)
    }
    async fn diff(&self, _: PullRequestDiffInput) -> Result<PullRequestDiffResult, TaggedError> {
        Err(pull_requests_unavailable())
    }
    async fn invalidate(&self, _: PullRequestInvalidateInput, _: bool) {}
    async fn refresh_after_turn(&self, _: &ProjectId) {}
    fn subscribe_merges(&self) -> EventStream<PullRequestMergeEvent> {
        futures::stream::pending().boxed()
    }
    fn subscribe_refreshes(&self) -> EventStream<u64> {
        futures::stream::pending().boxed()
    }
}

/// `CheckpointStore.isGitRepository` as the ingestion's repository probe (TS reads the
/// checkpoint store).
pub struct CheckpointStoreProbe(pub Arc<dyn CheckpointStore>);

#[async_trait]
impl RepositoryProbe for CheckpointStoreProbe {
    async fn is_git_repository(&self, cwd: &str) -> Result<bool, TaggedError> {
        self.0
            .is_git_repository(cwd)
            .await
            .map_err(|error| TaggedError::new("CheckpointStoreError", error.to_string()))
    }
}

/// `ThreadDeletionReactor.drainThrough` for the worktree bootstrap and storage cleanup.
pub struct DeletionDrain(pub Arc<ThreadDeletionReactor>);

#[async_trait]
impl ThreadDeletionDrain for DeletionDrain {
    async fn drain_through(&self, sequence: i64) {
        self.0.drain_through(sequence).await;
    }
}

/// `ws.ts`: returning from `thread.create` is the handoff point at which clients may start
/// resources for the new incarnation, so wait for the deletion cleanup of every earlier
/// incarnation (`threadDeletionReactor.drainThrough(sequence)`).
pub struct DeletionFence(pub Arc<ThreadDeletionReactor>);

#[async_trait]
impl DispatchHook for DeletionFence {
    async fn after(&self, command: &OrchestrationCommand, result: &DispatchResult) {
        if matches!(command, OrchestrationCommand::ClientOrchestrationCommandThreadCreate(_)) {
            self.0.drain_through(result.sequence).await;
        }
    }
}

/// The reactors (`OrchestrationReactor`), started in the `reactors.start` phase.
pub struct ReactorsPlugin {
    ingestion: Arc<ProviderRuntimeIngestion>,
    command_reactor: Arc<ProviderCommandReactor>,
    deletion: Arc<ThreadDeletionReactor>,
    settlement: Arc<ThreadSettlementReactor>,
    external: Mutex<Vec<(ReactorSlot, Arc<dyn ExternalReactor>)>>,
    stop: CancellationToken,
}

impl ReactorsPlugin {
    pub fn new(state: &Arc<AppState>, pull_requests: Arc<dyn PullRequests>) -> Self {
        let stop = CancellationToken::new();
        let engine: Arc<dyn OrchestrationDispatch> = Arc::new(state.engine.clone());
        let reads: Arc<dyn ReactorReads> = Arc::new(ProjectionReactorReads::new(state.reads.clone(), state.db.clone()));
        let providers: Arc<dyn ProviderService> = Arc::new(state.providers.service.clone());
        let settings: Arc<dyn SettingsService> = Arc::new(state.settings.clone());
        let git: Arc<dyn GitWorkflow> = Arc::new(state.vcs.workflow.clone());
        let vcs_status: Arc<dyn VcsStatusRefresher> = Arc::new(state.vcs.broadcaster.clone());
        let terminals: Arc<dyn TerminalManager> = Arc::new(state.terminals.clone());
        let clock: Arc<dyn ReactorClock> = Arc::new(SystemClock);
        let uuids = zc_reactors::common::system_uuids();

        let ingestion = Arc::new(ProviderRuntimeIngestion::new(
            IngestionDeps {
                engine: engine.clone(),
                reads: reads.clone(),
                providers: providers.clone(),
                settings: settings.clone(),
                repositories: Arc::new(CheckpointStoreProbe(Arc::new(VcsCheckpointStore::new(state.vcs_registry.clone())))),
                liveness: state.shared.liveness.clone(),
                plan_progress: state.shared.plan_progress.clone(),
                clock: clock.clone(),
                uuids: uuids.clone(),
            },
            stop.child_token(),
        ));
        let command_reactor = Arc::new(ProviderCommandReactor::new(
            CommandReactorDeps {
                engine: engine.clone(),
                reads: reads.clone(),
                providers: providers.clone(),
                provider_status: Arc::new(state.providers.registry.clone()),
                provider_auth: Arc::new(NoProviderAuthCommands),
                workspace_snapshots: Some(Arc::new(state.providers.registry.clone())),
                git: git.clone(),
                vcs_status,
                text_generation: state.text_generation.clone(),
                settings: settings.clone(),
                terminals: terminals.clone(),
                clock: clock.clone(),
                uuids: uuids.clone(),
                path_exists: fs_path_exists(),
                title_retry_base: Duration::from_secs(2),
            },
            stop.child_token(),
        ));
        let deletion = Arc::new(ThreadDeletionReactor::new(engine.clone(), providers, terminals, stop.child_token()));
        let settlement = Arc::new(ThreadSettlementReactor::new(
            SettlementDeps {
                engine,
                reads,
                settings,
                git,
                pull_requests,
                clock,
                uuids,
                path_exists: fs_path_exists(),
                interval: Some(SWEEP_INTERVAL),
            },
            stop.child_token(),
        ));
        Self {
            ingestion,
            command_reactor,
            deletion,
            settlement,
            external: Mutex::new(Vec::new()),
            stop,
        }
    }

    /// The deletion reactor's drain, for the packages that wait on it.
    pub fn deletion_drain(&self) -> Arc<dyn ThreadDeletionDrain> {
        Arc::new(DeletionDrain(self.deletion.clone()))
    }

    /// Reactors of other packages, started in their slot.
    pub fn add_external(&self, reactors: Vec<(ReactorSlot, Arc<dyn ExternalReactor>)>) {
        self.external.lock().unwrap_or_else(|p| p.into_inner()).extend(reactors);
    }
}

#[async_trait]
impl Plugin for ReactorsPlugin {
    fn name(&self) -> &'static str {
        "reactors"
    }

    fn capabilities(&self, capabilities: &mut Capabilities) {
        // The settlement reactor settles threads on its own; the command reactor folds a
        // message's inline context into the provider prompt and regenerates titles (WP-17).
        capabilities.enable(&["threadAutoSettlement", "inlineMessageContext", "threadTitleRegeneration"]);
    }

    fn dispatch_hooks(&self) -> Vec<Arc<dyn DispatchHook>> {
        vec![Arc::new(DeletionFence(self.deletion.clone()))]
    }

    async fn start(&self) -> anyhow::Result<()> {
        let reactor = OrchestrationReactor {
            ingestion: Some(self.ingestion.clone()),
            command_reactor: Some(self.command_reactor.clone()),
            deletion: Some(self.deletion.clone()),
            settlement: Some(self.settlement.clone()),
            external: self.external.lock().unwrap_or_else(|p| p.into_inner()).clone(),
        };
        let started = reactor.start().await;
        tracing::debug!(?started, "orchestration reactors started");
        Ok(())
    }

    async fn shutdown(&self) {
        self.ingestion.stop();
        self.command_reactor.stop();
        self.deletion.stop();
        self.settlement.stop();
        self.stop.cancel();
    }
}
