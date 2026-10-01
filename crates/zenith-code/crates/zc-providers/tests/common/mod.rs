//! Test doubles shared by the integration tests: a scripted [`ProviderAdapter`] (the Rust
//! counterpart of `makeFakeCodexAdapter` in `ProviderService.test.ts`), fake drivers and an
//! in-memory settings service. No real provider CLI is ever started.

#![allow(dead_code)]

pub mod drivers;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Value};
use zc_contracts::{
    ApprovalRequestId, ModelSelection, ProviderApprovalDecision, ProviderDriverKind, ProviderRuntimeEvent, ProviderSendTurnInput, ProviderSession,
    ProviderSessionStartInput, ProviderTurnStartResult, ProviderUploadFeedbackInput, ProviderUploadFeedbackResult, ProviderUserInputAnswers, ThreadId, TurnId,
};
use zc_core::PubSub;
use zc_ports::adapter::{
    AdapterCapabilities, AdapterError, AdapterResult, Compaction, ProviderAdapter, SessionModelSwitch, ThreadSnapshot, ThreadTurnSnapshot,
};
use zc_ports::contracts::{ServerSettings, ServerSettingsError, ServerSettingsPatch};
use zc_ports::{EventStream, SettingsService};

pub const NOW: &str = "2026-01-01T00:00:00.000Z";

/// Build a runtime event from its wire JSON (`createdAt` and `eventId` defaulted).
pub fn event(value: Value) -> ProviderRuntimeEvent {
    let mut value = value;
    let object = value.as_object_mut().unwrap();
    object.entry("createdAt").or_insert(json!(NOW));
    if !object.contains_key("eventId") {
        let id = format!("evt-{}", zc_core::uuid_v4());
        object.insert("eventId".into(), json!(id));
    }
    serde_json::from_value(value).expect("valid runtime event")
}

#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    StartSession(ProviderSessionStartInput),
    SendTurn(ProviderSendTurnInput),
    StartCompaction(ThreadId),
    Interrupt(ThreadId, Option<TurnId>),
    RespondToRequest(ThreadId, ApprovalRequestId, ProviderApprovalDecision),
    RespondToUserInput(ThreadId, ApprovalRequestId, ProviderUserInputAnswers),
    StopSession(ThreadId),
    ReadThread(ThreadId),
    Rollback(ThreadId, u32),
    UploadFeedback(ThreadId),
    StopAll,
}

type SendScript = Box<dyn Fn(&FakeAdapter, &ProviderSendTurnInput) -> AdapterResult<Option<ProviderTurnStartResult>> + Send + Sync>;

/// A scripted adapter: keeps sessions in memory, records every call, emits events on demand.
pub struct FakeAdapter {
    pub provider: ProviderDriverKind,
    pub capabilities: Mutex<AdapterCapabilities>,
    pub compaction: Option<Compaction>,
    pub supports_feedback: bool,
    sessions: Mutex<Vec<ProviderSession>>,
    events: PubSub<ProviderRuntimeEvent>,
    calls: Mutex<Vec<Call>>,
    send_script: Mutex<Option<SendScript>>,
    start_failure: Mutex<Option<AdapterError>>,
    stop_all_failure: Mutex<Option<AdapterError>>,
    native_compaction_emits: Mutex<bool>,
}

impl FakeAdapter {
    /// Like the TS fake: Codex compacts natively and uploads feedback, Claude sends `/compact`,
    /// Cursor sends `/compress`.
    pub fn new(provider: &str) -> Arc<Self> {
        let compaction = match provider {
            "codex" => Some(Compaction::Native),
            "cursor" => Some(Compaction::SlashCommand("/compress")),
            "claudeAgent" => Some(Compaction::SlashCommand("/compact")),
            _ => None,
        };
        Arc::new(Self {
            provider: ProviderDriverKind::from(provider),
            capabilities: Mutex::new(AdapterCapabilities {
                session_model_switch: SessionModelSwitch::InSession,
                promptless_turn_continuation: provider == "codex",
                supports_conversation_rollback: true,
            }),
            compaction,
            supports_feedback: provider == "codex",
            sessions: Mutex::new(Vec::new()),
            events: PubSub::new(),
            calls: Mutex::new(Vec::new()),
            send_script: Mutex::new(None),
            start_failure: Mutex::new(None),
            stop_all_failure: Mutex::new(None),
            native_compaction_emits: Mutex::new(true),
        })
    }

    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    pub fn clear_calls(&self) {
        self.calls.lock().unwrap().clear();
    }

    pub fn emit(&self, event: ProviderRuntimeEvent) {
        self.events.publish(event);
    }

    pub fn emit_json(&self, value: Value) {
        self.emit(event(value));
    }

