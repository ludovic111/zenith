//! Fakes and helpers shared by the integration tests: scripted ports (providers, VCS status,
//! pull requests, settings, terminals), projection reads folded from the real engine's command
//! read model, and temp git repositories. Every name and identity here is made up.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use zc_contracts::{
    MessageId, OrchestrationCommand, OrchestrationMessageRole, OrchestrationProject, OrchestrationProjectShell, OrchestrationReadModel,
    OrchestrationSearchThreadsInput, OrchestrationSearchThreadsResult, OrchestrationShellSnapshot, OrchestrationThread, OrchestrationThreadActivity,
    OrchestrationThreadDetailSnapshot, OrchestrationThreadDetailWindow, OrchestrationThreadShell, ProjectId, ProviderInstanceId, ServerSettings,
    ServerSettingsError, ServerSettingsPatch, ThreadId,
};
use zc_core::pubsub::PubSub;
use zc_db::Db;
use zc_orchestration::engine::{EngineConfig, OrchestrationEngine};
use zc_ports::contracts::*;
use zc_ports::orchestration::{
    DeletedWorktreeThread, FullThreadDiffContext, ImportedAgentSessionSource, ReplayStats, SnapshotCounts, ThreadCheckpointContext, ThreadDetailQuery,
    ThreadPullRequests, ThreadRuntimeContext, TurnStartMessage,
};
use zc_ports::provider::{ProviderAdapterCapabilities, ProviderInstanceRoutingInfo};
use zc_ports::pull_requests::PullRequestMergeEvent;
use zc_ports::{EventStream, ProjectionReads, ProviderService, PullRequests, SettingsService, TaggedError, VcsStatusRefresher};

pub const NOW: &str = "2026-01-01T00:00:00.000Z";

// ---------------------------------------------------------------------------------------------
// git

pub fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git").args(args).current_dir(cwd).output().expect("git runs");
    assert!(output.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn git_ok(cwd: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `createGitRepository()`: `README.md` = `v1\n`, committed on `main`.
pub fn create_git_repository(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "--initial-branch=main"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test User"]);
    std::fs::write(dir.join("README.md"), "v1\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", "Initial"]);
}

pub fn git_ref_exists(cwd: &Path, reference: &str) -> bool {
    git_ok(cwd, &["show-ref", "--verify", "--quiet", reference])
}

pub fn git_show(cwd: &Path, reference: &str, file: &str) -> String {
    git(cwd, &["show", &format!("{reference}:{file}")])
}

/// A canonical temp directory (macOS temp paths go through `/private`).
pub fn temp_dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new().prefix("zc-checkpoints-").tempdir().unwrap();
    let path = std::fs::canonicalize(dir.path()).unwrap();
    (dir, path)
}

pub fn store() -> Arc<dyn zc_checkpoints::CheckpointStore> {
    let registry = zc_vcs::VcsDriverRegistry::new(zc_vcs::VcsProjectConfig::new(), Arc::new(zc_vcs::GitVcsProcessDriver::default()));
    Arc::new(zc_checkpoints::VcsCheckpointStore::new(registry))
}

// ---------------------------------------------------------------------------------------------
// Engine and projections

pub fn decode<T: serde::de::DeserializeOwned>(value: Value) -> T {
    let text = value.to_string();
    serde_json::from_value(value).unwrap_or_else(|error| panic!("cannot decode {text}: {error}"))
}

pub fn command(value: Value) -> OrchestrationCommand {
    decode(value)
}

pub async fn engine() -> OrchestrationEngine {
    OrchestrationEngine::start(EngineConfig::standalone(Db::open_in_memory().unwrap()))
        .await
        .unwrap()
}

pub async fn dispatch(engine: &OrchestrationEngine, value: Value) -> i64 {
    engine
        .dispatch(command(value), None)
        .await
        .unwrap_or_else(|e| panic!("dispatch failed: {e:?}"))
        .sequence
}

pub fn model_selection() -> Value {
    json!({"instanceId": "codex", "model": "gpt-5-codex"})
}

pub fn unused<T>() -> Result<T, TaggedError> {
    Err(TaggedError::new("Unused", "unused in this test"))
}

/// `ProjectionSnapshotQuery` over the engine's command read model (the event log folded
/// through the projector), standing in for the SQL projections of WP-09.
#[derive(Clone)]
pub struct EngineProjections {
    pub engine: OrchestrationEngine,
}

