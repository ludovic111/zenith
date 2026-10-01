//! Shared test helpers: the real orchestration engine with the SQL projections on an in-memory
//! database (like the server assembles them), the provider session directory on the same
//! database, and git helpers. Every name here is made up.
#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use serde_json::{json, Value};
use zc_contracts::OrchestrationCommand;
use zc_db::Db;
use zc_orchestration::engine::{EngineConfig, ProjectionEngineReads};
use zc_orchestration::{NoBackgroundLiveness, OrchestrationEngine, SystemEnv};
use zc_ports::ProjectionReads;
use zc_projections::{NoRepositoryIdentities, NoThreadLiveState, ProjectionSnapshotQuery};
use zc_providers::ProviderSessionDirectory;

pub const NOW: &str = "2026-01-01T00:00:00.000Z";

pub struct Stack {
    pub db: Db,
    pub engine: OrchestrationEngine,
    pub reads: Arc<dyn ProjectionReads>,
    pub directory: ProviderSessionDirectory,
    pub attachments: tempfile::TempDir,
}

pub async fn stack() -> Stack {
    let db = Db::open_in_memory().unwrap();
    let attachments = tempfile::tempdir().unwrap();
    let projections = Arc::new(ProjectionSnapshotQuery::new(
        db.clone(),
        Arc::new(NoRepositoryIdentities),
        Arc::new(NoThreadLiveState),
    ));
    let reads: Arc<dyn ProjectionReads> = projections;
    let engine = OrchestrationEngine::start(EngineConfig {
        db: db.clone(),
        reads: Arc::new(ProjectionEngineReads(reads.clone())),
        pipeline: Arc::new(zc_projections::engine::EnginePipeline {
            pipeline: zc_projections::ProjectionPipeline::new(attachments.path()),
            db: db.clone(),
        }),
        liveness: Arc::new(NoBackgroundLiveness),
        env: Arc::new(SystemEnv),
    })
    .await
    .unwrap();
    Stack {
        directory: ProviderSessionDirectory::new(db.clone()),
        db,
        engine,
        reads,
        attachments,
    }
}

pub fn command(value: Value) -> OrchestrationCommand {
    let text = value.to_string();
    serde_json::from_value(value).unwrap_or_else(|error| panic!("cannot decode {text}: {error}"))
}

impl Stack {
    pub async fn dispatch(&self, value: Value) -> i64 {
        self.engine
            .dispatch(command(value), None)
            .await
            .unwrap_or_else(|e| panic!("dispatch failed: {e:?}"))
            .sequence
    }

    pub async fn create_project(&self, project_id: &str, workspace_root: &str) {
        self.dispatch(json!({
            "type": "project.create",
            "commandId": format!("create-{project_id}"),
            "projectId": project_id,
            "title": "Sample",
            "workspaceRoot": workspace_root,
            "createdAt": NOW,
        }))
        .await;
    }
}

pub fn git(cwd: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git").args(args).current_dir(cwd).output().expect("git runs");
    assert!(output.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A canonical temp directory (macOS temp paths go through `/private`).
pub fn temp_dir(prefix: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::Builder::new().prefix(prefix).tempdir().unwrap();
    let path = std::fs::canonicalize(dir.path()).unwrap();
    (dir, path)
}
