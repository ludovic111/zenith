//! `project add|remove|rename` (`cli/project.ts`, plan §6.17).
//!
//! When `userdata/server-runtime.json` points at a server that answers, the command goes
//! through its HTTP API with a temporary administrative bearer session (revoked afterwards):
//! `GET /api/orchestration/snapshot`, then `POST /api/orchestration/dispatch`, each with a 1 s
//! timeout. A runtime state file whose server does not answer is deleted. Otherwise the command
//! opens the database and runs the orchestration engine itself (projections bootstrapped first,
//! like the server).
//!
//! Output (stdout): `Added project <id> (<title>) at <root>.`, `Removed project <id> (<title>).`,
//! `Renamed project <id> to <title>.`, `Project <id> is already named <title>.`. Failures are
//! errors (stderr, exit 1), e.g. `An active project already exists for '<root>'.` (the
//! dashboard matches `/already exists/i`).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail};
use serde_json::{json, Value};
use zc_auth::{EnvironmentAuth, IssueBearerSessionInput, ADMINISTRATIVE_SCOPES};
use zc_contracts::{OrchestrationCommand, OrchestrationReadModel};
use zc_core::config::ServerConfig;
use zc_core::runtime_state::{clear_persisted_server_runtime_state, read_persisted_server_runtime_state};
use zc_orchestration::engine::{EngineConfig, ProjectionEngineReads};
use zc_orchestration::{NoBackgroundLiveness, OrchestrationEngine, SystemEnv};
use zc_ports::ProjectionReads;
use zc_projections::{NoRepositoryIdentities, NoThreadLiveState, ProjectionSnapshotQuery};

use crate::cli::{open_environment_auth, resolve_auth_config, AuthLocation, ProjectCommand};

/// `PROJECT_CLI_LIVE_SERVER_TIMEOUT`.
const LIVE_SERVER_TIMEOUT: Duration = Duration::from_secs(1);

/// Where commands go: the running server, or the engine in this process.
enum Target {
    Live {
        origin: String,
        token: String,
        session_id: String,
    },
    Offline {
        engine: OrchestrationEngine,
        reads: Arc<dyn ProjectionReads>,
    },
}

/// A project the command acts on.
struct ProjectTarget {
    id: String,
    title: String,
    workspace_root: String,
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(LIVE_SERVER_TIMEOUT).build()?)
}

/// `projectCommandErrorFromLiveServerRequest`.
async fn live_error(response: reqwest::Response) -> anyhow::Error {
    let status = response.status().as_u16();
    let body: Option<Value> = response.json().await.ok();
    match body
        .as_ref()
        .and_then(|b| Some((b.get("code")?.as_str()?.to_owned(), b.get("traceId")?.as_str()?.to_owned())))
    {
        Some((code, trace_id)) => anyhow!("Server request failed ({code}, trace {trace_id})."),
        None => anyhow!("Server request failed with undeclared status {status}."),
    }
}

async fn fetch_live_snapshot(origin: &str, token: &str) -> anyhow::Result<OrchestrationReadModel> {
    let response = http_client()?
        .get(format!("{origin}/api/orchestration/snapshot"))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|_| anyhow!("Failed to call the running server."))?;
    if !response.status().is_success() {
        return Err(live_error(response).await);
    }
    response.json().await.map_err(|_| anyhow!("Failed to call the running server."))
}

async fn dispatch_live(origin: &str, token: &str, command: &Value) -> anyhow::Result<()> {
    let response = http_client()?
        .post(format!("{origin}/api/orchestration/dispatch"))
        .bearer_auth(token)
        .json(command)
        .send()
        .await
        .map_err(|_| anyhow!("Failed to call the running server."))?;
    if !response.status().is_success() {
        return Err(live_error(response).await);
    }
    Ok(())
}