fn project_shell(project: &OrchestrationProject) -> OrchestrationProjectShell {
    decode(serde_json::to_value(project).unwrap())
}

fn thread_shell(thread: &OrchestrationThread) -> OrchestrationThreadShell {
    let mut value = serde_json::to_value(thread).unwrap();
    let object = value.as_object_mut().unwrap();
    let latest_user_message_at = thread
        .messages
        .iter()
        .filter(|m| m.role == OrchestrationMessageRole::User)
        .map(|m| m.created_at.clone())
        .max();
    object.insert("latestUserMessageAt".into(), json!(latest_user_message_at));
    object.insert("hasPendingApprovals".into(), json!(false));
    object.insert("hasPendingUserInput".into(), json!(false));
    object.insert("hasActionableProposedPlan".into(), json!(false));
    decode(value)
}

impl EngineProjections {
    pub async fn model(&self) -> OrchestrationReadModel {
        self.engine.command_read_model().await.unwrap()
    }

    pub async fn thread(&self, id: &str) -> OrchestrationThread {
        self.model().await.threads.into_iter().find(|t| t.id.as_str() == id).expect("thread exists")
    }
}

#[async_trait]
impl ProjectionReads for EngineProjections {
    async fn get_user_input_activity(&self, _: &ThreadId, _: &ApprovalRequestId) -> Result<Option<OrchestrationThreadActivity>, TaggedError> {
        unused()
    }
    async fn list_activities_by_kind(&self, _: &str) -> Result<Vec<OrchestrationThreadActivity>, TaggedError> {
        unused()
    }
    async fn get_command_read_model(&self) -> Result<OrchestrationReadModel, TaggedError> {
        Ok(self.model().await)
    }
    async fn get_snapshot(&self) -> Result<OrchestrationReadModel, TaggedError> {
        Ok(self.model().await)
    }
    async fn get_shell_snapshot(&self, _unsettled_only: bool) -> Result<OrchestrationShellSnapshot, TaggedError> {
        let model = self.model().await;
        Ok(OrchestrationShellSnapshot {
            snapshot_sequence: model.snapshot_sequence,
            projects: model.projects.iter().filter(|p| p.deleted_at.is_none()).map(project_shell).collect(),
            threads: model
                .threads
                .iter()
                .filter(|t| t.deleted_at.is_none() && t.archived_at.is_none())
                .map(thread_shell)
                .collect(),
            updated_at: model.updated_at,
        })
    }
    async fn get_archived_shell_snapshot(&self) -> Result<OrchestrationShellSnapshot, TaggedError> {
        let model = self.model().await;
        Ok(OrchestrationShellSnapshot {
            snapshot_sequence: model.snapshot_sequence,
            projects: model.projects.iter().filter(|p| p.deleted_at.is_none()).map(project_shell).collect(),
            threads: model
                .threads
                .iter()
                .filter(|t| t.deleted_at.is_none() && t.archived_at.is_some())
                .map(thread_shell)
                .collect(),
            updated_at: model.updated_at,
        })
    }
    async fn list_threads_with_pull_requests(&self) -> Result<Vec<ThreadPullRequests>, TaggedError> {
        unused()
    }
    async fn get_deleted_worktree_threads(&self) -> Result<Vec<DeletedWorktreeThread>, TaggedError> {
        unused()
    }
    async fn search_threads(&self, _: OrchestrationSearchThreadsInput) -> Result<OrchestrationSearchThreadsResult, TaggedError> {
        unused()
    }
    async fn get_snapshot_sequence(&self) -> Result<i64, TaggedError> {
        Ok(self.engine.latest_sequence())
    }
    async fn get_counts(&self) -> Result<SnapshotCounts, TaggedError> {
        unused()
    }
    async fn get_event_replay_stats(&self, _: i64, _: i64) -> Result<ReplayStats, TaggedError> {
        unused()
    }
    async fn get_active_project_by_workspace_root(&self, workspace_root: &str) -> Result<Option<OrchestrationProject>, TaggedError> {
        Ok(self
            .model()
            .await
            .projects
            .into_iter()
            .find(|p| p.deleted_at.is_none() && p.workspace_root == workspace_root))
    }
    async fn get_project_shell_by_id(&self, project_id: &ProjectId) -> Result<Option<OrchestrationProjectShell>, TaggedError> {
        Ok(self
            .model()
            .await
            .projects
            .iter()
            .find(|p| &p.id == project_id && p.deleted_at.is_none())
            .map(project_shell))
    }
    async fn get_project_shells(&self, _: Option<Vec<ProjectId>>) -> Result<Vec<OrchestrationProjectShell>, TaggedError> {
        unused()
    }
    async fn get_first_active_thread_id_by_project_id(&self, _: &ProjectId) -> Result<Option<ThreadId>, TaggedError> {
        unused()
    }
    async fn get_imported_agent_session_sources(&self, _: &ProjectId) -> Result<Vec<ImportedAgentSessionSource>, TaggedError> {
        unused()
    }
    async fn get_thread_checkpoint_context(&self, thread_id: &ThreadId) -> Result<Option<ThreadCheckpointContext>, TaggedError> {
        let model = self.model().await;
        let Some(thread) = model.threads.iter().find(|t| &t.id == thread_id && t.deleted_at.is_none()) else {
            return Ok(None);
        };
        let Some(project) = model.projects.iter().find(|p| p.id == thread.project_id) else {
            return Ok(None);
        };
        Ok(Some(ThreadCheckpointContext {
            thread_id: thread.id.clone(),
            project_id: thread.project_id.clone(),
            workspace_root: project.workspace_root.clone(),
            worktree_path: thread.worktree_path.clone(),
            checkpoints: thread.checkpoints.clone(),
        }))
    }
    async fn get_full_thread_diff_context(&self, thread_id: &ThreadId, to_turn_count: i64) -> Result<Option<FullThreadDiffContext>, TaggedError> {
        let Some(context) = self.get_thread_checkpoint_context(thread_id).await? else {
            return Ok(None);
        };
        Ok(Some(FullThreadDiffContext {
            thread_id: context.thread_id,
            project_id: context.project_id,
            workspace_root: context.workspace_root,
            worktree_path: context.worktree_path,
            latest_checkpoint_turn_count: context.checkpoints.iter().map(|c| c.checkpoint_turn_count).max().unwrap_or(0),
            to_checkpoint_ref: context
                .checkpoints
                .iter()
                .find(|c| c.checkpoint_turn_count == to_turn_count)
                .map(|c| c.checkpoint_ref.clone()),
        }))
    }
    async fn get_thread_shell_by_id(&self, thread_id: &ThreadId) -> Result<Option<OrchestrationThreadShell>, TaggedError> {
        Ok(self
            .model()
            .await
            .threads
            .iter()
            .find(|t| &t.id == thread_id && t.deleted_at.is_none())
            .map(thread_shell))
    }
    async fn get_thread_runtime_context(&self, _: &ThreadId) -> Result<Option<ThreadRuntimeContext>, TaggedError> {
        unused()
    }
    async fn get_turn_start_message(&self, _: &ThreadId, _: &MessageId) -> Result<Option<TurnStartMessage>, TaggedError> {
        unused()
    }
    async fn get_thread_detail_by_id(&self, thread_id: &ThreadId, query: ThreadDetailQuery) -> Result<Option<OrchestrationThread>, TaggedError> {
        let mut thread = self.model().await.threads.into_iter().find(|t| &t.id == thread_id && t.deleted_at.is_none());
        if let (Some(thread), Some(kinds)) = (thread.as_mut(), query.activity_kinds) {
            thread.activities.retain(|a| kinds.contains(&a.kind));
        }
        Ok(thread)
    }
    async fn get_thread_detail_snapshot(
        &self,
        _: &ThreadId,
        _: Option<OrchestrationThreadDetailWindow>,
    ) -> Result<Option<OrchestrationThreadDetailSnapshot>, TaggedError> {
        unused()
    }
}