    pub fn set_send_script(
        &self,
        script: impl Fn(&FakeAdapter, &ProviderSendTurnInput) -> AdapterResult<Option<ProviderTurnStartResult>> + Send + Sync + 'static,
    ) {
        *self.send_script.lock().unwrap() = Some(Box::new(script));
    }

    pub fn fail_next_start(&self, error: AdapterError) {
        *self.start_failure.lock().unwrap() = Some(error);
    }

    pub fn fail_stop_all(&self, error: AdapterError) {
        *self.stop_all_failure.lock().unwrap() = Some(error);
    }

    pub fn set_native_compaction_emits(&self, emits: bool) {
        *self.native_compaction_emits.lock().unwrap() = emits;
    }

    pub fn set_rollback_support(&self, supported: bool) {
        self.capabilities.lock().unwrap().supports_conversation_rollback = supported;
    }

    /// Drop a session as if the provider process went away (server restart).
    pub fn forget_session(&self, thread_id: &str) {
        self.sessions.lock().unwrap().retain(|session| session.thread_id.as_str() != thread_id);
    }

    pub fn insert_session(&self, session: ProviderSession) {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|existing| existing.thread_id != session.thread_id);
        sessions.push(session);
    }

    pub fn update_session(&self, thread_id: &str, update: impl FnOnce(&mut ProviderSession)) {
        if let Some(session) = self.sessions.lock().unwrap().iter_mut().find(|session| session.thread_id.as_str() == thread_id) {
            update(session);
        }
    }

    fn has(&self, thread_id: &ThreadId) -> bool {
        self.sessions.lock().unwrap().iter().any(|session| &session.thread_id == thread_id)
    }

    fn not_found(&self, thread_id: &ThreadId) -> AdapterError {
        AdapterError::SessionNotFound {
            provider: self.provider.to_string(),
            thread_id: thread_id.to_string(),
        }
    }
}

#[async_trait]
impl ProviderAdapter for FakeAdapter {
    fn provider(&self) -> ProviderDriverKind {
        self.provider.clone()
    }

    fn capabilities(&self) -> AdapterCapabilities {
        *self.capabilities.lock().unwrap()
    }

    fn compaction(&self) -> Option<Compaction> {
        self.compaction.clone()
    }

    async fn start_session(&self, input: ProviderSessionStartInput) -> AdapterResult<ProviderSession> {
        self.calls.lock().unwrap().push(Call::StartSession(input.clone()));
        if let Some(error) = self.start_failure.lock().unwrap().take() {
            return Err(error);
        }
        let resume_cursor = match &input.resume_cursor {
            Some(cursor) if !cursor.is_null() => cursor.clone(),
            _ => json!({"opaque": format!("resume-{}", input.thread_id)}),
        };
        let session = ProviderSession {
            provider: self.provider.clone(),
            provider_instance_id: input.provider_instance_id.clone(),
            status: zc_contracts::ProviderSessionStatus::Ready,
            runtime_mode: input.runtime_mode,
            cwd: Some(
                input
                    .cwd
                    .clone()
                    .unwrap_or_else(|| std::env::current_dir().unwrap().to_string_lossy().into_owned()),
            ),
            model: None,
            thread_id: input.thread_id.clone(),
            resume_cursor: Some(resume_cursor),
            active_turn_id: None,
            created_at: NOW.into(),
            updated_at: NOW.into(),
            last_error: None,
        };
        self.insert_session(session.clone());
        Ok(session)
    }

    async fn send_turn(&self, input: ProviderSendTurnInput) -> AdapterResult<ProviderTurnStartResult> {
        self.calls.lock().unwrap().push(Call::SendTurn(input.clone()));
        if !self.has(&input.thread_id) {
            return Err(self.not_found(&input.thread_id));
        }
        let scripted = {
            let script = self.send_script.lock().unwrap();
            match script.as_ref() {
                Some(script) => script(self, &input)?,
                None => None,
            }
        };
        let turn = scripted.unwrap_or_else(|| ProviderTurnStartResult {
            thread_id: input.thread_id.clone(),
            turn_id: TurnId::from(format!("turn-{}", input.thread_id)),
            resume_cursor: None,
        });
        self.update_session(input.thread_id.as_str(), |session| {
            session.status = zc_contracts::ProviderSessionStatus::Running;
            session.active_turn_id = Some(turn.turn_id.clone());
        });
        Ok(turn)
    }

    async fn start_compaction(&self, thread_id: &ThreadId, _model_selection: Option<ModelSelection>) -> AdapterResult<()> {
        self.calls.lock().unwrap().push(Call::StartCompaction(thread_id.clone()));
        if *self.native_compaction_emits.lock().unwrap() {
            self.emit_json(json!({
                "type": "thread.state.changed",
                "eventId": "evt-native-compact",
                "provider": self.provider,
                "threadId": thread_id,
                "payload": {"state": "compacted"}
            }));
        }
        Ok(())
    }

