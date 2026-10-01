//! Test doubles and helpers shared by the reactor tests: an engine on an in-memory database
//! read back through [`EventLogReactorReads`], a scripted provider service, in-memory settings,
//! and JSON matchers in the spirit of Jest's `toMatchObject` / `toEqual`.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Map, Value};
use zc_core::PubSub;
use zc_db::Db;
use zc_orchestration::decider::SystemEnv;
use zc_orchestration::engine::{EngineConfig, EventLogReads, OrchestrationEngine};
use zc_orchestration::pipeline::NoopProjectionPipeline;
use zc_ports::contracts as ports;
use zc_ports::provider::{ProviderAdapterCapabilities, ProviderContinuationIdentity, ProviderInstanceRoutingInfo, SessionModelSwitchMode};
use zc_ports::{EventStream, OrchestrationDispatch, ProviderService, SettingsService, TaggedError};
use zc_reactors::common::system_uuids;
use zc_reactors::registries::{ThreadBackgroundLivenessRegistry, ThreadPlanProgressRegistry};
use zc_reactors::{EventLogReactorReads, FixedRepositoryProbe, IngestionDeps, ManualClock, ProviderRuntimeIngestion, ReactorReads, RepositoryProbe};

pub mod command;

pub const NOW: &str = "2026-01-01T00:00:00.000Z";

// ---------------------------------------------------------------------------------------------
// JSON matchers

/// Jest `toMatchObject`: every key of `expected` matches in `actual` (objects recursively,
/// arrays element-wise with the same length).
pub fn matches(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Object(actual), Value::Object(expected)) => expected.iter().all(|(key, value)| actual.get(key).is_some_and(|actual| matches(actual, value))),
        (Value::Array(actual), Value::Array(expected)) => actual.len() == expected.len() && actual.iter().zip(expected).all(|(a, e)| matches(a, e)),
        (Value::Number(a), Value::Number(e)) => a.as_f64() == e.as_f64(),
        (actual, expected) => actual == expected,
    }
}

#[track_caller]
pub fn assert_match(actual: &Value, expected: Value) {
    assert!(
        matches(actual, &expected),
        "expected to match\n  expected: {}\n  actual:   {}",
        serde_json::to_string_pretty(&expected).unwrap(),
        serde_json::to_string_pretty(actual).unwrap()
    );
}

/// Whether some element of `items` matches `expected` (`toContainEqual(objectContaining)`).
pub fn contains_match(items: &Value, expected: &Value) -> bool {
    items.as_array().is_some_and(|items| items.iter().any(|item| matches(item, expected)))
}

#[track_caller]
pub fn assert_contains(items: &Value, expected: Value) {
    assert!(
        contains_match(items, &expected),
        "expected an element matching {}\n  in: {}",
        serde_json::to_string_pretty(&expected).unwrap(),
        serde_json::to_string_pretty(items).unwrap()
    );
}

/// Elements of a JSON array matching a predicate.
pub fn filter(items: &Value, predicate: impl Fn(&Value) -> bool) -> Vec<&Value> {
    items
        .as_array()
        .map(|items| items.iter().filter(|item| predicate(item)).collect())
        .unwrap_or_default()
}

pub fn find(items: &Value, predicate: impl Fn(&Value) -> bool) -> Option<&Value> {
    items.as_array().and_then(|items| items.iter().find(|item| predicate(item)))
}

pub fn s<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

// ---------------------------------------------------------------------------------------------
// Settings

/// An in-memory `ServerSettingsService.layerTest(overrides)`.
pub struct MemorySettings {
    current: Mutex<ports::ServerSettings>,
    changes: PubSub<ports::ServerSettings>,
}

impl MemorySettings {
    pub fn new(overrides: Value) -> Arc<Self> {
        let value = zc_settings::settings::test_settings(&overrides);
        Arc::new(Self {
            current: Mutex::new(serde_json::from_value(value).expect("test settings decode")),
            changes: PubSub::new(),
        })
    }