// ---------------------------------------------------------------------------------------------
// Providers

/// `createProviderServiceHarness`: one session (optional), scripted runtime events, recorded
/// rollbacks.
pub struct FakeProviders {
    pub sessions: Mutex<Vec<Value>>,
    pub events: PubSub<ProviderRuntimeEvent>,
    pub rollbacks: Mutex<Vec<(String, u32)>>,
    pub rollback_supported: Mutex<Option<TaggedError>>,
    pub rollback_checked: tokio::sync::Notify,
}

impl FakeProviders {
    pub fn new(session: Option<Value>) -> Arc<Self> {
        Arc::new(Self {
            sessions: Mutex::new(session.into_iter().collect()),
            events: PubSub::new(),
            rollbacks: Mutex::new(Vec::new()),
            rollback_supported: Mutex::new(None),
            rollback_checked: tokio::sync::Notify::new(),
        })
    }

    pub fn session(thread_id: &str, cwd: &str, provider: &str) -> Value {
        json!({"provider": provider, "status": "ready", "runtimeMode": "full-access", "threadId": thread_id, "cwd": cwd, "createdAt": NOW, "updatedAt": NOW})
    }

    pub fn emit(&self, event: Value) {
        self.events.publish(ProviderRuntimeEvent(event));
    }

    pub fn rollbacks(&self) -> Vec<(String, u32)> {
        self.rollbacks.lock().unwrap().clone()
    }
}

