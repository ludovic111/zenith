//! WP-24 (zc-workspace) in the server: workspace search (`@` mentions), file reads and writes,
//! the folder browser, opening in an editor, and the editors of `ServerConfig`.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use zc_rpc::RpcRouterBuilder;
use zc_workspace::{ExternalLauncher, WorkspaceFileSystem, WorkspacePaths, WorkspaceRpcServices};

use super::config::EditorDiscovery;
use super::{AppState, Plugin};

/// `ExternalLauncher.resolveAvailableEditors` / `resolveFileManagerRevealKind` for
/// `ServerConfig` (bounded by the config contributor's discovery timeout).
pub struct LauncherEditors(pub Arc<ExternalLauncher>);

#[async_trait]
impl EditorDiscovery for LauncherEditors {
    async fn available_editors(&self) -> Vec<Value> {
        self.0
            .resolve_available_editors()
            .await
            .into_iter()
            .map(|editor| Value::String(editor.as_str().to_owned()))
            .collect()
    }

    async fn file_manager_reveal_kind(&self) -> Option<String> {
        self.0.resolve_file_manager_reveal_kind().await.map(|kind| kind.as_str().to_owned())
    }
}

/// WP-24's plugin: `projects.searchEntries|searchContents|listEntries|readFile|writeFile`,
/// `filesystem.browse`, `shell.openInEditor`.
pub struct WorkspacePlugin {
    services: WorkspaceRpcServices,
}

impl WorkspacePlugin {
    pub fn new(state: &Arc<AppState>) -> Self {
        let entries = state.shared.workspace_entries.clone();
        Self {
            services: WorkspaceRpcServices {
                file_system: WorkspaceFileSystem::new(WorkspacePaths::new(), entries.clone()),
                entries,
                launcher: state.shared.launcher.clone(),
            },
        }
    }
}

#[async_trait]
impl Plugin for WorkspacePlugin {
    fn name(&self) -> &'static str {
        "workspace"
    }

    fn register_rpc(&self, builder: RpcRouterBuilder) -> RpcRouterBuilder {
        zc_workspace::register(builder, self.services.clone())
    }
}
