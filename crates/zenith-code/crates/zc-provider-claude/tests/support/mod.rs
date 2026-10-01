//! Test doubles for the adapter: an in-memory query (`FakeClaudeQuery` of the TS tests), a
//! factory that records every `createQuery` input, deterministic ids and clock, and helpers to
//! collect the canonical events.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use zc_contracts::{
    ClaudeSettings, ProviderInstanceId, ProviderRuntimeEvent, ProviderSendTurnInput, ProviderSession, ProviderSessionStartInput, ProviderTurnStartResult,
};
use zc_ports::adapter::ProviderAdapter;
use zc_provider_claude::adapter::{ClaudeAdapter, ClaudeAdapterOptions};
use zc_provider_claude::catalog::{ClaudeCatalogModel, ClaudeCodeCompatibility, ClaudeCodeProfile, ClaudeModelCatalog};
use zc_provider_claude::history::HistoryOps;
use zc_provider_claude::home::Env;
use zc_provider_claude::mapping::{Clock, IdSource};
use zc_provider_claude::options::ClaudeQueryOptions;
use zc_provider_claude::query::{ClaudeQueryFactory, ClaudeQueryHandle, ClaudeQueryRuntime, PromptReceiver, QueryCallbacks, QueryError};
use zc_provider_claude::usage_limits::ScopedLimitNamesRef;

pub const THREAD_ID: &str = "thread-claude-1";
pub const CAPABLE: &str = "claude-synthetic-capable";
pub const COLLIDING_ALIAS: &str = "synthetic-collision";
pub const STANDARD: &str = "claude-synthetic-standard";
pub const THINKING: &str = "claude-synthetic-thinking";

/// `SYNTHETIC_CLAUDE_MODEL_CATALOG`.
pub fn synthetic_catalog() -> ClaudeModelCatalog {
    let effort = json!({
        "id": "effort", "label": "Reasoning", "type": "select",
        "options": [{"id": "low", "label": "Low"}, {"id": "high", "label": "High", "isDefault": true}, {"id": "max", "label": "Max"}, {"id": "ultrathink", "label": "Ultrathink"}],
        "promptInjectedValues": ["ultrathink"]
    });
    let context_window = json!({
        "id": "contextWindow", "label": "Context Window", "type": "select",
        "options": [{"id": "standard", "label": "Standard"}, {"id": "expanded", "label": "Expanded", "isDefault": true}]
    });
    let runtime = ClaudeCodeProfile {
        effort_map: Some(BTreeMap::from([("ultrathink".to_string(), None)])),
        model_suffixes: Some(vec![(
            "contextWindow".into(),
            BTreeMap::from([("expanded".to_string(), "[expanded]".to_string())]),
        )]),
        context_window_tokens: Some(BTreeMap::from([("standard".to_string(), 200_000.0), ("expanded".to_string(), 1_000_000.0)])),
        fixed_context_window_tokens: None,
    };
    ClaudeModelCatalog {
        models: vec![
            ClaudeCatalogModel {
                model: json!({"slug": CAPABLE, "name": "Claude Synthetic Capable", "aliases": [COLLIDING_ALIAS], "isCustom": false,
                    "capabilities": {"optionDescriptors": [effort.clone(), {"id": "fastMode", "label": "Fast Mode", "type": "boolean"}, context_window.clone()]}}),
                runtime: runtime.clone(),
                compatibility: ClaudeCodeCompatibility::default(),
            },
            ClaudeCatalogModel {
                model: json!({"slug": STANDARD, "name": "Claude Synthetic Standard", "isCustom": false, "capabilities": {"optionDescriptors": [effort, context_window]}}),
                runtime,
                compatibility: ClaudeCodeCompatibility::default(),
            },
            ClaudeCatalogModel {
                model: json!({"slug": THINKING, "name": "Claude Synthetic Thinking", "isCustom": false,
                    "capabilities": {"optionDescriptors": [{"id": "thinking", "label": "Thinking", "type": "boolean"}]}}),
                runtime: ClaudeCodeProfile::default(),
                compatibility: ClaudeCodeCompatibility::default(),
            },
        ],
    }
}

/// UUID-shaped sequential ids.
#[derive(Default)]
pub struct SequentialIds(AtomicUsize);

impl IdSource for SequentialIds {
    fn next_id(&self) -> String {
        let n = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        format!("00000000-0000-4000-8000-{n:012}")
    }
}

/// A clock that starts at a fixed instant and can be set.
pub struct TestClock(pub AtomicI64);