async fn issue_cli_session(auth: &EnvironmentAuth) -> anyhow::Result<(String, String)> {
    let issued = auth
        .issue_session(IssueBearerSessionInput {
            scopes: Some(ADMINISTRATIVE_SCOPES.to_vec()),
            label: Some("t3 project cli".into()),
            ..IssueBearerSessionInput::default()
        })
        .await?;
    Ok((issued.session_id, issued.token))
}

async fn revoke_cli_session(auth: &EnvironmentAuth, session_id: &str) {
    if let Err(error) = auth.revoke_session(session_id).await {
        tracing::warn!(%error, "could not revoke the project CLI session");
    }
}

/// `tryResolveLiveProjectExecutionMode`: the running server's origin, if it answers.
async fn live_origin(auth: &EnvironmentAuth, config: &ServerConfig) -> anyhow::Result<Option<String>> {
    let path = &config.paths.server_runtime_state_path;
    let Some(state) = read_persisted_server_runtime_state(path).await else {
        return Ok(None);
    };
    let (session_id, token) = issue_cli_session(auth).await?;
    let attempt = fetch_live_snapshot(&state.origin, &token).await;
    revoke_cli_session(auth, &session_id).await;
    match attempt {
        Ok(_) => Ok(Some(state.origin)),
        Err(error) => {
            tracing::debug!(origin = state.origin, %error, "Failed to connect to the persisted project CLI server.");
            clear_persisted_server_runtime_state(path).await;
            Ok(None)
        }
    }
}

/// The engine over the database, with its projections (`OrchestrationLayerLive`).
async fn offline_engine(config: &ServerConfig, db: zc_db::Db) -> anyhow::Result<(OrchestrationEngine, Arc<dyn ProjectionReads>)> {
    let reads: Arc<dyn ProjectionReads> = Arc::new(ProjectionSnapshotQuery::new(
        db.clone(),
        Arc::new(NoRepositoryIdentities),
        Arc::new(NoThreadLiveState),
    ));
    let engine = OrchestrationEngine::start(EngineConfig {
        db: db.clone(),
        reads: Arc::new(ProjectionEngineReads(reads.clone())),
        pipeline: Arc::new(zc_projections::engine::EnginePipeline {
            pipeline: zc_projections::ProjectionPipeline::new(&config.paths.attachments_dir),
            db,
        }),
        liveness: Arc::new(NoBackgroundLiveness),
        env: Arc::new(SystemEnv),
    })
    .await
    .map_err(|error| anyhow!("{error}"))?;
    Ok((engine, reads))
}

impl Target {
    async fn snapshot(&self) -> anyhow::Result<OrchestrationReadModel> {
        match self {
            Self::Live { origin, token, .. } => fetch_live_snapshot(origin, token).await,
            Self::Offline { reads, .. } => reads.get_command_read_model().await.map_err(|error| anyhow!("{error}")),
        }
    }

    async fn dispatch(&self, command: Value) -> anyhow::Result<()> {
        match self {
            Self::Live { origin, token, .. } => dispatch_live(origin, token, &command).await,
            Self::Offline { engine, .. } => {
                let command: OrchestrationCommand = serde_json::from_value(command)?;
                engine.dispatch(command, None).await.map_err(|error| anyhow!("{error}"))?;
                Ok(())
            }
        }
    }
}

/// `normalizeWorkspaceRoot` (the path must exist and be a directory).
fn normalize_root(raw: &str) -> anyhow::Result<String> {
    zc_orchestration::normalizer::normalize_workspace_root(raw, false).map_err(|error| anyhow!("{}", error.message))
}

/// `resolveProjectTitle`.
fn resolve_title(workspace_root: &str, explicit: Option<&str>) -> anyhow::Result<String> {
    match explicit {
        Some(title) => {
            let trimmed = title.trim();
            if trimmed.is_empty() {
                bail!("Project title cannot be empty.");
            }
            Ok(trimmed.to_owned())
        }
        None => Ok(std::path::Path::new(workspace_root)
            .file_name()
            .map(|name| name.to_string_lossy().trim().to_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "project".into())),
    }
}

