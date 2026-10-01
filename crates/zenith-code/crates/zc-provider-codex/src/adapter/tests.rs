//! Ported from `CodexAdapter.test.ts`: the adapter driven by a fake session runtime.

use std::sync::Mutex as StdMutex;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use zc_contracts::{ProviderEvent, ProviderSessionStatus, RuntimeMode};

use super::*;

struct FakeRuntime {
    options: CodexSessionRuntimeOptions,
    resume_cursor: Option<Value>,
    sender: StdMutex<Option<mpsc::UnboundedSender<ProviderEvent>>>,
    receiver: StdMutex<Option<mpsc::UnboundedReceiver<ProviderEvent>>>,
    send_turns: StdMutex<Vec<SendTurnInput>>,
    feedback: StdMutex<Vec<Option<String>>>,
    rollbacks: StdMutex<Vec<usize>>,
    closes: StdMutex<usize>,
    fail_start: bool,
}

impl FakeRuntime {
    fn new(options: CodexSessionRuntimeOptions) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        Self {
            options,
            resume_cursor: None,
            sender: StdMutex::new(Some(sender)),
            receiver: StdMutex::new(Some(receiver)),
            send_turns: StdMutex::new(Vec::new()),
            feedback: StdMutex::new(Vec::new()),
            rollbacks: StdMutex::new(Vec::new()),
            closes: StdMutex::new(0),
            fail_start: false,
        }
    }

    fn emit(&self, event: Value) {
        let event: ProviderEvent = serde_json::from_value(event).unwrap();
        self.sender.lock().unwrap().as_ref().unwrap().send(event).unwrap();
    }

    fn session(&self) -> ProviderSession {
        ProviderSession {
            provider: ProviderDriverKind::new("codex"),
            provider_instance_id: None,
            status: ProviderSessionStatus::Ready,
            runtime_mode: self.options.runtime_mode,
            cwd: Some(self.options.cwd.clone()),
            model: self.options.model.clone(),
            thread_id: self.options.thread_id.clone(),
            resume_cursor: self.resume_cursor.clone(),
            active_turn_id: None,
            created_at: "2026-01-01T00:00:00.000Z".into(),
            updated_at: "2026-01-01T00:00:00.000Z".into(),
            last_error: None,
        }
    }
}

#[async_trait]
impl CodexRuntime for FakeRuntime {
    async fn start(&self) -> Result<ProviderSession, CodexSessionRuntimeError> {
        if self.fail_start {
            return Err(CodexSessionRuntimeError::AppServer(CodexAppServerError::InputStreamEnded));
        }
        Ok(self.session())
    }
    async fn get_session(&self) -> ProviderSession {
        self.session()
    }
    async fn send_turn(&self, input: SendTurnInput) -> Result<ProviderTurnStartResult, CodexSessionRuntimeError> {
        self.send_turns.lock().unwrap().push(input);
        Ok(ProviderTurnStartResult {
            thread_id: self.options.thread_id.clone(),
            turn_id: TurnId::new("turn-1"),
            resume_cursor: None,
        })
    }
    async fn compact_thread(&self) -> Result<(), CodexSessionRuntimeError> {
        Ok(())
    }
    async fn interrupt_turn(&self, _turn_id: Option<TurnId>) -> Result<(), CodexSessionRuntimeError> {
        Ok(())
    }
    async fn read_thread(&self) -> Result<CodexThreadSnapshot, CodexSessionRuntimeError> {
        Ok(CodexThreadSnapshot {
            thread_id: "provider-thread-1".into(),
            turns: Vec::new(),
        })
    }
    async fn rollback_thread(&self, num_turns: usize) -> Result<CodexThreadSnapshot, CodexSessionRuntimeError> {
        self.rollbacks.lock().unwrap().push(num_turns);
        Ok(CodexThreadSnapshot {
            thread_id: "provider-thread-1".into(),
            turns: Vec::new(),
        })
    }
    async fn upload_feedback(&self, reason: Option<String>) -> Result<String, CodexSessionRuntimeError> {
        self.feedback.lock().unwrap().push(reason);
        Ok("provider-thread-1".into())
    }
    async fn respond_to_request(&self, _request_id: &ApprovalRequestId, _decision: ProviderApprovalDecision) -> Result<(), CodexSessionRuntimeError> {
        Ok(())
    }
    async fn respond_to_user_input(&self, _request_id: &ApprovalRequestId, _answers: ProviderUserInputAnswers) -> Result<(), CodexSessionRuntimeError> {
        Ok(())
    }
    fn take_events(&self) -> Option<mpsc::UnboundedReceiver<ProviderEvent>> {
        self.receiver.lock().unwrap().take()
    }
    async fn close(&self) {
        *self.closes.lock().unwrap() += 1;
        self.sender.lock().unwrap().take();
    }
}

#[derive(Clone, Default)]
struct Factory {
    runtimes: Arc<StdMutex<Vec<Arc<FakeRuntime>>>>,
    resume_cursor: Option<Value>,
    fail_construction: bool,
    fail_start: bool,
}

impl Factory {
    fn make(&self) -> RuntimeFactory {
        let this = self.clone();
        Arc::new(move |options| {
            let this = this.clone();
            Box::pin(async move {
                if this.fail_construction {
                    return Err(CodexAppServerError::Spawn {
                        command: Some(format!("{} app-server", options.binary_path)),
                        cause: "runtime construction failed".into(),
                    });
                }
                let mut runtime = FakeRuntime::new(options);
                runtime.resume_cursor = this.resume_cursor.clone();
                runtime.fail_start = this.fail_start;
                let runtime = Arc::new(runtime);
                this.runtimes.lock().unwrap().push(runtime.clone());
                Ok(runtime as Arc<dyn CodexRuntime>)
            })
        })
    }

    fn last(&self) -> Arc<FakeRuntime> {
        self.runtimes.lock().unwrap().last().cloned().expect("a runtime")
    }
}

fn settings(value: Value) -> CodexSettings {
    crate::model::from_json(value)
}

fn adapter_with(factory: &Factory, config: Value, options: CodexAdapterOptions) -> CodexAdapter {
    CodexAdapter::new(
        settings(config),
        CodexAdapterOptions {
            make_runtime: Some(factory.make()),
            default_cwd: Some("/work".into()),
            environment: options.environment.clone().or_else(|| Some(Environment::new())),
            ..options
        },
    )
}

fn start_input(thread: &str) -> ProviderSessionStartInput {
    crate::model::from_json(json!({"provider": "codex", "threadId": thread, "runtimeMode": "full-access"}))
}

fn selection(instance: &str, model: &str, options: Value) -> ModelSelection {
    crate::model::from_json(json!({"instanceId": instance, "model": model, "options": options}))
}

async fn lifecycle() -> (CodexAdapter, Arc<FakeRuntime>, zc_core::Subscription<ProviderRuntimeEvent>) {
    let factory = Factory::default();
    let adapter = adapter_with(&factory, json!({}), CodexAdapterOptions::default());
    let events = adapter.subscribe();
    adapter.start_session(start_input("thread-1")).await.unwrap();
    (adapter, factory.last(), events)
}

async fn take(events: &mut zc_core::Subscription<ProviderRuntimeEvent>, n: usize) -> Vec<Value> {
    let mut out = Vec::new();
    for _ in 0..n {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("an event in time")
            .expect("open");
        out.push(serde_json::to_value(event).unwrap());
    }
    out
}

async fn until_type(events: &mut zc_core::Subscription<ProviderRuntimeEvent>, kind: &str) -> Value {
    loop {
        let event = take(events, 1).await.remove(0);
        if event["type"] == kind {
            return event;
        }
    }
}

fn native(id: &str, kind: &str, method: &str, extra: Value) -> Value {
    let mut event = json!({"id": id, "kind": kind, "provider": "codex", "threadId": "thread-1", "createdAt": "2026-01-01T00:00:00.000Z", "method": method});
    for (key, value) in extra.as_object().unwrap() {
        event[key] = value.clone();
    }
    event
}

fn turn_event(method: &str, turn: &str) -> Value {
    let payload = if method == "turn/started" {
        json!({})
    } else {
        json!({"threadId": "thread-1", "turn": {"id": turn, "items": [], "status": "completed"}})
    };
    native(
        &format!("evt-{method}-{turn}"),
        "notification",
        method,
        json!({"turnId": turn, "payload": payload}),
    )
}

