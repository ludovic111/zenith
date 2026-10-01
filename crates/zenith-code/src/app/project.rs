//! WP-25 plugged in (zc-project): repository identities on project shells (the
//! `repository_identities` slot, capability `repositoryIdentity`), project clones
//! (`projectClone.*`, `subscribeProjectClones`, the clone guard on dispatch, capability
//! `projectCloneTracking`) and the import of external agent sessions (`agentSessions.*`).
//!
//! The clone hooks dispatch through the client dispatcher with the requesting connection's
//! origin, like `ws.ts` (`project.create` with `createWorkspaceRootIfMissing`, then after git:
//! a refreshed identity, `project.meta.update` to re-emit the shell, a git status refresh).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use zc_contracts::{ClientOrchestrationCommand, OrchestrationCommand, OrchestrationDispatchCommandError, ProjectId};
use zc_ports::DispatchResult;
use zc_project::clone::{ClonedProject, ProjectCloneHooks};
use zc_project::sessions::{AgentSessionImporter, AgentSessionScanner, ScannerConfig};
use zc_project::{ForgejoIdentityRefiner, ProjectCloneTracker, ProjectRpcServices, RepositoryIdentities, RepositoryIdentityOptions};
use zc_rpc::{RequestContext, RpcRouterBuilder};
use zc_sourcecontrol::SourceControl;

use super::environment::Capabilities;
use super::orchestration::{connection_origin, dispatch_failed, ClientDispatcher, DispatchHook};
use super::{AppState, Plugin};

/// The `repository_identities` slot: git identities behind the 15 min / 1 min caches, with the
/// Forgejo web-URL refinement of `server.ts` (through the server's one source control
/// provider registry, [`super::plugins::SharedServices::source_control`]).
pub fn repository_identities(source_control: &SourceControl) -> Arc<RepositoryIdentities> {
    Arc::new(RepositoryIdentities::new(RepositoryIdentityOptions {
        refine: Some(Arc::new(ForgejoIdentityRefiner(source_control.registry.clone()))),
        ..RepositoryIdentityOptions::default()
    }))
}

/// The project package built from the running core.
pub struct ProjectPlugin {
    state: Arc<AppState>,
    clones: ProjectCloneTracker,
    scanner: AgentSessionScanner,
    importer: AgentSessionImporter,
}

impl ProjectPlugin {
    pub fn new(state: &Arc<AppState>) -> Self {
        let config = &state.config;
        // The clone tracker clones through the shared repository service (WP-20).
        let repositories = state.shared.source_control.repositories.clone();
        let scanner = AgentSessionScanner::new(
            ScannerConfig::new(&config.base_dir, &config.paths.worktrees_dir),
            Arc::new(state.settings.clone()),
            state.reads.clone(),
        );
        let importer = AgentSessionImporter::new(
            scanner.clone(),
            Arc::new(state.engine.clone()),
            state.reads.clone(),
            state.providers.directory.clone(),
        );
        Self {
            state: state.clone(),
            clones: ProjectCloneTracker::new(Arc::new(repositories)),
            scanner,
            importer,
        }
    }

    pub fn clones(&self) -> &ProjectCloneTracker {
        &self.clones
    }
}

/// The hooks of one `projectClone.start` (dispatching with the connection's origin).
struct CloneHooks {
    state: Arc<AppState>,
    origin: Option<zc_contracts::OrchestrationClientOrigin>,
}

async fn dispatch_client(
    dispatcher: &ClientDispatcher,
    command: Value,
    origin: Option<zc_contracts::OrchestrationClientOrigin>,
) -> Result<DispatchResult, OrchestrationDispatchCommandError> {
    let command: ClientOrchestrationCommand =
        serde_json::from_value(command).map_err(|error| dispatch_failed("Invalid orchestration command", "SchemaError", &error.to_string()))?;
    dispatcher.dispatch(command, origin).await.map_err(|failure| failure.into_error())
}

#[async_trait]
impl ProjectCloneHooks for CloneHooks {
    async fn create_project(&self, project: ClonedProject) -> Result<(), OrchestrationDispatchCommandError> {
        let command = json!({
            "type": "project.create",
            "commandId": zc_core::ids::server_command_id("project-clone-create"),
            "projectId": project.project_id,
            "title": project.title,
            "workspaceRoot": project.workspace_root,
            "createWorkspaceRootIfMissing": true,
            "createdAt": project.created_at,
        });
        dispatch_client(&self.state.dispatcher, command, self.origin.clone()).await.map(|_| ())
    }

    async fn on_cloned(&self, project_id: &ProjectId, workspace_root: &str) {
        // The project was created on an empty directory, so its cached identity is "not a
        // repository" until this refresh; re-emitting the shell carries the new one to clients.
        self.state.repository_identities.refresh(workspace_root).await;
        let command = json!({
            "type": "project.meta.update",
            "commandId": zc_core::ids::server_command_id("project-clone-done"),
            "projectId": project_id,
        });
        if let Err(error) = dispatch_client(&self.state.dispatcher, command, self.origin.clone()).await {
            tracing::warn!(project_id = %project_id, message = %error.message, "could not re-emit the cloned project");
        }
        let broadcaster = self.state.vcs.broadcaster.clone();
        let cwd = workspace_root.to_owned();
        tokio::spawn(async move {
            if let Err(error) = broadcaster.refresh_status(&cwd).await {
                tracing::warn!(?error, "could not refresh the git status of a cloned project");
            }
        });
    }
}

/// `rejectCommandsDuringClone` before every client dispatch, `discardCloneForDeletedProject`
/// after it.
struct CloneGuard(ProjectCloneTracker);

#[async_trait]
impl DispatchHook for CloneGuard {
    async fn before(&self, command: &ClientOrchestrationCommand) -> Result<(), OrchestrationDispatchCommandError> {
        let encoded = serde_json::to_value(command).unwrap_or(Value::Null);
        zc_project::reject_commands_during_clone(&self.0, &encoded)
    }

    async fn after(&self, command: &OrchestrationCommand, _result: &DispatchResult) {
        let encoded = serde_json::to_value(command).unwrap_or(Value::Null);
        zc_project::discard_clone_for_deleted_project(&self.0, &encoded).await;
    }
}

#[async_trait]
impl Plugin for ProjectPlugin {
    fn name(&self) -> &'static str {
        "project"
    }

    fn capabilities(&self, capabilities: &mut Capabilities) {
        capabilities.enable(&["repositoryIdentity", "projectCloneTracking"]);
    }

    fn register_rpc(&self, builder: RpcRouterBuilder) -> RpcRouterBuilder {
        let state = self.state.clone();
        let clone_hooks: zc_project::CloneHooksFactory = Arc::new(move |ctx: &RequestContext| {
            Arc::new(CloneHooks {
                state: state.clone(),
                origin: connection_origin(ctx),
            }) as Arc<dyn ProjectCloneHooks>
        });
        zc_project::register(
            builder,
            ProjectRpcServices {
                clones: self.clones.clone(),
                clone_hooks,
                scanner: self.scanner.clone(),
                importer: self.importer.clone(),
            },
        )
    }

    fn dispatch_hooks(&self) -> Vec<Arc<dyn DispatchHook>> {
        vec![Arc::new(CloneGuard(self.clones.clone()))]
    }

    async fn shutdown(&self) {
        self.clones.shutdown();
    }
}