    /// Replaces the settings with `overrides` applied to the test defaults and publishes them.
    pub fn set(&self, overrides: Value) {
        let value = zc_settings::settings::test_settings(&overrides);
        let settings: ports::ServerSettings = serde_json::from_value(value).expect("test settings decode");
        *self.current.lock().unwrap() = settings.clone();
        self.changes.publish(settings);
    }
}

#[async_trait]
impl SettingsService for MemorySettings {
    async fn get_settings(&self) -> Result<ports::ServerSettings, ports::ServerSettingsError> {
        Ok(self.current.lock().unwrap().clone())
    }

    async fn update_settings(&self, _patch: ports::ServerSettingsPatch) -> Result<ports::ServerSettings, ports::ServerSettingsError> {
        Ok(self.current.lock().unwrap().clone())
    }

    fn subscribe_changes(&self) -> EventStream<ports::ServerSettings> {
        self.changes.subscribe().boxed()
    }
}

// ---------------------------------------------------------------------------------------------
// Provider service

fn unsupported(operation: &str) -> TaggedError {
    TaggedError::new("Defect", format!("Unsupported provider call in test: {operation}"))
}

/// The driver kind an instance id maps to in the command reactor tests.
pub fn driver_of(instance_id: &str) -> String {
    if instance_id.starts_with("claude") {
        "claudeAgent".into()
    } else if instance_id.starts_with("codex") {
        "codex".into()
    } else if instance_id.starts_with("antigravity") {
        "antigravity".into()
    } else {
        instance_id.into()
    }
}

/// The ingestion tests' provider service: sessions to list, everything else unsupported.
pub struct FakeProviders {
    pub sessions: Mutex<Vec<Value>>,
    pub events: PubSub<ports::ProviderRuntimeEvent>,
}

impl FakeProviders {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            sessions: Mutex::new(Vec::new()),
            events: PubSub::new(),
        })
    }

    /// `setSession(session)`: replace the thread's session.
    pub fn set_session(&self, session: Value) {
        let mut sessions = self.sessions.lock().unwrap();
        match sessions.iter_mut().find(|entry| entry["threadId"] == session["threadId"]) {
            Some(entry) => *entry = session,
            None => sessions.push(session),
        }
    }
}

#[async_trait]
impl ProviderService for FakeProviders {
    async fn start_session(&self, _thread_id: &ports::ThreadId, _input: ports::ProviderSessionStartInput) -> Result<ports::ProviderSession, TaggedError> {
        Err(unsupported("startSession"))
    }
    async fn send_turn(&self, _input: ports::ProviderSendTurnInput) -> Result<ports::ProviderTurnStartResult, TaggedError> {
        Err(unsupported("sendTurn"))
    }
    async fn compact_thread(
        &self,
        _thread_id: &ports::ThreadId,
        _model: Option<ports::ModelSelection>,
        _request: Option<ports::MessageId>,
    ) -> Result<(), TaggedError> {
        Err(unsupported("compactThread"))
    }
    async fn interrupt_turn(&self, _input: ports::ProviderInterruptTurnInput) -> Result<(), TaggedError> {
        Err(unsupported("interruptTurn"))
    }
    async fn respond_to_request(&self, _input: ports::ProviderRespondToRequestInput) -> Result<(), TaggedError> {
        Err(unsupported("respondToRequest"))
    }
    async fn respond_to_user_input(&self, _input: ports::ProviderRespondToUserInputInput) -> Result<(), TaggedError> {
        Err(unsupported("respondToUserInput"))
    }
    async fn stop_session(&self, _input: ports::ProviderStopSessionInput) -> Result<(), TaggedError> {
        Err(unsupported("stopSession"))
    }
    async fn list_sessions(&self) -> Vec<ports::ProviderSession> {
        self.sessions.lock().unwrap().iter().cloned().map(ports::ProviderSession).collect()
    }
    async fn get_capabilities(&self, _instance_id: &ports::ProviderInstanceId) -> Result<ProviderAdapterCapabilities, TaggedError> {
        Ok(ProviderAdapterCapabilities {
            session_model_switch: SessionModelSwitchMode::InSession,
            promptless_turn_continuation: None,
            supports_conversation_rollback: None,
        })
    }
    async fn get_instance_info(&self, instance_id: &ports::ProviderInstanceId) -> Result<ProviderInstanceRoutingInfo, TaggedError> {
        let driver = ports::ProviderDriverKind::new(instance_id.as_str());
        Ok(ProviderInstanceRoutingInfo {
            instance_id: instance_id.clone(),
            driver_kind: driver.clone(),
            display_name: None,
            accent_color: None,
            enabled: true,
            continuation_identity: ProviderContinuationIdentity::default_for(&driver, instance_id),
        })
    }
    async fn assert_conversation_rollback_supported(&self, _thread_id: &ports::ThreadId) -> Result<(), TaggedError> {
        Err(unsupported("assertConversationRollbackSupported"))
    }
    async fn rollback_conversation(&self, _thread_id: &ports::ThreadId, _num_turns: u32) -> Result<(), TaggedError> {
        Err(unsupported("rollbackConversation"))
    }
    async fn upload_feedback(&self, _input: ports::ProviderUploadFeedbackInput) -> Result<ports::ProviderUploadFeedbackResult, TaggedError> {
        Err(unsupported("uploadFeedback"))
    }
    fn subscribe_events(&self) -> EventStream<ports::ProviderRuntimeEvent> {
        self.events.subscribe().boxed()
    }
}