impl TestClock {
    pub fn new(start_ms: i64) -> Self {
        Self(AtomicI64::new(start_ms))
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

pub type InterruptHook = Arc<dyn Fn(&FakeQuery) + Send + Sync>;

/// `FakeClaudeQuery`.
#[derive(Default)]
pub struct FakeQuery {
    sender: Mutex<Option<mpsc::UnboundedSender<Result<Value, QueryError>>>>,
    pub set_model_calls: Mutex<Vec<Option<String>>>,
    pub set_permission_mode_calls: Mutex<Vec<String>>,
    pub close_calls: AtomicUsize,
    pub close_error: Mutex<Option<String>>,
    /// Graceful interrupt (absent by default, like the TS double).
    pub interrupt: Mutex<Option<InterruptHook>>,
}

impl FakeQuery {
    pub fn emit(&self, message: Value) {
        if let Some(sender) = self.sender.lock().unwrap().as_ref() {
            let _ = sender.send(Ok(message));
        }
    }

    pub fn fail(&self, message: &str) {
        if let Some(sender) = self.sender.lock().unwrap().take() {
            let _ = sender.send(Err(QueryError::new(message)));
        }
    }

    pub fn finish(&self) {
        self.sender.lock().unwrap().take();
    }

    pub fn set_interrupt(&self, behavior: impl Fn(&FakeQuery) + Send + Sync + 'static) {
        *self.interrupt.lock().unwrap() = Some(Arc::new(behavior));
    }
}

#[async_trait]
impl ClaudeQueryRuntime for FakeQuery {
    fn supports_interrupt(&self) -> bool {
        self.interrupt.lock().unwrap().is_some()
    }

    async fn interrupt(&self) -> Result<(), QueryError> {
        let behavior = self.interrupt.lock().unwrap().clone();
        if let Some(behavior) = behavior {
            behavior(self);
        }
        Ok(())
    }

    async fn set_model(&self, model: Option<&str>) -> Result<(), QueryError> {
        self.set_model_calls.lock().unwrap().push(model.map(str::to_string));
        Ok(())
    }

    async fn set_permission_mode(&self, mode: &str) -> Result<(), QueryError> {
        self.set_permission_mode_calls.lock().unwrap().push(mode.to_string());
        Ok(())
    }

    fn close(&self) -> Result<(), QueryError> {
        self.close_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = self.close_error.lock().unwrap().clone() {
            return Err(QueryError::new(error));
        }
        self.finish();
        Ok(())
    }
}

/// One `createQuery` call.
pub struct Created {
    pub options: ClaudeQueryOptions,
    pub prompt: tokio::sync::Mutex<PromptReceiver>,
    pub callbacks: Arc<dyn QueryCallbacks>,
    pub query: Arc<FakeQuery>,
}

impl Created {
    /// The next queued user message.
    pub async fn next_prompt(&self) -> Value {
        tokio::time::timeout(Duration::from_secs(5), self.prompt.lock().await.recv())
            .await
            .expect("a prompt in time")
            .expect("a prompt")
    }
}

#[derive(Default)]
pub struct FakeFactory {
    pub created: Mutex<Vec<Arc<Created>>>,
    pub create_error: Mutex<Option<String>>,
}

impl FakeFactory {
    pub fn last(&self) -> Arc<Created> {
        self.created.lock().unwrap().last().cloned().expect("a query was created")
    }

    pub fn count(&self) -> usize {
        self.created.lock().unwrap().len()
    }
}

impl ClaudeQueryFactory for FakeFactory {
    fn create(&self, options: ClaudeQueryOptions, prompt: PromptReceiver, callbacks: Arc<dyn QueryCallbacks>) -> Result<ClaudeQueryHandle, QueryError> {
        if let Some(error) = self.create_error.lock().unwrap().clone() {
            return Err(QueryError::new(error));
        }
        let (sender, receiver) = mpsc::unbounded_channel();
        let query = Arc::new(FakeQuery {
            sender: Mutex::new(Some(sender)),
            ..FakeQuery::default()
        });
        self.created.lock().unwrap().push(Arc::new(Created {
            options,
            prompt: tokio::sync::Mutex::new(prompt),
            callbacks,
            query: query.clone(),
        }));
        Ok(ClaudeQueryHandle {
            runtime: query,
            messages: receiver,
        })
    }
}

/// Harness configuration (`makeHarness(config)`).
#[derive(Default)]
pub struct HarnessConfig {
    pub claude_config: Option<Value>,
    pub instance_id: Option<String>,
    pub environment: Option<Env>,
    pub history: Option<Arc<dyn HistoryOps>>,
    pub scoped_limit_names: Option<ScopedLimitNamesRef>,
    pub attachments_dir: Option<PathBuf>,
    pub clock_ms: Option<i64>,
}

pub struct Harness {
    pub adapter: ClaudeAdapter,
    pub factory: Arc<FakeFactory>,
    pub clock: Arc<TestClock>,
    pub events: futures::stream::BoxStream<'static, ProviderRuntimeEvent>,
    pub attachments_dir: PathBuf,
    _temp: tempfile::TempDir,
}

pub fn settings(value: Value) -> ClaudeSettings {
    serde_json::from_value(value).expect("valid ClaudeSettings")
}

impl Harness {
    pub fn new(config: HarnessConfig) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let attachments_dir = config.attachments_dir.clone().unwrap_or_else(|| temp.path().join("attachments"));
        let factory = Arc::new(FakeFactory::default());
        let clock = Arc::new(TestClock::new(config.clock_ms.unwrap_or(1_772_323_200_000)));
        let mut options = ClaudeAdapterOptions::new(
            settings(config.claude_config.unwrap_or_else(|| json!({}))),
            ProviderInstanceId::from(config.instance_id.as_deref().unwrap_or("claudeAgent")),
            config.environment.unwrap_or_default(),
            attachments_dir.clone(),
        );
        options.catalog = Arc::new(synthetic_catalog);
        options.query_factory = factory.clone();
        options.ids = Arc::new(SequentialIds::default());
        options.clock = clock.clone();
        options.history = config.history;
        options.interrupt_grace = Duration::from_millis(300);
        if let Some(names) = config.scoped_limit_names {
            options.scoped_limit_names = names;
        }
        let adapter = ClaudeAdapter::new(options);
        let events = adapter.subscribe_events();
        Self {
            adapter,
            factory,
            clock,
            events,
            attachments_dir,
            _temp: temp,
        }
    }

    pub fn default() -> Self {
        Self::new(HarnessConfig::default())
    }

    pub fn query(&self) -> Arc<FakeQuery> {
        self.factory.last().query.clone()
    }

    pub fn emit(&self, message: Value) {
        self.query().emit(message);
    }

    pub async fn start(&self, value: Value) -> ProviderSession {
        let mut input = json!({ "threadId": THREAD_ID, "provider": "claudeAgent", "runtimeMode": "full-access" });
        for (key, v) in value.as_object().unwrap() {
            input[key] = v.clone();
        }
        let input: ProviderSessionStartInput = serde_json::from_value(input).unwrap();
        self.adapter.start(input).await.expect("session starts")
    }

    pub async fn send(&self, value: Value) -> ProviderTurnStartResult {
        self.try_send(value).await.expect("turn starts")
    }

    pub async fn try_send(&self, value: Value) -> Result<ProviderTurnStartResult, zc_ports::adapter::AdapterError> {
        let mut input = json!({ "threadId": THREAD_ID, "attachments": [] });
        for (key, v) in value.as_object().unwrap() {
            input[key] = v.clone();
        }
        let input: ProviderSendTurnInput = serde_json::from_value(input).unwrap();
        self.adapter.send(input).await
    }

    pub async fn next_event(&mut self) -> Value {
        let event = tokio::time::timeout(Duration::from_secs(5), self.events.next())
            .await
            .expect("an event in time")
            .expect("the stream is open");
        serde_json::to_value(event).unwrap()
    }

    pub async fn take(&mut self, count: usize) -> Vec<Value> {
        let mut events = Vec::new();
        for _ in 0..count {
            events.push(self.next_event().await);
        }
        events
    }

    /// Events through the first of type `kind` (inclusive).
    pub async fn take_until(&mut self, kind: &str) -> Vec<Value> {
        let mut events = Vec::new();
        loop {
            let event = self.next_event().await;
            let done = event["type"] == kind;
            events.push(event);
            if done {
                return events;
            }
        }
    }

    /// Whatever arrives within `wait`.
    pub async fn drain(&mut self, wait: Duration) -> Vec<Value> {
        let mut events = Vec::new();
        while let Ok(Some(event)) = tokio::time::timeout(wait, self.events.next()).await {
            events.push(serde_json::to_value(event).unwrap());
        }
        events
    }
}

pub fn err_json(error: &zc_ports::adapter::AdapterError) -> Value {
    serde_json::to_value(error).unwrap()
}

pub fn types(events: &[Value]) -> Vec<String> {
    events.iter().map(|e| e["type"].as_str().unwrap_or_default().to_string()).collect()
}

pub fn of_type<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["type"] == kind).collect()
}

pub fn first_of<'a>(events: &'a [Value], kind: &str) -> &'a Value {
    events
        .iter()
        .find(|e| e["type"] == kind)
        .unwrap_or_else(|| panic!("no {kind} event in {:?}", types(events)))
}

pub fn result_success(session_id: &str, uuid: &str) -> Value {
    json!({ "type": "result", "subtype": "success", "is_error": false, "errors": [], "session_id": session_id, "uuid": uuid })
}

pub fn stream_event(session_id: &str, uuid: &str, event: Value) -> Value {
    json!({ "type": "stream_event", "session_id": session_id, "uuid": uuid, "parent_tool_use_id": null, "event": event })
}

pub fn assistant(session_id: &str, uuid: &str, message_id: &str, content: Value) -> Value {
    json!({ "type": "assistant", "session_id": session_id, "uuid": uuid, "parent_tool_use_id": null, "message": { "id": message_id, "content": content } })
}

pub fn tool_result(session_id: &str, uuid: &str, tool_use_id: &str, content: &str) -> Value {
    json!({ "type": "user", "session_id": session_id, "uuid": uuid, "parent_tool_use_id": null,
        "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": tool_use_id, "content": content }] } })
}