    async fn interrupt_turn(&self, thread_id: &ThreadId, turn_id: Option<&TurnId>) -> AdapterResult<()> {
        self.calls.lock().unwrap().push(Call::Interrupt(thread_id.clone(), turn_id.cloned()));
        Ok(())
    }

    async fn respond_to_request(&self, thread_id: &ThreadId, request_id: &ApprovalRequestId, decision: ProviderApprovalDecision) -> AdapterResult<()> {
        self.calls
            .lock()
            .unwrap()
            .push(Call::RespondToRequest(thread_id.clone(), request_id.clone(), decision));
        Ok(())
    }

    async fn respond_to_user_input(&self, thread_id: &ThreadId, request_id: &ApprovalRequestId, answers: ProviderUserInputAnswers) -> AdapterResult<()> {
        self.calls
            .lock()
            .unwrap()
            .push(Call::RespondToUserInput(thread_id.clone(), request_id.clone(), answers));
        Ok(())
    }

    async fn stop_session(&self, thread_id: &ThreadId) -> AdapterResult<()> {
        self.calls.lock().unwrap().push(Call::StopSession(thread_id.clone()));
        self.sessions.lock().unwrap().retain(|session| &session.thread_id != thread_id);
        Ok(())
    }

    async fn list_sessions(&self) -> Vec<ProviderSession> {
        self.sessions.lock().unwrap().clone()
    }

    async fn has_session(&self, thread_id: &ThreadId) -> bool {
        self.has(thread_id)
    }

    async fn read_thread(&self, thread_id: &ThreadId) -> AdapterResult<ThreadSnapshot> {
        self.calls.lock().unwrap().push(Call::ReadThread(thread_id.clone()));
        Ok(ThreadSnapshot {
            thread_id: thread_id.clone(),
            turns: vec![ThreadTurnSnapshot {
                id: TurnId::from("turn-1"),
                items: Vec::new(),
            }],
        })
    }

    async fn rollback_thread(&self, thread_id: &ThreadId, num_turns: u32) -> AdapterResult<ThreadSnapshot> {
        self.calls.lock().unwrap().push(Call::Rollback(thread_id.clone(), num_turns));
        Ok(ThreadSnapshot {
            thread_id: thread_id.clone(),
            turns: Vec::new(),
        })
    }

    async fn upload_feedback(&self, input: ProviderUploadFeedbackInput) -> Option<AdapterResult<ProviderUploadFeedbackResult>> {
        if !self.supports_feedback {
            return None;
        }
        self.calls.lock().unwrap().push(Call::UploadFeedback(input.thread_id.clone()));
        if !self.has(&input.thread_id) {
            return Some(Err(self.not_found(&input.thread_id)));
        }
        Some(Ok(ProviderUploadFeedbackResult {
            feedback_id: format!("feedback-{}", input.thread_id),
        }))
    }

    async fn stop_all(&self) -> AdapterResult<()> {
        self.calls.lock().unwrap().push(Call::StopAll);
        self.sessions.lock().unwrap().clear();
        match self.stop_all_failure.lock().unwrap().take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn subscribe_events(&self) -> BoxStream<'static, ProviderRuntimeEvent> {
        self.events.subscribe().boxed()
    }
}

/// An in-memory settings service (`ServerSettingsService.layerTest`).
pub struct MemorySettings {
    current: Mutex<Value>,
    subscribers: PubSub<ServerSettings>,
}

impl MemorySettings {
    pub fn new(value: Value) -> Arc<Self> {
        Arc::new(Self {
            current: Mutex::new(value),
            subscribers: PubSub::new(),
        })
    }

    pub fn set(&self, value: Value) {
        *self.current.lock().unwrap() = value.clone();
        self.subscribers.publish(typed(value));
    }
}

/// Settings as the settings service holds them: decoded, defaults filled in.
fn typed(value: Value) -> ServerSettings {
    let canonical = zc_settings::settings::schema::decode_settings(&value).expect("test settings decode");
    serde_json::from_value(canonical).expect("test settings are ServerSettings")
}

#[async_trait]
impl SettingsService for MemorySettings {
    async fn get_settings(&self) -> Result<ServerSettings, ServerSettingsError> {
        Ok(typed(self.current.lock().unwrap().clone()))
    }

    async fn update_settings(&self, patch: ServerSettingsPatch) -> Result<ServerSettings, ServerSettingsError> {
        self.set(serde_json::to_value(&patch).unwrap_or_default());
        self.get_settings().await
    }

    fn subscribe_changes(&self) -> EventStream<ServerSettings> {
        self.subscribers.subscribe().boxed()
    }
}

/// Wait until `check` holds (polling every millisecond, 2 s at most).
pub async fn eventually(mut check: impl FnMut() -> bool) {
    for _ in 0..2000 {
        if check() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    assert!(check(), "condition never held");
}