/// `findActiveProjectTarget`: by exact id, else by (normalized) workspace root.
fn find_project(snapshot: &OrchestrationReadModel, identifier: &str) -> anyhow::Result<ProjectTarget> {
    let trimmed = identifier.trim();
    if trimmed.is_empty() {
        bail!("Project identifier cannot be empty.");
    }
    let active: Vec<_> = snapshot.projects.iter().filter(|project| project.deleted_at.is_none()).collect();
    let to_target = |project: &&zc_contracts::OrchestrationProject| ProjectTarget {
        id: project.id.to_string(),
        title: project.title.to_string(),
        workspace_root: project.workspace_root.to_string(),
    };
    if let Some(project) = active.iter().find(|project| project.id.as_str() == trimmed) {
        return Ok(to_target(project));
    }
    // A stored workspace path still identifies its project after the directory is gone.
    let root = normalize_root(trimmed).unwrap_or_else(|_| trimmed.to_owned());
    match active.iter().find(|project| project.workspace_root.as_str() == root) {
        Some(project) => Ok(to_target(project)),
        None => bail!("No active project found for '{trimmed}'."),
    }
}

async fn run_mutation(target: &Target, command: ProjectCommand) -> anyhow::Result<String> {
    let snapshot = target.snapshot().await?;
    match command {
        ProjectCommand::Add { path, title, .. } => {
            let workspace_root = normalize_root(&path)?;
            if snapshot
                .projects
                .iter()
                .any(|project| project.deleted_at.is_none() && project.workspace_root.as_str() == workspace_root)
            {
                bail!("An active project already exists for '{workspace_root}'.");
            }
            let title = resolve_title(&workspace_root, title.as_deref())?;
            let project_id = zc_core::uuid_v4();
            target
                .dispatch(json!({
                    "type": "project.create",
                    "commandId": zc_core::uuid_v4(),
                    "projectId": project_id,
                    "title": title,
                    "workspaceRoot": workspace_root,
                    "createdAt": zc_core::now_iso(),
                }))
                .await?;
            Ok(format!("Added project {project_id} ({title}) at {workspace_root}."))
        }
        ProjectCommand::Remove { project, force, .. } => {
            let project = find_project(&snapshot, &project)?;
            target
                .dispatch(json!({
                    "type": "project.delete",
                    "commandId": zc_core::uuid_v4(),
                    "projectId": project.id,
                    "force": force,
                }))
                .await?;
            Ok(format!("Removed project {} ({}).", project.id, project.title))
        }
        ProjectCommand::Rename { project, title, .. } => {
            let project = find_project(&snapshot, &project)?;
            let next = resolve_title(&project.workspace_root, Some(&title))?;
            if next == project.title {
                return Ok(format!("Project {} is already named {next}.", project.id));
            }
            target
                .dispatch(json!({
                    "type": "project.meta.update",
                    "commandId": zc_core::uuid_v4(),
                    "projectId": project.id,
                    "title": next,
                }))
                .await?;
            Ok(format!("Renamed project {} to {next}.", project.id))
        }
    }
}

/// Runs a `project …` command.
pub async fn run_project(command: ProjectCommand, log: Option<&str>) -> anyhow::Result<()> {
    let location: AuthLocation = match &command {
        ProjectCommand::Add { location, .. } | ProjectCommand::Remove { location, .. } | ProjectCommand::Rename { location, .. } => location.clone(),
    };
    let config = resolve_auth_config(&location, log).await?;
    let (db, auth) = open_environment_auth(&config).await?;
    let target = match live_origin(&auth, &config).await? {
        Some(origin) => {
            let (session_id, token) = issue_cli_session(&auth).await?;
            Target::Live { origin, token, session_id }
        }
        None => {
            let (engine, reads) = offline_engine(&config, db).await?;
            Target::Offline { engine, reads }
        }
    };
    let outcome = run_mutation(&target, command).await;
    if let Target::Live { session_id, .. } = &target {
        revoke_cli_session(&auth, session_id).await;
    }
    let output = outcome?;
    crate::cli::print_stdout(&output)
}