// ---------------------------------------------------------------------------------------------
// Engine

/// An engine on an in-memory database, with the reactors' liveness registry.
pub async fn engine(liveness: Arc<ThreadBackgroundLivenessRegistry>) -> (Db, OrchestrationEngine) {
    let db = Db::open_in_memory().expect("in-memory database");
    let engine = OrchestrationEngine::start(EngineConfig {
        db: db.clone(),
        reads: Arc::new(EventLogReads::new(db.clone())),
        pipeline: Arc::new(NoopProjectionPipeline),
        liveness,
        env: Arc::new(SystemEnv),
    })
    .await
    .expect("start the engine");
    (db, engine)
}

pub async fn dispatch(engine: &dyn OrchestrationDispatch, command: Value) -> Result<i64, TaggedError> {
    zc_reactors::common::dispatch_json(engine, command).await
}

#[track_caller]
pub fn ok<T>(result: Result<T, TaggedError>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("dispatch failed: [{}] {}", error.tag, error.message),
    }
}

// ---------------------------------------------------------------------------------------------
// Counting reads

/// [`ReactorReads`] that counts every read (the TS tests count SQL statements).
pub struct CountingReads {
    pub inner: Arc<EventLogReactorReads>,
    pub count: std::sync::atomic::AtomicUsize,
}