#[async_trait]
impl ProviderService for FakeProviders {
    async fn start_session(&self, _: &ThreadId, _: ProviderSessionStartInput) -> Result<ProviderSession, TaggedError> {
        unused()
    }
    async fn send_turn(&self, _: ProviderSendTurnInput) -> Result<ProviderTurnStartResult, TaggedError> {
        unused()
    }
    async fn compact_thread(&self, _: &ThreadId, _: Option<ModelSelection>, _: Option<MessageId>) -> Result<(), TaggedError> {
        unused()
    }
    async fn interrupt_turn(&self, _: ProviderInterruptTurnInput) -> Result<(), TaggedError> {
        unused()
    }
    async fn respond_to_request(&self, _: ProviderRespondToRequestInput) -> Result<(), TaggedError> {
        unused()
    }
    async fn respond_to_user_input(&self, _: ProviderRespondToUserInputInput) -> Result<(), TaggedError> {
        unused()
    }
    async fn stop_session(&self, _: ProviderStopSessionInput) -> Result<(), TaggedError> {
        unused()
    }
    async fn list_sessions(&self) -> Vec<ProviderSession> {
        self.sessions.lock().unwrap().iter().cloned().map(ProviderSession).collect()
    }
    async fn get_capabilities(&self, _: &ProviderInstanceId) -> Result<ProviderAdapterCapabilities, TaggedError> {
        unused()
    }
    async fn get_instance_info(&self, _: &ProviderInstanceId) -> Result<ProviderInstanceRoutingInfo, TaggedError> {
        unused()
    }
    async fn assert_conversation_rollback_supported(&self, _: &ThreadId) -> Result<(), TaggedError> {
        let result = self.rollback_supported.lock().unwrap().clone();
        self.rollback_checked.notify_waiters();
        match result {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    async fn rollback_conversation(&self, thread_id: &ThreadId, num_turns: u32) -> Result<(), TaggedError> {
        self.rollbacks.lock().unwrap().push((thread_id.0.clone(), num_turns));
        Ok(())
    }
    async fn upload_feedback(&self, _: ProviderUploadFeedbackInput) -> Result<ProviderUploadFeedbackResult, TaggedError> {
        unused()
    }
    fn subscribe_events(&self) -> EventStream<ProviderRuntimeEvent> {
        self.events.subscribe().boxed()
    }
}

// ---------------------------------------------------------------------------------------------
// VCS status and pull requests

/// The `VcsStatusBroadcaster` stand-in: a scripted local ref name, recorded refreshes.
pub struct FakeVcsStatus {
    pub local_ref_name: Option<String>,
    pub local_refreshes: Mutex<Vec<String>>,
    pub pull_request_refreshes: Mutex<Vec<String>>,
    pub full_refreshes: Mutex<Vec<String>>,
    /// When set, `refresh_pull_request_status` waits for it.
    pub pull_request_gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    pub pull_request_started: tokio::sync::Notify,
}

impl FakeVcsStatus {
    pub fn new(local_ref_name: Option<&str>) -> Arc<Self> {
        Arc::new(Self {
            local_ref_name: local_ref_name.map(str::to_owned),
            local_refreshes: Mutex::new(Vec::new()),
            pull_request_refreshes: Mutex::new(Vec::new()),
            full_refreshes: Mutex::new(Vec::new()),
            pull_request_gate: Mutex::new(None),
            pull_request_started: tokio::sync::Notify::new(),
        })
    }
}

#[async_trait]
impl VcsStatusRefresher for FakeVcsStatus {
    async fn refresh_local_status(&self, cwd: &str) -> Result<VcsStatusLocalResult, TaggedError> {
        self.local_refreshes.lock().unwrap().push(cwd.to_owned());
        let ref_name = self.local_ref_name.clone();
        Ok(VcsStatusLocalResult(json!({
            "isRepo": true,
            "hasPrimaryRemote": false,
            "isDefaultRef": ref_name.as_deref().is_none_or(|r| r == "main"),
            "refName": ref_name.unwrap_or_else(|| "main".into()),
            "hasWorkingTreeChanges": false,
            "workingTree": {"files": [], "insertions": 0, "deletions": 0},
        })))
    }
    async fn refresh_status(&self, cwd: &str) -> Result<VcsStatusResult, TaggedError> {
        self.full_refreshes.lock().unwrap().push(cwd.to_owned());
        Ok(VcsStatusResult(json!({})))
    }
    async fn refresh_pull_request_status(&self, cwd: &str) -> Result<Option<VcsStatusRemoteResult>, TaggedError> {
        self.pull_request_refreshes.lock().unwrap().push(cwd.to_owned());
        self.pull_request_started.notify_waiters();
        let gate = self.pull_request_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            let _permit = gate.acquire().await;
        }
        Ok(None)
    }
}

#[derive(Default)]
pub struct FakePullRequests {
    pub refreshes: Mutex<Vec<String>>,
}

#[async_trait]
impl PullRequests for FakePullRequests {
    async fn summary(&self, _: PullRequestRef, _: bool) -> Result<PullRequestSummary, TaggedError> {
        unused()
    }
    async fn stack(&self, _: PullRequestRef, _: bool) -> Result<Option<PullRequestStack>, TaggedError> {
        unused()
    }
    async fn diff(&self, _: PullRequestDiffInput) -> Result<PullRequestDiffResult, TaggedError> {
        unused()
    }
    async fn invalidate(&self, _: PullRequestInvalidateInput, _: bool) {}
    async fn refresh_after_turn(&self, project_id: &ProjectId) {
        self.refreshes.lock().unwrap().push(project_id.0.clone());
    }
    fn subscribe_merges(&self) -> EventStream<PullRequestMergeEvent> {
        futures::stream::empty().boxed()
    }
    fn subscribe_refreshes(&self) -> EventStream<u64> {
        futures::stream::empty().boxed()
    }
}

// ---------------------------------------------------------------------------------------------
// Settings

/// `ServerSettingsService.layerTest(overrides)`: settings decoded from JSON, with patches
/// merged shallowly per top-level key (deeply for `storageCleanup`).
pub struct MemorySettings {
    pub current: Mutex<Value>,
    pub changes: PubSub<ServerSettings>,
    pub reads: std::sync::atomic::AtomicUsize,
    /// `(after this many reads, apply this patch)`.
    pub scheduled: Mutex<Vec<(usize, Value)>>,
}

impl MemorySettings {
    pub fn new(overrides: Value) -> Arc<Self> {
        let _: ServerSettings = decode(overrides.clone());
        Arc::new(Self {
            current: Mutex::new(overrides),
            changes: PubSub::new(),
            reads: std::sync::atomic::AtomicUsize::new(0),
            scheduled: Mutex::new(Vec::new()),
        })
    }