fn usage_event(id: &str, turn: &str, total: [i64; 5], last: Option<[i64; 5]>) -> Value {
    let breakdown = |[input, cached, cache_creation, output, reasoning]: [i64; 5]| json!({"inputTokens": input, "cachedInputTokens": cached, "cacheWriteInputTokens": cache_creation, "outputTokens": output, "reasoningOutputTokens": reasoning, "totalTokens": input + output});
    native(
        id,
        "notification",
        "thread/tokenUsage/updated",
        json!({"turnId": turn, "payload": {"threadId": "thread-1", "turnId": turn, "tokenUsage": {"total": breakdown(total), "last": breakdown(last.unwrap_or(total))}}}),
    )
}

// -- validation and options ----------------------------------------------------------------------

#[tokio::test]
async fn rejects_another_provider_on_start_session() {
    let factory = Factory::default();
    let adapter = adapter_with(&factory, json!({}), CodexAdapterOptions::default());
    let mut input = start_input("thread-1");
    input.provider = Some(ProviderDriverKind::new("claudeAgent"));
    let error = adapter.start_session(input).await.unwrap_err();
    assert_eq!(
        serde_json::to_value(&error).unwrap(),
        json!({"_tag": "ProviderAdapterValidationError", "provider": "codex", "operation": "startSession", "issue": "Expected provider 'codex' but received 'claudeAgent'."})
    );
    assert!(factory.runtimes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn maps_model_options_before_starting_a_session() {
    let factory = Factory::default();
    let adapter = adapter_with(&factory, json!({}), CodexAdapterOptions::default());
    let mut input = start_input("thread-1");
    input.model_selection = Some(selection("codex", "gpt-5.3-codex", json!([{"id": "serviceTier", "value": "priority"}])));
    adapter.start_session(input).await.unwrap();
    let options = &factory.last().options;
    assert_eq!(options.binary_path, "codex");
    assert_eq!(options.cwd, "/work");
    assert_eq!(options.launch_args.as_deref(), Some(""));
    assert_eq!(options.model.as_deref(), Some("gpt-5.3-codex"));
    assert_eq!(options.provider_instance_id.as_ref().map(ProviderInstanceId::as_str), Some("codex"));
    assert_eq!(options.service_tier.as_deref(), Some("priority"));
    assert_eq!(options.runtime_mode, RuntimeMode::FullAccess);
    assert_eq!(options.home_path, None);
    assert_eq!(options.resume_cursor, None);
}

#[tokio::test]
async fn missing_sessions_are_session_not_found() {
    let factory = Factory::default();
    let adapter = adapter_with(&factory, json!({}), CodexAdapterOptions::default());
    let input: ProviderSendTurnInput = crate::model::from_json(json!({"threadId": "sess-missing", "input": "hello", "attachments": []}));
    let error = adapter.send_turn(input).await.unwrap_err();
    assert!(matches!(error, AdapterError::SessionNotFound { ref thread_id, .. } if thread_id == "sess-missing"));
    let feedback: ProviderUploadFeedbackInput = crate::model::from_json(json!({"threadId": "thread-feedback-missing"}));
    assert!(matches!(
        adapter.upload_feedback(feedback).await,
        Some(Err(AdapterError::SessionNotFound { .. }))
    ));
}

#[tokio::test]
async fn compaction_emits_the_compacted_state() {
    let (adapter, runtime, mut events) = lifecycle().await;
    assert_eq!(adapter.compaction(), Some(Compaction::Native));
    adapter.start_compaction(&ThreadId::new("thread-1"), None).await.unwrap();
    runtime.emit(native(
        "evt-compaction-item-completed",
        "notification",
        "item/completed",
        json!({"payload": {"completedAtMs": 1_778_000_000_000_i64, "threadId": "provider-thread-1", "turnId": "provider-compact-turn", "item": {"id": "provider-compact-item", "type": "contextCompaction"}}}),
    ));
    let event = until_type(&mut events, "thread.state.changed").await;
    assert_eq!(event["payload"]["state"], "compacted");
    assert_eq!(event["eventId"], "evt-compaction-item-completed:thread-compacted");
    adapter.stop_session(&ThreadId::new("thread-1")).await.unwrap();
}

#[tokio::test]
async fn uploads_feedback_for_the_active_thread() {
    let (adapter, runtime, _events) = lifecycle().await;
    let input: ProviderUploadFeedbackInput = crate::model::from_json(json!({"threadId": "thread-1", "reason": "The agent stopped early."}));
    let result = adapter.upload_feedback(input).await.unwrap().unwrap();
    assert_eq!(result.feedback_id, "provider-thread-1");
    assert_eq!(*runtime.feedback.lock().unwrap(), vec![Some("The agent stopped early.".to_owned())]);
}

#[tokio::test]
async fn maps_model_options_before_sending_a_turn() {
    let (adapter, runtime, _events) = lifecycle().await;
    let mut input: ProviderSendTurnInput = crate::model::from_json(json!({"threadId": "thread-1", "input": "hello", "attachments": []}));
    input.model_selection = Some(selection(
        "codex",
        "gpt-5.3-codex",
        json!([{"id": "reasoningEffort", "value": "high"}, {"id": "serviceTier", "value": "priority"}]),
    ));
    adapter.send_turn(input).await.unwrap();
    assert_eq!(
        runtime.send_turns.lock().unwrap()[0],
        SendTurnInput {
            input: Some("hello".into()),
            model: Some("gpt-5.3-codex".into()),
            effort: Some("high".into()),
            service_tier: Some("priority".into()),
            ..SendTurnInput::default()
        }
    );
}

#[tokio::test]
async fn passes_image_attachments_by_path() {
    let attachments = tempfile::tempdir().unwrap();
    let factory = Factory::default();
    let adapter = adapter_with(
        &factory,
        json!({}),
        CodexAdapterOptions {
            attachments: Some(Arc::new(AttachmentsDir(attachments.path().to_path_buf()))),
            ..CodexAdapterOptions::default()
        },
    );
    adapter.start_session(start_input("thread-image")).await.unwrap();
    let path = attachments.path().join("attachment-local-image-1.png");
    std::fs::write(&path, [0x89u8; 4]).unwrap();
    let input: ProviderSendTurnInput = crate::model::from_json(json!({"threadId": "thread-image", "input": "Use this image.", "attachments": [
        {"type": "image", "id": "attachment-local-image-1", "name": "generated.png", "mimeType": "image/png", "sizeBytes": 4},
        {"type": "file", "id": "notes", "name": "notes.txt", "mimeType": "text/plain", "sizeBytes": 4}
    ]}));
    adapter.send_turn(input).await.unwrap();
    let expected = zc_core::paths::resolve_path(&path).to_string_lossy().into_owned();
    assert_eq!(factory.last().send_turns.lock().unwrap()[0].attachments, Some(vec![expected]));
}

#[tokio::test]
async fn launch_args_from_settings_and_from_the_environment() {
    let factory = Factory::default();
    let adapter = adapter_with(&factory, json!({"launchArgs": "--strict-config --enable foo"}), CodexAdapterOptions::default());
    adapter.start_session(start_input("sess-launch-args")).await.unwrap();
    assert_eq!(factory.last().options.launch_args.as_deref(), Some("--strict-config --enable foo"));

    let factory = Factory::default();
    let adapter = adapter_with(
        &factory,
        json!({"launchArgs": "--enable settings-feature"}),
        CodexAdapterOptions {
            environment: Some(
                [("T3CODE_CODEX_LAUNCH_ARGS".to_owned(), " --strict-config --enable env-feature ".to_owned())]
                    .into_iter()
                    .collect(),
            ),
            ..CodexAdapterOptions::default()
        },
    );
    adapter.start_session(start_input("sess-launch-args-env")).await.unwrap();
    assert_eq!(factory.last().options.launch_args.as_deref(), Some("--strict-config --enable env-feature"));
}

#[tokio::test]
async fn maps_model_options_for_the_bound_custom_instance() {
    let factory = Factory::default();
    let adapter = adapter_with(
        &factory,
        json!({}),
        CodexAdapterOptions {
            instance_id: Some(ProviderInstanceId::new("codex_personal")),
            ..CodexAdapterOptions::default()
        },
    );
    adapter.start_session(start_input("sess-custom-instance")).await.unwrap();
    let mut input: ProviderSendTurnInput = crate::model::from_json(json!({"threadId": "sess-custom-instance", "input": "hello", "attachments": []}));
    input.model_selection = Some(selection(
        "codex_personal",
        "gpt-5.3-codex",
        json!([{"id": "reasoningEffort", "value": "high"}, {"id": "serviceTier", "value": "flex"}]),
    ));
    adapter.send_turn(input.clone()).await.unwrap();
    // Another instance's selection is not ours.
    input.model_selection = Some(selection("codex", "gpt-5.3-codex", json!([{"id": "reasoningEffort", "value": "high"}])));
    adapter.send_turn(input).await.unwrap();
    let turns = factory.last().send_turns.lock().unwrap().clone();
    assert_eq!(
        turns[0],
        SendTurnInput {
            input: Some("hello".into()),
            model: Some("gpt-5.3-codex".into()),
            effort: Some("high".into()),
            service_tier: Some("flex".into()),
            ..SendTurnInput::default()
        }
    );
    assert_eq!(
        turns[1],
        SendTurnInput {
            input: Some("hello".into()),
            ..SendTurnInput::default()
        }
    );
}

#[tokio::test]
async fn mcp_sessions_add_the_t3_code_server_flags_and_token() {
    struct Lookup;
    impl McpSessionLookup for Lookup {
        fn read(&self, _thread_id: &ThreadId) -> Option<McpProviderSession> {
            Some(McpProviderSession {
                endpoint: "http://127.0.0.1:4000/mcp".into(),
                authorization_header: "Bearer secret-token".into(),
                capabilities: ["preview".to_owned()].into_iter().collect(),
                agent_device_environment: Some(
                    [("PATH".to_owned(), "/shim".to_owned()), ("AGENT_DEVICE_HOST".to_owned(), "h".to_owned())]
                        .into_iter()
                        .collect(),
                ),
            })
        }
    }
    let factory = Factory::default();
    let adapter = adapter_with(
        &factory,
        json!({}),
        CodexAdapterOptions {
            mcp_sessions: Some(Arc::new(Lookup)),
            environment: Some([("PATH".to_owned(), "/usr/bin".to_owned())].into_iter().collect()),
            ..CodexAdapterOptions::default()
        },
    );
    adapter.start_session(start_input("thread-mcp")).await.unwrap();
    let options = &factory.last().options;
    let environment = options.environment.as_ref().unwrap();
    assert_eq!(environment["T3_MCP_BEARER_TOKEN"], "secret-token");
    assert_eq!(environment["PATH"], "/shim:/usr/bin");
    assert_eq!(environment["AGENT_DEVICE_HOST"], "h");
    assert_eq!(
        options.app_server_args.as_deref(),
        Some(
            &[
                "-c".to_owned(),
                "mcp_servers.t3-code.url=http://127.0.0.1:4000/mcp".to_owned(),
                "-c".to_owned(),
                "mcp_servers.t3-code.bearer_token_env_var=\"T3_MCP_BEARER_TOKEN\"".to_owned()
            ][..]
        )
    );
    assert_eq!(options.mcp_capabilities.as_ref().map(|caps| caps.contains("preview")), Some(true));
}

// -- turn token usage ----------------------------------------------------------------------------

#[tokio::test]
async fn one_turn_total_from_cumulative_counters() {
    let (_adapter, runtime, mut events) = lifecycle().await;
    runtime.emit(turn_event("turn/started", "turn-usage"));
    runtime.emit(usage_event("evt-usage-1", "turn-usage", [100, 40, 10, 20, 8], None));
    runtime.emit(turn_event("turn/started", "turn-usage"));
    runtime.emit(usage_event("evt-usage-duplicate", "turn-usage", [100, 40, 10, 20, 8], None));
    runtime.emit(native(
        "evt-collab-activity",
        "notification",
        "collabAgent/activity",
        json!({"turnId": "turn-usage", "payload": {"agentThreadId": "child-1", "agentPath": "/root/child-1", "activityKind": "started"}}),
    ));
    runtime.emit(usage_event("evt-usage-2", "turn-usage", [150, 60, 15, 30, 12], None));
    runtime.emit(turn_event("turn/completed", "turn-usage"));
    let completed = until_type(&mut events, "turn.completed").await;
    assert_eq!(
        completed["payload"]["tokenUsage"],
        json!({"usageStatus": "complete", "usageScope": "main_agent", "inputTokens": 150, "cachedInputTokens": 60, "cacheCreationTokens": 15, "outputTokens": 30, "reasoningTokens": 12, "hasSubagents": true})
    );
}

#[tokio::test]
async fn a_late_prior_turn_update_is_not_charged_to_the_next_turn() {
    let (_adapter, runtime, mut events) = lifecycle().await;
    runtime.emit(turn_event("turn/started", "turn-first"));
    runtime.emit(usage_event("evt-late-1", "turn-first", [100, 40, 10, 20, 8], None));
    runtime.emit(turn_event("turn/completed", "turn-first"));
    runtime.emit(turn_event("turn/started", "turn-second"));
    runtime.emit(usage_event("evt-late-2", "turn-first", [150, 60, 15, 30, 12], None));
    runtime.emit(usage_event("evt-late-3", "turn-second", [170, 65, 16, 35, 14], None));
    runtime.emit(turn_event("turn/completed", "turn-second"));
    until_type(&mut events, "turn.completed").await;
    let second = until_type(&mut events, "turn.completed").await;
    assert_eq!(
        second["payload"]["tokenUsage"],
        json!({"usageStatus": "complete", "usageScope": "main_agent", "inputTokens": 20, "cachedInputTokens": 5, "cacheCreationTokens": 1, "outputTokens": 5, "reasoningTokens": 2, "hasSubagents": false})
    );
}

#[tokio::test]
async fn cache_and_reasoning_subsets_are_clamped() {
    let (_adapter, runtime, mut events) = lifecycle().await;
    runtime.emit(turn_event("turn/started", "turn-clamp"));
    runtime.emit(usage_event("evt-clamp-1", "turn-clamp", [100, 140, 120, 20, 30], None));
    runtime.emit(turn_event("turn/completed", "turn-clamp"));
    let completed = until_type(&mut events, "turn.completed").await;
    assert_eq!(
        completed["payload"]["tokenUsage"],
        json!({"usageStatus": "complete", "usageScope": "main_agent", "inputTokens": 100, "cachedInputTokens": 100, "cacheCreationTokens": 100, "outputTokens": 20, "reasoningTokens": 20, "hasSubagents": false})
    );
}

#[tokio::test]
async fn a_reset_running_total_counts_the_last_response() {
    let (_adapter, runtime, mut events) = lifecycle().await;
    runtime.emit(turn_event("turn/started", "turn-reset"));
    runtime.emit(usage_event(
        "evt-reset-1",
        "turn-reset",
        [5_000, 4_000, 100, 500, 200],
        Some([100, 80, 10, 20, 8]),
    ));
    runtime.emit(usage_event("evt-reset-2", "turn-reset", [150, 90, 5, 30, 12], None));
    runtime.emit(turn_event("turn/completed", "turn-reset"));
    let completed = until_type(&mut events, "turn.completed").await;
    assert_eq!(
        completed["payload"]["tokenUsage"],
        json!({"usageStatus": "complete", "usageScope": "main_agent", "inputTokens": 250, "cachedInputTokens": 170, "cacheCreationTokens": 15, "outputTokens": 50, "reasoningTokens": 20, "hasSubagents": false})
    );
}

#[tokio::test]
async fn without_a_prior_total_the_last_response_counts_and_rollback_resets() {
    let factory = Factory::default();
    let adapter = adapter_with(&factory, json!({}), CodexAdapterOptions::default());
    let mut events = adapter.subscribe();
    let mut input = start_input("thread-1");
    input.resume_cursor = Some(json!({"threadId": "provider-thread-1"}));
    adapter.start_session(input).await.unwrap();
    assert_eq!(factory.last().options.resume_cursor, Some(json!({"threadId": "provider-thread-1"})));
    let runtime = factory.last();
    runtime.emit(turn_event("turn/started", "turn-resumed"));
    runtime.emit(usage_event(
        "evt-resume-baseline",
        "turn-resumed",
        [1_000, 400, 100, 200, 80],
        Some([300, 120, 30, 60, 24]),
    ));
    runtime.emit(turn_event("turn/completed", "turn-resumed"));
    runtime.emit(turn_event("turn/started", "turn-after-resume"));
    runtime.emit(usage_event("evt-after-resume", "turn-after-resume", [1_100, 440, 110, 220, 88], None));
    runtime.emit(turn_event("turn/completed", "turn-after-resume"));
    let first = until_type(&mut events, "turn.completed").await;
    let second = until_type(&mut events, "turn.completed").await;
    adapter.rollback_thread(&ThreadId::new("thread-1"), 1).await.unwrap();
    assert_eq!(*runtime.rollbacks.lock().unwrap(), vec![1]);
    runtime.emit(turn_event("turn/started", "turn-after-rollback"));
    runtime.emit(usage_event(
        "evt-after-rollback",
        "turn-after-rollback",
        [1_050, 420, 105, 210, 84],
        Some([50, 20, 5, 10, 4]),
    ));
    runtime.emit(turn_event("turn/completed", "turn-after-rollback"));
    let third = until_type(&mut events, "turn.completed").await;
    let usage = |input: i64, cached: i64, creation: i64, output: i64, reasoning: i64| json!({"usageStatus": "complete", "usageScope": "main_agent", "inputTokens": input, "cachedInputTokens": cached, "cacheCreationTokens": creation, "outputTokens": output, "reasoningTokens": reasoning, "hasSubagents": false});
    assert_eq!(first["payload"]["tokenUsage"], usage(300, 120, 30, 60, 24));
    assert_eq!(second["payload"]["tokenUsage"], usage(100, 40, 10, 20, 8));
    assert_eq!(third["payload"]["tokenUsage"], usage(50, 20, 5, 10, 4));
    assert!(matches!(
        adapter.rollback_thread(&ThreadId::new("thread-1"), 0).await,
        Err(AdapterError::Validation { .. })
    ));
}

// -- collab ----------------------------------------------------------------------------------------

#[tokio::test]
async fn child_model_metadata_rides_on_every_task_event() {
    let (_adapter, runtime, mut events) = lifecycle().await;
    let cases = [
        ("collabAgent/started", json!({})),
        ("collabAgent/activity", json!({"activityKind": "started"})),
        ("collabAgent/turnStarted", json!({})),
        ("collabAgent/turnCompleted", json!({"turn": {"status": "completed"}})),
        ("collabAgent/statusChanged", json!({"status": {"type": "active", "activeFlags": []}})),
        ("collabAgent/tokenUsage", json!({"tokenUsage": {"total": {"totalTokens": 42}}})),
        ("collabAgent/item", json!({"item": {"type": "commandExecution", "command": "pwd"}})),
        ("collabAgent/closed", json!({})),
        ("collabAgent/metadataUpdated", json!({})),
    ];
    for (index, (method, extra)) in cases.iter().enumerate() {
        let mut payload = json!({"agentThreadId": "child-model", "agentPath": "/root/model-check", "model": " gpt-5.6-sol ", "effort": " high "});
        for (key, value) in extra.as_object().unwrap() {
            payload[key] = value.clone();
        }
        runtime.emit(native(
            &format!("evt-child-model-{index}"),
            "notification",
            method,
            json!({"turnId": "turn-1", "payload": payload}),
        ));
    }
    runtime.emit(native(
        "evt-child-model-blank",
        "notification",
        "collabAgent/metadataUpdated",
        json!({"turnId": "turn-1", "payload": {"agentThreadId": "child-model", "model": "  ", "effort": ""}}),
    ));
    let seen = take(&mut events, 10).await;
    assert_eq!(
        seen.iter().map(|event| event["type"].as_str().unwrap()).collect::<Vec<_>>(),
        vec![
            "task.started",
            "task.started",
            "task.updated",
            "task.updated",
            "task.updated",
            "task.progress",
            "task.progress",
            "task.updated",
            "task.updated",
            "task.updated"
        ]
    );
    for event in &seen[..9] {
        assert_eq!(event["payload"]["model"], "gpt-5.6-sol");
        assert_eq!(event["payload"]["effort"], "high");
    }
    assert!(seen[8]["payload"].get("status").is_none());
    for key in ["status", "model", "effort"] {
        assert!(seen[9]["payload"].get(key).is_none(), "{key}");
    }
}

#[tokio::test]
async fn an_idle_child_is_not_reactivated_by_a_parent_interaction() {
    let (_adapter, runtime, mut events) = lifecycle().await;
    let child = |id: &str, method: &str, payload: Value| native(id, "notification", method, json!({"turnId": "turn-1", "payload": payload}));
    runtime.emit(child(
        "evt-child-running",
        "collabAgent/turnStarted",
        json!({"agentThreadId": "child-1", "agentPath": "/root/audit"}),
    ));
    runtime.emit(child(
        "evt-child-idle",
        "collabAgent/turnCompleted",
        json!({"agentThreadId": "child-1", "agentPath": "/root/audit", "turn": {"status": "completed"}}),
    ));
    runtime.emit(child(
        "evt-child-interacted",
        "collabAgent/activity",
        json!({"agentThreadId": "child-1", "agentPath": "/root/audit", "activityKind": "interacted"}),
    ));
    runtime.emit(child(
        "evt-other-child-running",
        "collabAgent/turnStarted",
        json!({"agentThreadId": "child-2", "agentPath": "/root/other"}),
    ));
    let seen = take(&mut events, 3).await;
    assert_eq!(
        seen.iter()
            .map(|event| (event["payload"]["taskId"].clone(), event["payload"]["status"].clone()))
            .collect::<Vec<_>>(),
        vec![
            (json!("child-1"), json!("running")),
            (json!("child-1"), json!("idle")),
            (json!("child-2"), json!("running"))
        ]
    );
}

// -- item mapping ------------------------------------------------------------------------------------

fn item_completed(id: &str, item: Value) -> Value {
    native(
        id,
        "notification",
        "item/completed",
        json!({"turnId": "turn-1", "itemId": item["id"], "payload": {"completedAtMs": 1_778_000_000_000_i64, "threadId": "thread-1", "turnId": "turn-1", "item": item}}),
    )
}

#[tokio::test]
async fn completed_agent_messages_and_mcp_items() {
    let (_adapter, runtime, mut events) = lifecycle().await;
    runtime.emit(item_completed(
        "evt-msg-complete",
        json!({"type": "agentMessage", "id": "msg_1", "text": "done"}),
    ));
    let event = take(&mut events, 1).await.remove(0);
    assert_eq!(event["type"], "item.completed");
    assert_eq!(event["itemId"], "msg_1");
    assert_eq!(event["turnId"], "turn-1");
    assert_eq!(event["payload"]["itemType"], "assistant_message");

    let item = json!({"type": "mcpToolCall", "id": "mcp_1", "server": "t3-code", "tool": "preview_status", "arguments": {}, "durationMs": 12, "error": null, "result": {"content": [{"type": "text", "text": "attached"}]}, "status": "completed"});
    runtime.emit(item_completed("evt-mcp-complete", item.clone()));
    let event = take(&mut events, 1).await.remove(0);
    assert_eq!(event["payload"]["itemType"], "mcp_tool_call");
    assert_eq!(event["payload"]["title"], "t3-code · preview_status");
    assert_eq!(
        event["payload"]["data"],
        json!({"completedAtMs": 1_778_000_000_000_i64, "threadId": "thread-1", "turnId": "turn-1", "item": item})
    );
}

#[tokio::test]
async fn browser_and_computer_use_calls_get_codex_style_titles_and_sources() {
    let (_adapter, runtime, mut events) = lifecycle().await;
    let long_title = format!("  {}   {}😀bc  ", "a".repeat(39), "a".repeat(38));
    let over_contract_url = format!("https://example.com/?query={}", "😀".repeat(400));
    runtime.emit(native(
        "evt-computer-start",
        "notification",
        "item/started",
        json!({"turnId": "turn-1", "itemId": "computer_1", "payload": {"startedAtMs": 1_778_000_000_000_i64, "threadId": "thread-1", "turnId": "turn-1", "item": {
            "type": "mcpToolCall", "id": "computer_1", "server": "node_repl", "tool": "js",
            "arguments": {"code": "await sky.click({ app: \"Finder\", x: 10, y: 20 })", "title": long_title},
            "durationMs": null, "error": null,
            "result": {"_meta": {"codex/toolSurface": {"kind": "computerUse", "app": {"kind": "appId", "appId": "com.apple.finder"}}}, "content": []},
            "status": "inProgress"
        }}}),
    ));
    runtime.emit(item_completed(
        "evt-browser-complete",
        json!({
            "type": "mcpToolCall", "id": "browser_1", "server": "node_repl", "tool": "js",
            "arguments": {"code": "await tab.playwright.domSnapshot()", "title": "Inspect checkout"}, "durationMs": 12, "error": null,
            "result": {"_meta": {"codex/toolSurface": {"kind": "browserUse", "backend": "chrome", "openTabs": [{
                "pageUrl": "https://www.mathworks.com/help/matlab/", "faviconUrl": "https://www.mathworks.com/favicon.ico",
                "faviconUrlDark": "https://www.mathworks.com/favicon-dark.ico", "url": "https://www.mathworks.com/help/matlab/"
            }]}, "browser_use": {"url": over_contract_url}}, "content": []},
            "status": "completed"
        }),
    ));
    runtime.emit(item_completed(
        "evt-computer-use-complete",
        json!({
            "type": "mcpToolCall", "id": "computer_2", "server": "computer-use", "tool": "type_text",
            "arguments": {"text": "Hello world", "app": "TextEdit"}, "durationMs": 12, "error": null,
            "result": {"_meta": {"codex/toolSurface": {"kind": "computerUse", "app": {"kind": "displayName", "displayName": "TextEdit"}}}, "content": []},
            "status": "completed"
        }),
    ));
    let seen = take(&mut events, 3).await;
    let summary: Vec<Value> = seen
        .iter()
        .map(|event| json!({"type": event["type"], "title": event["payload"]["title"], "toolSurface": event["payload"]["toolSurface"], "toolIcon": event["payload"]["toolIcon"], "toolSource": event["payload"]["toolSource"]}))
        .collect();
    let finder = json!({"_tag": "native-app", "app": {"_tag": "app-id", "appId": "com.apple.finder"}});
    let textedit = json!({"_tag": "native-app", "app": {"_tag": "display-name", "displayName": "TextEdit"}});
    assert_eq!(
        summary,
        vec![
            json!({"type": "item.started", "title": format!("{} {}😀…", "a".repeat(39), "a".repeat(38)), "toolSurface": "computer", "toolIcon": finder, "toolSource": {"key": "native-app:com.apple.finder", "name": "Finder", "kind": "computer", "icon": finder}}),
            json!({"type": "item.completed", "title": "Inspect checkout", "toolSurface": "browser",
                "toolIcon": {"_tag": "website", "pageUrl": "https://www.mathworks.com/help/matlab/", "faviconUrl": "https://www.mathworks.com/favicon.ico", "faviconUrlDark": "https://www.mathworks.com/favicon-dark.ico"},
                "toolSource": {"key": "browser-use:chrome", "name": "Chrome", "kind": "integration", "icon": {"_tag": "native-app", "app": {"_tag": "display-name", "displayName": "Google Chrome"}}}}),
            json!({"type": "item.completed", "title": "Typed text in TextEdit", "toolSurface": "computer", "toolIcon": textedit, "toolSource": {"key": "native-app-name:textedit", "name": "TextEdit", "kind": "computer", "icon": textedit}}),
        ]
    );
}

#[tokio::test]
async fn failed_and_declined_outcomes_are_kept() {
    let (_adapter, runtime, mut events) = lifecycle().await;
    let max_app_id = format!("com.{}", "a".repeat(508));
    let colliding = format!("com.{}b", "a".repeat(507));
    let items = vec![
        json!({"type": "commandExecution", "id": "failed-command", "command": "vp test run", "commandActions": [], "cwd": "/tmp", "exitCode": 1, "status": "failed"}),
        json!({"type": "mcpToolCall", "id": "failed-mcp", "server": "simulator", "tool": "build", "arguments": {}, "error": {"message": "Build failed"}, "status": "failed"}),
        json!({"type": "mcpToolCall", "id": "failed-computer", "server": "computer-use", "tool": "click", "arguments": {"app": "Finder"}, "error": {"message": "Click failed"}, "result": {"_meta": {"codex/toolSurface": {"kind": "computerUse", "app": {"kind": "appId", "appId": max_app_id}}}, "content": []}, "status": "failed"}),
        json!({"type": "mcpToolCall", "id": "failed-computer-collision", "server": "computer-use", "tool": "click", "arguments": {"app": "Other"}, "error": {"message": "Click failed"}, "result": {"_meta": {"codex/toolSurface": {"kind": "computerUse", "app": {"kind": "appId", "appId": colliding}}}, "content": []}, "status": "failed"}),
        json!({"type": "fileChange", "id": "declined-change", "changes": [], "status": "declined"}),
    ];
    let mut keys = BTreeSet::new();
    for item in items {
        runtime.emit(item_completed(&format!("evt-{}", item["id"].as_str().unwrap()), item.clone()));
        let event = take(&mut events, 1).await.remove(0);
        assert_eq!(event["payload"]["status"], item["status"]);
        if item["id"].as_str().unwrap().starts_with("failed-computer") {
            assert_eq!(event["payload"]["title"], "computer-use · click");
            let key = event["payload"]["toolSource"]["key"].as_str().unwrap().to_owned();
            assert_eq!(key.len(), 512);
            keys.insert(key);
        }
    }
    assert_eq!(keys.len(), 2);
}

#[tokio::test]
async fn plans_permissions_and_session_lifecycle() {
    let (_adapter, runtime, mut events) = lifecycle().await;
    runtime.emit(item_completed(
        "evt-plan-complete",
        json!({"type": "plan", "id": "plan_1", "text": "## Final plan\n\n- one\n- two"}),
    ));
    let plan = take(&mut events, 1).await.remove(0);
    assert_eq!(plan["type"], "turn.proposed.completed");
    assert_eq!(plan["payload"]["planMarkdown"], "## Final plan\n\n- one\n- two");

    runtime.emit(native(
        "evt-plan-delta",
        "notification",
        "item/plan/delta",
        json!({"turnId": "turn-1", "itemId": "plan_1", "payload": {"threadId": "thread-1", "turnId": "turn-1", "itemId": "plan_1", "delta": "## Final plan"}}),
    ));
    let delta = take(&mut events, 1).await.remove(0);
    assert_eq!(
        (delta["type"].as_str(), delta["payload"]["delta"].as_str()),
        (Some("turn.proposed.delta"), Some("## Final plan"))
    );

    runtime.emit(native(
        "evt-app-permission-request",
        "request",
        "item/permissions/requestApproval",
        json!({"requestId": "req-perm-1", "requestKind": "permission", "turnId": "turn-1", "itemId": "app_1", "payload": {
            "cwd": "/tmp/project", "itemId": "app_1", "permissions": {"network": {"enabled": true}}, "reason": "Fetch data from api.example.com", "startedAtMs": 1_778_000_000_000_i64, "threadId": "thread-1", "turnId": "turn-1"
        }}),
    ));
    let permission = take(&mut events, 1).await.remove(0);
    assert_eq!(permission["type"], "request.opened");
    assert_eq!(permission["payload"]["requestType"], "permission_approval");
    assert_eq!(permission["payload"]["detail"], "Fetch data from api.example.com");

    runtime.emit(native("evt-session-closed", "session", "session/closed", json!({"message": "Session stopped"})));
    let closed = take(&mut events, 1).await.remove(0);
    assert_eq!(closed["type"], "session.exited");
    assert_eq!(closed["payload"], json!({"reason": "Session stopped", "exitKind": "graceful"}));
}

#[tokio::test]
async fn errors_stderr_and_realtime() {
    let (_adapter, runtime, mut events) = lifecycle().await;
    runtime.emit(native(
        "evt-retryable-error",
        "notification",
        "error",
        json!({"turnId": "turn-1", "payload": {"threadId": "thread-1", "turnId": "turn-1", "error": {"message": "Reconnecting... 2/5"}, "willRetry": true}}),
    ));
    let warning = take(&mut events, 1).await.remove(0);
    assert_eq!(
        (warning["type"].as_str(), warning["turnId"].as_str(), warning["payload"]["message"].as_str()),
        (Some("runtime.warning"), Some("turn-1"), Some("Reconnecting... 2/5"))
    );

    runtime.emit(native(
        "evt-process-stderr",
        "notification",
        "process/stderr",
        json!({"turnId": "turn-1", "message": "The filename or extension is too long. (os error 206)"}),
    ));
    assert_eq!(take(&mut events, 1).await[0]["type"], "runtime.warning");

    runtime.emit(native(
        "evt-realtime-started",
        "notification",
        "thread/realtime/started",
        json!({"payload": {"threadId": "thread-1", "realtimeSessionId": "realtime-session-1", "version": "v2"}}),
    ));
    let realtime = take(&mut events, 1).await.remove(0);
    assert_eq!(
        (realtime["type"].as_str(), realtime["payload"]["realtimeSessionId"].as_str()),
        (Some("thread.realtime.started"), Some("realtime-session-1"))
    );

    let fatal = "2026-03-31T18:14:06.833399Z ERROR codex_api::endpoint::responses_websocket: failed to connect to websocket: HTTP error: 503 Service Unavailable, url: wss://chatgpt.com/backend-api/codex/responses";
    runtime.emit(native(
        "evt-process-stderr-websocket",
        "notification",
        "process/stderr",
        json!({"turnId": "turn-1", "message": fatal}),
    ));
    let error = take(&mut events, 1).await.remove(0);
    assert_eq!(
        (error["type"].as_str(), error["payload"]["class"].as_str(), error["payload"]["message"].as_str()),
        (Some("runtime.error"), Some("provider_error"), Some(fatal))
    );
}

#[tokio::test]
async fn request_types_survive_resolution() {
    let (_adapter, runtime, mut events) = lifecycle().await;
    runtime.emit(native(
        "evt-request-resolved",
        "notification",
        "serverRequest/resolved",
        json!({"requestKind": "command", "requestId": "req-1", "payload": {"threadId": "thread-1", "requestId": "req-1"}}),
    ));
    assert_eq!(take(&mut events, 1).await[0]["payload"]["requestType"], "command_execution_approval");
    runtime.emit(native(
        "evt-file-read-request-resolved",
        "notification",
        "serverRequest/resolved",
        json!({"requestKind": "file-read", "requestId": "req-file-read-1", "payload": {"threadId": "thread-1", "requestId": "req-file-read-1"}}),
    ));
    assert_eq!(take(&mut events, 1).await[0]["payload"]["requestType"], "file_read_approval");
    runtime.emit(native(
        "evt-mcp-elicitation-resolved",
        "notification",
        "item/requestApproval/decision",
        json!({"requestKind": "mcp-elicitation", "requestId": "req-safari", "payload": {"decision": "acceptAlways"}}),
    ));
    let resolved = take(&mut events, 1).await.remove(0);
    assert_eq!(
        (resolved["payload"]["requestType"].as_str(), resolved["payload"]["decision"].as_str()),
        (Some("mcp_elicitation_approval"), Some("acceptAlways"))
    );
}

fn patch_request(id: &str, payload: Value) -> Value {
    native(
        id,
        "request",
        "applyPatchApproval",
        json!({"requestKind": "file-change", "requestId": format!("req-{id}"), "turnId": "turn-1", "payload": payload}),
    )
}

#[tokio::test]
async fn apply_patch_and_file_change_details() {
    let (_adapter, runtime, mut events) = lifecycle().await;
    runtime.emit(patch_request("p1", json!({"callId": "call-1", "conversationId": "provider-thread-1", "fileChanges": {"/tmp/removed.md": {"type": "delete", "content": "gone"}, "/tmp/added.ts": {"type": "add", "content": "export {};"}}})));
    let first = take(&mut events, 1).await.remove(0);
    assert_eq!(first["payload"]["requestType"], "apply_patch_approval");
    assert_eq!(first["payload"]["detail"], "add /tmp/added.ts\ndelete /tmp/removed.md");

    runtime.emit(patch_request("p2", json!({"callId": "call-2", "conversationId": "provider-thread-1", "reason": "Needs to rewrite the changelog", "fileChanges": {"/tmp/CHANGELOG.md": {"type": "add", "content": "x"}}})));
    assert_eq!(take(&mut events, 1).await[0]["payload"]["detail"], "Needs to rewrite the changelog");

    runtime.emit(native(
        "evt-file-change",
        "request",
        "item/fileChange/requestApproval",
        json!({"requestKind": "file-change", "requestId": "req-file-change", "turnId": "turn-1", "payload": {"itemId": "item-1", "grantRoot": "/tmp/workspace", "startedAtMs": 0, "threadId": "provider-thread-1", "turnId": "turn-1"}}),
    ));
    let file_change = take(&mut events, 1).await.remove(0);
    assert_eq!(
        (file_change["payload"]["requestType"].as_str(), file_change["payload"]["detail"].as_str()),
        (Some("file_change_approval"), Some("/tmp/workspace"))
    );

    runtime.emit(patch_request("p3", json!({"callId": "call-3", "conversationId": "provider-thread-1", "reason": "   ", "fileChanges": {"/tmp/moved.ts": {"type": "update", "unified_diff": "@@", "move_path": "/tmp/renamed.ts"}}})));
    assert_eq!(take(&mut events, 1).await[0]["payload"]["detail"], "update /tmp/moved.ts -> /tmp/renamed.ts");

    let many: serde_json::Map<String, Value> = (0..25)
        .map(|index| (format!("/tmp/file-{index:02}.ts"), json!({"type": "add", "content": "x"})))
        .collect();
    runtime.emit(patch_request(
        "p4",
        json!({"callId": "call-4", "conversationId": "provider-thread-1", "fileChanges": many}),
    ));
    let detail = take(&mut events, 1).await[0]["payload"]["detail"].as_str().unwrap().to_owned();
    assert_eq!(detail.split('\n').count(), 21);
    assert!(detail.ends_with("+5 more"));

    runtime.emit(patch_request(
        "p5",
        json!({"callId": "call-5", "conversationId": "provider-thread-1", "fileChanges": {}}),
    ));
    assert!(take(&mut events, 1).await[0]["payload"].get("detail").is_none());
}

#[tokio::test]
async fn mcp_elicitation_requests_become_app_access_approvals() {
    let (_adapter, runtime, mut events) = lifecycle().await;
    runtime.emit(native(
        "evt-mcp-elicitation",
        "request",
        "mcpServer/elicitation/request",
        json!({"requestKind": "mcp-elicitation", "requestId": "req-safari", "turnId": "turn-1", "payload": {
            "mode": "form", "message": "Allow ChatGPT to use Safari?", "serverName": "computer-use", "threadId": "provider-thread-1", "turnId": "turn-1",
            "_meta": {"app_name": "Safari", "persist": ["session", "always"]}, "requestedSchema": {"type": "object", "properties": {}}
        }}),
    ));
    let event = take(&mut events, 1).await.remove(0);
    assert_eq!(event["payload"]["requestType"], "mcp_elicitation_approval");
    assert_eq!(event["payload"]["appName"], "Safari");
    assert_eq!(event["payload"]["detail"], "Allow ChatGPT to use Safari?");
    assert_eq!(
        event["payload"]["options"],
        json!([
            {"decision": "cancel", "label": "Cancel"}, {"decision": "decline", "label": "Decline"},
            {"decision": "acceptForSession", "label": "Always allow this session"}, {"decision": "acceptAlways", "label": "Always allow"},
            {"decision": "accept", "label": "Approve"}
        ])
    );
}

#[tokio::test]
async fn user_input_requests_answers_and_async_questions() {
    let (_adapter, runtime, mut events) = lifecycle().await;
    runtime.emit(native(
        "evt-user-input-empty",
        "notification",
        "item/tool/requestUserInput/answered",
        json!({"payload": {"answers": {"scope": {"answers": []}}}}),
    ));
    assert_eq!(take(&mut events, 1).await[0]["payload"]["answers"], json!({"scope": []}));

    runtime.emit(native(
        "evt-user-input-requested",
        "request",
        "item/tool/requestUserInput",
        json!({"requestId": "req-user-input-1", "payload": {"isBlocking": true, "itemId": "item-user-input-1", "threadId": "thread-1", "turnId": "turn-1", "questions": [
            {"id": "sandbox_mode", "header": "Sandbox", "question": "Which mode should be used?", "options": [{"label": "workspace-write", "description": "Allow workspace writes only"}]}
        ]}}),
    ));
    runtime.emit(native(
        "evt-user-input-resolved",
        "notification",
        "item/tool/requestUserInput/answered",
        json!({"requestId": "req-user-input-1", "payload": {"answers": {"sandbox_mode": {"answers": ["workspace-write"]}}}}),
    ));
    let seen = take(&mut events, 2).await;
    assert_eq!(seen[0]["type"], "user-input.requested");
    assert_eq!(seen[0]["requestId"], "req-user-input-1");
    assert_eq!(seen[0]["payload"]["questions"][0]["id"], "sandbox_mode");
    assert_eq!(seen[0]["payload"]["questions"][0]["multiSelect"], false);
    assert_eq!(seen[1]["payload"]["answers"], json!({"sandbox_mode": "workspace-write"}));

    runtime.emit(native(
        "evt-async-question",
        "notification",
        "item/completed",
        json!({"payload": {"completedAtMs": 0, "threadId": "thread-1", "turnId": "turn-1", "item": {
            "type": "agentMessage", "id": "async-question-1", "text": "Which package manager?\n- pnpm\n- npm\n\nWhat should it be named?", "phase": "final_answer", "delivery": "async",
            "questions": [{"title": "Which package manager?", "options": ["pnpm", "npm"]}, {"title": "What should it be named?"}]
        }}}),
    ));
    runtime.emit(native(
        "evt-async-continued",
        "notification",
        "item/agentMessage/delta",
        json!({"payload": {"threadId": "thread-1", "turnId": "turn-1", "itemId": "message-2", "delta": "I will keep working."}}),
    ));
    let seen = take(&mut events, 2).await;
    assert_eq!(seen[0]["type"], "user-input.requested");
    assert_eq!(seen[0]["requestId"], "codex-async:thread-1:async-question-1");
    assert_eq!(
        seen[0]["payload"],
        json!({"responseMode": "message", "questions": [
            {"id": "0", "header": "Question", "question": "Which package manager?", "options": [{"label": "pnpm", "description": ""}, {"label": "npm", "description": ""}], "allowCustomAnswer": true, "multiSelect": false},
            {"id": "1", "header": "Question", "question": "What should it be named?", "options": [], "allowCustomAnswer": true, "multiSelect": false}
        ]})
    );
    assert_eq!(seen[1]["type"], "content.delta");
}

#[tokio::test]
async fn windows_sandbox_failure_and_token_usage_snapshots() {
    let (_adapter, runtime, mut events) = lifecycle().await;
    runtime.emit(native(
        "evt-windows-sandbox-failed",
        "notification",
        "windowsSandbox/setupCompleted",
        json!({"message": "Sandbox setup failed", "payload": {"mode": "unelevated", "success": false, "error": "unsupported environment"}}),
    ));
    let seen = take(&mut events, 2).await;
    assert_eq!(
        (
            seen[0]["type"].as_str(),
            seen[0]["payload"]["state"].as_str(),
            seen[0]["payload"]["reason"].as_str()
        ),
        (Some("session.state.changed"), Some("error"), Some("Sandbox setup failed"))
    );
    assert_eq!(
        (seen[1]["type"].as_str(), seen[1]["payload"]["message"].as_str()),
        (Some("runtime.warning"), Some("Sandbox setup failed"))
    );

    runtime.emit(native(
        "evt-codex-thread-token-usage-updated",
        "notification",
        "thread/tokenUsage/updated",
        json!({"turnId": "turn-1", "payload": {"threadId": "thread-1", "turnId": "turn-1", "tokenUsage": {
            "total": {"inputTokens": 11_833, "cachedInputTokens": 3456, "outputTokens": 6, "reasoningOutputTokens": 0, "totalTokens": 11_839},
            "last": {"inputTokens": 120, "cachedInputTokens": 0, "outputTokens": 6, "reasoningOutputTokens": 0, "totalTokens": 126},
            "modelContextWindow": 258_400
        }}}),
    ));
    let usage = take(&mut events, 1).await.remove(0);
    assert_eq!(
        usage["payload"]["usage"],
        json!({"usedTokens": 126, "totalProcessedTokens": 11_839, "maxTokens": 258_400, "inputTokens": 120, "cachedInputTokens": 0, "outputTokens": 6, "reasoningOutputTokens": 0,
            "lastUsedTokens": 126, "lastInputTokens": 120, "lastCachedInputTokens": 0, "lastOutputTokens": 6, "lastReasoningOutputTokens": 0, "compactsAutomatically": true})
    );
}

#[tokio::test]
async fn keeps_consuming_events_after_start_session_returns() {
    let factory = Factory::default();
    let adapter = adapter_with(&factory, json!({}), CodexAdapterOptions::default());
    let mut events = adapter.subscribe();
    let spawned = adapter.clone();
    tokio::spawn(async move { spawned.start_session(start_input("thread-outlives-start")).await.unwrap() })
        .await
        .unwrap();
    let mut item = item_completed(
        "evt-after-start-session",
        json!({"type": "agentMessage", "id": "msg_after_start", "text": "emitted after startSession returned"}),
    );
    item["threadId"] = json!("thread-outlives-start");
    factory.last().emit(item);
    assert_eq!(take(&mut events, 1).await[0]["type"], "item.completed");
}

// -- lifecycle -----------------------------------------------------------------------------------------

#[tokio::test]
async fn stop_session_closes_the_runtime() {
    let (adapter, runtime, _events) = lifecycle().await;
    adapter.stop_session(&ThreadId::new("thread-1")).await.unwrap();
    assert_eq!(*runtime.closes.lock().unwrap(), 1);
    assert!(!adapter.has_session(&ThreadId::new("thread-1")).await);
}

#[tokio::test]
async fn construction_and_start_failures_are_process_errors() {
    let factory = Factory {
        fail_construction: true,
        ..Factory::default()
    };
    let adapter = adapter_with(&factory, json!({}), CodexAdapterOptions::default());
    assert!(matches!(
        adapter.start_session(start_input("thread-fail")).await,
        Err(AdapterError::Process { .. })
    ));
    assert!(!adapter.has_session(&ThreadId::new("thread-fail")).await);

    let factory = Factory {
        fail_start: true,
        ..Factory::default()
    };
    let adapter = adapter_with(&factory, json!({}), CodexAdapterOptions::default());
    assert!(matches!(
        adapter.start_session(start_input("thread-fail")).await,
        Err(AdapterError::Process { .. })
    ));
    assert_eq!(*factory.last().closes.lock().unwrap(), 1);
    assert!(!adapter.has_session(&ThreadId::new("thread-fail")).await);
}

#[tokio::test]
async fn native_events_reach_the_sink() {
    struct Sink(StdMutex<Vec<String>>);
    impl NativeEventSink for Sink {
        fn write(&self, event: &ProviderEvent) {
            self.0.lock().unwrap().push(serde_json::to_string(event).unwrap());
        }
    }
    let sink = Arc::new(Sink(StdMutex::new(Vec::new())));
    let factory = Factory::default();
    let adapter = adapter_with(
        &factory,
        json!({}),
        CodexAdapterOptions {
            native_event_sink: Some(sink.clone()),
            ..CodexAdapterOptions::default()
        },
    );
    let mut events = adapter.subscribe();
    adapter.start_session(start_input("thread-logger")).await.unwrap();
    let mut event = native("evt-native-log", "notification", "process/stderr", json!({"message": "native flush test"}));
    event["threadId"] = json!("thread-logger");
    factory.last().emit(event);
    take(&mut events, 1).await;
    assert!(sink.0.lock().unwrap()[0].contains("\"message\":\"native flush test\""));
}

// -- usage limits --------------------------------------------------------------------------------------

const NOW_SECONDS: i64 = 1_767_225_600;
const OUT_OF_CREDITS: &str = "Your workspace is out of credits. Ask your workspace owner to refill in order to continue.";

fn error_notification(id: &str, message: &str, info: Option<&str>) -> Value {
    let mut error = json!({"message": message});
    if let Some(info) = info {
        error["codexErrorInfo"] = json!(info);
    }
    native(
        id,
        "notification",
        "error",
        json!({"turnId": "turn-limit", "payload": {"threadId": "thread-1", "turnId": "turn-limit", "willRetry": false, "error": error}}),
    )
}

fn rate_limits(id: &str, reached: Option<&str>, primary: Option<(i64, i64)>, secondary: Option<(i64, i64)>) -> Value {
    let mut limits = json!({"limitId": "codex"});
    if let Some(reached) = reached {
        limits["rateLimitReachedType"] = json!(reached);
    }
    if let Some((used, resets)) = primary {
        limits["primary"] = json!({"usedPercent": used, "resetsAt": NOW_SECONDS + resets, "windowDurationMins": 300});
    }
    if let Some((used, resets)) = secondary {
        limits["secondary"] = json!({"usedPercent": used, "resetsAt": NOW_SECONDS + resets, "windowDurationMins": 10_080});
    }
    native(
        id,
        "notification",
        "account/rateLimits/updated",
        json!({"turnId": "turn-limit", "payload": {"rateLimits": limits}}),
    )
}

fn limit_turn_failed(id: &str, turn: &str) -> Value {
    native(
        id,
        "notification",
        "turn/completed",
        json!({"turnId": turn, "payload": {"threadId": "thread-1", "turn": {"id": turn, "items": [], "status": "failed", "error": {"message": OUT_OF_CREDITS, "codexErrorInfo": "usageLimitExceeded"}}}}),
    )
}

#[tokio::test]
async fn usage_limit_stops_name_the_exhausted_window() {
    let (_adapter, runtime, mut events) = lifecycle().await;
    runtime.emit(error_notification("evt-limit-error", OUT_OF_CREDITS, Some("usageLimitExceeded")));
    runtime.emit(rate_limits(
        "evt-limit-rate-limits",
        Some("workspace_owner_credits_depleted"),
        Some((40, 3_600)),
        Some((100, 5 * 86_400 + 5 * 3_600)),
    ));
    runtime.emit(limit_turn_failed("evt-limit-turn", "turn-limit"));
    runtime.emit(limit_turn_failed("evt-limit-turn-2", "turn-limit-2"));
    let seen = take(&mut events, 5).await;
    let expected = "Codex usage limit reached. The weekly limit resets in 5d 5h. The workspace has no credits to continue sooner: ask your workspace owner to add credits, or send the message again once the limit resets.";
    assert_eq!(
        seen.iter().map(|event| event["type"].as_str().unwrap()).collect::<Vec<_>>(),
        vec![
            "account.rate-limits.updated",
            "runtime.error",
            "turn.completed",
            "runtime.error",
            "turn.completed"
        ]
    );
    for event in &seen {
        if event["type"] == "runtime.error" {
            assert_eq!(event["payload"]["message"], expected);
            assert_eq!(event["payload"]["detail"], OUT_OF_CREDITS);
        }
        if event["type"] == "turn.completed" {
            assert_eq!(event["payload"]["errorMessage"], expected);
        }
    }
}

#[tokio::test]
async fn usage_limit_stops_read_earlier_snapshots_and_fall_back() {
    let (_adapter, runtime, mut events) = lifecycle().await;
    runtime.emit(error_notification("evt-plan-error", "You've hit your usage limit.", Some("usageLimitExceeded")));
    runtime.emit(rate_limits(
        "evt-plan-rate-limits",
        Some("rate_limit_reached"),
        Some((100, 3 * 3_600 + 20 * 60)),
        None,
    ));
    runtime.emit(limit_turn_failed("evt-plan-turn", "turn-limit"));
    let completed = until_type(&mut events, "turn.completed").await;
    assert_eq!(
        completed["payload"]["errorMessage"],
        "Codex usage limit reached. The session limit resets in 3h 20m. Send the message again once the limit resets."
    );

    let (_adapter, runtime, mut events) = lifecycle().await;
    runtime.emit(rate_limits("evt-early-rate-limits", None, Some((100, 3 * 3_600 + 20 * 60)), None));
    runtime.emit(rate_limits("evt-sparse-rate-limits", Some("rate_limit_reached"), None, None));
    runtime.emit(limit_turn_failed("evt-early-turn", "turn-limit"));
    let completed = until_type(&mut events, "turn.completed").await;
    assert_eq!(
        completed["payload"]["errorMessage"],
        "Codex usage limit reached. The session limit resets in 3h 20m. Send the message again once the limit resets."
    );

    let (_adapter, runtime, mut events) = lifecycle().await;
    runtime.emit(error_notification("evt-bare-error", OUT_OF_CREDITS, Some("usageLimitExceeded")));
    runtime.emit(limit_turn_failed("evt-bare-turn", "turn-limit"));
    let seen = take(&mut events, 2).await;
    let expected = "Codex usage limit reached. Send the message again once the limit resets.";
    assert_eq!(
        seen.iter().map(|event| event["type"].as_str().unwrap()).collect::<Vec<_>>(),
        vec!["runtime.error", "turn.completed"]
    );
    assert_eq!(seen[0]["payload"]["message"], expected);
    assert_eq!(seen[1]["payload"]["errorMessage"], expected);

    let (_adapter, runtime, mut events) = lifecycle().await;
    runtime.emit(error_notification(
        "evt-other-error",
        "Codex is temporarily unavailable.",
        Some("internalServerError"),
    ));
    let error = take(&mut events, 1).await.remove(0);
    assert_eq!(
        (error["payload"]["message"].as_str(), error["payload"]["class"].as_str()),
        (Some("Codex is temporarily unavailable."), Some("provider_error"))
    );
}

// -- managed mode --------------------------------------------------------------------------------------

#[tokio::test]
async fn managed_rotation_restarts_and_resumes_the_same_native_thread() {
    let revision = Arc::new(StdMutex::new("first".to_owned()));
    let current = revision.clone();
    let factory = Factory {
        resume_cursor: Some(json!({"threadId": "native-managed-thread"})),
        ..Factory::default()
    };
    let adapter = adapter_with(
        &factory,
        json!({}),
        CodexAdapterOptions {
            resolve_runtime: Some(Arc::new(move || {
                let revision = current.lock().unwrap().clone();
                Box::pin(async move {
                    Ok(CodexEffectiveRuntime {
                        config: settings(
                            json!({"binaryPath": "/t3/tools/codex/0.155.1/bin/codex", "homePath": "/t3/caches/codex/home", "launchArgs": "-c 'model_provider=managed'"}),
                        ),
                        environment: [("ACCESS_TOKEN".to_owned(), format!("dummy-{revision}"))].into_iter().collect(),
                        revision,
                    })
                })
            })),
            ..CodexAdapterOptions::default()
        },
    );
    let thread = ThreadId::new("managed-token-rotation");
    let mut input = start_input("managed-token-rotation");
    input.provider = None;
    adapter.start_session(input).await.unwrap();
    let turn = |text: &str| -> ProviderSendTurnInput { crate::model::from_json(json!({"threadId": "managed-token-rotation", "input": text})) };
    adapter.send_turn(turn("first")).await.unwrap();
    assert_eq!(factory.runtimes.lock().unwrap().len(), 1);
    *revision.lock().unwrap() = "rotated".into();
    adapter.send_turn(turn("second")).await.unwrap();
    let runtimes = factory.runtimes.lock().unwrap().clone();
    assert_eq!(runtimes.len(), 2);
    assert_eq!(*runtimes[0].closes.lock().unwrap(), 1);
    assert_eq!(runtimes[1].options.resume_cursor, Some(json!({"threadId": "native-managed-thread"})));
    assert_eq!(runtimes[1].options.environment.as_ref().unwrap()["ACCESS_TOKEN"], "dummy-rotated");
    assert_eq!(runtimes[1].options.binary_path, "/t3/tools/codex/0.155.1/bin/codex");
    assert_eq!(runtimes[1].options.service_tier, None);
    assert!(adapter.has_session(&thread).await);
}

#[tokio::test]
async fn managed_turn_failures_keep_the_sharing_limit_code() {
    let factory = Factory::default();
    let adapter = adapter_with(
        &factory,
        json!({}),
        CodexAdapterOptions {
            resolve_runtime: Some(Arc::new(|| {
                Box::pin(async {
                    Ok(CodexEffectiveRuntime {
                        config: settings(json!({})),
                        environment: Environment::new(),
                        revision: "managed".into(),
                    })
                })
            })),
            ..CodexAdapterOptions::default()
        },
    );
    let mut events = adapter.subscribe();
    adapter.start_session(start_input("thread-1")).await.unwrap();
    factory.last().emit(native(
        "managed-sharing-limit",
        "notification",
        "turn/completed",
        json!({"turnId": "turn-limit", "payload": {"threadId": "thread-1", "turn": {"id": "turn-limit", "items": [], "status": "failed", "error": {"message": "subscription_sharing_usage_limit_exceeded", "codexErrorInfo": "other"}}}}),
    ));
    let seen = take(&mut events, 2).await;
    assert_eq!(seen[0]["type"], "runtime.error");
    assert_eq!(seen[0]["payload"]["code"], "subscription_sharing_usage_limit_exceeded");
    assert!(seen[0]["payload"]["message"].as_str().unwrap().contains("ChatGPT usage limit"));
    assert_eq!(
        (seen[1]["type"].as_str(), seen[1]["payload"]["state"].as_str()),
        (Some("turn.completed"), Some("failed"))
    );
}