impl CountingReads {
    fn hit(&self) {
        self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn count(&self) -> usize {
        self.count.load(std::sync::atomic::Ordering::SeqCst)
    }
}

type R<T> = Result<T, TaggedError>;
use zc_contracts::{MessageId as CMessageId, ProjectId as CProjectId, ThreadId as CThreadId, TurnId as CTurnId};
use zc_db::repos::proposed_plans::ProjectionThreadProposedPlan;
use zc_db::repos::thread_activities::ProjectionThreadActivity;
use zc_db::repos::thread_messages::ProjectionThreadMessage;
use zc_db::repos::turns::{PendingTurnStart, ProjectionTurn};
use zc_reactors::reads::TurnStartMessage;

#[async_trait]
impl ReactorReads for CountingReads {
    async fn thread_runtime_context(&self, thread_id: &CThreadId) -> R<Option<Value>> {
        self.hit();
        self.inner.thread_runtime_context(thread_id).await
    }
    async fn thread_shell(&self, thread_id: &CThreadId) -> R<Option<Value>> {
        self.hit();
        self.inner.thread_shell(thread_id).await
    }
    async fn project_shell(&self, project_id: &CProjectId) -> R<Option<Value>> {
        self.hit();
        self.inner.project_shell(project_id).await
    }
    async fn project_shells(&self, project_ids: Option<Vec<CProjectId>>) -> R<Vec<Value>> {
        self.hit();
        self.inner.project_shells(project_ids).await
    }
    async fn thread_detail(&self, thread_id: &CThreadId) -> R<Option<Value>> {
        self.hit();
        self.inner.thread_detail(thread_id).await
    }
    async fn turn_start_message(&self, thread_id: &CThreadId, message_id: &CMessageId) -> R<Option<TurnStartMessage>> {
        self.hit();
        self.inner.turn_start_message(thread_id, message_id).await
    }
    async fn command_read_model_threads(&self) -> R<Vec<Value>> {
        self.hit();
        self.inner.command_read_model_threads().await
    }
    async fn thread_checkpoint_context(&self, thread_id: &CThreadId) -> R<Option<Value>> {
        self.hit();
        self.inner.thread_checkpoint_context(thread_id).await
    }
    async fn shell_snapshot(&self, unsettled_only: bool) -> R<Value> {
        self.hit();
        self.inner.shell_snapshot(unsettled_only).await
    }
    async fn snapshot_sequence(&self) -> R<i64> {
        self.hit();
        self.inner.snapshot_sequence().await
    }
    async fn pending_turn_start(&self, thread_id: &CThreadId) -> R<Option<PendingTurnStart>> {
        self.hit();
        self.inner.pending_turn_start(thread_id).await
    }
    async fn turn(&self, thread_id: &CThreadId, turn_id: &CTurnId) -> R<Option<ProjectionTurn>> {
        self.hit();
        self.inner.turn(thread_id, turn_id).await
    }
    async fn message(&self, message_id: &CMessageId) -> R<Option<ProjectionThreadMessage>> {
        self.hit();
        self.inner.message(message_id).await
    }
    async fn has_assistant_message_for_turn(&self, thread_id: &CThreadId, turn_id: &CTurnId, streaming_only: bool) -> R<bool> {
        self.hit();
        self.inner.has_assistant_message_for_turn(thread_id, turn_id, streaming_only).await
    }
    async fn proposed_plan(&self, thread_id: &CThreadId, plan_id: &str) -> R<Option<ProjectionThreadProposedPlan>> {
        self.hit();
        self.inner.proposed_plan(thread_id, plan_id).await
    }
    async fn user_input_lifecycle(&self, thread_id: &CThreadId) -> R<Vec<ProjectionThreadActivity>> {
        self.hit();
        self.inner.user_input_lifecycle(thread_id).await
    }
    async fn latest_task_activity(&self, thread_id: &CThreadId, task_id: &str) -> R<Option<ProjectionThreadActivity>> {
        self.hit();
        self.inner.latest_task_activity(thread_id, task_id).await
    }
    async fn activities(&self, thread_id: &CThreadId, kinds: &[&str], limit: Option<i64>) -> R<Vec<ProjectionThreadActivity>> {
        self.hit();
        self.inner.activities(thread_id, kinds, limit).await
    }
}

// ---------------------------------------------------------------------------------------------
// Ingestion harness

#[derive(Default)]
pub struct IngestionOptions {
    pub server_settings: Option<Value>,
    pub thread_title: Option<String>,
    pub workspace_subdirectory: Option<String>,
    pub repositories: Option<Arc<dyn RepositoryProbe>>,
}

pub struct IngestionHarness {
    pub engine: Arc<OrchestrationEngine>,
    pub reads: Arc<EventLogReactorReads>,
    /// What the ingestion reads through (counts reads).
    pub counting: Arc<CountingReads>,
    pub ingestion: ProviderRuntimeIngestion,
    pub providers: Arc<FakeProviders>,
    pub settings: Arc<MemorySettings>,
    pub clock: Arc<ManualClock>,
    pub liveness: Arc<ThreadBackgroundLivenessRegistry>,
    pub plan_progress: Arc<ThreadPlanProgressRegistry>,
    pub workspace_root: String,
    _dir: tempfile::TempDir,
}

/// Fills `provider`, `threadId` and `createdAt` when a test event leaves them out, and turns
/// the legacy `turn.completed {status, errorMessage}` shape into its payload.
pub fn normalize_event(mut event: Value) -> Value {
    let object = event.as_object_mut().expect("event object");
    object.entry("provider").or_insert(json!("codex"));
    object.entry("threadId").or_insert(json!("thread-1"));
    object.entry("createdAt").or_insert(json!(NOW));
    if object.get("type") == Some(&json!("turn.completed")) && object.get("payload").is_none() {
        if let Some(status) = object.remove("status") {
            let mut payload = Map::new();
            payload.insert("state".into(), status);
            if let Some(message) = object.remove("errorMessage") {
                payload.insert("errorMessage".into(), message);
            }
            object.insert("payload".into(), Value::Object(payload));
        }
    }
    event
}

impl IngestionHarness {
    pub async fn new(options: IngestionOptions) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut workspace_root = dir.path().to_string_lossy().into_owned();
        if let Some(sub) = &options.workspace_subdirectory {
            let path = dir.path().join(sub);
            std::fs::create_dir_all(&path).unwrap();
            workspace_root = path.to_string_lossy().into_owned();
        }
        let liveness = Arc::new(ThreadBackgroundLivenessRegistry::new());
        let plan_progress = Arc::new(ThreadPlanProgressRegistry::new());
        let (_db, engine) = engine(liveness.clone()).await;
        let engine = Arc::new(engine);
        let reads = Arc::new(EventLogReactorReads::new(engine.clone()));
        let counting = Arc::new(CountingReads {
            inner: reads.clone(),
            count: Default::default(),
        });
        let providers = FakeProviders::new();
        let settings = MemorySettings::new(options.server_settings.clone().unwrap_or(json!({})));
        let clock = Arc::new(ManualClock::shifted());
        let ingestion = ProviderRuntimeIngestion::new(
            IngestionDeps {
                engine: engine.clone(),
                reads: counting.clone(),
                providers: providers.clone(),
                settings: settings.clone(),
                repositories: options.repositories.clone().unwrap_or_else(|| Arc::new(FixedRepositoryProbe(true))),
                liveness: liveness.clone(),
                plan_progress: plan_progress.clone(),
                clock: clock.clone(),
                uuids: system_uuids(),
            },
            tokio_util::sync::CancellationToken::new(),
        );
        ingestion.start();
        let harness = Self {
            engine,
            reads,
            counting,
            ingestion,
            providers,
            settings,
            clock,
            liveness,
            plan_progress,
            workspace_root: workspace_root.clone(),
            _dir: dir,
        };
        harness
            .dispatch(json!({
                "type": "project.create",
                "commandId": "cmd-provider-project-create",
                "projectId": "project-1",
                "title": "Provider Project",
                "workspaceRoot": workspace_root,
                "defaultModelSelection": {"instanceId": "codex", "model": "gpt-5-codex"},
                "createdAt": NOW,
            }))
            .await;
        harness
            .dispatch(json!({
                "type": "thread.create",
                "commandId": "cmd-thread-create",
                "threadId": "thread-1",
                "projectId": "project-1",
                "title": options.thread_title.clone().unwrap_or_else(|| "Thread".into()),
                "modelSelection": {"instanceId": "codex", "model": "gpt-5-codex"},
                "interactionMode": "default",
                "runtimeMode": "approval-required",
                "branch": null,
                "worktreePath": null,
                "createdAt": NOW,
            }))
            .await;
        harness
            .dispatch(json!({
                "type": "thread.session.set",
                "commandId": "cmd-session-seed",
                "threadId": "thread-1",
                "session": {
                    "threadId": "thread-1",
                    "status": "ready",
                    "providerName": "codex",
                    "runtimeMode": "approval-required",
                    "activeTurnId": null,
                    "updatedAt": NOW,
                    "lastError": null,
                },
                "createdAt": NOW,
            }))
            .await;
        harness.providers.set_session(json!({
            "provider": "codex",
            "status": "ready",
            "runtimeMode": "approval-required",
            "threadId": "thread-1",
            "createdAt": NOW,
            "updatedAt": NOW,
        }));
        harness
    }