    pub fn apply(&self, patch: Value) {
        let next = {
            let mut current = self.current.lock().unwrap();
            merge(&mut current, patch);
            current.clone()
        };
        self.changes.publish(decode(next));
    }
}

fn merge(target: &mut Value, patch: Value) {
    match (target, patch) {
        (Value::Object(target), Value::Object(patch)) => {
            for (key, value) in patch {
                match target.get_mut(&key) {
                    Some(existing) if existing.is_object() && value.is_object() && key != "worktreeCleanup" => merge(existing, value),
                    _ => {
                        target.insert(key, value);
                    }
                }
            }
        }
        (target, patch) => *target = patch,
    }
}

#[async_trait]
impl SettingsService for MemorySettings {
    async fn get_settings(&self) -> Result<ServerSettings, ServerSettingsError> {
        let count = self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let due: Vec<Value> = {
            let mut scheduled = self.scheduled.lock().unwrap();
            let (due, later): (Vec<_>, Vec<_>) = scheduled.drain(..).partition(|(at, _)| *at <= count);
            *scheduled = later;
            due.into_iter().map(|(_, patch)| patch).collect()
        };
        for patch in due {
            self.apply(patch);
        }
        Ok(decode(self.current.lock().unwrap().clone()))
    }
    async fn update_settings(&self, patch: ServerSettingsPatch) -> Result<ServerSettings, ServerSettingsError> {
        self.apply(serde_json::to_value(patch).unwrap());
        Ok(decode(self.current.lock().unwrap().clone()))
    }
    fn subscribe_changes(&self) -> EventStream<ServerSettings> {
        self.changes.subscribe().boxed()
    }
}

// ---------------------------------------------------------------------------------------------
// Terminals

/// A terminal manager that records calls and lets the test emit terminal events.
#[derive(Default)]
pub struct FakeTerminals {
    pub opens: Mutex<Vec<Value>>,
    pub writes: Mutex<Vec<Value>>,
    pub closes: Mutex<Vec<Value>>,
    pub close_idles: Mutex<Vec<(String, Option<String>)>>,
    pub events: PubSub<TerminalEvent>,
    pub metadata: Mutex<Vec<Value>>,
    pub open_error: Mutex<Option<TaggedError>>,
    pub write_error: Mutex<Option<TaggedError>>,
}

impl FakeTerminals {
    pub fn emit(&self, event: Value) {
        self.events.publish(TerminalEvent(event));
    }
    pub fn subscribers(&self) -> usize {
        self.events.subscriber_count()
    }
}

#[async_trait]
impl zc_ports::TerminalManager for FakeTerminals {
    async fn open(&self, input: TerminalOpenInput) -> Result<TerminalSessionSnapshot, TaggedError> {
        self.opens.lock().unwrap().push(input.0.clone());
        if let Some(error) = self.open_error.lock().unwrap().clone() {
            return Err(error);
        }
        Ok(TerminalSessionSnapshot(
            json!({"threadId": input.0["threadId"], "terminalId": input.0["terminalId"]}),
        ))
    }
    async fn attach(&self, _: TerminalAttachInput) -> Result<EventStream<TerminalAttachStreamEvent>, TaggedError> {
        unused()
    }
    async fn write(&self, input: TerminalWriteInput) -> Result<(), TaggedError> {
        self.writes.lock().unwrap().push(input.0);
        match self.write_error.lock().unwrap().clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    async fn resize(&self, _: TerminalResizeInput) -> Result<(), TaggedError> {
        Ok(())
    }
    async fn clear(&self, _: TerminalClearInput) -> Result<(), TaggedError> {
        Ok(())
    }
    async fn restart(&self, _: TerminalRestartInput) -> Result<TerminalSessionSnapshot, TaggedError> {
        unused()
    }
    async fn close(&self, input: TerminalCloseInput) -> Result<(), TaggedError> {
        self.closes.lock().unwrap().push(input.0);
        Ok(())
    }
    async fn close_idle(&self, thread_id: &ThreadId, terminal_id: Option<&str>) {
        self.close_idles.lock().unwrap().push((thread_id.0.clone(), terminal_id.map(str::to_owned)));
    }
    fn subscribe(&self) -> EventStream<TerminalEvent> {
        self.events.subscribe().boxed()
    }
    fn subscribe_metadata(&self) -> EventStream<TerminalMetadataStreamEvent> {
        let snapshot = json!({"type": "snapshot", "terminals": self.metadata.lock().unwrap().clone()});
        futures::stream::once(async move { TerminalMetadataStreamEvent(snapshot) })
            .chain(futures::stream::pending())
            .boxed()
    }
}

/// Polls `condition` until true (15 s), like the TS `waitFor…` helpers.
pub async fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !condition() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Lets spawned forwarding tasks run.
pub async fn settle() {
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
}

pub fn map<K: std::hash::Hash + Eq, V>(entries: impl IntoIterator<Item = (K, V)>) -> HashMap<K, V> {
    entries.into_iter().collect()
}
