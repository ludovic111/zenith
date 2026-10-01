//! WP-19 (zc-git) in the server: the `git.*` RPCs over the VCS workflow, whose backend is the
//! GitManager [`super::plugins::git_manager`] builds. The header "Commit" button and the
//! commit / push / PR dialog of the web app run `git.runStackedAction`; the PR thread dialog
//! runs `git.resolvePullRequest` and `git.preparePullRequestThread`.

use std::sync::Arc;

use async_trait::async_trait;
use zc_checkpoints::setup_script::{ProjectSetupScriptRunner, SetupScriptInput};
use zc_checkpoints::SetupScriptRunner;
use zc_git::GitRpcServices;
use zc_rpc::RpcRouterBuilder;

use super::{AppState, Plugin};

/// `ProjectSetupScriptRunner.runForThread` for PR worktrees.
pub struct SetupScripts(pub ProjectSetupScriptRunner);

#[async_trait]
impl zc_git::PullRequestSetupScripts for SetupScripts {
    async fn run_for_thread(&self, thread_id: &str, project_cwd: &str, worktree_path: &str) -> Result<(), String> {
        self.0
            .run_for_thread(SetupScriptInput {
                thread_id: thread_id.to_owned(),
                project_cwd: Some(project_cwd.to_owned()),
                worktree_path: worktree_path.to_owned(),
                ..SetupScriptInput::default()
            })
            .await
            .map(drop)
            .map_err(|error| error.to_string())
    }
}

/// WP-19's plugin.
pub struct GitPlugin {
    services: GitRpcServices,
}

impl GitPlugin {
    pub fn new(state: &Arc<AppState>) -> Self {
        Self {
            services: GitRpcServices {
                workflow: state.vcs.workflow.clone(),
                broadcaster: state.vcs.broadcaster.clone(),
                engine: Arc::new(state.engine.clone()),
                projections: state.reads.clone(),
                uuids: Arc::new(zc_core::uuid_v4),
            },
        }
    }
}

#[async_trait]
impl Plugin for GitPlugin {
    fn name(&self) -> &'static str {
        "git"
    }

    fn register_rpc(&self, builder: RpcRouterBuilder) -> RpcRouterBuilder {
        zc_git::rpc::register(builder, self.services.clone())
    }
}