    /// Dispatches a command that must be accepted.
    pub async fn dispatch(&self, command: Value) -> i64 {
        ok(dispatch(&*self.engine, command).await)
    }

    pub async fn try_dispatch(&self, command: Value) -> Result<i64, TaggedError> {
        dispatch(&*self.engine, command).await
    }

    /// `emit(event)`: what the provider stream delivers, in order.
    pub fn emit(&self, event: Value) {
        self.ingestion.enqueue_runtime_event(normalize_event(event));
    }

    /// Publishes on the provider event stream (the subscription path of `start`).
    pub fn publish(&self, event: Value) {
        self.providers.events.publish(ports::ProviderRuntimeEvent(normalize_event(event)));
    }

    pub async fn drain(&self) {
        self.ingestion.drain().await;
    }

    pub async fn emit_and_drain(&self, events: Vec<Value>) {
        for event in events {
            self.emit(event);
        }
        self.drain().await;
    }

    pub fn advance_clock(&self, millis: i64) {
        self.clock.advance(millis);
    }

    pub fn set_provider_session(&self, session: Value) {
        self.providers.set_session(session);
    }

    /// The read model the TS tests read from `getSnapshot()`: the command read model folded
    /// from the event log, with `latestTurn` as the SQL snapshot reports it.
    pub async fn read_model(&self) -> Value {
        self.reads.read_model().await.expect("read model")
    }

    pub async fn thread_by_id(&self, thread_id: &str) -> Value {
        let model = self.read_model().await;
        find(&model["threads"], |thread| s(thread, "id") == thread_id).cloned().unwrap_or(Value::Null)
    }

    pub async fn thread(&self) -> Value {
        self.thread_by_id("thread-1").await
    }

    /// `waitForThread(readModel, predicate)`.
    pub async fn wait_for_thread(&self, predicate: impl Fn(&Value) -> bool) -> Value {
        self.wait_for_thread_by_id("thread-1", predicate).await
    }

    pub async fn wait_for_thread_by_id(&self, thread_id: &str, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            self.drain().await;
            let thread = self.thread_by_id(thread_id).await;
            if predicate(&thread) {
                return thread;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("Timed out waiting for thread state: {}", serde_json::to_string_pretty(&thread).unwrap());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    pub async fn read_turn(&self, turn_id: &str) -> Option<Value> {
        self.reads
            .turn(&zc_contracts::ThreadId::new("thread-1"), &zc_contracts::TurnId::new(turn_id))
            .await
            .expect("read turn")
            .map(|turn| serde_json::to_value(turn).unwrap())
    }

    /// Every stored event (wire JSON), `readEvents(0)`.
    pub async fn events(&self) -> Vec<Value> {
        let mut stream = zc_ports::OrchestrationDispatch::read_events(&*self.engine, 0, None);
        let mut out = Vec::new();
        while let Some(event) = stream.next().await {
            out.push(serde_json::to_value(event.expect("event")).unwrap());
        }
        out
    }

    pub async fn thread_shell(&self) -> Value {
        self.reads
            .thread_shell(&zc_contracts::ThreadId::new("thread-1"))
            .await
            .expect("shell")
            .unwrap_or(Value::Null)
    }
}

/// `{type: "thread.turn.start", …}` for a user message.
pub fn turn_start(command_id: &str, message_id: &str, text: &str, created_at: &str) -> Value {
    json!({
        "type": "thread.turn.start",
        "commandId": command_id,
        "threadId": "thread-1",
        "message": {"messageId": message_id, "role": "user", "text": text, "attachments": []},
        "interactionMode": "default",
        "runtimeMode": "approval-required",
        "createdAt": created_at,
    })
}

pub fn session_set(command_id: &str, status: &str, provider: &str, active_turn_id: Value, updated_at: &str) -> Value {
    json!({
        "type": "thread.session.set",
        "commandId": command_id,
        "threadId": "thread-1",
        "session": {
            "threadId": "thread-1",
            "status": status,
            "providerName": provider,
            "runtimeMode": "approval-required",
            "activeTurnId": active_turn_id,
            "lastError": null,
            "updatedAt": updated_at,
        },
        "createdAt": updated_at,
    })
}
